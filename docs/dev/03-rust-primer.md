# 3. Rust primer for this codebase

This is not a Rust tutorial. It is the subset of Rust you need to *read this repository*,
with each idea pinned to a file you can open. If a concept is new to you, the "Read more"
links go to the official source; come back here to see how the engine uses it.

Paths are relative to the repository root. Symbols are named so you can `grep` for them —
line numbers are deliberately left out, because they move with every PR.

The pinned toolchain is in `rust-toolchain.toml` (a fixed `channel`, plus `rustfmt` and
`clippy`). `rustup` honours it automatically, so you build with the same compiler CI uses.

---

## 3.1 The Cargo workspace

The repository is one Cargo **workspace**: a root `Cargo.toml` with `[workspace] members = [...]`
and one `Cargo.toml` per crate. Three conventions matter here:

1. **Every path and every version is written once, in the root.** The root has a
   `[workspace.dependencies]` table listing both the engine's own crates (by `path`) and every
   third-party crate (by `version`). A member never writes a version; it writes:

   ```toml
   # crates/foundation/delonix-runtime-core/Cargo.toml
   [dependencies]
   serde = { workspace = true }
   thiserror = { workspace = true }
   ```

   A member may add `features = [...]`, and that is all. `default-features = false` lives in
   the root because a member cannot turn off defaults the workspace turned on.
   `scripts/arch_fitness.py` (function `inline_versions`) fails the build if a member writes
   its own version.

2. **The directory is the layer.** Crates live in `crates/foundation/`, `crates/contexts/`,
   `crates/adapters/`, `crates/providers/`, `crates/interfaces/` and binaries in `bins/`
   (ADR-0040). `scripts/arch_fitness.py` refuses a crate whose directory does not match its
   declared layer, and a dependency that points against the allowed direction. See
   [5. Architecture](05-architecture.md) for the layer rules.

3. **Lints are inherited.** The root declares `[workspace.lints.clippy]` with
   `undocumented_unsafe_blocks = "deny"`, and each member opts in with `[lints] workspace = true`.
   Every `unsafe` block therefore carries a `// SAFETY:` comment (see §3.4).

The root also sets `[workspace.package]` (the shared `version`, `edition`, `license`), which
members consume as `version.workspace = true`. The version is not decoration: see
[2. Build and test](02-build-and-test.md) for the version gate.

