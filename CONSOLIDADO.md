# CONSOLIDADO — retomar o trabalho do `delonix-runtime` noutra máquina

> Ficheiro de trabalho do ramo `develop`. **Nunca vai para a `main`** — é o estado
> de uma consolidação, não documentação do produto. Escrito em pt-AO de propósito:
> é um prompt para o dono ditar, não doc canónica (essa é em inglês).

**Gatilho.** Escrever `consolidado` numa sessão aberta neste repositório significa:
«lê este ficheiro inteiro, confirma o estado contra o remoto, e continua de onde a
consolidação de 2026-10-14 parou». Não refazer a consolidação — ela está feita.

## 1. O que a `develop` já tem

Criada de `origin/main` em `3cd14ec7` (PR #837) a 2026-10-14, por **quatro merges
`--no-ff`**, um por linha de trabalho. **35 commits próprios, 165 ficheiros,
+13 686/−687.** A versão continua `5.0.2`, igual à última tag — trabalho entre
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

## 3. Os dois PRs abertos ficam redundantes

| PR | Ramo | Estado face à `develop` |
|---|---|---|
| [#835](https://github.com/angolardevops/delonix-runtime/pull/835) | `sec/owasp-audit` | **contido** (via `integra/owasp-audit`) |
| [#836](https://github.com/angolardevops/delonix-runtime/pull/836) | `plan/s24-gitops` | **contido** (via `plan/s17-vip-ipv6`) |

Decisão do dono: ou se fundem os dois à `main` pelo caminho normal e a `develop`
rebaseia, ou se fecham e a `develop` passa a ser o veículo. **Não fundir os dois E a
`develop`** — seria o mesmo trabalho duas vezes com hashes diferentes.

## 4. O que já está PROVADO nesta ponta (2026-10-14)

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

1. **`make ci`** — `cargo fmt --check`, `clippy -D warnings` e `cargo test --workspace`
   não foram concluídos nesta ponta (ver o estado em `/tmp/claude-1000/ci-develop.log`
   se a sessão for a mesma; senão correr de novo).
2. **UM gate está VERMELHO, e é um achado do ramo de origem, não da consolidação.**
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
3. **Os gates que o CI apanha e o local não** (correr sem `tail`, que esconde a causa):
   `python3 scripts/lang_ratchet.py`, `scripts/arch_fitness.py`,
   `scripts/contract_gate.py`, `scripts/version_gate.py`, `scripts/docs_cli_gate.py`,
   `scripts/cli_exec_ratchet.py`, `scripts/tmp_roots_gate.py`, `scripts/test_*.py`.
   Quatro baselines já vieram mexidas dos ramos (`cli_baseline.tsv`,
   `cli_exec_baseline.json`, `cli_exec_trace.tsv`, `lang_baseline.json`) — se
   discordarem, **a corrida da `develop` é que manda**: regravar com a proveniência
   (`--commit <sha da develop>`), nunca carimbar o HEAD de outra ponta.
4. **A bateria E2E e o caos** — não correm no CI (o runner alojado bloqueia userns),
   por isso só esta máquina (ou o lab) os prova. Com os **quatro binários irmãos** no
   mesmo `target` (`-cri`, `-mgmt`, `-mcp`, `-node-api`), senão dá FAIL falso, e com
   **os dois roots isolados**:
   ```bash
   DELONIX_ROOT=/tmp/dlxdev DELONIX_NET_RUNTIME_DIR=/tmp/dlxdev-net scripts/e2e.sh
   ```
   Só `DELONIX_ROOT` aponta o teardown à infra real — **meia isolação é pior que
   nenhuma** e já reiniciou um container de produção deste host.
5. **O push.** O hook recusa `git push` sem veredicto `APPROVED` do agente
   `cli-tester` para `HEAD^{tree}` (`cli_funcional_gate`). Num ramo de consolidação
   isso é caro; saltar o gate é **decisão do dono**, não minha.

## 6. Armadilhas desta árvore que custaram tempo antes

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

## 7. As outras oito sessões de consolidação (2026-10-14, 18:19Z)

O mesmo pedido foi para nove sessões, uma por repositório — cada uma consolida a sua
`develop` e escreve o seu próprio ficheiro. Esta é a do `delonix-runtime`.

`delonix-deploy` · `delonix-admin` · `delonix-console` · `delonix-paas` ·
`delonix-meet` · `abada` · `ayize` · `Kaeso-Multicompany` · `device-sentinel`

**Fronteira de âmbito:** uma sessão aberta neste repositório **não** edita, propõe PRs
nem corre builds nesses outros repos, mesmo quando o assunto lhes pertence. O que for
achado e for deles **reporta-se**, não se corrige daqui (decisão do dono, 2026-09-16).

## 8. Sessões anteriores deste repositório

| Sessão | PR | Estado |
|---|---|---|
| Delonix-runtime segurança e arquitetura | #835 | aberto — é a dona da auditoria OWASP |
| Plano unificado e benchmark final | — | fundido |
| Delonix CLI functional test skill | — | fundido |
| Comandos build e release GitHub | — | sem PR |

O plano em vigor continua a ser `docs/discovery/68_ENGINE_COMPLETION_PLAN.md` §0.
