<!-- translated-from: build-and-test.md sha256:a509235c85941dead7c564eb0fe69325be76ad29f67752755461f3637af80128 -->
# Clonar, compilar e testar

**Antes de leres:** [Preparar o teu ambiente](environment.md): a toolchain fixada, o `protoc`, e um host que passa nas suas verificações.

Esta página assume o host de [Preparar o teu ambiente](environment.md): a toolchain de Rust
fixada e o `protoc`. Depois dela consegues compilar e instalar a tua árvore, correr cada gate
de CI localmente, e correr a bateria E2E e o arnês de caos sem tocar em estado real do motor.

Tudo aqui corre a partir da raiz da tua checkout — idealmente um **git worktree**: um directório
de trabalho separado com o seu próprio branch, um por tarefa, criado a partir de `origin/main` (o
comando está em [Começa aqui, passo 2](start-here.md#2-open-a-worktree-from-originmain); as
regras estão em [Fluxo de contribuição](contributing-workflow.md#one-worktree-per-task)).

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

## Instalar a tua build localmente

A maioria das mudanças nunca precisa de uma build instalada: corre `./target/debug/delonix` a partir
do teu worktree. Instala só quando precisas de um caminho estável — uma unit do systemd, um script
noutra shell, um kubelet a falar com o `delonix-cri`. Antes e depois de instalar, confirma **que
build** estás a correr:

```bash
./target/debug/delonix --version    # commit: <hash> (+N commits since vX.Y.Z) · built: <date>
command -v delonix                  # which `delonix` your shell would run instead
```

A linha `commit:` vem de `bins/delonix-runtime-bin/build.rs` (`DELONIX_GIT_HASH`,
`DELONIX_GIT_SINCE`). Entre releases todas as builds levam o mesmo número de versão, por isso o
commit é a única forma de distinguir a tua build da publicada.

### Como o `delonix` encontra os seus binários de servidor

`delonix serve cri`, `delonix serve api` e `delonix mcp` não contêm os servidores: fazem `exec` de
`delonix-cri`, `delonix-mgmt` e `delonix-mcp` (`exec_server` em
`bins/delonix-runtime-bin/src/cmd/serve.rs`). A procura é:

1. o ficheiro com esse nome **ao lado do `delonix` em execução**;
2. senão, o nome no `PATH`.

O `delonix` passa ao servidor a sua própria versão em `DELONIX_DISPATCH_VERSION`, e um servidor de
outra release recusa-se a arrancar. Passa-se também a si próprio em `DELONIX_BIN`, para que o
servidor volte a chamar a mesma CLI. Um servidor arrancado directamente (por exemplo por uma unit)
encontra a CLI através de `DELONIX_BIN`, depois de um `delonix` ao seu lado, depois do `PATH`
(`cli_bin` em `crates/contexts/delonix-node/src/dispatch.rs`). **Mantém juntos os quatro
binários de uma mesma build**; uma mistura da tua build com uma release é recusada, ou corre código
que não querias testar.

`delonix cluster kubeadm` e `delonix image vm build` procuram o `delonix-cri` pela sua própria ordem
(`resolve_cri_bin` em `bins/delonix-runtime-bin/src/cmd/vmimage.rs`): `--cri-bin`, depois ao lado
do `delonix`, depois um `cargo build --release -p delonix-cri` se o directório actual estiver dentro
de uma checkout do código-fonte, e só então um download do asset publicado.

Compila os quatro antes de os instalares:

```bash
cargo build --release -p delonix-runtime-bin -p delonix-cri -p delonix-mgmt-bin -p delonix-mcp-bin
```

### Opção A — corrê-la a partir do worktree (a mais segura)

Nada é copiado, por isso nada fora da tua checkout a pode apanhar por acidente:

```bash
alias delonix-dev="$PWD/target/release/delonix"
delonix-dev --version
```

Os servidores são encontrados porque estão ao lado dele em `target/release/`. No Ubuntu 23.10+ este
caminho precisa do seu próprio perfil AppArmor (ver
[AppArmor](environment.md#ubuntu-2310-apparmor-blocks-user-namespaces-for-your-dev-binary)).

### Opção B — instalar para o teu utilizador em `~/.local/bin`

```bash
install -d ~/.local/bin
install -m 0755 target/release/delonix target/release/delonix-cri \
  target/release/delonix-mgmt target/release/delonix-mcp ~/.local/bin/
hash -r                              # forget the path your shell cached
command -v delonix && delonix --version
```

Se também estiver instalada uma release em `/usr/local/bin`, ganha o directório que aparecer
primeiro no `PATH`.

**AppArmor.** O `scripts/install.sh` escreve um perfil, `/etc/apparmor.d/delonix`, associado a
`<install dir>/delonix`, e só em hosts com
`kernel.apparmor_restrict_unprivileged_userns=1`. Um binário que copiaste para um caminho novo não
está coberto. Não voltes a correr o instalador para "mover" esse perfil numa máquina que também usa
uma instalação publicada: o ficheiro do perfil é reescrito, e o binário publicado perde-o. Acrescenta
antes um segundo perfil com outro nome — a mesma forma que o instalador escreve, por isso não
substitui nada:

```bash
printf 'abi <abi/4.0>,\ninclude <tunables/global>\nprofile delonix-dev %s flags=(unconfined) {\n  userns,\n}\n' \
  "$HOME/.local/bin/delonix" | sudo tee /etc/apparmor.d/delonix-dev >/dev/null
sudo apparmor_parser -r /etc/apparmor.d/delonix-dev
```

*Não verificado aqui:* este comando reproduz o `install.sh` (o bloco do AppArmor) com outro nome de
perfil e outro ficheiro; não foi carregado num host com a restrição activa enquanto esta página era
escrita.

O instalador acrescenta também completion da shell, man pages e ficheiros de sintaxe para editores,
mas só na sua fase do binário. Para a tua própria build, gera-os a partir do binário se os quiseres:

```bash
mkdir -p ~/.local/share/bash-completion/completions
delonix completion shell bash > ~/.local/share/bash-completion/completions/delonix
delonix man --dir ~/.local/share/man
```

### Opção C — instalação de sistema em `/usr/local/bin`

```bash
sudo install -m 0755 target/release/delonix target/release/delonix-cri \
  target/release/delonix-mgmt target/release/delonix-mcp /usr/local/bin/
```

**Só numa máquina onde nenhum workload Delonix esteja em uso.** O binário instalado não é só um
comando:

- as units de arranque escritas por `delonix system boot enable` arrancam
  `ExecStart=<exe> container start <name>`, onde `<exe>` é o caminho do binário que correu o
  `enable` (`bins/delonix-runtime-bin/src/cmd/boot.rs`, prefixo de unit `delonix-boot-`); substituir
  esse ficheiro muda o que sobe depois do próximo reboot;
- o `dist/delonix-cri.service` corre `/usr/local/bin/delonix-cri`, por isso num nó Kubernetes o
  kubelet recebe a tua build no próximo reinício dessa unit;
- os processos de longa duração arrancados antes (o pin de rede e o processo de controlo, os
  supervisores de containers) continuam a correr o código com que arrancaram, por isso durante algum
  tempo correm duas builds lado a lado.

Verifica primeiro:

```bash
delonix container ls -a; delonix vm ls
ls ~/.config/systemd/user/delonix-boot-* /etc/systemd/system/delonix-* 2>/dev/null
pgrep -a delonix
```

### Preparar o host

O `scripts/install.sh` faz dois trabalhos separados. Só o primeiro diz respeito ao binário:

| Parte | O que faz | Flag que a salta ou activa |
|---|---|---|
| Binário | descarrega uma release, verifica a assinatura minisign e o sha256, instala o `delonix` (mais `delonix-mcp`, `delonix-mgmt`, e `delonix-cri` com `--with-cri`), depois completion, man pages, sintaxe para editores e a extensão de editor | saltada com `--no-binary`; `--user` escolhe `~/.local/bin` |
| Pacotes do host | `slirp4netns`, `uidmap`, `nftables`, `iproute2`, `conntrack` | sempre |
| Identidade rootless | intervalos em `/etc/subuid` e `/etc/subgid` para o teu utilizador | sempre |
| AppArmor | perfil para `<dir>/delonix` quando a restrição de userns está activa | sempre (quando a restrição está ligada) |
| Debian antigo | `kernel.unprivileged_userns_clone=1` quando está a `0` | sempre (quando necessário) |
| Dependências de VM | libvirt, qemu, ferramentas de cloud-init; Cloud Hypervisor e o seu firmware descarregados do upstream | saltadas com `--no-vm` |
| Afinação do kernel | `/etc/modules-load.d/delonix.conf`, `/etc/sysctl.d/99-delonix.conf` | saltada com `--no-tune` |
| Delegação de cgroup | drop-in do `user@.service`, só se ainda não estiver delegado | saltada com `--no-delegate` |
| Aceleradores | CDI da NVIDIA e grupo `render`, só quando há uma GPU | saltados com `--no-gpu` |
| Opt-ins | portas abaixo de 1024 (`--low-ports`), construção de imagens de VM (`--with-image-build`), afinação para escala (`--production`) | desligados por omissão |

Para preparares um host para a tua própria build **sem descarregar nenhuma release do Delonix**,
corre o instalador a partir da tua checkout com `--no-binary`:

```bash
bash scripts/install.sh --no-binary            # add --no-vm if you do not need VM dependencies
bash scripts/install.sh --help                 # the full flag list, from the script header
```

Com `--no-binary`, o perfil AppArmor é escrito para o directório do `delonix` que o
`command -v delonix` encontrar (ou `/usr/local/bin` se não houver nenhum) — a mesma cautela de cima
aplica-se numa máquina com uma instalação publicada. O script usa `sudo` para os passos do host.

Depois pergunta ao binário se o host está pronto (só leitura):

```bash
delonix system doctor     # every prerequisite, and how to fix each; --strict exits non-zero on a failure
delonix system info       # state root, rootless, cgroup delegation, network infra
```

Ver [Diagnosticar o host](environment.md#diagnosing-the-host) para o significado de cada
verificação.

### Usar um state root isolado

Uma build instalada usa por omissão o teu state root **real**: os mesmos containers, redes e volumes
que a release. Exporta primeiro `DELONIX_ROOT` e `DELONIX_NET_RUNTIME_DIR` (ver
[Isolar o estado do motor](#isolating-the-engines-state)), e ver
[Variáveis de ambiente](environment-variables.md) para todas as outras variáveis que a tua
build lê.

### Desinstalar e reverter

Não há flag de desinstalação no `install.sh`. Remove o que copiaste:

```bash
rm -f ~/.local/bin/delonix ~/.local/bin/delonix-cri ~/.local/bin/delonix-mgmt ~/.local/bin/delonix-mcp
hash -r
sudo apparmor_parser -R /etc/apparmor.d/delonix-dev && sudo rm /etc/apparmor.d/delonix-dev   # if you added it
```

Para voltares a um binário publicado, corre o instalador outra vez; ele substitui os binários no seu
directório de instalação pela release que indicares:

```bash
curl -fsSL https://github.com/angolardevops/delonix-runtime/releases/latest/download/install.sh | bash -s -- --user
curl -fsSL https://github.com/angolardevops/delonix-runtime/releases/latest/download/install.sh | bash -s -- --version vX.Y.Z
```

Os ficheiros de completion e as man pages que geraste à mão não são removidos por nenhum dos dois
passos. Termina com `delonix --version` para confirmares o commit onde voltaste a estar.

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
| `test-arm64` | test (arm64) |
| `deny` | cargo-deny |
| `fuzz` | fuzz (60s smoke, per target) |
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
| `version` | `python3 scripts/version_gate.py` | a versão do workspace não é a tag mais recente que o commit contém (ver [Fluxo de contribuição](contributing-workflow.md#version-alignment)) ou o branch não contém a tag mais recente. Precisa das tags |
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

## Receitas de imagens de VM (`scripts/verify-images.sh`)

As receitas em `images/` são verificadas de duas maneiras. Um teste unitário no crate da CLI
(`vmspec::every_shipped_recipe_is_valid_and_complete`) falha se uma receita deixar de ser lida ou
apontar para um ficheiro ou um builder que não existe. O `scripts/verify-images.sh` vai mais longe:
constrói offline as quatro distros de cloud image num `DELONIX_ROOT` isolado e lê o qcow2 resultante
contra o que a receita declarou; o `--self-test` prova que as verificações conseguem falhar numa imagem
que ninguém construiu. Precisa de `libguestfs-tools` (ver [Construir microVMs](microvm-setup.md)) e não
faz parte dos gates da CI. As fases `--packages`, `--profile`, `--boot` e `--appliance` existem mas
não tinham sido corridas quando a v4.2.0 foi lançada.

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
**worktrees** não — ver [Fluxo de contribuição](contributing-workflow.md#one-worktree-per-task).

---

**Seguinte:** [Estrutura do projecto](project-structure.md) — o mapa do repositório: o que é cada directório, quem o muda, e o que é gerado.
