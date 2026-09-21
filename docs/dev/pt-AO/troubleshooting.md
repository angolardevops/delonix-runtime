<!-- translated-from: troubleshooting.md sha256:47d1c5984d20cc95f303cbc9d9579447fa3fec9218d0e0c4f6e052daf1781f74 -->
# Diagnóstico de problemas

**Antes de leres:** [Preparar o teu ambiente](environment.md) (armadilhas do host) e [Clonar, compilar e testar](build-and-test.md#the-gates-ci-runs) (os gates, e isolar o estado do motor).

Esta página é um índice por **sintoma**: o texto literal que um gate ou uma corrida ao vivo
imprime, o que significa de facto, e a correcção. Não volta a ensinar o que [Preparar o teu
ambiente](environment.md) e [Clonar, compilar e testar](build-and-test.md) já cobrem em
profundidade — aponta antes para a secção certa, para um clone acabado de fazer ir de "estou
preso" a "sei que secção ler" sem ter de ler nenhuma das duas páginas de fio a pavio primeiro. Se
um sintoma não estiver aqui, [Começa aqui § Quando estás preso](start-here.md#when-you-are-stuck)
é o sítio seguinte a olhar — esta página é um atalho para o caso específico em que a *própria
ferramenta já te disse o que está errado*, num texto que ainda não reconheceste.

## Índice rápido

| Viste | É | Corrigido em |
|---|---|---|
| `FAIL <name>: N (baseline M) — new debt entered` | `scripts/arch_fitness.py` | [Um ratchet de dívida moveu-se](#a-debt-ratchet-moved-arch_fitnesspy) |
| `FALHA <kind>: N > M — entrou português novo.` | `scripts/lang_ratchet.py` | [O ratchet de língua](#new-portuguese-entered-lang_ratchetpy) |
| `<nome> (<camada>) → <nome> (<camada>): forbidden direction` | `scripts/arch_fitness.py` | [Uma dependência vai contra a direcção das camadas](#a-dependency-goes-against-the-layer-direction) |
| `<ficheiro>:<n>: names a consumer (…) — the engine knows none` | `scripts/arch_fitness.py` | [Um nome de consumidor entrou no motor](#a-consumer-name-leaked-into-the-engine) |
| `FAIL <tag> is published but this commit does not contain it` | `scripts/version_gate.py` | [O ramo é anterior à tag mais recente](#the-version-gate-refuses-your-branch) |
| `FAIL Cargo.toml says X, above Y, and docs/releases/vX.md does not exist` | `scripts/version_gate.py` | [Um bump de versão sem commit de release](#the-version-gate-refuses-your-branch) |
| `FAIL  buf format` / `buf lint` / `buf breaking against …` / `has no google.api.http mapping` / `openapi.yaml is not the generated one` | `scripts/contract_gate.py` | [O gate do contrato do nó](#the-node-contract-gate) |
| `unshare()` falha, `EPERM` | AppArmor + user namespaces | [Preparar o teu ambiente § AppArmor](environment.md#ubuntu-2310-apparmor-blocks-user-namespaces-for-your-dev-binary) |
| `-m`/`--cpus`/`--cpu-weight` recusados, saída `69` | delegação de cgroup | [Preparar o teu ambiente § Delegação de cgroup](environment.md#cgroup-delegation-some-limits-are-refused-others-are-not-enforced) |
| `path must be shorter than SUN_LEN` | `DELONIX_NET_RUNTIME_DIR` demasiado longo | [Clonar, compilar e testar § Isolar o estado do motor](build-and-test.md#isolating-the-engines-state) |
| Um gate falha contra um diff que não escreveste, ou um build termina depressa demais | uma cache de build partilhada ou obsoleta | [Uma cache de build partilhada ou obsoleta](#a-shared-or-stale-build-cache) |
| O binário responde com uma versão antiga ou um comando que não existe | um `delonix` obsoleto no `PATH` | [Preparar o teu ambiente § Um PATH obsoleto](environment.md#a-stale-delonix-on-your-path) |

## Gates de CI com a razão na mensagem

Todo gate diz o que corrigir — a tabela abaixo só serve para reconheceres a *forma* da mensagem
antes de a leres com atenção. Corre o comando local de [Clonar, compilar e testar § Os gates que a
CI corre](build-and-test.md#the-gates-ci-runs) para reproduzir qualquer um destes sem esperar
pela CI.

### Entrou português novo (`lang_ratchet.py`)

```
FALHA identifiers: 1051 > 1050 — entrou português novo.
       `python3 scripts/lang_ratchet.py --list --only identifiers` mostra onde.
```

(As mensagens do próprio gate são em português — a LANG-01 é sobre o *código*, não sobre este
script; vê [Fluxo de contribuição § Língua](contributing-workflow.md#language-english-in-the-code-lang-01).)
A contagem de identificadores, comentários ou mensagens ao utilizador em português subiu.
`--list --only <kind>` nomeia as linhas novas. Se *traduziste* alguma coisa e a contagem em vez
disso **desceu** sem teres baixado a linha de base, a mensagem é a imagem espelhada ("traduziste,
mas não baixaste a linha de base") — corre `python3 scripts/lang_ratchet.py --update` e comita
`scripts/lang_baseline.json` no mesmo commit que a tradução.

### Um ratchet de dívida moveu-se (`arch_fitness.py`)

```
FAIL  self_exec_sites: 6 (baseline 5) — new debt entered
```

Um dos cinco números seguidos (`self_exec_sites`, `library_prints`, `env_writes`,
`shared_error_imports`, `raw_error_variant_matches`) subiu. `python3 scripts/arch_fitness.py
--list` imprime todos os sítios que cada um conta, com uma explicação curta do que a contagem
significa ao lado — vê [Fluxo de contribuição § Regras de arquitectura que os gates
impõem](contributing-workflow.md#architecture-rules-the-gates-enforce) para saber o que é cada um.
Tal como no ratchet de língua, a mesma forma de mensagem aparece quando a contagem **desce** sem a
linha de base se mover com ela no mesmo commit ("debt was paid; lower the baseline … `--update`").

### Uma dependência vai contra a direcção das camadas

```
FAIL  delonix-oci (adapters) → delonix-cri (interfaces): forbidden direction
```

Um crate importou outro crate de uma camada de que não pode depender — vê [Arquitectura § Camadas
e a direcção permitida](architecture.md#layers-and-the-allowed-direction) para a tabela de quem
pode depender de quem. Ou a dependência está errada (o caso mais frequente — um adapter não tem
nada que fazer a depender de uma interface), ou a mudança precisa mesmo de uma excepção declarada
e faseada em `EXCEPTIONS` dentro de `scripts/arch_fitness.py`, que o próprio gate se recusa a
aceitar sem uma fase que a remova e uma razão.

### Um nome de consumidor entrou no motor

```
FAIL  crates/adapters/delonix-oci/src/registry.rs:42: names a consumer ('SomeControlPlaneName') — the engine knows none
```

A fronteira canónica do motor — *«o motor não conhece nenhum consumidor»*, a secção no topo do
[`AGENTS.md`](../../../AGENTS.md) — é imposta por grep, não só pela revisão. Isto dispara sobre o
nome de qualquer plataforma, control plane, consola ou agente que use o motor, no código **ou nos
comentários**, em qualquer sítio dentro de `crates/`, `bins/` ou `proto/`. Generaliza o requisito
para a capacidade genérica que ele de facto é, no vocabulário do próprio motor; a história que
precisa do nome externo pertence a `docs/`, nunca ao código.

### O gate de versão recusa o teu ramo

```
FAIL  v1.4.2 is published but this commit does not contain it (newest contained: v1.4.0) —
      merge origin/main first; merging a branch that predates a release undoes what it shipped
```

O teu ramo começou antes de uma release que entretanto saiu. `git fetch --tags origin && git
merge origin/main` (este repositório faz merge, não faz rebase de ramos de funcionalidade sobre
releases — vê [Fluxo de contribuição § Um worktree por
tarefa](contributing-workflow.md#one-worktree-per-task) para saber porque é que um histórico
linear continua a ser esperado dos teus próprios commits).

```
FAIL  Cargo.toml says 1.5.0, above 1.4.2, and docs/releases/v1.5.0.md does not exist —
      a bump belongs only to the release commit
```

Fizeste bump ao `version` no `Cargo.toml`. Não faças isso — isso só se faz no commit de release,
junto com o ficheiro de notas da release. Reverte o bump; vê [Releases e estabilidade § O gate de
versão](releases-and-stability.md#the-version-gate).

### O gate do contrato do nó

`scripts/contract_gate.py` envolve cinco verificações independentes, e cada uma imprime a sua
própria linha `FAIL` — corrê-lo localmente é a forma mais rápida de ver qual das cinco é a tua:

```
FAIL  buf format — run `buf format -w proto`
FAIL  buf lint
FAIL  buf breaking against v1.4.0
<segue o próprio stdout/stderr do buf, a nomear o campo ou o RPC que mudou de forma incompatível>
FAIL  node.proto: SomeRpc has no google.api.http mapping
FAIL  docs/api/openapi.yaml is not the generated one — run `python3 scripts/contract_gate.py --update` and commit it
```

Precisa de `protoc`, `buf` (fixado na v1.73.0) e `protoc-gen-openapi` (fixado na v0.7.1) no
`PATH`, e das tags do git — uma ferramenta em falta falha com o "command not found" mais familiar,
mas uma tag em falta faz a verificação `buf breaking` imprimir `ok` a dizer explicitamente que
ainda não há linha de base para comparar, em vez de a saltar em silêncio. Uma quebra genuína ao
`proto/delonix/node/v1` precisa de um ADR primeiro, tal como qualquer mudança a um contrato de nó
estável — vê [Fluxo de contribuição § Quando escrever um
ADR](contributing-workflow.md#when-to-write-an-adr). Se o que de facto mudou é a saída do
gerador (um campo novo, um RPC novo), `python3 scripts/contract_gate.py --update` regenera
`docs/api/openapi.yaml`; comita-o no mesmo commit que a mudança ao `.proto`.

## Uma cache de build partilhada ou obsoleta

[Clonar, compilar e testar § Compilar](build-and-test.md#build) já nomeia o compromisso: apontar
vários worktrees para um `CARGO_TARGET_DIR` partilhado poupa disco, mas dois builds a correr
contra ele ao mesmo tempo esperam um pelo outro e **podem invalidar os artefactos um do outro**. O
sintoma é específico e fácil de ler como uma falha a sério: um gate (em particular `test` ou
`clippy`) falha contra código que parece não ter relação com a tua mudança, ou um build termina
depressa demais a mais e o binário que produziu comporta-se como uma versão mais antiga —
incluindo um hook local de pre-commit ou pre-push que reaproveita um directório de target
partilhado entre sessões e liga contra o que quer que estejam lá os ficheiros objecto de um build
diferente e concorrente.

Isto não é um bug do gate: está genuinamente a compilar a coisa errada. Descarta uma cache
obsoleta antes de depurares a "falha" em si:

```bash
cargo clean -p delonix-runtime-bin   # ou o crate para onde a falha aponta
cargo build -p delonix-runtime-bin   # recompila limpo, e corre outra vez o gate que falhou
```

Se trabalhas rotineiramente a partir de vários worktrees ao mesmo tempo, dar a cada um o seu
próprio `CARGO_TARGET_DIR` (sem exportar o partilhado, ou exportando um caminho local ao
worktree) remove esta classe de sintoma por inteiro, ao custo de um primeiro build mais lento em
cada um — o mesmo compromisso que [Clonar, compilar e testar](build-and-test.md#build) já
enuncia.

---

**A seguir:** [Como os nomes chegam ao `/etc/hosts`](service-names-and-hosts.md) — o bloco que publica os nomes de serviço e os hosts de rota, e porque é que ele recusa. Depois, [Convenções de código](coding-conventions.md) — como o código neste repositório tem de ser escrito, cada regra etiquetada com o gate ou a decisão que está por trás.
