<!-- translated-from: 02-build-and-test.md sha256:f6f565efdea9ee4a51a174b403817cc3a4bf45f688e9a1906b64109a1e8b5797 -->
# 2. Clonar, compilar e testar

Esta página assume o host de [01 — Preparar o teu ambiente](01-environment.md): a toolchain de Rust
fixada e o `protoc`. Tudo aqui corre a partir da raiz da tua checkout — idealmente um
**git worktree**, ver [10 — Fluxo de contribuição](10-contributing-workflow.md).

## Clonar

```bash
git clone https://github.com/angolardevops/delonix-runtime.git
cd delonix-runtime
git fetch --tags origin      # several gates compare against the release tags
```

## Compilar

```bash
cargo build --workspace                    # every crate and every binary
cargo build -p delonix-runtime-bin         # just the `delonix` CLI
cargo build --release -p delonix-runtime-bin   # what the docs generator and the CLI gates use
```

O workspace produz estes binários, a partir destes pacotes:

| Binário | Pacote | Saída |
|---|---|---|
| `delonix` (a CLI) | `delonix-runtime-bin` | `target/debug/delonix` ou `target/release/delonix` |
| `delonix-cri` | `delonix-cri` | `target/<profile>/delonix-cri` |
| `delonix-mcp` | `delonix-mcp-bin` | `target/<profile>/delonix-mcp` |
| `delonix-mgmt` | `delonix-mgmt-bin` | `target/<profile>/delonix-mgmt` |

O workflow de release compila exactamente estes quatro pacotes. Se definires `CARGO_TARGET_DIR`, os
binários vão para lá em vez de `target/`.

Duas notas práticas:

- **Testa sempre o binário que compilaste** (`./target/debug/delonix`), nunca um `delonix` no teu
  `PATH` — esse é uma release instalada, muitas vezes várias versões atrás.
- Se trabalhas em vários worktrees, apontá-los todos para um `CARGO_TARGET_DIR` partilhado poupa
  disco, mas dois builds a correr ao mesmo tempo sobre ele esperam um pelo outro e podem invalidar os
  artefactos um do outro. Um directório de target por worktree é mais lento da primeira vez e
  previsível depois disso.

## Correr os testes

```bash
cargo test --workspace                       # the whole suite
cargo test -p delonix-sdn                    # one crate
cargo test -p delonix-stack -- reconcile       # tests whose path contains "reconcile"
cargo test -p delonix-stack -- --exact kinds::tests::nenhum_kind_aparece_duas_vezes
```

Os testes que precisam de privilégios ou de um host real saltam-se a si próprios em vez de falharem,
por isso a suite tem significado num portátil e na CI. Alguns testes ao vivo estão marcados com
`#[ignore]` e nomeiam o comando para os correr no seu doc comment (por exemplo em
`crates/adapters/delonix-vm/src/lib.rs`); corre-os só numa máquina que é tua:

```bash
cargo test -p <crate> -- --ignored <test-name>
```

