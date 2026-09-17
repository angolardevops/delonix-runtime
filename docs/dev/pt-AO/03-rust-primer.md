<!-- translated-from: 03-rust-primer.md sha256:dd6253c8cdac5303088447fbb92e95dbaf879d687b06fbd24391c45de9887aeb -->
# 3. Introdução ao Rust para esta base de código

Isto não é um tutorial de Rust. É o subconjunto de Rust de que precisas para *ler este repositório*,
com cada ideia presa a um ficheiro que podes abrir. Se um conceito for novo para ti, os links
«Ler mais» levam à fonte oficial; volta aqui para ver como o motor o usa.

Os caminhos são relativos à raiz do repositório. Os símbolos são nomeados para os poderes procurar
com `grep` — os números de linha ficam de fora de propósito, porque mudam a cada PR.

A toolchain fixada está no `rust-toolchain.toml` (um `channel` fixo, mais `rustfmt` e
`clippy`). O `rustup` respeita-o automaticamente, por isso compilas com o mesmo compilador que a CI usa.

---

## 3.1 O workspace do Cargo

O repositório é um único **workspace** do Cargo: um `Cargo.toml` na raiz com
`[workspace] members = [...]` e um `Cargo.toml` por crate. Aqui importam três convenções:

1. **Cada caminho e cada versão escreve-se uma vez, na raiz.** A raiz tem uma tabela
   `[workspace.dependencies]` que lista tanto os crates do próprio motor (por `path`) como todos
   os crates de terceiros (por `version`). Um membro nunca escreve uma versão; escreve:

   ```toml
   # crates/foundation/delonix-runtime-core/Cargo.toml
   [dependencies]
   serde = { workspace = true }
   thiserror = { workspace = true }
   ```

   Um membro pode acrescentar `features = [...]`, e mais nada. O `default-features = false` vive na
   raiz porque um membro não consegue desligar defaults que o workspace ligou.
   O `scripts/arch_fitness.py` (função `inline_versions`) faz falhar o build se um membro escrever
   a sua própria versão.

2. **O directório é a camada.** Os crates vivem em `crates/foundation/`, `crates/contexts/`,
   `crates/adapters/`, `crates/providers/`, `crates/interfaces/` e os binários em `bins/`
   (ADR-0040). O `scripts/arch_fitness.py` recusa um crate cujo directório não corresponda à sua
   camada declarada, e uma dependência que aponte contra a direcção permitida. Ver
   [5. Arquitectura](05-architecture.md) para as regras das camadas.

3. **Os lints são herdados.** A raiz declara `[workspace.lints.clippy]` com
   `undocumented_unsafe_blocks = "deny"`, e cada membro adere com `[lints] workspace = true`.
   Todos os blocos `unsafe` levam por isso um comentário `// SAFETY:` (ver §3.4).

A raiz define também `[workspace.package]` (a `version`, a `edition` e a `license` partilhadas), que
os membros consomem como `version.workspace = true`. A versão não é decoração: ver
[2. Compilar e testar](02-build-and-test.md) para o gate de versão.

