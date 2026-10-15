# CONSOLIDADO — retomar o trabalho do `delonix-runtime` noutra máquina

> Ficheiro de trabalho do ramo `develop`. **Nunca vai para a `main`** — é o estado
> de uma consolidação, não documentação do produto. Escrito em pt-AO de propósito:
> é um prompt para o dono ditar, não doc canónica (essa é em inglês).

**Gatilho.** `/consolidado` é uma **skill multi-repo** (10 repos; o ficheiro canónico
dela vive em `docs/CONSOLIDADO.md` do repo que a definiu, e instala-se com
`cp docs/CONSOLIDADO.md ~/.claude/commands/consolidado.md`). **Este ficheiro não é o
comando — é o ESTADO do `delonix-runtime`** que o passo 1 dessa skill descobriria, já
descoberto e medido. Escrever `consolidado` numa sessão deste repositório significa: lê
isto primeiro, confirma contra o remoto, e **continua** de onde a consolidação de
2026-10-14 parou. **Não refazer a consolidação** — está feita, e o passo 3 da skill
(cherry-pick) daria conflitos sobre trabalho que já lá está.

## 1. O que a `develop` já tem

Criada de `origin/main` em `3cd14ec7` (PR #837) a 2026-10-14, por **quatro merges
`--no-ff`**, um por linha de trabalho, mais dois commits próprios (as páginas geradas e
os checks recuperados). **36 commits próprios, 166 ficheiros.** No remoto está como
**`consolida/develop-2026-10-14`** (ver §5.4 para o porquê de a `develop` não ter sido
empurrada directamente). A versão continua `5.0.2`, igual à última tag — trabalho entre
releases, como o `version_gate.py` exige.

| Merge | Branch de origem | Próprios | O que traz |
|---|---|---|---|
| `e26bf58e` | `plan/s17-vip-ipv6` | 19 | `kind: GitOpsSource` e a passagem de pull sem daemon (ADR-0021) **mais** o VIP L4 do `kind: Service` com readiness (ADR-0066 F1–F3, D5, D6, D9), o cenário de caos `service_lb` e `network vip-pool` |
| `979f698c` | `integra/owasp-audit` | 14 | Auditoria OWASP K8s/Docker Top 10: `workloadSecurity: baseline`, admissão de `kind: VirtualMachine` e de `vm update`, cabeçalhos do ingress-proxy, credencial de túnel fora do argv, CRI/MCP a não passar nome do chamador como opção, OPNsense a recusar http/userinfo, DX-7802 |
| `7e385b6d` | `metrics/workload-pressure-series` | 1 | Séries por workload no `/metrics`: throttling, pressão e OOM |
| `a59f77de` | `cgroup/root-parent-and-vm-leaf` | 1 | O grupo de um container em modo root, e o VMM do Cloud Hypervisor dentro de um leaf de cgroup limitado (DX-6515) |

**Três conflitos, todos resolvidos mantendo os dois lados** — eram acrescentos
independentes, não disputas: o dicionário de códigos (`DX-1349` do OWASP ao lado de
`DX-1360/1361` do VIP; `DX-6515` da VM ao lado de `DX-6601..6603` do GitOps, por
ordem numérica), o `pt.po` e o `AGENTS.md` (a regra da casa: são entradas distintas,
escolher uma apaga o achado de outra pessoa).

## 2. O que ficou DE FORA, e porquê (medido, não suposto)

- **`wip/adr0046-1b-holder-dns` (2 commits, 2026-09-20) — SUPERADO, não consolidar.**
  O merge dava 36 blocos de conflito em 10 ficheiros. A razão: o commit `9f485fac` da
  `main` é literalmente «vm: expose HTTP/S services of a VM by name; hosts: host
  (ADR-0046 **phases 1 and 1b**, ADR-0047) (#444)» — as duas fases que o WIP traz já
  estão na `main` por outro caminho. O ramo é trabalho morto: apagar, não rebasear.
- **`backup/s17-pre-rebase-2` (13) — subconjunto** de `plan/s17-vip-ipv6`: 12 dos 13
  commits têm `patch-id` idêntico; o 13.º (`1dd496ca`) é a versão pré-rebase do mesmo
  «service: an L4 VIP … (ADR-0066 F1-F3)». Nada a salvar.
- **`sec/owasp-audit` (13) — subconjunto** de `integra/owasp-audit`, que é a versão
  integrada mais o DX-7802.
- **`plan/s24-gitops` (12) — subconjunto** de `plan/s17-vip-ipv6`, que partiu dele.
- Sem commits próprios nenhuns: `net/b-67-egress-rfc1918`,
  `proxmox/adr-0063-ipam-controller-subnets-in-place` (remoto já apagado),
  `claude/competent-euclid-f5aecf`, `claude/infallible-pare-330da8`,
  `claude/silly-lovelace-2b696b`, `worktree-agent-a207e8b6eace29570`,
  `worktree-agent-ab20a26997fd591e1`, `worktree-agent-ac2db8ad83b6435b3`.

## 3. Os dois PRs redundantes estão FECHADOS

Por decisão do dono, a 2026-10-14. Cada um levou a razão em comentário e o `git cherry`
que a mede (**0 patches ausentes** nos dois). O repositório ficou **sem PRs abertos**.

| PR | Ramo | Absorvido por |
|---|---|---|
| #835 | `sec/owasp-audit` | merge de `integra/owasp-audit` |
| #836 | `plan/s24-gitops` | merge de `plan/s17-vip-ipv6` |

A consolidação passa a ser o único veículo deste trabalho.

## 4. O que já está PROVADO nesta ponta (2026-10-14)

- **`cargo test --workspace`: 3413 passados, 0 falhados**, `TEST_RC=0`. O número conta:
  um filtro que não casa com nada sai 0 e não prova nada — e a primeira corrida desta
  consolidação foi feita com `| tail -40`, que cortou o resumo e deixou um `EXIT=0` que
  era do `echo` final, não dos testes. Falso verde apanhado e refeito sem filtro.
- **`cargo fmt --check` e `clippy --workspace --all-targets -- -D warnings`**: limpos.

- **O workspace consolidado compila**: `cargo build --release --workspace` → `EXIT=0`,
  os cinco binários mais o `delonix-storage-helper`. É a primeira prova de que os
  quatro merges são coerentes entre si, e não só textualmente limpos.
- **As docs foram regeneradas** (`docs/gen.py`, `dev_docs.py`, `dev_docs_site.py`) e o
  `docs/codigos.html` traz os **oito** códigos das quatro linhas — DX-1349, 1360, 1361,
  6515, 6601, 6602, 6603 e 7802 —, o que prova que a resolução do dicionário manteve os
  dois lados nos dois conflitos.
- **Gates verdes**: `lang_ratchet.py`, `arch_fitness.py`, `version_gate.py`,
  `contract_gate.py`, `docs_cli_gate.py`, `test_tmp_roots_gate.py`.

## 5. O que FALTA, por ordem

1. **UM gate está VERMELHO, e é um achado do ramo de origem, não da consolidação.**
   `scripts/cli_exec_ratchet.py` chumba: «the CLI grew: 274 leaves in the baseline,
   277 today». As três folhas novas são **`network vip-pool set`, `network vip-pool
   show` e `network vip-pool clear`** (`--list-missing` confirma-o), acrescentadas pelo
   `plan/s17-vip-ipv6` — que trouxe um `cli_exec_trace.tsv` gravado numa corrida do
   `plan/s24-gitops` (`3568f895`), ou seja o ramo do VIP nunca regravou o trace depois
   de acrescentar as suas próprias folhas. **Não baixar a baseline sem a corrida**: é
   precisamente o falso verde que o ratchet existe para impedir. A correcção é dar-lhes
   um check na bateria e regravar o trace de uma corrida DA `develop`:
   ```bash
   python3 scripts/cli_exec_ratchet.py --from-results $OUT/results.jsonl \
     --update --commit $(git rev-parse HEAD) --note "<proveniência da corrida>"
   ```
   O `--commit` é obrigatório: o default carimba o HEAD, e regravar uma corrida antiga
   numa ponta mais nova já nomeou o commit errado uma vez.
   **Metade disto já está feita**: o commit `ee3e00c6` dá às três folhas um check
   julgado cada — recuperado de trabalho **por commitar** que estava no worktree
   `s17-vip-ipv6` e que uma limpeza de worktrees teria destruído. O terceiro check
   substitui o `vip-pool clear >/dev/null 2>&1` do teardown por um check julgado, que é
   a própria regra do ratchet. **Falta só a corrida** que regrava o trace.
2. **Os gates que o CI apanha e o local não** (correr sem `tail`, que esconde a causa):
   `python3 scripts/lang_ratchet.py`, `scripts/arch_fitness.py`,
   `scripts/contract_gate.py`, `scripts/version_gate.py`, `scripts/docs_cli_gate.py`,
   `scripts/cli_exec_ratchet.py`, `scripts/tmp_roots_gate.py`, `scripts/test_*.py`.
   Quatro baselines já vieram mexidas dos ramos (`cli_baseline.tsv`,
   `cli_exec_baseline.json`, `cli_exec_trace.tsv`, `lang_baseline.json`) — se
   discordarem, **a corrida da `develop` é que manda**: regravar com a proveniência
   (`--commit <sha da develop>`), nunca carimbar o HEAD de outra ponta.
3. **A bateria E2E e o caos** — não correm no CI (o runner alojado bloqueia userns),
   por isso só esta máquina (ou o lab) os prova. Com os **quatro binários irmãos** no
   mesmo `target` (`-cri`, `-mgmt`, `-mcp`, `-node-api`), senão dá FAIL falso, e com
   **os dois roots isolados**:
   ```bash
   DELONIX_ROOT=/tmp/dlxdev DELONIX_NET_RUNTIME_DIR=/tmp/dlxdev-net scripts/e2e.sh
   ```
   Só `DELONIX_ROOT` aponta o teardown à infra real — **meia isolação é pior que
   nenhuma** e já reiniciou um container de produção deste host.
4. **O push: são DOIS hooks, e o segundo foi surpresa.**
   - `cli_funcional_gate` (PreToolUse) nega o push sem veredicto `APPROVED` do agente
     `cli-tester` para `HEAD^{tree}`. Saltou-se com `DELONIX_CLI_GATE=skip`, **com
     autorização explícita do dono** — o próprio gate diz que um modelo só o usa assim.
   - `pre-push` (`ngolacloud/ci/hooks`) trata **`develop` como ramo protegido**: recebe
     por PR, e um push directo faz a CI **publicar uma imagem `<versão>-<sha8>`** desde
     2026-08-28. Por isso a consolidação foi para **`consolida/develop-2026-10-14`**
     (ramo de funcionalidade, portão leve, já no remoto) e a `develop` ficou por criar
     no remoto. Para a criar são duas vias, e a escolha é do dono:
     ```bash
     # a) directo, publica a imagem:
     NGOLA_GITFLOW_FORCE=1 DELONIX_CLI_GATE=skip git push -u origin develop
     # b) por PR, sem publicar: criar develop no GitHub a partir de consolida/… e
     #    abrir PR consolida/… -> develop
     ```

## 6. Trabalho fora dos ramos — o que a skill manda olhar e quase se perdeu

- **RECUPERADO: três checks por commitar** no worktree `s17-vip-ipv6`
  (`scripts/e2e.sh`, +6/−1). Era o remédio do gate vermelho e **uma limpeza de
  worktrees teria-o destruído**. Está em `ee3e00c6`.
- **Dois stashes, nenhum é trabalho a recuperar** — e a leitura foi medida, não lida da
  mensagem deles:
  - `stash@{0}` (2026-10-10, «resíduo Sprint 9 (ja em 00399db4)»): o commit `00399db4`
    («cli: fold `image vm` … Sprint 9») **é ancestral de `origin/main`**, e o diff é
    dominado por deleções (857+/−1129) — árvore atrasada face à `main`, não trabalho
    novo. Consolidá-lo reverteria a `main`.
  - `stash@{1}` (2026-08-27, «reversão espúria do #128»): numa branch
    `integra/perf-recursos` que já não existe, e é uma *reversão* — o oposto de
    trabalho.
  - **Ficam onde estão.** São a única cópia e ninguém pediu para os apagar.
- Nenhum outro worktree tem ficheiros sujos.

## 7. Armadilhas desta árvore que custaram tempo antes

- **Este host corre produção** (Odoo, Postgres, Redis, registries, nós k8s). Nunca
  `prune`/`rm -f`/`--force` sobre o que não é da corrida. Um `DELONIX_ROOT` derivado
  que saia **vazio** manda o comando para o root real — já apagou dois containers.
- **Um `CARGO_TARGET_DIR` por árvore.** Duas cópias do repo a partilhar um target
  fazem o cargo servir o binário da outra e um antes/depois mede o lado errado. Esta
  consolidação usou `/home/walter/.cache/delonix-target-develop`.
- **`cargo fmt -- <ficheiro>` reformata 180 ficheiros** — usar `rustfmt` directo.
- **O `ngola-ci` do pre-push mete um merge commit na ref**, e o «FAIL» dele esconde a
  causa em 8 linhas truncadas.
- **Builds longos**: `setsid nohup`, log fora de `/tmp` do sistema, e esperar pela
  **condição** (`until grep -q '^EXIT='`), nunca por tempo.

## 8. As outras oito sessões de consolidação (2026-10-14, 18:19Z)

O mesmo pedido foi para nove sessões, uma por repositório — cada uma consolida a sua
`develop` e escreve o seu próprio ficheiro. Esta é a do `delonix-runtime`.

`delonix-deploy` · `delonix-admin` · `delonix-console` · `delonix-paas` ·
`delonix-meet` · `abada` · `ayize` · `Kaeso-Multicompany` · `device-sentinel`

**Fronteira de âmbito:** uma sessão aberta neste repositório **não** edita, propõe PRs
nem corre builds nesses outros repos, mesmo quando o assunto lhes pertence. O que for
achado e for deles **reporta-se**, não se corrige daqui (decisão do dono, 2026-09-16).

## 9. Sessões anteriores deste repositório

| Sessão | PR | Estado |
|---|---|---|
| Delonix-runtime segurança e arquitetura | #835 | aberto — é a dona da auditoria OWASP |
| Plano unificado e benchmark final | — | fundido |
| Delonix CLI functional test skill | — | fundido |
| Comandos build e release GitHub | — | sem PR |

O plano em vigor continua a ser `docs/discovery/68_ENGINE_COMPLETION_PLAN.md` §0.
