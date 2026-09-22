# Clone, build and test

**Before you read:** [Preparing your environment](environment.md): the pinned toolchain, `protoc`, and a host that passes its checks.

This page assumes the host from [Preparing your environment](environment.md): the pinned
Rust toolchain and `protoc`. After it you can build and install your tree, run every CI gate
locally, and run the E2E battery and the chaos harness without touching real engine state.

Everything here runs from the root of your checkout — ideally a **git worktree**: a separate
working directory with its own branch, one per task, created from `origin/main` (the command is in
[Start here, step 2](start-here.md#2-open-a-worktree-from-originmain); the rules are in
[Contribution workflow](contributing-workflow.md#one-worktree-per-task)).

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

## Installing your build locally

Most changes never need an installed build: run `./target/debug/delonix` from your worktree. Install
only when you need a stable path — a systemd unit, a script on another shell, a kubelet talking to
`delonix-cri`. Before and after installing, confirm **which build** you are running:

```bash
./target/debug/delonix --version    # commit: <hash> (+N commits since vX.Y.Z) · built: <date>
command -v delonix                  # which `delonix` your shell would run instead
```

The `commit:` line comes from `bins/delonix-runtime-bin/build.rs` (`DELONIX_GIT_HASH`,
`DELONIX_GIT_SINCE`). Between releases every build carries the same version number, so the commit
is the only way to tell your build from the released one.

### How `delonix` finds its server binaries

`delonix serve cri`, `delonix serve api` and `delonix mcp` do not contain the servers: they `exec`
`delonix-cri`, `delonix-mgmt` and `delonix-mcp` (`exec_server` in
`bins/delonix-runtime-bin/src/cmd/serve.rs`). The lookup is:

1. the file of that name **next to the running `delonix`**;
2. otherwise the name on `PATH`.

`delonix` passes the server its own version in `DELONIX_DISPATCH_VERSION`, and a server from a
different release refuses to start. It also passes itself in `DELONIX_BIN`, so the server calls back
the same CLI. A server started directly (for example by a unit) finds the CLI through
`DELONIX_BIN`, then a `delonix` next to itself, then `PATH` (`cli_bin` in
`crates/contexts/delonix-node/src/dispatch.rs`). **Keep the four binaries of one build
together**; a mix of your build and a release is refused, or runs code you did not mean to test.

`delonix cluster kubeadm` and `delonix image vm build` look for `delonix-cri` in their own order
(`resolve_cri_bin` in `bins/delonix-runtime-bin/src/cmd/vmimage.rs`): `--cri-bin`, then next to
`delonix`, then a `cargo build --release -p delonix-cri` if the current directory is inside a
source checkout, and only then a download of the released asset.

Build the four before installing them:

```bash
cargo build --release -p delonix-runtime-bin -p delonix-cri -p delonix-mgmt-bin -p delonix-mcp-bin
```

### Option A — run it from the worktree (safest)

Nothing is copied, so nothing outside your checkout can pick it up by accident:

```bash
alias delonix-dev="$PWD/target/release/delonix"
delonix-dev --version
```

The servers are found because they sit next to it in `target/release/`. On Ubuntu 23.10+ this path
needs its own AppArmor profile (see
[AppArmor](environment.md#ubuntu-2310-apparmor-blocks-user-namespaces-for-your-dev-binary)).

### Option B — install for your user in `~/.local/bin`

```bash
install -d ~/.local/bin
install -m 0755 target/release/delonix target/release/delonix-cri \
  target/release/delonix-mgmt target/release/delonix-mcp ~/.local/bin/
hash -r                              # forget the path your shell cached
command -v delonix && delonix --version
```

If a release is also installed in `/usr/local/bin`, whichever directory comes first on `PATH` wins.

**AppArmor.** `scripts/install.sh` writes one profile, `/etc/apparmor.d/delonix`, bound to
`<install dir>/delonix`, and only on hosts with
`kernel.apparmor_restrict_unprivileged_userns=1`. A binary you copied to a new path is not covered.
Do not re-run the installer to "move" that profile on a machine that also uses a released
installation: the profile file is rewritten, and the released binary loses it. Add a second profile
with a different name instead — the same shape the installer writes, so it replaces nothing:

```bash
printf 'abi <abi/4.0>,\ninclude <tunables/global>\nprofile delonix-dev %s flags=(unconfined) {\n  userns,\n}\n' \
  "$HOME/.local/bin/delonix" | sudo tee /etc/apparmor.d/delonix-dev >/dev/null
sudo apparmor_parser -r /etc/apparmor.d/delonix-dev
```

*Unverified here:* this command mirrors `install.sh` (the AppArmor block) with a different profile
name and file; it was not loaded on a host with the restriction active while writing this page.

The installer also adds shell completion, man pages and editor syntax files, but only in its binary
phase. For your own build, generate them from the binary if you want them:

```bash
mkdir -p ~/.local/share/bash-completion/completions
delonix completion shell bash > ~/.local/share/bash-completion/completions/delonix
delonix man --dir ~/.local/share/man
```

### Option C — system install in `/usr/local/bin`

```bash
sudo install -m 0755 target/release/delonix target/release/delonix-cri \
  target/release/delonix-mgmt target/release/delonix-mcp /usr/local/bin/
```

**Only on a machine where no Delonix workload is in use.** The installed binary is not just a
command:

- boot units written by `delonix system boot enable` start `ExecStart=<exe> container start <name>`,
  where `<exe>` is the path of the binary that ran `enable` (`bins/delonix-runtime-bin/src/cmd/boot.rs`,
  unit prefix `delonix-boot-`); replacing that file changes what comes up after the next reboot;
- `dist/delonix-cri.service` runs `/usr/local/bin/delonix-cri`, so on a Kubernetes node the kubelet
  gets your build at the next restart of that unit;
- long-lived processes started earlier (the network pin and control process, container supervisors)
  keep running the code they started with, so for a while two builds run side by side.

Check first:

```bash
delonix container ls -a; delonix vm ls
ls ~/.config/systemd/user/delonix-boot-* /etc/systemd/system/delonix-* 2>/dev/null
pgrep -a delonix
```

### Preparing the host

`scripts/install.sh` does two separate jobs. Only the first one is about the binary:

| Part | What it does | Flag that skips or enables it |
|---|---|---|
| Binary | downloads a release, verifies the minisign signature and sha256, installs `delonix` (plus `delonix-mcp`, `delonix-mgmt`, and `delonix-cri` with `--with-cri`), then completion, man pages, editor syntax and the editor extension | skipped with `--no-binary`; `--user` picks `~/.local/bin` |
| Host packages | `slirp4netns`, `uidmap`, `nftables`, `iproute2`, `conntrack` | always |
| Rootless identity | `/etc/subuid` and `/etc/subgid` ranges for your user | always |
| AppArmor | profile for `<dir>/delonix` when the userns restriction is active | always (when the restriction is on) |
| Old Debian | `kernel.unprivileged_userns_clone=1` when it is `0` | always (when needed) |
| VM dependencies | libvirt, qemu, cloud-init tooling; Cloud Hypervisor and its firmware downloaded from upstream | skipped with `--no-vm` |
| Kernel tuning | `/etc/modules-load.d/delonix.conf`, `/etc/sysctl.d/99-delonix.conf` | skipped with `--no-tune` |
| cgroup delegation | `user@.service` drop-in, only if not already delegated | skipped with `--no-delegate` |
| Accelerators | NVIDIA CDI and `render` group, only when a GPU is present | skipped with `--no-gpu` |
| Opt-ins | ports below 1024 (`--low-ports`), VM image building (`--with-image-build`), scale tuning (`--production`) | off by default |

To prepare a host for your own build **without downloading any Delonix release**, run the installer
from your checkout with `--no-binary`:

```bash
bash scripts/install.sh --no-binary            # add --no-vm if you do not need VM dependencies
bash scripts/install.sh --help                 # the full flag list, from the script header
```

With `--no-binary` the AppArmor profile is written for the directory of the `delonix` that
`command -v delonix` finds (or `/usr/local/bin` if none) — the same caution as above applies on a
machine with a released installation. The script uses `sudo` for the host steps.

Then ask the binary whether the host is ready (read-only):

```bash
delonix system doctor     # every prerequisite, and how to fix each; --strict exits non-zero on a failure
delonix system info       # state root, rootless, cgroup delegation, network infra
```

See [Diagnosing the host](environment.md#diagnosing-the-host) for what each check means.

### Use an isolated state root

An installed build uses your **real** state root by default: the same containers, networks and
volumes as the release. Export `DELONIX_ROOT` and `DELONIX_NET_RUNTIME_DIR` first (see
[Isolating the engine's state](#isolating-the-engines-state)), and see
[Environment variables](environment-variables.md) for every other variable your build reads.

### Uninstall and roll back

There is no uninstall flag in `install.sh`. Remove what you copied:

```bash
rm -f ~/.local/bin/delonix ~/.local/bin/delonix-cri ~/.local/bin/delonix-mgmt ~/.local/bin/delonix-mcp
hash -r
sudo apparmor_parser -R /etc/apparmor.d/delonix-dev && sudo rm /etc/apparmor.d/delonix-dev   # if you added it
```

To go back to a released binary, run the installer again; it replaces the binaries in its install
directory with the release you name:

```bash
curl -fsSL https://github.com/angolardevops/delonix-runtime/releases/latest/download/install.sh | bash -s -- --user
curl -fsSL https://github.com/angolardevops/delonix-runtime/releases/latest/download/install.sh | bash -s -- --version vX.Y.Z
```

Completion files and man pages you generated by hand are not removed by either step. Finish with
`delonix --version` to confirm the commit you are back on.

## Run the tests

```bash
cargo test --workspace                       # the whole suite
cargo test -p delonix-sdn                    # one crate
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
| `test-arm64` | test (arm64) |
| `deny` | cargo-deny |
| `fuzz` | fuzz (60s smoke, per target) |
| `release-verify` | release verify |
| `docs` | generated docs and valid examples |
<!-- dev-docs:end ci-gates -->

Every job in `.github/workflows/ci.yml` can be reproduced locally. Run the ones that match what you
touched before you push; run all of them before asking for review.

You can run a gate before you understand the rule behind it; its failure message names what to fix.
The rules are taught later in the course: layers, the dependency direction and the debt ratchets in
[Architecture](architecture.md#layers-and-the-allowed-direction), the node contract in
[Architecture](architecture.md#one-set-of-operations-several-interfaces), and LANG-01 and version
alignment in [Contribution workflow](contributing-workflow.md#language-english-in-the-code-lang-01).

| Job | Local command | Fails when |
|---|---|---|
| `fmt` | `cargo fmt --all --check` | the code is not rustfmt-formatted (default config) |
| `lang` | `python3 scripts/lang_ratchet.py` | Portuguese identifiers, comments or messages **increase** — or decrease without lowering `scripts/lang_baseline.json` in the same commit (`--list` shows them, `--update` lowers the baseline) |
| `arch` | `python3 scripts/arch_fitness.py` | a dependency goes against the layer direction, a crate sits in the wrong directory, a member crate pins a dependency version, a consumer name appears in code, or a debt ratchet moves (`--list`, `--update`) |
| `arch` | `python3 scripts/dev_docs.py --check` | a generated fact in `docs/dev/` is stale — run `python3 scripts/dev_docs.py` and commit |
| `contract` | `python3 scripts/contract_gate.py` | the node contract in `proto/delonix/node/v1` is not `buf format`-clean, fails `buf lint`, breaks compatibility with the last tag, lacks an HTTP mapping, or `docs/api/openapi.yaml` is not the generated one (`--update` rewrites it). Needs `protoc`, `buf` v1.73.0 and `protoc-gen-openapi` v0.7.1 on `PATH`, and the tags |
| `version` | `python3 scripts/version_gate.py` | the workspace version is not the newest tag the commit contains (see [Contribution workflow](contributing-workflow.md#version-alignment)) or the branch does not contain the newest tag. Needs the tags |
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
(`runtime_dir`/`root_suffix` in `crates/adapters/delonix-sdn/src/infra.rs`), which closes that
collision for the common case. Keep exporting both anyway: it makes the isolation explicit, keeps
the socket path short and under your control, and it is what `scripts/e2e.sh` and
`scripts/chaos.sh` do (e2e fills in whichever variable you did not export).

Keep `DELONIX_NET_RUNTIME_DIR` short: a unix socket path longer than about 108 bytes fails with
`path must be shorter than SUN_LEN`. `e2e.sh` refuses a runtime dir longer than 80 bytes.

When you are done, tear down the isolated network infra with the same two variables exported:

```bash
./target/debug/delonix net netns down
```

## VM image recipes (`scripts/verify-images.sh`)

The recipes in `images/` are checked in two ways. A unit test in the CLI crate
(`vmspec::every_shipped_recipe_is_valid_and_complete`) fails if a recipe stops parsing or points at
a file or builder that does not exist. `scripts/verify-images.sh` goes further: it builds the four
cloud-image distros offline in an isolated `DELONIX_ROOT` and reads the resulting qcow2 back against
what the recipe declared; `--self-test` proves the checks can fail on an image nobody built. It
needs `libguestfs-tools` (see [Building microVMs](microvm-setup.md)) and is not part of the CI
gates. The `--packages`, `--profile`, `--boot` and `--appliance` phases exist but had not been run
when v4.2.0 was released.

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
not — see [Contribution workflow](contributing-workflow.md#one-worktree-per-task).

---

**Next:** [Project structure](project-structure.md) — the map of the repository: what each directory is, who changes it, and what is generated.