Um `cargo test` verde prova a lógica pura. **Não** prova que uma mudança em namespaces, cgroups, no
holder de rede ou no arranque de VMs funciona — isso precisa de uma corrida ao vivo (ver
[Bateria end-to-end](#end-to-end-battery-scriptse2esh) e [Arnês de caos](#chaos-harness-scriptschaossh)).

## Os gates que a CI corre

<!-- dev-docs:begin ci-gates -->
| Job de CI | O que verifica |
|---|---|
| `fmt` | rustfmt |
| `lang` | lang ratchet |
| `arch` | arch fitness |
| `contract` | contract gate |
| `version` | version gate |
| `cli-surface` | cli surface |
| `clippy` | clippy -D warnings |
| `test` | test |
| `deny` | cargo-deny |
| `docs` | generated docs and valid examples |
<!-- dev-docs:end ci-gates -->

Todos os jobs de `.github/workflows/ci.yml` podem ser reproduzidos localmente. Corre os que
correspondem ao que tocaste antes de fazeres push; corre todos antes de pedires revisão.

| Job | Comando local | Falha quando |
|---|---|---|
| `fmt` | `cargo fmt --all --check` | o código não está formatado pelo rustfmt (configuração por omissão) |
| `lang` | `python3 scripts/lang_ratchet.py` | identificadores, comentários ou mensagens em português **aumentam** — ou diminuem sem baixar o `scripts/lang_baseline.json` no mesmo commit (`--list` mostra-os, `--update` baixa a linha de base) |
| `arch` | `python3 scripts/arch_fitness.py` | uma dependência vai contra a direcção das camadas, um crate está no directório errado, um crate membro fixa a versão de uma dependência, um nome de consumidor aparece no código, ou um ratchet de dívida se move (`--list`, `--update`) |
| `arch` | `python3 scripts/dev_docs.py --check` | um facto gerado em `docs/dev/` está desactualizado — corre `python3 scripts/dev_docs.py` e faz commit |
| `contract` | `python3 scripts/contract_gate.py` | o contrato de nó em `proto/delonix/node/v1` não está limpo segundo o `buf format`, falha o `buf lint`, quebra a compatibilidade com a última tag, não tem um mapeamento HTTP, ou o `docs/api/openapi.yaml` não é o gerado (`--update` reescreve-o). Precisa de `protoc`, `buf` v1.73.0 e `protoc-gen-openapi` v0.7.1 no `PATH`, e das tags |
| `version` | `python3 scripts/version_gate.py` | a versão do workspace não é a tag mais recente que o commit contém (ver [10](10-contributing-workflow.md#version-alignment)) ou o branch não contém a tag mais recente. Precisa das tags |
| `cli-surface` | `cargo build --release -p delonix-runtime-bin && scripts/cli-tree.sh --gate` | uma folha da CLI foi acrescentada, removida ou reclassificada sem actualizar o `scripts/cli_baseline.tsv` no mesmo commit (`scripts/cli-tree.sh --update`) |
| `cli-surface` | `python3 scripts/docs_cli_gate.py` | um comando `delonix …` citado na documentação actual não existe na árvore do binário |
| `clippy` | `cargo clippy --workspace --all-targets --locked -- -D warnings` | qualquer aviso |
| `test` | `cargo build --workspace --locked && cargo test --workspace --locked --no-fail-fast` | qualquer teste falha |
| `deny` | `cargo deny check advisories licenses sources` | um aviso RUSTSEC, uma licença ou fonte não permitida (`deny.toml`) |
| `docs` | `cargo build --release -p delonix-runtime-bin && python3 docs/gen.py && git diff --exit-code -- docs/` | o site em commit não é o que o gerador produz a partir deste binário |
| `docs` | `./target/release/delonix stack apply -f examples/<file>.yaml --dry-run` e `./target/release/delonix stack validate -f examples/<file>.yaml` | um exemplo publicado usa uma forma obsoleta ou tem referências por resolver |

O `cli-tree.sh` e o `docs_cli_gate.py` lêem a árvore a partir do `--help` real do binário; define
`DELONIX_BIN=/path/to/delonix` para escolher qual binário. O `docs/gen.py` usa por omissão
`target/release/delonix` e precisa do módulo Python `markdown`. O job `docs` também gera as
man pages (`delonix man --dir <dir> --index`) e verifica-as com `groff -mandoc -ww -z`.

Workflows à parte, não exigidos em todas as mudanças: o `chaos.yml` corre o arnês de caos num runner
limpo (e reporta `skipped` quando o runner bloqueia user namespaces), o `release.yml` publica uma
tag, e o `vm-image.yml` / `vm-appliances.yml` constroem imagens de VM.

## Isolar o estado do motor

Tudo o que vá além do `--help` toca no estado do motor. Por omissão esse é **o teu estado real**: os
teus containers, redes, volumes e o holder de rede. Antes de correres o motor para testar —
à mão, através do `e2e.sh`, ou através de qualquer script — aponta **as duas** raízes de estado para
um directório descartável:

```bash
export DELONIX_ROOT=$HOME/scratch/dlx/root          # containers, images, networks, IPAM, volumes
export DELONIX_NET_RUNTIME_DIR=/tmp/dlx-run          # the holder's control and slirp sockets
mkdir -p "$DELONIX_ROOT" "$DELONIX_NET_RUNTIME_DIR"
```

**As duas, sempre. Meia isolação é pior do que nenhuma.** Os sockets de rede e os pidfiles são
resolvidos separadamente: os pidfiles vivem debaixo da *raiz de estado*, enquanto os sockets de
controlo e do slirp do holder vivem num *directório de runtime* (por omissão `/tmp/delonix-net-<uid>`).
Quando duas raízes de estado acabaram no mesmo directório de runtime, cada uma leu o seu próprio
pidfile (ausente), concluiu que não havia infra de rede, e arrancou ou desmontou infra por cima dos
sockets da outra. Num host de desenvolvimento com workloads vivos isto acabou com a raiz real a
reconstruir a sua infra de rede e a reiniciar containers reais.

O motor deriva agora um sufixo a partir de um `DELONIX_ROOT` que não seja o de omissão para o
directório de runtime (`runtime_dir`/`root_suffix` em `crates/adapters/delonix-sdn/src/infra.rs`), o
que fecha essa colisão no caso comum. Continua a exportar as duas na mesma: torna a isolação
explícita, mantém o caminho do socket curto e sob o teu controlo, e é o que o `scripts/e2e.sh` e o
`scripts/chaos.sh` fazem (o e2e preenche a variável que não tiveres exportado).

Mantém o `DELONIX_NET_RUNTIME_DIR` curto: um caminho de socket unix com mais de cerca de 108 bytes
falha com `path must be shorter than SUN_LEN`. O `e2e.sh` recusa um directório de runtime com mais de
80 bytes.

Quando terminares, desmonta a infra de rede isolada com as mesmas duas variáveis exportadas:

```bash
./target/debug/delonix net netns down
```

## Bateria end-to-end (`scripts/e2e.sh`)

O `e2e.sh` corre a CLI contra o kernel real: o `--help` de todas as folhas, mais execuções reais de
uma grande parte da superfície, e imprime um relatório PASS/FAIL/SKIP/XFAIL (detalhe em JSONL em
`$OUT/results.jsonl`, por omissão `OUT=/tmp/delonix-e2e`).

```bash
./scripts/e2e.sh                          # uses ./target/debug/delonix
./scripts/e2e.sh ./target/release/delonix
```

- **Isola-se por omissão**: define `DELONIX_ROOT` e `DELONIX_NET_RUNTIME_DIR` para directórios
  próprios (a menos que exportes as duas primeiro) e desmonta a infra que arrancou.
  `E2E_SHARED_STATE=1` corre contra o estado real da máquina — só para diagnosticar um host.
- O código de saída é diferente de zero quando uma verificação falha, ou quando uma verificação
  marcada como defeito conhecido (`XFAIL`) passa inesperadamente. Os SKIPs não fazem falhar a corrida
  mas são listados num bloco próprio: uma verificação saltada não provou nada.
- Precisa de acesso à rede para fazer pull de imagens; as secções cujas pré-condições faltem saltam-se
  com a razão.
- Uma corrida verde significa que o `--help` de todas as folhas foi verificado e que *algumas* folhas
  foram executadas. Lê o cabeçalho do script para saber o que é executado e o que não é.

## Arnês de caos (`scripts/chaos.sh`)

O arnês de caos parte de propósito um motor a correr — mata o holder, enche o disco, faz attaches
concorrentes, applies parciais — e reporta se degradou da forma que promete.

```bash
scripts/chaos.sh                        # every scenario, ./target/debug/delonix
scripts/chaos.sh holder_kill oom        # selected scenarios
scripts/chaos.sh --keep scale           # leave the sandbox up for a post-mortem
scripts/chaos.sh --clean                # tear the kept sandbox down
```

- Redirecciona sempre as duas raízes para o seu sandbox (`DELONIX_CHAOS_DIR`, por omissão
  `/tmp/dlx-chaos`) e nunca toca nos containers, redes ou registos do motor real. Os directórios de
  imagens (`images`, `layers`, `blobs`) são **symlinks para o teu store real**, para evitar downloads:
  na prática o arnês só os lê, mas um cenário que escrevesse uma imagem escreveria no store real.
- **Recusa-se a correr numa máquina ocupada** (carga acima de um limiar, partilhado com o
  `scripts/bench.sh` através do `scripts/bancada.sh`): sob carga, os cenários falham por razões que
  pertencem à bancada, não ao produto. `--max-load N` muda o limiar; `--force` corre na mesma e marca
  o veredicto como não publicável.
- O código de saída só é 0 quando nenhum cenário falha. Os SKIPs são listados à parte.
- Alguns cenários precisam de recursos externos e saltam-se sem eles (por exemplo o `truenas_destroy`
  precisa de `DELONIX_CHAOS_TRUENAS_URL`/`_USER`/`_PASS`).

Os directórios descartáveis debaixo de `/tmp` servem para estes sandboxes efémeros. Os teus
**worktrees** não — ver [10](10-contributing-workflow.md#one-worktree-per-task).
