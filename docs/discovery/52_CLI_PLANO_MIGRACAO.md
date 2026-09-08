# Plano de migração da CLI — das 243 folhas às 103 da especificação

Medido a 2026-08-27 contra o binário da fatia em curso (243 folhas), não contra
a especificação lida por cima. O inventário da fase CLI-0 está no
[51_CLI_INVENTARIO.md](51_CLI_INVENTARIO.md); este documento é o **sequenciamento**.

## 1. Os dois números, e o que os separa

| | |
|---|---|
| folhas hoje | **243** |
| folhas que a especificação pede | **103** |
| já na forma nova | **61** |
| a especificação pede e não existe | **42** |

**A diferença de 140 folhas não é excesso de funcionalidade — é a mesma
capacidade escrita várias vezes.** Quatro grupos (`net` 41, `image` 32,
`container` 29, `vm` 28) são 130 folhas, mais de metade da árvore, e é aí que
está quase toda a redução.

**A contagem de "folhas hoje" já não é 243.** `scripts/cli_baseline.tsv`
tem, a 2026-09-01, **249** — subiu com capacidade nova do B3/dia-2 (e o MCP),
desceu com os cortes do B4-B9 abaixo; os dois movimentos não se cancelaram. A
proporção "243→103" e o `57%`/`~80%` da secção 4 ficam por remedir — não se
recalcularam aqui para não inventar um número sem contar folha a folha de
novo.

## 2. O que decide a ordem

Três restrições, e nenhuma é preferência:

**Um corte só é honesto quando o destino já faz o que a origem fazia.** Remover
`network ls` antes de `get networks` cobrir a `Network` tira capacidade em vez de
a arrumar. Portanto: **construir primeiro, cortar depois** — sempre.

**O que está declarado estável não se corta fora de um major.** O
`docs/cli-stability.md` promete os atalhos de topo (`ps`, `run`, `exec`, `logs`,
`rm`, `images`), os verbos de ciclo de vida do container, os exit codes e a saída
JSON do `inspect`. Declara NÃO estáveis o `cluster`, `vm`, `pod`, `workload`,
`storage`, `sharevolume` e `net` — e é essa lista que diz o que pode mover agora.

**O CI só corre em PR para a `main`.** PR empilhados não têm verificação
nenhuma, e `0 checks` lê-se como `0 falhas`. Cada bloco vai a PR contra a `main`.

## 3. Os blocos, por ordem

### B1 — Verbos genéricos, o resto dos Kinds  ·  ADITIVO  ·  FECHADO

Levar o `get`/`describe`/`delete` de 9 para os 12 Kinds que têm estado próprio.
**Medido de novo antes de tocar em código (2026-08-31)**: o texto original
estava desactualizado em dois pontos — `describe` de `Secret` e `Image` já
estavam wired (`SecretCmd::Inspect`/`ImageCmd::Describe`). Os gaps reais eram
três: `NetworkRoute` (describe+delete), `NetworkPolicy` (get/describe/delete,
o desenho por decidir) e o `describe` de `KubernetesCluster` (não existia
NENHUMA função de describe por-cluster).

**`NetworkPolicy` resolvido**: `get networkpolicies` passou a combinar as duas
direcções por container numa só tabela (`firewall::list_all_policies`) — o
`net ingress ls`/`net egress ls` continuam intocados, uma direcção de cada
vez. Sem registo próprio (`FirewallPolicy` não persiste identidade de
documento — mesma razão que levou o `NetworkAccessRule` a precisar de
`origin`), a identidade endereçável por `describe`/`delete networkpolicies` é
`<target>/<direction>` (ex.: `web/ingress`), não um nome de documento.
`NetworkRoute` usa a mesma lógica já existente, `<from>-><to>`
(`netroute::route_name`, a identidade que `stack plan` já imprimia).

**Fechou o critério de saída da CLI-2. Nada quebrou** — sem subcomando nativo
novo em nenhum dos três Kinds, só parametrização dos verbos genéricos
já existentes.

### B2 — Renomeações que não tocam em contrato  ·  QUEBRA MENOR  ·  FECHADO

