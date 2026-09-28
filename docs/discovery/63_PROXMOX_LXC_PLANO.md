# 63 — O plano do provider LXC do Proxmox (ADR-0058)

**Data:** 2026-09-27 · **Base:** `origin/main` (`d3d6f394`, v4.4.0 + commits) · **Laboratório:**
cluster `lab` (`pve` 192.168.122.91 + `pve2` 192.168.122.55, PVE 9.2.2).

**O que é:** o plano para aplicar todas as correcções e melhorias que o spike do ADR-0058 mediu.
Cada fatia tem um portão de saída e uma prova ao vivo. **Não é uma autorização para construir**:
as decisões D1 a D4 abaixo vêm primeiro, e só a Fatia 0 avança sem elas, porque corrige um
defeito que já existe no caminho QEMU.

## A pergunta

O ADR-0058 decide que um container LXC do Proxmox **não** serve `kind: Container` (sem `exec`,
sem logs, sem código de saída, e fora do dataplane do motor), e que, se entrar, entra como um
recurso próprio de **system container**, com semântica próxima da de uma VM. A pergunta deste
plano é em que ordem se constrói esse recurso para que cada armadilha medida fique fechada por
código e travada por um teste antes do passo seguinte.

## As armadilhas medidas, e onde cada uma fecha

