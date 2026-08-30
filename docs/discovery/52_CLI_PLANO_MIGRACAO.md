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

### B1 — Verbos genéricos, o resto dos Kinds  ·  ADITIVO  ·  ~7 folhas

Levar o `get`/`describe`/`delete` de 9 para os 12 Kinds que têm estado próprio.
Falta `NetworkRoute`, `NetworkPolicy` e o `describe` de `Secret`/`Image`/
`KubernetesCluster`.

O `NetworkPolicy` é o único com desenho por decidir: o `net ingress ls` é **por
container**, não uma lista global, por isso `get networkpolicies` não tem hoje
pergunta equivalente.

**Fecha o critério de saída da CLI-2.** Nada quebra.

### B2 — Renomeações que não tocam em contrato  ·  QUEBRA MENOR  ·  ~20 folhas

Todas em grupos declarados NÃO estáveis:

| de | para |
|---|---|
| `dash` | `dashboard` (§22) |
| `syntax <editor>` | `completion editor <editor>` (§21) |
| `completion <shell>` | `completion shell <shell>` |
| `namespace` | `system namespace` |
| `net boot` | `system boot` |
| `volumes` | `volume` |
| `image ls` / `image rm` | `image list` / `image remove` (revertido em v2.0.0 — ver `docs/releases/v2.0.0.md`; `image remove` fica) |
| `schema print` | `manifest schema` (já existe; remove-se o antigo) |
| `restore` (raiz) | `backup restore` |

Corte limpo, sem alias — o precedente é a v0.30.0, e uma grafia antiga que
continua a funcionar é uma grafia de que ninguém migra. Falha com
`unrecognized subcommand`, nunca em silêncio.

### B3 — Capacidade nova: os grupos que não existem  ·  ADITIVO  ·  ~20 folhas

| grupo | folhas | natureza |
|---|---|---|
| `backup` | 6 | consolida o `backup`/`restore` de raiz + `list`/`inspect`/`schedule`/`remove` |
| `config` | 5 | contextos — **precisa de ADR**, ver §5 |
| `cluster` day-2 | 5 | `kubeconfig`/`health`/`upgrade`/`drain`/`uncordon` |
| `pod` day-2 | 4 | `exec`/`attach`/`cp`/`port-forward` |
| `vm` day-2 | 3 | `pause`/`resume`/`migrate` |
| `system` | 3 | `metrics`/`state` (o `boot`/`namespace` vêm do B2) |
| `network` | 2 | `diagnose`/`capture` (o `flow` existe em `net flow`) |
| `image sign` · `secret rotate` · `diff` | 3 | |

O `diff` é o único verbo canónico em falta com desenho por fazer: quer as três
faces (desired / last-applied / observed), que é output novo e não
encaminhamento.

### B4 — O colapso do `net`  ·  QUEBRA  ·  −41 folhas

O maior ganho isolado. `ingress`/`egress` → `NetworkPolicy`; `httproute` →
`HTTPRoute`; `tunnel` → `Gateway`; `boot` → `system boot` (feito no B2); `netns`
passa a oculto.

**Bloqueado pelo B1 e pelo B3**: sem `get networkpolicies` e sem
`network diagnose`, cortar o `net` tira capacidade.

### B5 — O colapso do armazenamento  ·  QUEBRA  ·  −22 folhas

`storage` e `sharevolume` desaparecem em `kind: Volume`; `volumes` já é `volume`
no B2. Os três grupos passam a `apply`/`get`/`delete` mais o day-2 de snapshot e
backup.

### B6 — `image --vm` e o `build`  ·  QUEBRA  ·  −11 folhas

`image build --type container|virtual-machine` substitui o `build` de raiz, o
`image --vm build` e o `vm build`. A flag `--vm` que troca o store inteiro
desaparece.

### B7 — Day-2 puro: `vm`, `cluster`, `pod`  ·  QUEBRA  ·  −25 folhas

`vm create|ls|rm|status` → `apply`/`get`/`delete`; idem para `pod` e `cluster`.
Só sobrevive o que não cabe num CRUD.

### B8 — Os atalhos de raiz e o `workload`  ·  QUEBRA DE CONTRATO  ·  −10 folhas

`ps`, `run`, `exec`, `logs`, `rm`, `images` estão **declarados estáveis**. O
`workload` não está, mas colapsa aqui por coerência.

**Este bloco é o major.** Precisa de nota de migração própria.

### B9 — Exit codes colidentes  ·  QUEBRA DE CONTRATO

`2`→64, `4`→66, `5`→73, e o `3` precisa de destino. Vai com o B8, no mesmo major.

## 4. O que pode sair no próximo bump

**B1 + B2 + parte do B3.** É o que não exige decisão nova nem quebra contrato
publicado:

* verbos genéricos nos 12 Kinds;
* as nove renomeações;
* o grupo `backup` consolidado;
* `system metrics`/`state`, `network diagnose`, `secret rotate`.

Levaria a conformidade de **57% para ~80%**, e é uma **minor** (`0.67.0`) com
nota de migração para as nove grafias renomeadas.

**O que NÃO cabe:** o `config` (precisa de ADR), o `diff` (output novo), o day-2
de `pod`/`vm`/`cluster` (capacidade a sério), e todos os blocos de colapso — que
dependem do B1/B3 estarem completos e, no caso do B8, de um major.

## 5. As três decisões que faltam

**O `Service`.** A especificação lista-o no §5.1 com `svc` e
`networking.delonix.io/v1alpha1`; o registo tem 16 Kinds e nenhum é `Service`. É
um Kind a desenhar — spec própria, semântica de balanceamento — não a
encaminhar. Sem isto, a contagem dos «12 Kinds operáveis» não fecha.

**O `config` e os contextos.** O §16 quer `endpoint`, `identity` e `tls` num
contexto. O ADR-0010 **recusou** a API de gestão remota, e um contexto com
endpoint remoto pressupõe-na. Ou o `config` nasce local-only (namespace e
preferência de output, sem endpoint), ou reabre-se o ADR-0010 com um consumidor
concreto.

**Quando é o major.** O B8 e o B9 quebram contratos publicados com exemplo em
bash. É `1.0.0` ou é um `0.x` com nota de migração? A resposta muda o
sequenciamento dos blocos B4–B7, que podem ir antes ou serem agrupados com ele.

## 6. O que este plano não promete

Não há estimativa de tempo. Os blocos B3 e B7 são capacidade nova — `pod
port-forward`, `vm migrate`, `cluster upgrade` e `network capture` são
funcionalidades, não renomeações, e cada uma precisa da sua validação ao vivo
contra infra real. Dar-lhes um prazo aqui seria inventar.