**Medido de novo antes de tocar em código (2026-08-31, agente `Explore` +
leitura directa de `main.rs`/`scripts/cli_baseline.tsv`): as 9 renomeações já
estavam TODAS feitas** — sessões anteriores já as tinham executado (dois
comentários no código citam "B2 da reestruturação da CLI" explicitamente,
`system.rs:258-263` e `system.rs:271-277`) e o documento nunca foi
actualizado para o reflectir.

| de | para | estado |
|---|---|---|
| `dash` | `dashboard` (§22) | feito — `main.rs:339` |
| `syntax <editor>` | `completion editor <editor>` (§21) | feito — `main.rs:59-73` |
| `completion <shell>` | `completion shell <shell>` | feito, mesmo braço |
| `namespace` | `system namespace` | feito — `system.rs:265` |
| `net boot` | `system boot` | feito — `system.rs:271`, cita B2 no comentário |
| `volumes` | `volume` | feito — `main.rs:144` |
| `image ls` / `image rm` | `image list` / `image remove` | **revertido** em v2.0.0 (`docs/releases/v2.0.0.md`) — `image ls`/`image remove` são a forma canónica |
| `schema print` | `manifest schema` | feito — `schema.rs`'s `SchemaCmd` já não está ligado a nenhum comando de topo |
| `restore` (raiz) | `backup restore` | feito — `rbackup.rs:806`, documentado no AGENTS.md |

Corrigidas de caminho duas secções do `AGENTS.md` que ainda usavam as grafias
antigas como título (`## delonix net boot` e `` `delonix volumes` `` na lista
de comandos) — história preservada no corpo do texto, só o título/rótulo
corrigido para a grafia actual.

**Nenhum código tocado neste bloco — era só actualizar o registo.**

### B3 — Capacidade nova: os grupos que não existem  ·  ADITIVO  ·  EM CURSO

| grupo | folhas | natureza | estado |
|---|---|---|---|
| `backup` | 6 | consolida o `backup`/`restore` de raiz + `list`/`inspect`/`schedule`/`remove` | já feito antes deste plano (ver AGENTS.md) |
| `diff` | 1 | as três faces (desired/last-applied/observed) de UM recurso nomeado | **FECHADO** — PR #200 |
| `system metrics` | 1 | `DashSummary` cru, `-o json`/tabela | **FECHADO** — PR #201 |
| `cluster` day-2 | 5 | `kubeconfig`/`health`/`upgrade`/`drain`/`uncordon` | `kubeconfig` já existia; **`health` FECHADO** — PR #202; `upgrade`/`drain`/`uncordon` por fazer — **a citação do ADR-0010 aqui era MÁ ATRIBUIÇÃO** (medido 2026-09-03): esse ADR recusa a API de gestão REMOTA (`delonix-mgmt` alcançável de fora do host); `drain`/`uncordon` não precisam sequer de SSH (`cluster kubeconfig` já cacheia localmente — são `kubectl --kubeconfig=<cache> drain/uncordon` directos) e `upgrade` reusa o MESMO `cmd/remote.rs::SshTarget` que `kubeadm_init`/`join` já usam. Nada bloqueado; por construir |
| `config` | 5 | contextos — **precisa de ADR**, ver §5 | **`output` FECHADO** (local-only, sem ADR reaberto) — PR #203; `namespace` fica de fora de propósito, sem ponto de leitura único |
| `system state` | — | — | **não se constrói** — já respondido por `system info` (ver plano da fatia 1) |
| `pod` day-2 | 4 | `exec`/`attach`/`cp`/`port-forward` | `exec`/`attach`/`cp` já existiam antes deste plano; só `port-forward` por fazer |
| `vm` day-2 | 3 | `pause`/`resume`/`migrate` | por fazer |
| `network` | 2 | `diagnose`/`capture` (o `flow` existe em `net flow`) | `diagnose` já existia; `capture` por fazer |
| `image sign` · `secret rotate` | 2 | | por fazer |

