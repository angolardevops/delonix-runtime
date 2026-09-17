# 2. Clone, build and test

This page assumes the host from [01 — Preparing your environment](01-environment.md): the pinned
Rust toolchain and `protoc`. Everything here runs from the root of your checkout — ideally a
**git worktree**, see [10 — Contribution workflow](10-contributing-workflow.md).

## Clone

```bash
git clone https://github.com/angolardevops/delonix-runtime.git
cd delonix-runtime
git fetch --tags origin      # several gates compare against the release tags
```

## Build

```bash
cargo build --workspace                    # every crate and every binary
cargo build -p delonix-runtime-bin         # just the `delonix` CLI
cargo build --release -p delonix-runtime-bin   # what the docs generator and the CLI gates use
```

The workspace ships these binaries, from these packages:

| Binary | Package | Output |
|---|---|---|
| `delonix` (the CLI) | `delonix-runtime-bin` | `target/debug/delonix` or `target/release/delonix` |
| `delonix-cri` | `delonix-cri` | `target/<profile>/delonix-cri` |
| `delonix-mcp` | `delonix-mcp-bin` | `target/<profile>/delonix-mcp` |
| `delonix-mgmt` | `delonix-mgmt-bin` | `target/<profile>/delonix-mgmt` |

The release workflow builds exactly these four packages. If you set `CARGO_TARGET_DIR`, binaries
land there instead of `target/`.

Two practical notes:

- **Always test the binary you built** (`./target/debug/delonix`), never a `delonix` on your
  `PATH` — that is an installed release, often several versions behind.
- If you work in several worktrees, pointing them all at one shared `CARGO_TARGET_DIR` saves disk,
  but two builds running at the same time on it will wait on each other and can invalidate each
  other's artefacts. A target directory per worktree is slower the first time and predictable
  afterwards.

## Run the tests

```bash
cargo test --workspace                       # the whole suite
cargo test -p delonix-net                    # one crate
cargo test -p delonix-stack -- reconcile       # tests whose path contains "reconcile"
cargo test -p delonix-stack -- --exact kinds::tests::nenhum_kind_aparece_duas_vezes
```

Tests that need privileges or a real host skip themselves instead of failing, so the suite is
meaningful on a laptop and on CI. A few live tests are marked `#[ignore]` and name the command to
run them in their doc comment (for example in `crates/adapters/delonix-vm/src/lib.rs`); run those
only on a machine you own:

```bash
cargo test -p <crate> -- --ignored <test-name>
```