| # | Armadilha (ADR-0058) | Fecha na | Portão |
|---|---|---|---|
| T1 | Sem `exec`, sem logs, sem código de saída na API | Fatia 3 | capacidades `unsupported-by-provider` com a razão; os verbos recusam por nome |
| T2 | `entrypoint` e `env` substituídos em silêncio na criação a partir de OCI | Fatia 3 | criar sem eles, `PUT …/config`, reler, comparar; divergência é erro |
| T3 | Um arranque com DHCP falhado acaba em `WARNINGS: 1`, e o motor lê-o como falha | **Fatia 0** (#546) e 0b | terceiro veredicto de tarefa, com as linhas `WARN:` lidas do log; no SDN, as tarefas-filhas de cada nó |
| T4 | O pull do nó só aceita tags, perde o digest, não aceita credenciais, e reescreve o nome | Fatias 1 e 2 | o motor puxa e verifica a imagem e envia-a por `upload`; o `oci-registry-pull` não é usado |
| T5 | O nó recusa um arquivo com media types Docker v2 («Unsupported CPU architecture») | Fatia 1 | o arquivo enviado leva media types OCI; teste com um manifesto Docker v2 real |
| T6 | Cada container é uma cópia inteira da imagem | Fatia 3 | dito no detalhe da capacidade; nada a corrigir, só a não esconder |
| T7 | O dataplane do motor (SDN, isolamento, DNS, `publish`) não se aplica | Fatia 3 | capacidades `net.*` do recurso declaradas pelo que são: bridge e VLAN do nó |

## Decisões do dono, antes de qualquer código das Fatias 1 a 6

**Decididas a 2026-09-28** pelo dono: D1 aceite (o ADR-0058 passa a Accepted); D2 — a
necessidade é a **paridade com o Proxmox**: cobrir no motor o LXC que o operador já usa no nó,
para o gerir pela mesma API declarativa; D3 — `kind: SystemContainer` em
`compute.delonix.io/v1alpha1`, sem grupo de CLI novo na v1; D4 cumprida (#541 fundido).

- **D1 — Aceitar o ADR-0058.** As duas primeiras decisões dele (não é `Container`; é um recurso
  próprio) são o que este plano executa.
- **D2 — Nomear a necessidade.** O ADR-0049 D5 constrói fatias contra uma necessidade, não
  contra uma contagem de rotas. Sem uma necessidade escrita (quem usa, para quê), o plano pára
  depois da Fatia 0.
- **D3 — O nome.** Recomendação: `kind: SystemContainer` no grupo `compute.delonix.io/v1alpha1`,
  e **sem grupo de CLI novo na v1**: os verbos genéricos `get`/`describe`/`delete` e o
  `apply -f` já servem qualquer Kind (B1 da restruturação da CLI). Um grupo próprio só entra se o
  dia 2 o pedir. O nome não pode ser `LxcContainer`: nomeia o provider, e a fronteira do motor
  proíbe isso.
- **D4 — Fundir o #541 primeiro.** O `upload_import` do ADR-0057 já implementa a rota `upload`
  com o sha256 no corpo. A Fatia 2 generaliza-o; não escreve uma segunda.

## Fatia 0 — A tarefa tem três saídas (vale já, QEMU incluído)

**Porquê primeiro:** é um defeito do caminho que já existe, não do LXC. O `task_verdict`
(`crates/providers/delonix-proxmox/src/lib.rs`) lê tudo o que não seja `"OK"` como falha. Um
`vzstart` com DHCP falhado acaba com `exitstatus: "WARNINGS: 1"` e o container a correr. Para uma
VM, o mesmo terceiro estado seria um `TaskFailed` sobre um efeito que aconteceu, e a operação
seguinte (um reenvio) duplicaria a acção.

- `task_verdict` passa a devolver `Ok`, `Warnings(n)` ou `Failed(razão)`. `WARNINGS: <n>` com
  `n` numérico é o único formato aceite como aviso; qualquer outro texto continua a ser falha.
- Com `Warnings`, o cliente lê `GET /nodes/{node}/tasks/{upid}/log` e guarda as linhas `WARN:`.
  A rota já era chamada (`task_error_line`, `task_log`), por isso o inventário não muda.
- O livro de tarefas ganha o estado `ok_with_warnings { warnings }`. A chamada devolve sucesso e
  os avisos sobem ao chamador (`tracing::warn!` no crate, impressos pela CLI). Um aviso nunca é
  descartado em silêncio, e nunca é contado como falha.
- **Portão:** teste unitário do `task_verdict` com os três formatos; um cenário novo no
  `tests/failure_injection.rs` (tarefa `WARNINGS: 1` + log com uma linha `WARN:`), que lê o livro
  e não só o `Ok`; e a mesma leitura contra o histórico de tarefas do laboratório, para dizer se
  alguma tarefa QEMU real já acabou em `WARNINGS` (medido, não suposto).
- **Não precisa de D1 a D4.**
- **Feita no #546** (2026-09-27): o cenário de injecção chumba com a correcção revertida. A
  medição do histórico do laboratório deu 51 tarefas em `WARNINGS`, e o que mostrou está na
  Fatia 0b.

## Fatia 0b — Um apply de SDN espera pelo reload de cada nó

**Porquê:** das 51 tarefas em `WARNINGS` no histórico do laboratório, 50 são `srvreload
networking`, o reload de rede por nó que um `PUT /cluster/sdn` desencadeia. Os avisos incluem
`missing 'source /etc/network/interfaces.d/sdn' directive` (24) e `reloading frr configuration
failed` (5). A tarefa que o motor espera, `reloadnetworkall`, acabou em `OK` nas 61 vezes: o PVE
lança o reload de cada nó em segundo plano e não o acompanha (`PVE/API2/Network/SDN.pm`, com um
`FIXME` do próprio upstream). O `OK` de um apply quer dizer «pedidos enviados», e foi assim que os
applies do #493 e do #497 foram aceites e nunca realizados.

- Antes do `PUT /cluster/sdn`, o `apply_sdn` guarda por nó online as tarefas `srvreload
  networking` que já existem; depois da mãe, a filha de cada nó é a primeira que não estava na
  lista, e é esperada com os três veredictos da Fatia 0. A hora da mãe não serve para a
  encontrar: no histórico do laboratório as filhas chegam até 37 s depois dela e as do apply
  anterior até 5 s antes. Um nó sem essa tarefa dentro de um prazo é um erro com o nome do
  nó, nunca um sucesso.
- Os avisos das filhas sobem ao chamador com o nome do nó. Uma filha falhada faz o apply falhar,
  mesmo com a mãe em `OK`.
- **Portão:** cenário de injecção com a mãe em `OK` e uma filha em `WARNINGS`, e outro com uma
  filha falhada; ao vivo, um apply no laboratório com a directiva `source` retirada de um nó
  tem de devolver o aviso desse nó.
- **Espera pelo #542**, que está a mexer no apply de SDN. Não precisa de D1 a D4.
- **Feita** (2026-09-27): os três cenários de injecção passam e o da filha falhada chumba com
  a espera revertida; ao vivo, com a directiva retirada do `pve2`, o apply devolveu o aviso com
  `node=pve2`, e sem ela nenhum. Uma filha a falhar de verdade no nó ficou só por injecção.

## Fatia 1 — O arquivo que o nó aceita

- `delonix-oci` ganha a escrita de um arquivo com **media types OCI**. A conversão de Docker v2
  para OCI muda só os campos `mediaType` (manifesto, config e layers `tar+gzip`); os bytes da
  config e das layers ficam iguais, e o digest do manifesto muda. O arquivo regista os dois
  digests: o do registo (identidade da imagem) e o do manifesto convertido (identidade do
  ficheiro enviado).
- O `delonix image save` de hoje fica como está: é lido por `docker load` e `ctr import`, e não
  há motivo medido para mudar a sua saída. A escrita OCI é uma função à parte, com um só chamador
  (a Fatia 2), até alguém pedir um `image save --oci`.
- Uma layer que não tenha equivalente OCI simples (zstd, estrangeira) é **recusada por nome**
  até se medir o que o nó faz com ela.
- **Portão:** teste com um manifesto Docker v2 real capturado (o do `alpine:3.20`) que exige os
  media types OCI e os blobs byte a byte iguais; e, ao vivo, o arquivo produzido pelo motor a ser
  aceite pelo nó (`Detected OCI archive`), com o controlo em Docker v2 a ser recusado na mesma
  corrida.

- **Fechada a 2026-09-28.** `delonix_oci::write_oci_media_archive` (com `build_oci_manifest`
  e a conversão pura `to_oci_manifest`) devolve os dois digests (`OciArchive`): o id da imagem,
  que a conversão não muda, e o do manifesto convertido. O store não guarda o digest do
  manifesto de registo; é a Fatia 2, que puxa a imagem, que o conhece. Uma layer zstd ou
  estrangeira é recusada com DX-1409 a nomear o media type e a layer. O `image save` ficou com o
  Docker v2 (teste a exigi-lo). Prova ao vivo no `pve` (PVE 9.2.2), na mesma corrida e pela API
  (`pvesh create /nodes/pve/lxc`): o arquivo do motor para `alpine:3.20` foi aceite
  («Detected container architecture: amd64», `vzcreate` `OK`); o controlo Docker v2 do
  `image save` foi recusado com «Unsupported CPU architecture».

## Fatia 2 — O upload genérico, e uma imagem enviada uma vez

- O `upload_import` do #541 passa a `upload(storage, content, …)`, com `content` a ser `import`
  ou `vztmpl`. A resposta perdida resolve-se como lá: listar o conteúdo do storage.
- O nome do ficheiro é derivado do digest do manifesto convertido (`dlx-<digest>.tar`, só com os
  caracteres que o nó não reescreve — medido: o `upload` guardou o nome tal como enviado). Uma
  imagem já presente no storage não se envia outra vez, como no ADR-0057.
- O motor puxa a imagem com o seu próprio cliente: digest verificado, e credenciais de
  `delonix image login`. Isto fecha T4 inteiro: pinning por digest e registos privados passam a
  funcionar, porque o nó deixa de puxar.
- **Portão:** caso ao vivo de envio com o checksum certo (tarefa `imgcopy` `OK`) e **com um
  checksum errado**. No spike este pedido não chegou a criar tarefa (`http=000`, causa não
  isolada); a fatia só fecha quando esse caminho estiver medido e a recusa for visível.

- **Fechada a 2026-09-28.** O `upload_import` passou a `Client::upload(storage, UploadContent, …)`
  (`import` ou `vztmpl`), e a listagem a `list_content(storage, UploadContent)`.
  `Client::stage_template(storage, archive, manifest_digest)` recusa antes de enviar um byte:
  - um digest que não seja sha256;
  - um storage sem `vztmpl` (DX-6513);
  - um storage sem espaço (DX-6514).

  Nomeia o ficheiro `dlx-<hex>.tar` pelo digest do manifesto convertido e não reenvia um arquivo
  que o nó já tenha. Uma resposta perdida resolve-se a listar o storage.
- **O portão, medido a 2026-09-28 no `pve`** (PVE 9.2.2): o `http=000` do spike não se repete.
  Com o checksum errado o nó responde 200 com um UPID, e a tarefa `imgcopy` acaba em erro
  («checksum mismatch: got '…' != expect '…'»), sem guardar ficheiro nenhum. Com o certo acaba
  em `OK` e guarda o ficheiro com o nome enviado. O caso ao vivo
  `a_container_archive_is_staged_as_vztmpl_and_a_wrong_checksum_is_refused` corre as duas
  coisas; correu duas vezes seguidas (envio, depois cache).
- **O que fica para a Fatia 3**: ligar o pull do motor (`pull_from_registry_with_creds`, que já
  verifica o digest de um pin e usa as credenciais do `image login`) ao
  `write_oci_media_archive` e ao `stage_template`. A composição vive no provider, porque o
  `delonix-proxmox` não depende do `delonix-oci`. Só com essa ligação o T4 fica fechado de ponta
  a ponta.

## Fatia 3 — A porta e o provider

- **A porta** vive em `delonix-compute`, num ficheiro novo, na forma do ADR-0044 (spec, handle,
  observação, `capabilities()`). Não toca no `vm_backend.rs`, porque a P4b.3 está a mexer nesse
  sítio.
- **O provider** vive num módulo novo, `delonix-proxmox/src/lxc.rs`, como o `vm_firewall.rs` e o
  `sdn.rs`. O `lib.rs` tem 7 755 linhas e é o ponto de conflito de todos os PRs Proxmox.
- **Criar:** `ostemplate` do upload, `unprivileged: 1` sempre. Um pedido `unprivileged: 0` é
  recusado por nome: a fronteira de privilégio no nó remoto é uma decisão à parte, com spike
  próprio (guarda-rio 5).
- **Configurar (T2):** criar sem `entrypoint`/`env`, aplicá-los com `PUT …/config`, reler a
  config e comparar campo a campo. Uma divergência é erro, e o container criado é destruído,
  para não ficar um recurso a correr com a configuração errada.
- **Arrancar (T3):** a tarefa passa pelo veredicto da Fatia 0. Com rede pedida, o resultado é
  julgado por `GET …/interfaces`: sem IPv4 depois do arranque, o recurso fica com a condição
  `NetworkReady=False` e o motivo lido do log, e não com «a correr, está tudo bem».
- **Parar e destruir:** `shutdown` com prazo e depois `stop`; `DELETE` com `purge` e
  `destroy-unreferenced-disks`. Tudo pelo livro de tarefas existente.
- **Capacidades:** linhas novas `system-container.*` no catálogo, que passa a `1.1.0` (entradas
  novas sobem o minor). `exec`, logs e código de saída ficam `unsupported-by-provider` com a
  razão medida (T1). A cópia inteira fica no detalhe (T6). A rede declara bridge e VLAN do nó
  (T7). Os providers `linux`, `libvirt` e `cloud-hypervisor` respondem às linhas novas, porque o
  `match` sem curinga do catálogo obriga a isso.
- **Portão:** um caso no `tests/live.rs` que percorre criar → configurar → arrancar → observar →
  parar → destruir com o trace ligado, e lê o livro no fim; cenários de injecção para T2
  (config relida diferente) e T3 (arranque com avisos); o gate de evidência do ADR-0050 verde.

- **Fechada a 2026-09-28.**
  - **Porta.** `delonix_compute::system_container`: `SystemContainerSpec` (arquivo OCI local +
    digest do manifesto, entrypoint, env, recursos, rede, `unprivileged`),
    `SystemContainerHandle { locator }`, `SystemContainerObservation { running, network }` com
    `NetworkState::{NotRequested, Ready, NotReady{reason}, Unknown}`. O `start` e o `observe`
    recebem o spec, porque é ele que diz se foi pedida uma rede por DHCP.
  - **Provider.** `delonix-proxmox/src/lxc.rs`: os métodos `lxc_*` do cliente, passados pelo
    livro de tarefas (`vzcreate`, `vzstart`, `vzshutdown`, `vzstop`, `vzdestroy`), e o
    `ProxmoxSystemContainerProvider`. O `task_inner` passou a devolver as linhas `WARN:`
    (`task_collecting_warnings`), para o arranque julgar a rede com elas.
  - **Recusas por nome (DX-1540):** `unprivileged: false`, um argumento do entrypoint com
    espaços, um nome de variável fora de `[A-Za-z0-9_]`, um hostname inválido. Uma config relida
    diferente do pedido destrói o container e acaba em erro que nomeia o campo.
  - **Catálogo 1.2.0.** O domínio `system-containers` e oito entradas `system-container.*`.
    Todos os providers respondem.
- **Medido no `pve` antes de escrever**, pela API:
  - o `env` é uma lista separada por NUL e o `entrypoint` uma linha, e o `PUT …/config` responde
    `null`;
  - o `vzstart` sem servidor DHCP no `vmbr0` acaba em `WARNINGS: 1` ao fim de cerca de 2 min,
    com o container a correr e só IPv6 link-local no `eth0`;
  - um shutdown com prazo de um init `sleep` acaba em «container did not stop»;
  - o delete com `purge` e `destroy-unreferenced-disks` leva o volume.
- **Portão.**
  - Caso ao vivo `a_system_container_runs_its_lifecycle_through_the_node`: criar, configurar,
    arrancar (`NotReady` com o aviso do nó), observar, parar e destruir. Passou em 134 s, com o
    trace ligado e o livro lido no fim.
  - Injecção T2 (config relida diferente → destruído) e T3 (arranque com avisos → `NotReady`);
    cada uma falha com a correcção revertida.
  - Gate de evidência do ADR-0050 verde. Matriz de rotas regenerada: as 9 rotas `…/lxc` do ciclo
    de vida passam a `supported+tested` (170 de 675).
- **O que fica para a Fatia 4**: puxar a imagem (`pull_from_registry_with_creds`, com o digest
  verificado) e escrevê-la com `write_oci_media_archive` antes de chamar a porta. O `-bin` é o
  único que conhece os dois lados.

## Fatia 4 — O Kind

Segue a lista de 12 passos de `docs/dev/adding-a-kind.md`: a linha `KindFacts`, o spec com
`JsonSchema`, `desired`/`actual`/`converge`, `hot_fields`, `presence`, os verbos genéricos, o
`drift`, o schema regenerado e o `pt.po`.

- **Campos quentes** (convergem sem recriar): `memory`, `swap`, `cores`. Antes de os declarar
  quentes, mede-se se o nó os aplica ao cgroup de um container a correr; o que não aplicar fica
  frio.
- **Campos frios:** `image`, `entrypoint`, `env`, `rootfs`. Mudá-los planeia um `Replace`, recusado
  sem `--replace`, como nos outros Kinds. Uma classe «reinicia para aplicar» é o ADR-0012
  (proposto) e não entra aqui.
- **Portão:** `stack apply` seguido de `stack plan --detailed-exitcode` com código 0; um
  `PUT …/config` feito à mão no nó dá código 2 no `plan` e aparece no `delonix drift`; `delete`
  deixa o nó sem container nem volume.

- **Fechada a 2026-09-28.** O `kind: SystemContainer` (`compute.delonix.io/v1alpha1`, `sc`,
  grupo `systemContainers`), em `bins/delonix-runtime-bin/src/cmd/system_container.rs`. O módulo
  é a raiz de composição: lê a mesma configuração Proxmox que as VMs e constrói o
  `ProxmoxSystemContainerProvider`.
  - **O pull é do motor:** `resolve_or_pull`, com o digest verificado e as credenciais do
    `image login`, e depois `write_oci_media_archive` para uma cache por digest.
  - **O registo guarda o localizador e o que foi declarado.** O `actual()` lê do nó a memória, a
    swap, os cores, o entrypoint e o env, através de métodos novos da porta (`configuration`,
    `resize`).
  - **O entrypoint e o env só entram na comparação quando são declarados.** Se não forem, o nó
    tem os da imagem, e compará-los com «nada» daria deriva em todos os planos.
- **Campos quentes, medidos antes de os declarar**, num container a correr no `pve`: `memory` e
  `swap` chegam ao cgroup logo, `cores` chega ao `cpuset` passados uns segundos, e nada fica
  pendente. `image`, `entrypoint`, `env`, `rootfs` e `network` são frios.
- **Portão, pela CLI, contra o `pve`, com root isolado:**
  - `stack apply` cria e arranca (18 s); `stack plan --detailed-exitcode` dá 0.
  - Um `PUT …/config memory=384` feito à mão faz o `plan` dar 2 (`memory: 384 → 256`), e o
    `delonix drift` mostra-o com código 2.
  - O `apply` converge a quente e o `plan` volta a 0.
  - A `memory` do manifesto a 512M converge a quente, com o container a correr e o cgroup a
    536870912.
  - Um `rootfs` diferente planeia `Replace`, que é recusado sem `--replace`, sem mexer em nada.
    Com `--replace` o container é destruído e recriado com 2G.
  - `delete systemcontainers` deixa o nó sem container e sem volume, e o registo vazio.
  - A recusa sem provider configurado sai com código 69, traduzida.

## Fatia 5 — Dia 2, só a pedido

Cada item entra quando for pedido, com o seu caso ao vivo: snapshots e rollback (medidos no
spike em `local-lvm`), a firewall por container (as rotas `…/lxc/{vmid}/firewall/*`, pelo
caminho do ADR-0052), backup com `vzdump`, clone e template, redimensionar o disco, e migração
entre nós (o LXC migra com reinício, não ao vivo; a medir antes de prometer).

## Fatia 6 — Fecho

- A matriz regenerada com o trace: as rotas usadas passam a `supported+tested`, e as restantes
  ficam `unsupported-by-design` a citar o ADR-0058 em vez do ADR-0049 D4.
- O ADR-0058 passa a `Accepted` com a data e os PRs de cada fatia.
- Uma secção no `AGENTS.md`, e checks no `scripts/e2e.sh` para as recusas que não precisam de
  nó (um `exec` num system container, `unprivileged: 0`, uma imagem com layers zstd).

- **Fechada a 2026-09-28.** A matriz já tinha as 9 rotas LXC do ciclo de vida em
  `supported+tested` (Fatia 3, trace do caso ao vivo); a razão das 53 restantes passou a nomear o
  ADR-0058 e o `kind: SystemContainer` em vez do ADR-0049 D4. O ADR-0058 ganhou a secção
  «Implementation» com os PRs de cada fatia e o que ficou medido e por medir. No `scripts/e2e.sh`,
  uma secção de 7 checks sem nó. **Apanhou um defeito da Fatia 4**: `unprivileged: false` era
  «campo desconhecido — ignorado», e com um nó o `apply` criava um container sem privilégio com
  código 0. Passou a DX-1540, antes de resolver o provider. O `exec` num system container não
  tem verbo (não há o que recusar pela CLI); o check lê a linha `system-container.exec`
  `unsupported-by-provider` do `provider describe`. A layer zstd fica no teste unitário
  `a_zstd_or_foreign_layer_is_refused_by_name`: pelo Kind só se chega lá com um nó. O laboratório
  ficou limpo: o token `dlxs2` revogado e os arquivos `dlx-*` de teste apagados do `local`.

## Rotas que o plano chama

Cerca de 13 das 62 rotas LXC: listar, criar, destruir, `config` (GET e PUT), `status/current`,
`start`, `stop`, `shutdown` e `interfaces`, mais `tasks/{upid}/log` e o `upload` e o `content` do
storage (partilhados com o ADR-0057). As restantes 49 só entram pela Fatia 5, cada uma com
necessidade própria.

## Fora do plano, de propósito

- Servir `kind: Container` a partir do LXC (a decisão 1 do ADR-0058).
- Emular `exec` ou logs por uma consola websocket (`termproxy`/`vncproxy`): não dá código de saída
  nem separa as streams.
- Entrar no nó por SSH e usar `pct exec`: fica fora da API, e o ADR-0049 D3 e o ADR-0010 mantêm-no
  fora do motor.
- Containers privilegiados, `nesting`, e o `oci-registry-pull` do nó.

## Riscos e dependências

- **Conflitos:** os PRs Proxmox abertos (#541 e #542) mexem nos mesmos cinco sítios (`codes.rs`,
  `pt.po`, `error.rs`, `lib.rs`, `tests/live.rs`). As fatias vão uma de cada vez, com rebase
  antes de cada push.
- **P4b.3** está a mexer na porta `VmBackend`. A porta nova fica num ficheiro próprio.
- **Laboratório:** na `vmbr0` o filtro anti-spoof do host larga as tramas de um convidado
  aninhado, por isso um DHCP dentro de um container nunca responde lá. Os casos de rede usam a
  `vmbr1` (NAT dentro do `pve`) com IP fixo. Depois de um reinício do host, as duas VMs do
  laboratório ficam desligadas, e o cluster só tem quórum com as duas ligadas.
- **A causa do `http=000`** no upload com checksum errado não foi isolada. É o primeiro trabalho
  da Fatia 2, não um detalhe.