**Read more:** Cargo Book —
[Workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html),
[`[workspace.dependencies]`](https://doc.rust-lang.org/cargo/reference/workspaces.html#the-dependencies-table),
[`[lints]`](https://doc.rust-lang.org/cargo/reference/manifest.html#the-lints-section),
[`rust-toolchain.toml`](https://rust-lang.github.io/rustup/overrides.html#the-toolchain-file).

---

## 3.2 Errors: one `Error`, and exit codes derived from its type

Almost every fallible function in the engine returns `delonix_runtime_core::Result<T>`, an alias
over the shared enum defined in `crates/foundation/delonix-model/src/error.rs`. The enum lives in the
pure foundation crate `delonix-model`; `delonix-runtime-core` re-exports `Error` and `Result`
(`pub use delonix_model::{Error, Result};`), so the `delonix_runtime_core::` path most call sites
use still works. It is built with
[`thiserror`](https://docs.rs/thiserror): `#[derive(Error)]` generates `Display` from the
`#[error("...")]` attribute, and `#[from]` generates `From` impls so `?` converts a lower-level
error automatically:

```rust
// crates/foundation/delonix-model/src/error.rs
#[error("I/O error: {0}")]
Io(#[from] std::io::Error),
```

The variants are about **what the caller should do next**, not about which subsystem failed:
`NotFound`, `NotRunning`, `Conflict`, `Unavailable` (a missing tool or kernel feature),
`Timeout`, `Invalid`, `Registry`, `Runtime { context, message }` for a failed syscall, and so on.
The doc comments on each variant explain why it exists; read them before adding a variant.

Those variants become **process exit codes** in one place:
`crates/foundation/delonix-model/src/exitcode.rs`, function `for_error`. The CLI re-exports the
module (`pub use delonix_model::exitcode;` in `bins/delonix-runtime-bin/src/cmd/mod.rs`) and
`bins/delonix-runtime-bin/src/main.rs` calls `std::process::exit(cmd::exitcode::for_error(&e))`.

```rust
// crates/foundation/delonix-model/src/exitcode.rs
pub fn for_error(e: &Error) -> i32 {
    match e {
        Error::NotFound(_) | Error::VmNotFound(_) => NOT_FOUND,
        Error::NotRunning(_) => NOT_RUNNING,
        // ...
```

Two things to notice:

- The `match` is **exhaustive, with no `_ =>` arm**. Adding a variant to `Error` stops the build
  in `for_error` (and in `Error::code` in `error.rs`) until someone decides its class. That is a
  deliberate use of the compiler as a checklist.
- Messages are translated for the operator, so scripts must branch on the exit code (or the
  stable machine code from `Error::code`), never on message text.

**Read more:** The Rust Book —
[Recoverable errors with `Result`](https://doc.rust-lang.org/book/ch09-02-recoverable-errors-with-result.html),
[the `?` operator](https://doc.rust-lang.org/book/ch09-02-recoverable-errors-with-result.html#a-shortcut-for-propagating-errors-the--operator);
Rust by Example — [Defining an error type](https://doc.rust-lang.org/rust-by-example/error/multiple_error_types/define_error_type.html);
[`thiserror` on docs.rs](https://docs.rs/thiserror).

---

## 3.3 Traits as ports: `VmBackend` and the backend registry

The engine talks to providers through **traits** ("ports"), and a provider is an implementation
of one. The clearest example is `VmBackend` in `crates/adapters/delonix-vm/src/lib.rs`:

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

Implementations: `CloudHypervisorBackend` and `LibvirtBackend` in the same file, and
`ProxmoxBackend` in `crates/providers/delonix-proxmox/src/lib.rs`. Methods with a **default
body** in the trait (for example `auto_selectable`) let a new backend inherit sensible behaviour
and override only what differs.

Backends are chosen at runtime, so they are handled as **trait objects**, `Box<dyn VmBackend>`.
They are created through a registry of factories:

```rust
// crates/adapters/delonix-vm/src/lib.rs
pub type BackendFactory = Box<dyn Fn() -> Result<Box<dyn VmBackend>> + Send + Sync>;

static BACKENDS: std::sync::OnceLock<std::sync::RwLock<Vec<BackendRegistration>>> =
    std::sync::OnceLock::new();
```

- The factory is a **closure** (`dyn Fn`), because a remote backend needs captured
  configuration (endpoint, node, credential) that a plain `fn()` pointer cannot carry.
- `Send + Sync` is required because the table is a process-wide `static`; the bound constrains
  the closure, not the `VmBackend` trait.
- `OnceLock` initialises the table lazily (`builtin_backends()` seeds the two local backends),
  and `RwLock` lets `register_backend` add a third one at startup. The binary does that in
  `bins/delonix-runtime-bin/src/cmd/vmbackends.rs` (`register_configured`).

The decision behind this shape is [ADR-0008](../adr/0008-proxmox-vm-backend.md). The same
"trait + implementations + one place that picks" pattern appears elsewhere (for example the
`VmNetwork` port, held in a `OnceLock<Box<dyn VmNetwork>>` near the top of the same file).

**Read more:** The Rust Book —
[Traits](https://doc.rust-lang.org/book/ch10-02-traits.html),
[Trait objects](https://doc.rust-lang.org/book/ch18-02-trait-objects.html),
[Closures](https://doc.rust-lang.org/book/ch13-01-closures.html),
[`Send` and `Sync`](https://doc.rust-lang.org/book/ch16-04-extensible-concurrency-sync-and-send.html);
std docs — [`OnceLock`](https://doc.rust-lang.org/std/sync/struct.OnceLock.html).

---

## 3.4 `unsafe`, FFI and Linux syscalls

A container engine is mostly system calls. This repo reaches the kernel through three crates:

| Crate | Used for | Example in this repo |
|---|---|---|
| [`nix`](https://docs.rs/nix) | Safe-ish wrappers: `clone`, `setns`, `unshare`, `pivot_root`, `fork`, `mount`, signals | `use nix::sched::{clone, setns, unshare, CloneFlags};` in `crates/adapters/delonix-linux/src/lib.rs` |
| [`libc`](https://docs.rs/libc) | Raw calls `nix` does not wrap, or where the exact struct matters | `libc::getsockopt(.., SO_PEERCRED, ..)` in `crates/foundation/delonix-runtime-core/src/peer_cred.rs` (`peer_uid`); `libc::flock` in `crates/adapters/delonix-state/src/store.rs` |
| [`rustix`](https://docs.rs/rustix) | The new mount API (`fsopen`/`fsconfig`/`fsmount`/`move_mount`) | `fsopen_overlay` in `crates/adapters/delonix-linux/src/lib.rs` |

**Every `unsafe` block states why it is sound**, next to it (the workspace lint enforces the
comment's presence; reviewers enforce its truth):

```rust
// crates/foundation/delonix-runtime-core/src/peer_cred.rs
// SAFETY: getsockopt on SO_PEERCRED with a correctly-sized ucred buffer.
let r = unsafe { libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, ...) };
```

### Where the container is born

`fn spawn` in `crates/adapters/delonix-linux/src/lib.rs` ends in:

```rust
// SAFETY: single-threaded; the child mounts the container and does `exec`.
let cloned = unsafe { clone(cb, &mut stack, flags, Some(Signal::SIGCHLD as i32)) };
```

The child runs `container_init`, which calls `setup_rootfs` (bind mounts, then `pivot_root`),
`drop_capabilities`, installs seccomp, and finally `execvp`. `setns` is how `exec` enters an
existing container's namespaces. The uid/gid maps of a user namespace are written from the
parent (`write_userns_maps`, which uses the `newuidmap`/`newgidmap` helpers when available).

### Why `fork`/`clone` in a multi-threaded process is dangerous

After `fork` or `clone`, only the calling thread exists in the child, but the child inherits
every lock *as it was* — including locks held by threads that no longer exist. If another thread
held the allocator lock at that instant, the child's first allocation blocks forever. `fork`
mitigates some of this with `pthread_atfork` handlers; raw `clone` does not run them.

This repo has two concrete rules because of it, both explained in comments you should read:

- **`serve docker-api` never calls `spawn` in-process.** The doc comment above the re-exec helper
  in `bins/delonix-runtime-bin/src/cmd/dockerapi.rs` explains that the server is a multi-threaded
  tokio runtime, so it re-executes the binary (`__apirun <spec.json>`) to get a fresh
  single-threaded process where the `clone` precondition is true again.
- **After a raw `fork`, only async-signal-safe work.** `reexec_mapped` and `reexec_mapped_hold`
  in `crates/adapters/delonix-linux/src/lib.rs` pre-compute every allocation *before* the fork.
  The comment on `reexec_mapped_hold` also records why they use raw `fork` instead of
  `std::process::Command` + `pre_exec`: `Command::spawn` waits for the child to reach `exec`, and
  a `pre_exec` hook that blocks waiting on the parent deadlocks.

### The new mount API, and why it exists here

`mount_overlay_if_marked` mounts a container's overlay root through `rustix::mount::fsopen` and
one `fsconfig_set_string(&fs, "lowerdir+", lower)` call per layer. The classic `mount(2)` packs
all options into a single page-sized `data` string, which the kernel truncates silently for
images with many layers. The measurement and decision are in
[ADR-0037](../adr/0037-overlay-mount-new-api.md). This is a good example of the repo's habit:
the `///` comment above a non-obvious syscall explains the failure that motivated it.

**Read more:** The Rust Book — [Unsafe Rust](https://doc.rust-lang.org/book/ch20-01-unsafe-rust.html);
the [Rustonomicon](https://doc.rust-lang.org/nomicon/) (especially
[FFI](https://doc.rust-lang.org/nomicon/ffi.html)); man pages
[`clone(2)`](https://man7.org/linux/man-pages/man2/clone.2.html),
[`setns(2)`](https://man7.org/linux/man-pages/man2/setns.2.html),
[`pivot_root(2)`](https://man7.org/linux/man-pages/man2/pivot_root.2.html),
[`fork(2)`](https://man7.org/linux/man-pages/man2/fork.2.html),
[`signal-safety(7)`](https://man7.org/linux/man-pages/man7/signal-safety.7.html),
[`fsopen(2)`](https://man7.org/linux/man-pages/man2/fsopen.2.html).

---

## 3.5 Serialization: serde, YAML manifests, generated schema

State on disk is JSON; manifests are YAML. Both go through
[`serde`](https://serde.rs/) derives.

**Backward-compatible records.** A record written by an older binary must still load. The rule
is: a new field gets `#[serde(default)]`, and when "absent" must mean something different from
the type's default, it becomes an `Option`:

```rust
// bins/delonix-runtime-bin/src/cmd/vmimage.rs  (VmImage)
#[serde(default)]
pub cloud_init: Option<bool>,
```

Here `None` (every record written before the field existed) is read as "yes"; a plain `bool`
would have defaulted to `false` and silently changed behaviour for old images. You will see the
same reasoning on `Mount::propagation` and `Mount::optional` in
`crates/foundation/delonix-runtime-core/src/lib.rs` (`#[serde(default, skip_serializing_if = "Option::is_none")]`).

**Manifests.** `bins/delonix-runtime-bin/src/cmd/manifest.rs` parses multi-document YAML with
`serde_yaml::Deserializer::from_str(text)` into `ManifestDoc`, whose `spec` stays a raw
`serde_yaml::Value` until the owning Kind deserializes it into its typed spec.

**Generated schema.** Spec types derive `schemars::JsonSchema` (for example `PodSpec` in
`crates/contexts/delonix-compute/src/pod.rs`), and `bins/delonix-runtime-bin/src/cmd/schema.rs`
generates the published JSON Schema from them ([ADR-0007](../adr/0007-generated-manifest-schema.md)).
Note the comment there: schemars honours `#[serde(rename)]` but not `#[serde(alias)]`.

**Read more:** [serde.rs](https://serde.rs/) —
[field attributes](https://serde.rs/field-attrs.html) (`default`, `skip_serializing_if`, `alias`);
[`serde_yaml`](https://docs.rs/serde_yaml); [`schemars`](https://docs.rs/schemars).

---

## 3.6 The CLI: clap derive and translated output

The `delonix` binary is `bins/delonix-runtime-bin`. Its top level is a
[`clap`](https://docs.rs/clap) derive in `src/main.rs`: `#[derive(Parser)] struct Cli` holding a
global `--l18n` flag and `#[command(subcommand)] cmd: Cmd`, where `enum Cmd` is a
`#[derive(Subcommand)]`. Each group has its own module and enum in `src/cmd/` — for example
`pub enum ContainerCmd` in `src/cmd/container.rs`.

Doc comments (`///`) on variants and fields become the `--help` text. You will see
`#[allow(clippy::large_enum_variant)]` on command enums, with a comment explaining why: they are
parsed once per invocation, so boxing variants would buy nothing.

**Source strings are English; Portuguese comes from a catalogue.** `src/cmd/po.rs` embeds
`data/pt.po` with `include_str!` and exposes:

- `po::t("already exists, nothing to do")` — a fixed string;
- `po::tf("port {port} is taken by '{owner}'", &[("port", &hp), ("owner", &ow)])` — a template
  with **named** placeholders, because a translation may reorder them;
- `po::translate_help`, which rewrites clap's help text after `po::peek_lang` has decided the
  language *before* parsing.

A new user-facing string is English in the code plus an entry in `data/pt.po`. The language
policy (LANG-01) and its gate are covered in [10. Contributing workflow](10-contributing-workflow.md).

**Read more:** [clap derive tutorial](https://docs.rs/clap/latest/clap/_derive/_tutorial/index.html);
std — [`include_str!`](https://doc.rust-lang.org/std/macro.include_str.html).

---

## 3.7 Async and gRPC — and why most of the engine is synchronous

The core engine is **synchronous**: the CLI starts, does its work with blocking syscalls and
file I/O, and exits. There is no daemon and no ambient async runtime. Async code appears only in
the *interfaces* that serve a protocol:

- **CRI server** — `crates/interfaces/delonix-cri`. The gRPC stubs are generated at build time:
  `build.rs` calls `tonic_build::configure()...compile_protos(&["proto/api.proto"], &["proto"])`.
  That needs **`protoc`** installed (CI installs `protobuf-compiler`); a missing `protoc` is the
  most common first build failure. `serve_blocking` in `src/lib.rs` creates its own
  `tokio::runtime::Builder::new_multi_thread()` runtime, and handlers push blocking engine work
  off the async workers through the `blocking` helper in `src/runtime_svc.rs` — otherwise a
  `clone` or a shell-out to `delonix` would stall the tokio workers.
- **L7 reverse proxy** — `bins/delonix-runtime-bin/src/cmd/ingress_proxy.rs` uses
  [`hyper`](https://docs.rs/hyper) directly (`hyper::service::service_fn`) on its own tokio
  runtime.
- **MCP server** — `crates/interfaces/delonix-mcp` uses [`rmcp`](https://docs.rs/rmcp) over
  stdio (`rmcp::transport::io::stdio`).
- **Telemetry** — `crates/adapters/delonix-telemetry/src/telemetry.rs` exports OpenTelemetry spans
  from a dedicated thread with a *blocking* HTTP client, precisely so the synchronous CLI does not
  need a runtime.

The node contract under `proto/delonix/node/v1` is a separate protobuf API with its own gate
(`scripts/contract_gate.py`, `buf`); see [2. Build and test](02-build-and-test.md).

**Read more:** [Asynchronous Programming in Rust](https://rust-lang.github.io/async-book/);
[Tokio tutorial](https://tokio.rs/tokio/tutorial);
[`tonic`](https://docs.rs/tonic) and [`tonic-build`](https://docs.rs/tonic-build);
[Protocol Buffers install](https://protobuf.dev/installation/).

---

## 3.8 Concurrency and shared state

There is no database. State is JSON files under the state root, and **several processes** (two
CLI invocations, the CRI server, a supervisor) may touch the same record at once. The pattern for
every read-modify-write is `update` with a closure, under an exclusive `flock`:

```rust
// crates/adapters/delonix-state/src/store.rs  (Store::update)
let id = self.load(id_or_name)?.id;
let _lock = FileLock::acquire(&self.lock_path(&id))?;
// Re-read UNDER the lock ...
let mut c = self.load(&id)?;
if !f(&mut c) { return Ok(c); }
self.save(&c)?;
```

`JsonStore<T>::update` in the same file is the generic version for other record types. Both
**refuse** to run when the lock cannot be taken (`Error::Lock`) instead of carrying on unlocked.
(`SecretStore::update` in `secret.rs` is the exception: its lock is best-effort.) Rules that
follow from it:

- Never do `load` → mutate → `save` by hand for a record another process can write; you will
  lose updates. Use `update`.
- Re-read inside the lock. The value you loaded earlier may already be stale.
- `FileLock` releases the lock in `Drop` — the RAII pattern.

Inside a single process, shared state uses the standard types: an
`Arc<std::sync::RwLock<Arc<Vec<Route>>>>` holds the proxy's hot-swappable route table
(`SharedRoutes` in `cmd/ingress_proxy.rs`), and the dashboard shares its slow sample through an
`Arc<Mutex<...>>` (`cmd/dash.rs`).

A related idea you will meet in `crates/foundation/delonix-runtime-core/src/typestate.rs`: the
**typestate** pattern, where lifecycle states are types and illegal transitions do not compile
(its doc tests include `compile_fail` examples).

**Read more:** The Rust Book —
[Shared-state concurrency](https://doc.rust-lang.org/book/ch16-03-shared-state.html),
[`Drop`](https://doc.rust-lang.org/book/ch15-03-drop.html);
[`flock(2)`](https://man7.org/linux/man-pages/man2/flock.2.html);
std — [`Arc`](https://doc.rust-lang.org/std/sync/struct.Arc.html),
[`RwLock`](https://doc.rust-lang.org/std/sync/struct.RwLock.html).

---

## 3.9 Tests

- **Unit tests live next to the code**, in a `#[cfg(test)] mod tests { ... }` at the bottom of
  the file. Many pure functions exist precisely so a decision can be tested as data (for example
  `reconcile::plan` in `crates/contexts/delonix-stack/src/reconcile.rs`).
- **Integration tests** live in `crates/<layer>/<crate>/tests/*.rs` and use only the crate's
  public API. Example: `crates/interfaces/delonix-cri/tests/grpc_status.rs` starts the CRI server
  on a Unix socket and talks to it with the *generated gRPC client* (which is why `build.rs` sets
  `build_client(true)`). Tests under `crates/providers/*/tests/live.rs` need a real provider and
  are opt-in.
- **Property tests** use [`proptest`](https://docs.rs/proptest): see
  `crates/adapters/delonix-sdn/tests/ip_invariants.rs` (`proptest! { ... }`).
- **Doc tests** run too — including `compile_fail` blocks such as those in `typestate.rs`.
- **Benchmarks** use [`criterion`](https://docs.rs/criterion) (`crates/adapters/delonix-oci/benches/`).
- **Names.** Test names describe the behaviour being proven. Many existing tests have Portuguese
  names; new identifiers are English (LANG-01, enforced as a ratchet by `scripts/lang_ratchet.py`).
- **Tests must not touch the host's real state.** Take a temporary directory and pass it in
  (the stores take a root path) rather than calling code that resolves the real state root.
  Anything that needs real namespaces, cgroups or a network holder belongs to the live/E2E
  validation described in [2. Build and test](02-build-and-test.md).

**Read more:** The Rust Book — [Writing tests](https://doc.rust-lang.org/book/ch11-01-writing-tests.html),
[Test organization](https://doc.rust-lang.org/book/ch11-03-test-organization.html);
rustdoc — [Documentation tests](https://doc.rust-lang.org/rustdoc/write-documentation/documentation-tests.html).

---

## 3.10 Tooling the CI enforces

| Tool | CI command (from `.github/workflows/ci.yml`) | What it catches |
|---|---|---|
| rustfmt | `cargo fmt --all --check` | formatting drift |
| clippy | `cargo clippy --workspace --all-targets --locked -- -D warnings` | any warning, including `undocumented_unsafe_blocks` |
| tests | `cargo test --workspace --locked --no-fail-fast` | regressions |
| cargo-deny | `EmbarkStudios/cargo-deny-action` with `deny.toml` | RUSTSEC advisories, licences, crate sources |

The repo also has Python gates (`scripts/arch_fitness.py`, `scripts/lang_ratchet.py` and others).
The full list and how to run each one locally is in [2. Build and test](02-build-and-test.md).

**Read more:** [Clippy](https://doc.rust-lang.org/clippy/),
[rustfmt](https://rust-lang.github.io/rustfmt/),
[cargo-deny](https://embarkstudios.github.io/cargo-deny/).