A green `cargo test` proves the pure logic. It does **not** prove that a change to namespaces,
cgroups, the network holder or VM boot works — that needs a live run (see
[End-to-end battery](#end-to-end-battery-scriptse2esh) and [Chaos harness](#chaos-harness-scriptschaossh)).

## The gates CI runs

<!-- dev-docs:begin ci-gates -->
| CI job | What it checks |
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

Every job in `.github/workflows/ci.yml` can be reproduced locally. Run the ones that match what you
touched before you push; run all of them before asking for review.

| Job | Local command | Fails when |
|---|---|---|
| `fmt` | `cargo fmt --all --check` | the code is not rustfmt-formatted (default config) |
| `lang` | `python3 scripts/lang_ratchet.py` | Portuguese identifiers, comments or messages **increase** — or decrease without lowering `scripts/lang_baseline.json` in the same commit (`--list` shows them, `--update` lowers the baseline) |
| `arch` | `python3 scripts/arch_fitness.py` | a dependency goes against the layer direction, a crate sits in the wrong directory, a member crate pins a dependency version, a consumer name appears in code, or a debt ratchet moves (`--list`, `--update`) |
| `arch` | `python3 scripts/dev_docs.py --check` | a generated fact in `docs/dev/` is stale — run `python3 scripts/dev_docs.py` and commit |
| `contract` | `python3 scripts/contract_gate.py` | the node contract in `proto/delonix/node/v1` is not `buf format`-clean, fails `buf lint`, breaks compatibility with the last tag, lacks an HTTP mapping, or `docs/api/openapi.yaml` is not the generated one (`--update` rewrites it). Needs `protoc`, `buf` v1.73.0 and `protoc-gen-openapi` v0.7.1 on `PATH`, and the tags |
| `version` | `python3 scripts/version_gate.py` | the workspace version is not the newest tag the commit contains (see [10](10-contributing-workflow.md#version-alignment)) or the branch does not contain the newest tag. Needs the tags |
| `cli-surface` | `cargo build --release -p delonix-runtime-bin && scripts/cli-tree.sh --gate` | a CLI leaf was added, removed or reclassified without updating `scripts/cli_baseline.tsv` in the same commit (`scripts/cli-tree.sh --update`) |
| `cli-surface` | `python3 scripts/docs_cli_gate.py` | a `delonix …` command quoted in current documentation does not exist in the binary's tree |
| `clippy` | `cargo clippy --workspace --all-targets --locked -- -D warnings` | any warning |
| `test` | `cargo build --workspace --locked && cargo test --workspace --locked --no-fail-fast` | any test fails |
| `deny` | `cargo deny check advisories licenses sources` | a RUSTSEC advisory, a disallowed licence or source (`deny.toml`) |
| `docs` | `cargo build --release -p delonix-runtime-bin && python3 docs/gen.py && git diff --exit-code -- docs/` | the committed site is not what the generator produces from this binary |
| `docs` | `./target/release/delonix stack apply -f examples/<file>.yaml --dry-run` and `./target/release/delonix stack validate -f examples/<file>.yaml` | a published example uses a deprecated form or has unresolved references |

`cli-tree.sh` and `docs_cli_gate.py` read the tree from the binary's real `--help`; set
`DELONIX_BIN=/path/to/delonix` to choose which binary. `docs/gen.py` defaults to
`target/release/delonix` and needs the Python `markdown` module. The `docs` job also generates the
man pages (`delonix man --dir <dir> --index`) and checks them with `groff -mandoc -ww -z`.

Separate workflows, not required on every change: `chaos.yml` runs the chaos harness on a clean
runner (and reports `skipped` when the runner blocks user namespaces), `release.yml` publishes a
tag, and `vm-image.yml` / `vm-appliances.yml` build VM images.

## Isolating the engine's state

Anything beyond `--help` touches engine state. By default that is **your real state**: your
containers, networks, volumes and the network holder. Before running the engine for testing —
by hand, through `e2e.sh`, or through any script — point **both** state roots at a scratch
directory:

```bash
export DELONIX_ROOT=$HOME/scratch/dlx/root          # containers, images, networks, IPAM, volumes
export DELONIX_NET_RUNTIME_DIR=/tmp/dlx-run          # the holder's control and slirp sockets
mkdir -p "$DELONIX_ROOT" "$DELONIX_NET_RUNTIME_DIR"
```

**Both, always. Half isolation is worse than none.** The network sockets and the pidfiles are
resolved separately: pidfiles live under the *state root*, while the holder's control and slirp
sockets live in a *runtime directory* (by default `/tmp/delonix-net-<uid>`). When two state roots
ended up on the same runtime directory, each read its own (absent) pidfile, concluded there was no
network infra, and started or tore down infra on top of the other one's sockets. On a development
host running live workloads this ended with the real root rebuilding its network infra and
restarting real containers.

The engine now derives a suffix from a non-default `DELONIX_ROOT` for the runtime directory
(`runtime_dir`/`root_suffix` in `crates/adapters/delonix-net/src/infra.rs`), which closes that
collision for the common case. Keep exporting both anyway: it makes the isolation explicit, keeps
the socket path short and under your control, and it is what `scripts/e2e.sh` and
`scripts/chaos.sh` do (e2e fills in whichever variable you did not export).

Keep `DELONIX_NET_RUNTIME_DIR` short: a unix socket path longer than about 108 bytes fails with
`path must be shorter than SUN_LEN`. `e2e.sh` refuses a runtime dir longer than 80 bytes.

When you are done, tear down the isolated network infra with the same two variables exported:

```bash
./target/debug/delonix net netns down
```

## End-to-end battery (`scripts/e2e.sh`)

`e2e.sh` runs the CLI against the real kernel: every leaf's `--help`, plus real executions of a
large part of the surface, and prints a PASS/FAIL/SKIP/XFAIL report (JSONL detail in
`$OUT/results.jsonl`, default `OUT=/tmp/delonix-e2e`).

```bash
./scripts/e2e.sh                          # uses ./target/debug/delonix
./scripts/e2e.sh ./target/release/delonix
```

- It **isolates itself by default**: it sets `DELONIX_ROOT` and `DELONIX_NET_RUNTIME_DIR` to its own
  directories (unless you export both first) and tears down the infra it started.
  `E2E_SHARED_STATE=1` runs against the real machine state — only for diagnosing a host.
- Exit code is non-zero when a check fails, or when a check marked as a known defect (`XFAIL`)
  unexpectedly passes. SKIPs do not fail the run but are listed in their own block: a skipped
  check proved nothing.
- It needs network access to pull images; sections whose preconditions are missing skip with the
  reason.
- A green run means the `--help` of every leaf was verified and *some* leaves were executed.
  Read the header of the script for what is executed and what is not.

## Chaos harness (`scripts/chaos.sh`)

The chaos harness breaks a running engine on purpose — killing the holder, filling the disk,
concurrent attaches, partial applies — and reports whether it degraded the way it promises to.

```bash
scripts/chaos.sh                        # every scenario, ./target/debug/delonix
scripts/chaos.sh holder_kill oom        # selected scenarios
scripts/chaos.sh --keep scale           # leave the sandbox up for a post-mortem
scripts/chaos.sh --clean                # tear the kept sandbox down
```

- It always redirects both roots into its sandbox (`DELONIX_CHAOS_DIR`, default `/tmp/dlx-chaos`)
  and never touches the real engine's containers, networks or records. The image directories
  (`images`, `layers`, `blobs`) are **symlinks to your real store** to avoid downloads: the harness
  only reads them in practice, but a scenario that wrote an image would write to the real store.
- It **refuses to run on a busy machine** (load above a threshold, shared with `scripts/bench.sh`
  through `scripts/bancada.sh`): under load, scenarios fail for reasons that belong to the bench,
  not the product. `--max-load N` changes the threshold; `--force` runs anyway and marks the
  verdict as not publishable.
- Exit code is 0 only when no scenario fails. SKIPs are listed separately.
- Some scenarios need external resources and skip without them (for example `truenas_destroy`
  needs `DELONIX_CHAOS_TRUENAS_URL`/`_USER`/`_PASS`).

Scratch directories under `/tmp` are fine for these throwaway sandboxes. Your **worktrees** are
not — see [10](10-contributing-workflow.md#one-worktree-per-task).