**Fatia 1 fechada (2026-08-31, PRs #200/#201/#202/#203)**: `diff`, `system
metrics`, `cluster health` e `config` (só `output`) — a decisão de âmbito de
cada um está no plano de execução dessa fatia, não repetida aqui. O `config`
nasceu **local-only**, sem reabrir o ADR-0010: não havia um consumidor
concreto para um contexto remoto, e `cluster drain`/`uncordon` ficam pela
mesma razão. O `diff` reaproveita o motor de diff de 3 vias que `stack plan`
já tinha (`cmd/reconcile.rs::diff_fields`) — zero motor novo.

**Por fazer nesta fatia**: as quatro peças grandes — `pod port-forward`
(precisa de um encaminhador processo-do-host↔netns-do-pod novo), `vm
pause`/`resume`/`migrate` (`migrate` pode não ser viável sem mecanismo de
live-migration/storage partilhado — por confirmar antes de desenhar),
`network capture` (privilégio/ferramenta a confirmar), `image sign`
(manuseio de chave privada — zero código de assinatura hoje, só `image
verify`), `secret rotate` (rotação de VALOR, distinta do `rotate-key` de
master-key já existente).

### B4–B9 — remedidos a 2026-09-01 (agente `Explore`, `origin/main` fresco)

**Esta secção descrevia B4–B9 como blocos por começar, e estava errada.**
Todos os seis já foram medidos e parcialmente executados — v0.68.0/v0.69.0
(29-30/08) cortaram B4/B5/B6/B7, e a v1.0.0 (29/08) executou o B8 e **rejeitou**
o B9 — tudo isto ANTES de o B1/B3 desta ronda sequer começarem (31/08 em
diante). Quem leu esta secção por cima re-derivava conclusões já assentes, ou
podia reabrir uma decisão já tomada (o B9). Números confirmados por leitura
directa do código e dos commits, não pelo `docs/releases/*` sozinho.

### B4 — O colapso do `net`  ·  QUEBRA  ·  prometidas −41, entregues **−4**

Os dois bloqueios que esta secção citava (`get networkpolicies`, `network
diagnose`) estão fechados e confirmados reais — mas não eram o bloqueio a
sério. **O bloqueio real, medido no v0.68.0**: `net ingress`/`net egress` são
**incrementais** (`allow`/`deny` acrescentam uma regra, "o último comando
ganha"), enquanto a única Kind declarativa que existia — `FirewallPolicy` —
**substitui o estado inteiro de uma direcção** a cada apply, e recusa dois
documentos para o mesmo (alvo, direcção). Duas chamadas incrementais não se
tornam duas aplicações de `FirewallPolicy` sem a segunda apagar a primeira.

Só **4 das 41** eram genuinamente duplicadas: `net tunnel ls/describe/rm` →
`get/describe/delete gateways`, `net httproute ls` → `get httproutes`
(`cmd/tunnel.rs`/`cmd/httproute.rs`, os `Ls`/`Describe`/`Rm` saíram dos enums
com comentário a citar o B4). O `kind: NetworkAccessRule` (ADR-0028,
31/08) nasceu justamente para reabrir este bloco — uma regra INCREMENTAL,
por-documento, com `origin` para se poder remover sem apagar as dos outros —
mas a colapsagem do `net ingress`/`net egress` sobre ela **não foi feita**; a
própria ADR-0028 di-lo: "se vale a pena trocar 17 leaves imperativos por
manifestos precisa da sua própria medição, não de uma suposição."

**Falta no próprio número do plano**: `l4guard` (3 leaves — `set`/`clear`/
`status`) nunca entrou na conta dos 41, e a ADR-0028 diz textualmente que é
"um mecanismo diferente" — fora do alcance do `NetworkAccessRule`.

**As três decisões em aberto — RESOLVIDAS (`docs/adr/0029-net-ingress-egress-collapse.md`)**:
(1) `net ingress/egress allow/deny` passam a escrever pela MESMA
contabilidade do `NetworkAccessRule` (`origin` sintético derivado do
`(dir, proto, port, src)`) — unifica os dois caminhos de mutação sem tirar
nem uma folha da CLI nem mudar UX nenhuma; (2) `publish`/`unpublish` (DNAT)
ficam FORA do `NetworkAccessRule` PERMANENTEMENTE — são outra grão (mapeamento
de porta, não decisão allow/deny) e nunca deviam ter contado nas "−41"
prometidas; (3) `net netns` fica VISÍVEL — é ferramenta de diagnóstico
documentada como tal, não plumbing interna (essa já está oculta antes do
clap). Nenhuma das três abre trabalho de corte de folhas novo — a única
mudança de código é a fusão dos dois caminhos de mutação do ponto (1).

### B5 — O colapso do armazenamento  ·  QUEBRA  ·  prometidas −22, entregues **−7**

`storage apply` saiu no v0.68.0 (idêntico byte-a-byte a `volume::apply`);
`storage dash/inspect/rm` saíram no v1.1.0 ("colapsam no `volume` — duplicados
genuínos, medidos campo a campo", `docs/releases/v1.1.0.md`; `volume inspect`
ganhou `device`/`options` para não perder o que só o `storage inspect` tinha).

**O que resta (`storage create`/`ls` — 2 leaves; `sharevolume apply/ls/
describe/rm/migrate` — 5 leaves) foi medido e não é duplicado**: `storage
create` tem onboarding de credenciais NFS/CIFS/SMB/WebDAV que `volume create`
não tem; `sharevolume` tem semântica de namespace/quota/storage-pai e um
`migrate` sem equivalente nenhum nos verbos genéricos. Colapsar mais exige
DESENHO novo (credenciais de rede dentro de `volume apply -f`, filtro
`--network-only`/coluna `DEVICE` em `volume ls`, um lar para `migrate`), não
apagar código.

### B6 — `image --vm` e o `build`  ·  QUEBRA  ·  prometidas −11, entregues **−1**

`vm build` (grupo de raiz) saiu no v0.69.0 — idêntico a `image --vm build`
(a única diferença aparente, `-v/--verbose`, já era no-op nos dois: os dois
leem `$DELONIX_VERBOSE` via `Progress::new()`).

**Achado novo nesta remedição, ainda por cortar**: `image --vm build` (a
flag antiga) e `image vm build` (o subcomando aninhado mais recente) são hoje
**duas grafias da MESMA operação** — `ImageCmd::Vm` espelha `VmSub` 1:1
(`image.rs`). Nenhuma release cortou uma das duas; é o duplicado barato que
falta medir e fechar antes da parte cara deste bloco.

`image build --type container|virtual-machine` **não existe e é desenho
novo, não corte**: os flag-sets divergem a sério (`BuildArgs` é forma
Dockerfile/Delonixfile; a variante VM tem ~18 flags próprias — distro,
cloud-init, receita dourada — sem equivalente do lado container). O v0.69.0
já mediu isto e concluiu que unificar sob um `--type` é engenharia nova, fora
do âmbito de "só cortar duplicados".

### B7 — Day-2 puro: `vm`, `cluster`, `pod`  ·  QUEBRA  ·  **FECHADO** — prometidas −25, entregues **−8**

`vm rm`/`vm describe`, `pod rm`/`pod describe`, `cluster ls`/`cluster delete`
saíram no v0.69.0 — confirmados idênticos aos genéricos `get`/`describe`/
`delete`, corpo extraído para uma função `pub(crate)` que o verbo genérico
chama directamente.

**Fechado a 2026-09-03 com a disciplina que faltava: as 40 leaves reais dos
três grupos (não as ~37 estimadas — a própria contagem nunca tinha sido
recontada a partir dos `enum`s) medidas UMA A UMA contra o que `get`/
`describe`/`delete` já cobrem**, não por julgamento de bloco. Achado: **38
são IRREDUTÍVEIS** (a maioria já suspeitada, mas nunca confirmada leaf a
leaf) e **2 eram cortáveis e tinham escapado à passagem anterior**:

- **`vm status` — CORTADO, sem alias.** Duplicava `get vms` (mesma chamada,
  `delonix_vm::list` → `status()` por VM) sem nome, e `describe vms <nome>`
  com o nome — zero flag própria, zero diferença de dados.
- **`pod ls` — CORTADO, sem alias, com um passo de "construir antes de
  cortar" primeiro.** Só diferia de `get pods` por aceitar `--namespace`, que
  o `get` genérico não tinha para NENHUMA Kind. Corrigido na origem: `delonix
  get <kind> -n <namespace>` passou a existir (recusado, nunca ignorado em
  silêncio, nas Kinds sem namespace — `Secret`, por exemplo), e só depois
  `pod ls` foi removido, coerente com a regra §2 deste documento ("um corte
  só é honesto quando o destino já faz o que a origem fazia").

**Os 38 restantes ficam, e agora com a razão escrita ao lado de cada um** (a
tabela completa da auditoria fica fora deste documento — o resumo por
categoria): sessões interactivas reais (`console`/`ssh`/`vnc`/`exec`/
`attach`), efeitos colaterais que um verbo de leitura não pode ter
(`stop`/`start`/`restart`/`bridge`/`unbridge`/`snapshot create`/`rm`/
`restore`), I/O de rede ao vivo que um `get` recusa de propósito
(`vm ls --ports`, `cluster health`), ou objectos sem Kind nenhuma
(imagens de VM, snapshots, ficheiro de kubeconfig, `kube generate`,
`vm default-backend`). `cluster kubeadm`/`kube`/`kubeconfig`/`health` ficam
por esta última razão. `upgrade`/`drain`/`uncordon` não são candidatos de B7
— são capacidade nova do B3 (ver a correcção na tabela do B3 acima: a
citação do ADR-0010 era má atribuição).

### B8 — Os atalhos de raiz e o `workload`  ·  QUEBRA DE CONTRATO  ·  **FECHADO na v1.0.0**

**Já não está bloqueado nem pendente — já aconteceu.** `ps`, `run`, `exec`,
`logs`, `rm`, `images` saíram da raiz na v1.0.0 (29/08), corte limpo sem
alias: a grafia antiga falha com `unrecognized subcommand`, nunca em
silêncio. `docs/cli-stability.md` já não os lista como atalhos — regista a
própria quebra e a tabela de migração (`ps`→`container ps`, etc.).

**`workload` foi avaliado e mantido de propósito**, não esquecido: é o único
caminho que desambigua um Container e uma VM com o MESMO nome — `get`/
`describe`/`delete` respondem por Kind, `workload` responde por nome através
dos dois. Cortá-lo precisa de uma ADR sucessora que nomeie a perda; uma linha
de plano não chega (`docs/releases/v1.0.0.md`).

### B9 — Exit codes colidentes  ·  **REJEITADO na v1.0.0, decisão fechada**

`exitcode.rs` já tinha, à data desta proposta, um esquema fechado e
exaustivamente testado (LSB: `3` não-corre, `4` não-existe, `5` conflito;
`sysexits.h`: `69`/`74`/`77`/`124`) — construído DEPOIS do texto do B9, e o
próprio módulo avalia e recusa o pedido literal: remapear `2/4/5` exigiria
primeiro separar o `Error::Invalid` (643 sítios, duas classes distintas
fundidas) em vez de um simples remap. Os exit codes já estavam na tabela
*Estável* do `cli-stability.md` desde a v0.49.0. **Nada muda aqui** —
decisão fechada, não uma pendência.

## 4. O que pode sair no próximo bump

**Esta secção ficou parcialmente ultrapassada pela execução (2026-08-31)**: o
`config` e o `diff` — que aqui apareciam como "NÃO cabe" — já foram fechados na
fatia 1 do B3, com âmbito reduzido (`config` só `output`, local-only, sem
reabrir o ADR-0010). O texto original fica abaixo por registo do raciocínio,
não como lista actual de pendências — essa está na tabela do B3, acima.

**B1 + B2 + parte do B3.** É o que não exige decisão nova nem quebra contrato
publicado:

* verbos genéricos nos 12 Kinds;
* as nove renomeações;
* o grupo `backup` consolidado;
* `system metrics`/`state`, `network diagnose`, `secret rotate`.

Levaria a conformidade de **57% para ~80%**, e é uma **minor** (`0.67.0`) com
nota de migração para as nove grafias renomeadas.

**O que NÃO cabia nesta leitura original:** o `config` (precisa de ADR), o
`diff` (output novo), o day-2 de `pod`/`vm`/`cluster` (capacidade a sério), e
todos os blocos de colapso — que dependem do B1/B3 estarem completos e, no
caso do B8, de um major.

## 5. As três decisões que faltam

**O `Service` — IMPLEMENTADO a 2026-09-05, `docs/adr/0032-service-kind-dns-round-robin.md`.**
Selecciona um conjunto de containers por `matchLabels` (o mesmo primitivo que a
ADR-0024 ainda desenha para `FirewallPolicy`, esse continua por construir) e
publica-o como vários registos DNS `A`, round-robin, sob o nome interno já
existente — sem VIP, sem dataplane novo, sem daemon. `delonix get/describe/
delete services`, converge (`hot_fields`), `stack plan`/`apply`, e schema
gerado. Validado ao vivo: selecção por labels, DNS multi-registo real
(`nslookup` a devolver as duas IPs), isolamento por namespace (incluindo a
excepção `default` = pública), selector vazio a avisar e não a falhar, e
actualização a quente da porta. Fecha a contagem das «12 Kinds operáveis».

**O `config` e os contextos — FECHADO, confirmado no código.** O `config.rs`
diz, no seu próprio comentário de módulo: uma preferência local pequena,
nunca um contexto — o §16 quer `endpoint`/`identity`/`tls` num contexto, e é
exactamente o que o ADR-0010 já recusou. `namespace` (o outro candidato do
plano) fica de fora de propósito: ao contrário do `output`, não há um ponto
de leitura único a que um default de namespace se prenda. `KNOWN_KEYS` tem
uma chave só: `"output"`. O ADR-0010 continua `Status: Rejected
(2026-08-10)` — "a API fica local, e essa é a resposta, não um adiamento."

**Quando é o major — já respondido pela EXECUÇÃO, não por um documento.** A
v1.0.0 aconteceu a 29/08, ANTES de o B1 sequer ter sido fundido (31/08) — a
pergunta desta secção («B4–B7 vão antes ou juntam-se ao major?») ficou sem
objecto: o major já saiu, estreito e desacoplado, levando só a parte menos
controversa do B8 (atalhos de raiz) e **rejeitando** o B9 (remap de exit
codes). B4–B7 continuam a forma `0.x`, aditiva-e-medida, como se a decisão
tivesse sido «sair o major cedo e estreito, continuar a colapsar o resto a
seguir, aos poucos». Registado aqui como facto, não como pergunta em aberto.

## 6. O que este plano não promete

Não há estimativa de tempo. `pod port-forward` (PR #207), `net capture`
(PR #208), `vm pause/unpause` (PR #206) e `image sign` (PR #209) já têm
código a aguardar revisão manual.

**`vm migrate` — investigado 2026-09-03, já não é "por confirmar".** Medido
contra o `VmBackend` real e a documentação upstream do Cloud Hypervisor e do
libvirt/QEMU: um MVP **stop-copy-start** (`vm stop` → scp do overlay qcow2 +
a golden para o host alvo → `vm create`/registo lá, com downtime real) é
directamente construível sobre primitivos já existentes, sem storage
partilhado nenhum. **Live migration a sério continua fora de alcance** — o
Cloud Hypervisor não tem mecanismo de migração de DISCO (só memória/estado),
e mesmo o caminho NBD do libvirt/QEMU (que existe e é maduro) exigiria
`VmBackend::migrate` + alcançabilidade de rede entre hosts + gestão de
convergência que este código não tem — isso fica como ADR próprio, não como
extensão incremental. O que se constrói agora é o MVP com downtime,
documentado como tal.

**`cluster upgrade`/`drain`/`uncordon` — já não estão bloqueados** (ver a
correcção na secção do B3/B7 acima). `drain`/`uncordon` não precisam de SSH
(kubeconfig já cacheado localmente); `upgrade` reusa o `SshTarget` que
`kubeadm_init`/`join` já usam. Por construir, sem PR aberto ainda.
