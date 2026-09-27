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
| T3 | Um arranque com DHCP falhado acaba em `WARNINGS: 1`, e o motor lê-o como falha | **Fatia 0** | terceiro veredicto de tarefa, com as linhas `WARN:` lidas do log |
| T4 | O pull do nó só aceita tags, perde o digest, não aceita credenciais, e reescreve o nome | Fatias 1 e 2 | o motor puxa e verifica a imagem e envia-a por `upload`; o `oci-registry-pull` não é usado |
| T5 | O nó recusa um arquivo com media types Docker v2 («Unsupported CPU architecture») | Fatia 1 | o arquivo enviado leva media types OCI; teste com um manifesto Docker v2 real |
| T6 | Cada container é uma cópia inteira da imagem | Fatia 3 | dito no detalhe da capacidade; nada a corrigir, só a não esconder |
| T7 | O dataplane do motor (SDN, isolamento, DNS, `publish`) não se aplica | Fatia 3 | capacidades `net.*` do recurso declaradas pelo que são: bridge e VLAN do nó |

## Decisões do dono, antes de qualquer código das Fatias 1 a 6

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
  É uma rota nova no inventário (`tasks/{upid}/log`).
- O livro de tarefas ganha o estado `ok_with_warnings { warnings }`. A chamada devolve sucesso e
  os avisos sobem ao chamador (`tracing::warn!` no crate, impressos pela CLI). Um aviso nunca é
  descartado em silêncio, e nunca é contado como falha.
- **Portão:** teste unitário do `task_verdict` com os três formatos; um cenário novo no
  `tests/failure_injection.rs` (tarefa `WARNINGS: 1` + log com uma linha `WARN:`), que lê o livro
  e não só o `Ok`; e a mesma leitura contra o histórico de tarefas do laboratório, para dizer se
  alguma tarefa QEMU real já acabou em `WARNINGS` (medido, não suposto).
- **Não precisa de D1 a D4.**

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