**Ler mais:** Cargo Book —
[Workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html),
[`[workspace.dependencies]`](https://doc.rust-lang.org/cargo/reference/workspaces.html#the-dependencies-table),
[`[lints]`](https://doc.rust-lang.org/cargo/reference/manifest.html#the-lints-section),
[`rust-toolchain.toml`](https://rust-lang.github.io/rustup/overrides.html#the-toolchain-file).

---

## 3.2 Erros: um só `Error`, e códigos de saída derivados do seu tipo

Quase todas as funções falíveis do motor devolvem `delonix_runtime_core::Result<T>`, um alias
sobre o enum partilhado definido em `crates/foundation/delonix-model/src/error.rs`. O enum vive no
crate de fundação puro `delonix-model`; o `delonix-runtime-core` re-exporta `Error` e `Result`
(`pub use delonix_model::{Error, Result};`), por isso o caminho `delonix_runtime_core::` que a maioria
dos pontos de chamada usa continua a funcionar. É construído com
[`thiserror`](https://docs.rs/thiserror): `#[derive(Error)]` gera o `Display` a partir do atributo
`#[error("...")]`, e `#[from]` gera impls de `From` para que o `?` converta automaticamente um erro de
nível mais baixo:

```rust
// crates/foundation/delonix-model/src/error.rs
#[error("I/O error: {0}")]
Io(#[from] std::io::Error),
```

As variantes dizem respeito ao **que quem chama deve fazer a seguir**, não a que subsistema falhou:
`NotFound`, `NotRunning`, `Conflict`, `Unavailable` (uma ferramenta ou funcionalidade do kernel em
falta), `Timeout`, `Invalid`, `Registry`, `Runtime { context, message }` para uma syscall falhada, e
por aí fora. Os doc comments de cada variante explicam porque é que ela existe; lê-os antes de
acrescentares uma variante.

Essas variantes tornam-se **códigos de saída do processo** num só sítio:
`crates/foundation/delonix-model/src/exitcode.rs`, função `for_error`. A CLI re-exporta o
módulo (`pub use delonix_model::exitcode;` em `bins/delonix-runtime-bin/src/cmd/mod.rs`) e o
`bins/delonix-runtime-bin/src/main.rs` chama `std::process::exit(cmd::exitcode::for_error(&e))`.

```rust
// crates/foundation/delonix-model/src/exitcode.rs
pub fn for_error(e: &Error) -> i32 {
    match e {
        Error::NotFound(_) | Error::VmNotFound(_) => NOT_FOUND,
        Error::NotRunning(_) => NOT_RUNNING,
        // ...
```

Duas coisas a notar:

- O `match` é **exaustivo, sem braço `_ =>`**. Acrescentar uma variante ao `Error` pára o build
  no `for_error` (e no `Error::code` em `error.rs`) até alguém decidir a sua classe. É um uso
  deliberado do compilador como lista de verificação.
- As mensagens são traduzidas para o operador, por isso os scripts têm de ramificar pelo código de
  saída (ou pelo código de máquina estável de `Error::code`), nunca pelo texto da mensagem.

**Ler mais:** The Rust Book —
[Recoverable errors with `Result`](https://doc.rust-lang.org/book/ch09-02-recoverable-errors-with-result.html),
[o operador `?`](https://doc.rust-lang.org/book/ch09-02-recoverable-errors-with-result.html#a-shortcut-for-propagating-errors-the--operator);
Rust by Example — [Defining an error type](https://doc.rust-lang.org/rust-by-example/error/multiple_error_types/define_error_type.html);
[`thiserror` no docs.rs](https://docs.rs/thiserror).

---

## 3.3 Traits como portas: `VmBackend` e o registo de backends

O motor fala com os providers através de **traits** («portas»), e um provider é uma implementação
de uma delas. O exemplo mais claro é o `VmBackend` em `crates/adapters/delonix-vm/src/lib.rs`:

```rust
pub trait VmBackend {
    fn id(&self) -> &'static str;
    fn available(&self) -> bool;
    fn boot(&self, vmdir: &Path, cfg: &VmConfig, overlay: &str,
            on: &dyn Fn(CreateStage)) -> Result<Boot>;
    fn is_running(&self, vm: &Vm) -> bool;
    fn ip(&self, vm: &Vm) -> Option<String>;
    // ... more methods, many with default bodies
}
```

Implementações: `CloudHypervisorBackend` e `LibvirtBackend` no mesmo ficheiro, e
`ProxmoxBackend` em `crates/providers/delonix-proxmox/src/lib.rs`. Os métodos com **corpo por
omissão** no trait (por exemplo `auto_selectable`) deixam um backend novo herdar um comportamento
sensato e sobrepor só o que difere.

Os backends são escolhidos em tempo de execução, por isso são tratados como **trait objects**,
`Box<dyn VmBackend>`. São criados através de um registo de factories:

```rust
// crates/adapters/delonix-vm/src/lib.rs
pub type BackendFactory = Box<dyn Fn() -> Result<Box<dyn VmBackend>> + Send + Sync>;

static BACKENDS: std::sync::OnceLock<std::sync::RwLock<Vec<BackendRegistration>>> =
    std::sync::OnceLock::new();
```

- A factory é uma **closure** (`dyn Fn`), porque um backend remoto precisa de configuração
  capturada (endpoint, nó, credencial) que um simples ponteiro `fn()` não consegue transportar.
- `Send + Sync` é exigido porque a tabela é um `static` de todo o processo; o bound restringe a
  closure, não o trait `VmBackend`.
- O `OnceLock` inicializa a tabela de forma preguiçosa (`builtin_backends()` semeia os dois backends
  locais), e o `RwLock` deixa o `register_backend` acrescentar um terceiro no arranque. O binário
  fá-lo em `bins/delonix-runtime-bin/src/cmd/vmbackends.rs` (`register_configured`).

A decisão por trás desta forma é o [ADR-0008](../../adr/0008-proxmox-vm-backend.md). O mesmo padrão
«trait + implementações + um só sítio que escolhe» aparece noutros lados (por exemplo a porta
`VmNetwork`, guardada num `OnceLock<Box<dyn VmNetwork>>` perto do topo do mesmo ficheiro).

**Ler mais:** The Rust Book —
[Traits](https://doc.rust-lang.org/book/ch10-02-traits.html),
[Trait objects](https://doc.rust-lang.org/book/ch18-02-trait-objects.html),
[Closures](https://doc.rust-lang.org/book/ch13-01-closures.html),
[`Send` e `Sync`](https://doc.rust-lang.org/book/ch16-04-extensible-concurrency-sync-and-send.html);
documentação da std — [`OnceLock`](https://doc.rust-lang.org/std/sync/struct.OnceLock.html).

---

## 3.4 `unsafe`, FFI e syscalls do Linux

Um motor de containers é sobretudo chamadas de sistema. Este repo chega ao kernel através de três crates:

| Crate | Usado para | Exemplo neste repo |
|---|---|---|
| [`nix`](https://docs.rs/nix) | Wrappers mais ou menos seguros: `clone`, `setns`, `unshare`, `pivot_root`, `fork`, `mount`, sinais | `use nix::sched::{clone, setns, unshare, CloneFlags};` em `crates/adapters/delonix-linux/src/lib.rs` |
| [`libc`](https://docs.rs/libc) | Chamadas cruas que o `nix` não embrulha, ou onde a struct exacta importa | `libc::getsockopt(.., SO_PEERCRED, ..)` em `crates/foundation/delonix-runtime-core/src/peer_cred.rs` (`peer_uid`); `libc::flock` em `crates/adapters/delonix-state/src/store.rs` |
| [`rustix`](https://docs.rs/rustix) | A nova API de mount (`fsopen`/`fsconfig`/`fsmount`/`move_mount`) | `fsopen_overlay` em `crates/adapters/delonix-linux/src/lib.rs` |

**Todos os blocos `unsafe` dizem porque são correctos**, ao lado deles (o lint do workspace impõe a
presença do comentário; os revisores impõem a sua veracidade):

```rust
// crates/foundation/delonix-runtime-core/src/peer_cred.rs
// SAFETY: getsockopt on SO_PEERCRED with a correctly-sized ucred buffer.
let r = unsafe { libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, ...) };
```

### Onde o container nasce

A `fn spawn` em `crates/adapters/delonix-linux/src/lib.rs` termina em:

```rust
// SAFETY: single-threaded; the child mounts the container and does `exec`.
let cloned = unsafe { clone(cb, &mut stack, flags, Some(Signal::SIGCHLD as i32)) };
```

O filho corre `container_init`, que chama `setup_rootfs` (bind mounts, depois `pivot_root`),
`drop_capabilities`, instala o seccomp, e por fim faz `execvp`. O `setns` é a forma como o `exec`
entra nos namespaces de um container existente. Os mapas de uid/gid de um user namespace são escritos
a partir do pai (`write_userns_maps`, que usa os helpers `newuidmap`/`newgidmap` quando disponíveis).

### Porque é que `fork`/`clone` num processo multi-thread é perigoso

Depois de um `fork` ou `clone`, só a thread que chamou existe no filho, mas o filho herda todos os
locks *tal como estavam* — incluindo locks seguros por threads que já não existem. Se outra thread
segurava o lock do alocador nesse instante, a primeira alocação do filho bloqueia para sempre. O
`fork` mitiga parte disto com handlers `pthread_atfork`; o `clone` cru não os corre.

Este repo tem duas regras concretas por causa disto, ambas explicadas em comentários que deves ler:

- **O `serve docker-api` nunca chama `spawn` dentro do processo.** O doc comment acima do helper de
  re-exec em `bins/delonix-runtime-bin/src/cmd/dockerapi.rs` explica que o servidor é um runtime tokio
  multi-thread, por isso re-executa o binário (`__apirun <spec.json>`) para obter um processo novo de
  uma só thread, onde a pré-condição do `clone` volta a ser verdadeira.
- **Depois de um `fork` cru, só trabalho async-signal-safe.** O `reexec_mapped` e o
  `reexec_mapped_hold` em `crates/adapters/delonix-linux/src/lib.rs` pré-calculam todas as alocações
  *antes* do fork. O comentário no `reexec_mapped_hold` regista também porque usam `fork` cru em vez de
  `std::process::Command` + `pre_exec`: o `Command::spawn` espera que o filho chegue ao `exec`, e
  um hook `pre_exec` que bloqueie à espera do pai faz deadlock.

### A nova API de mount, e porque existe aqui

O `mount_overlay_if_marked` monta a raiz overlay de um container através de `rustix::mount::fsopen` e
de uma chamada `fsconfig_set_string(&fs, "lowerdir+", lower)` por camada. O `mount(2)` clássico mete
todas as opções numa única string `data` do tamanho de uma página, que o kernel trunca em silêncio
para imagens com muitas camadas. A medição e a decisão estão no
[ADR-0037](../../adr/0037-overlay-mount-new-api.md). É um bom exemplo do hábito do repo:
o comentário `///` acima de uma syscall pouco óbvia explica a falha que a motivou.

**Ler mais:** The Rust Book — [Unsafe Rust](https://doc.rust-lang.org/book/ch20-01-unsafe-rust.html);
o [Rustonomicon](https://doc.rust-lang.org/nomicon/) (sobretudo
[FFI](https://doc.rust-lang.org/nomicon/ffi.html)); man pages
[`clone(2)`](https://man7.org/linux/man-pages/man2/clone.2.html),
[`setns(2)`](https://man7.org/linux/man-pages/man2/setns.2.html),
[`pivot_root(2)`](https://man7.org/linux/man-pages/man2/pivot_root.2.html),
[`fork(2)`](https://man7.org/linux/man-pages/man2/fork.2.html),
[`signal-safety(7)`](https://man7.org/linux/man-pages/man7/signal-safety.7.html),
[`fsopen(2)`](https://man7.org/linux/man-pages/man2/fsopen.2.html).

---

## 3.5 Serialização: serde, manifestos YAML, schema gerado

O estado em disco é JSON; os manifestos são YAML. Os dois passam por derives do
[`serde`](https://serde.rs/).

**Registos retrocompatíveis.** Um registo escrito por um binário mais antigo tem de continuar a
carregar. A regra é: um campo novo leva `#[serde(default)]`, e quando «ausente» tem de significar algo
diferente do default do tipo, passa a `Option`:

```rust
// bins/delonix-runtime-bin/src/cmd/vmimage.rs  (VmImage)
#[serde(default)]
pub cloud_init: Option<bool>,
```

Aqui `None` (todos os registos escritos antes de o campo existir) é lido como «sim»; um simples `bool`
teria como default `false` e mudaria em silêncio o comportamento das imagens antigas. Vais ver o
mesmo raciocínio em `Mount::propagation` e `Mount::optional` em
`crates/foundation/delonix-runtime-core/src/lib.rs` (`#[serde(default, skip_serializing_if = "Option::is_none")]`).

**Manifestos.** O `bins/delonix-runtime-bin/src/cmd/manifest.rs` lê YAML multi-documento com
`serde_yaml::Deserializer::from_str(text)` para `ManifestDoc`, cujo `spec` fica como um
`serde_yaml::Value` cru até o Kind dono o desserializar no seu spec tipado.

**Schema gerado.** Os tipos de spec derivam `schemars::JsonSchema` (por exemplo `PodSpec` em
`crates/contexts/delonix-compute/src/pod.rs`), e o `bins/delonix-runtime-bin/src/cmd/schema.rs`
gera a partir deles o JSON Schema publicado ([ADR-0007](../../adr/0007-generated-manifest-schema.md)).
Repara no comentário lá: o schemars respeita `#[serde(rename)]` mas não `#[serde(alias)]`.

**Ler mais:** [serde.rs](https://serde.rs/) —
[atributos de campo](https://serde.rs/field-attrs.html) (`default`, `skip_serializing_if`, `alias`);
[`serde_yaml`](https://docs.rs/serde_yaml); [`schemars`](https://docs.rs/schemars).

---

## 3.6 A CLI: clap derive e saída traduzida

O binário `delonix` é o `bins/delonix-runtime-bin`. O seu nível de topo é um derive do
[`clap`](https://docs.rs/clap) em `src/main.rs`: `#[derive(Parser)] struct Cli`, que contém uma
flag global `--l18n` e `#[command(subcommand)] cmd: Cmd`, onde `enum Cmd` é um
`#[derive(Subcommand)]`. Cada grupo tem o seu próprio módulo e enum em `src/cmd/` — por exemplo
`pub enum ContainerCmd` em `src/cmd/container.rs`.

Os doc comments (`///`) nas variantes e nos campos tornam-se o texto do `--help`. Vais ver
`#[allow(clippy::large_enum_variant)]` nos enums de comandos, com um comentário a explicar porquê: são
lidos uma vez por invocação, por isso pôr as variantes em `Box` não compraria nada.

**As strings na fonte são em inglês; o português vem de um catálogo.** O `src/cmd/po.rs` embute
`data/pt.po` com `include_str!` e expõe:

- `po::t("already exists, nothing to do")` — uma string fixa;
- `po::tf("port {port} is taken by '{owner}'", &[("port", &hp), ("owner", &ow)])` — um template
  com placeholders **nomeados**, porque uma tradução pode reordená-los;
- `po::translate_help`, que reescreve o texto de ajuda do clap depois de o `po::peek_lang` ter
  decidido a língua *antes* do parse.

Uma string nova visível ao utilizador é inglês no código mais uma entrada em `data/pt.po`. A política
de língua (LANG-01) e o seu gate são tratados em [10. Fluxo de contribuição](10-contributing-workflow.md).

**Ler mais:** [tutorial do clap derive](https://docs.rs/clap/latest/clap/_derive/_tutorial/index.html);
std — [`include_str!`](https://doc.rust-lang.org/std/macro.include_str.html).

---

## 3.7 Async e gRPC — e porque é que a maior parte do motor é síncrona

O núcleo do motor é **síncrono**: a CLI arranca, faz o seu trabalho com syscalls bloqueantes e I/O de
ficheiros, e sai. Não há daemon nem runtime async ambiente. O código async só aparece nas
*interfaces* que servem um protocolo:

- **Servidor CRI** — `crates/interfaces/delonix-cri`. Os stubs gRPC são gerados em tempo de build:
  o `build.rs` chama `tonic_build::configure()...compile_protos(&["proto/api.proto"], &["proto"])`.
  Isso precisa do **`protoc`** instalado (a CI instala o `protobuf-compiler`); um `protoc` em falta é
  a falha mais comum no primeiro build. O `serve_blocking` em `src/lib.rs` cria o seu próprio runtime
  `tokio::runtime::Builder::new_multi_thread()`, e os handlers empurram o trabalho bloqueante do motor
  para fora dos workers async através do helper `blocking` em `src/runtime_svc.rs` — senão um
  `clone` ou uma chamada ao `delonix` pela shell bloquearia os workers do tokio.
- **Reverse proxy L7** — o `bins/delonix-runtime-bin/src/cmd/ingress_proxy.rs` usa
  [`hyper`](https://docs.rs/hyper) directamente (`hyper::service::service_fn`) sobre o seu próprio
  runtime tokio.
- **Servidor MCP** — o `crates/interfaces/delonix-mcp` usa [`rmcp`](https://docs.rs/rmcp) sobre
  stdio (`rmcp::transport::io::stdio`).
- **Telemetria** — o `crates/adapters/delonix-telemetry/src/telemetry.rs` exporta spans OpenTelemetry
  a partir de uma thread dedicada com um cliente HTTP *bloqueante*, precisamente para que a CLI
  síncrona não precise de um runtime.

O contrato de nó em `proto/delonix/node/v1` é uma API protobuf separada com o seu próprio gate
(`scripts/contract_gate.py`, `buf`); ver [2. Compilar e testar](02-build-and-test.md).

**Ler mais:** [Asynchronous Programming in Rust](https://rust-lang.github.io/async-book/);
[tutorial do Tokio](https://tokio.rs/tokio/tutorial);
[`tonic`](https://docs.rs/tonic) e [`tonic-build`](https://docs.rs/tonic-build);
[instalação do Protocol Buffers](https://protobuf.dev/installation/).

---

## 3.8 Concorrência e estado partilhado

Não há base de dados. O estado são ficheiros JSON debaixo da raiz de estado, e **vários processos**
(duas invocações da CLI, o servidor CRI, um supervisor) podem tocar no mesmo registo ao mesmo tempo. O
padrão para qualquer leitura-modificação-escrita é o `update` com uma closure, sob um `flock`
exclusivo:

```rust
// crates/adapters/delonix-state/src/store.rs  (Store::update)
let id = self.load(id_or_name)?.id;
let _lock = FileLock::acquire(&self.lock_path(&id))?;
// Re-read UNDER the lock ...
let mut c = self.load(&id)?;
if !f(&mut c) { return Ok(c); }
self.save(&c)?;
```

O `JsonStore<T>::update` no mesmo ficheiro é a versão genérica para outros tipos de registo. Ambos
**recusam** correr quando o lock não pode ser obtido (`Error::Lock`), em vez de continuarem sem lock.
(O `SecretStore::update` em `secret.rs` é a excepção: o lock dele é best-effort.) Regras que
daí decorrem:

- Nunca faças `load` → mutar → `save` à mão para um registo que outro processo possa escrever; vais
  perder actualizações. Usa o `update`.
- Volta a ler dentro do lock. O valor que carregaste antes pode já estar desactualizado.
- O `FileLock` liberta o lock no `Drop` — o padrão RAII.

Dentro de um único processo, o estado partilhado usa os tipos padrão: um
`Arc<std::sync::RwLock<Arc<Vec<Route>>>>` guarda a tabela de rotas trocável a quente do proxy
(`SharedRoutes` em `cmd/ingress_proxy.rs`), e o dashboard partilha a sua amostra lenta através de um
`Arc<Mutex<...>>` (`cmd/dash.rs`).

Uma ideia relacionada que vais encontrar em `crates/foundation/delonix-runtime-core/src/typestate.rs`: o
padrão **typestate**, em que os estados do ciclo de vida são tipos e as transições ilegais não
compilam (os seus doc tests incluem exemplos `compile_fail`).

**Ler mais:** The Rust Book —
[Shared-state concurrency](https://doc.rust-lang.org/book/ch16-03-shared-state.html),
[`Drop`](https://doc.rust-lang.org/book/ch15-03-drop.html);
[`flock(2)`](https://man7.org/linux/man-pages/man2/flock.2.html);
std — [`Arc`](https://doc.rust-lang.org/std/sync/struct.Arc.html),
[`RwLock`](https://doc.rust-lang.org/std/sync/struct.RwLock.html).

---

## 3.9 Testes

- **Os testes unitários vivem ao lado do código**, num `#[cfg(test)] mod tests { ... }` no fundo do
  ficheiro. Muitas funções puras existem precisamente para que uma decisão possa ser testada como dados
  (por exemplo `reconcile::plan` em `crates/contexts/delonix-stack/src/reconcile.rs`).
- **Os testes de integração** vivem em `crates/<layer>/<crate>/tests/*.rs` e usam só a API pública do
  crate. Exemplo: o `crates/interfaces/delonix-cri/tests/grpc_status.rs` arranca o servidor CRI
  num socket Unix e fala com ele através do *cliente gRPC gerado* (é por isso que o `build.rs` define
  `build_client(true)`). Os testes em `crates/providers/*/tests/live.rs` precisam de um provider real e
  são opt-in.
- **Os testes de propriedade** usam [`proptest`](https://docs.rs/proptest): ver
  `crates/adapters/delonix-sdn/tests/ip_invariants.rs` (`proptest! { ... }`).
- **Os doc tests** também correm — incluindo blocos `compile_fail` como os do `typestate.rs`.
- **Os benchmarks** usam [`criterion`](https://docs.rs/criterion) (`crates/adapters/delonix-oci/benches/`).
- **Nomes.** Os nomes dos testes descrevem o comportamento que está a ser provado. Muitos testes
  existentes têm nomes em português; os identificadores novos são em inglês (LANG-01, imposto como
  ratchet pelo `scripts/lang_ratchet.py`).
- **Os testes não podem tocar no estado real do host.** Obtém um directório temporário e passa-o
  (os stores recebem um caminho de raiz) em vez de chamares código que resolve a raiz de estado real.
  Tudo o que precise de namespaces reais, cgroups ou um holder de rede pertence à validação ao vivo/E2E
  descrita em [2. Compilar e testar](02-build-and-test.md).

**Ler mais:** The Rust Book — [Writing tests](https://doc.rust-lang.org/book/ch11-01-writing-tests.html),
[Test organization](https://doc.rust-lang.org/book/ch11-03-test-organization.html);
rustdoc — [Documentation tests](https://doc.rust-lang.org/rustdoc/write-documentation/documentation-tests.html).

---

## 3.10 Ferramentas que a CI impõe

| Ferramenta | Comando da CI (de `.github/workflows/ci.yml`) | O que apanha |
|---|---|---|
| rustfmt | `cargo fmt --all --check` | deriva de formatação |
| clippy | `cargo clippy --workspace --all-targets --locked -- -D warnings` | qualquer aviso, incluindo `undocumented_unsafe_blocks` |
| testes | `cargo test --workspace --locked --no-fail-fast` | regressões |
| cargo-deny | `EmbarkStudios/cargo-deny-action` com `deny.toml` | avisos RUSTSEC, licenças, fontes de crates |

O repo tem também gates em Python (`scripts/arch_fitness.py`, `scripts/lang_ratchet.py` e outros).
A lista completa e a forma de correr cada um localmente estão em [2. Compilar e testar](02-build-and-test.md).

**Ler mais:** [Clippy](https://doc.rust-lang.org/clippy/),
[rustfmt](https://rust-lang.github.io/rustfmt/),
[cargo-deny](https://embarkstudios.github.io/cargo-deny/).
