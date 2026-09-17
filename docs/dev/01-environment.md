# 1. Preparing your environment

Delonix Runtime is **Linux-only**: every primitive it uses — namespaces, cgroups v2, nftables,
`pivot_root`, the new mount API — lives in the Linux kernel. You can *compile* most of the
workspace and run its pure-logic tests on any Linux box with the toolchain below; to *run*
containers and exercise the live paths you need a host that satisfies the kernel and package
requirements in this page.

A lot of what looks like an engine bug on a fresh machine is a host prerequisite. Read the
[Known host traps](#known-host-traps) section before you open an issue.

## Toolchain

<!-- dev-docs:begin toolchain -->
- **Rust toolchain:** `1.96.0` (pinned in `rust-toolchain.toml`; `rustup` installs it on the first `cargo` call)
- **Components:** `rustfmt`, `clippy`
<!-- dev-docs:end toolchain -->

Install [`rustup`](https://rustup.rs/) and let it pick up the pinned channel; do not override it
with `stable`. CI uses exactly the same file (`rustup show` in every job).

### `protoc` (required to build)

`crates/interfaces/delonix-cri/build.rs` compiles the Kubernetes CRI protobuf with
`tonic-build`/`prost`, which needs the Protocol Buffers compiler on `PATH`. The `delonix` binary
depends on `delonix-cri`, so **a plain `cargo build --workspace` fails without it**:

```bash
# Debian / Ubuntu
sudo apt install protobuf-compiler
# Fedora / RHEL family
sudo dnf install protobuf-compiler
```

Or download a release from <https://github.com/protocolbuffers/protobuf/releases>. CI installs
`protobuf-compiler` from apt in every job that compiles.

### Optional tools, only for specific gates

| Tool | Needed for | Where it is pinned |
|---|---|---|
| Python 3.11+ | every `scripts/*.py` gate (they use `tomllib`) | — |
| `buf` v1.73.0 and `protoc-gen-openapi` v0.7.1 (Go toolchain to install them) | `scripts/contract_gate.py` | `contract` job in `.github/workflows/ci.yml` |
| `cargo-deny` | the supply-chain check | `deny` job, config in `deny.toml` |
| Python `markdown` module | `docs/gen.py` (renders `ARCHITECTURE.md`) | `docs` job |
| `groff` | checking the generated man pages | `docs` job |

See [02 — Clone, build and test](02-build-and-test.md) for how each one is run.

## Kernel requirements

These are the kernel features the engine relies on. The installer (`scripts/install.sh`) and
`delonix system doctor` check most of them for you.

| Requirement | Why | How to check |
|---|---|---|
| **cgroup v2** (unified hierarchy) | resource limits and accounting are written to `/sys/fs/cgroup` | `stat -fc %T /sys/fs/cgroup` prints `cgroup2fs` |
| **Unprivileged user namespaces** | the rootless model: the engine becomes "root" only inside its own user namespace | `unshare -r -n true` succeeds |
| **`/dev/net/tun`** | `slirp4netns` (rootless networking) and VM taps | `test -e /dev/net/tun` |
| **overlayfs** with the new mount API and `lowerdir+` (Linux **6.5** or newer) | container root filesystems are overlay mounts built with `fsopen`/`fsconfig`/`fsmount`, one `lowerdir+` call per layer — see [ADR-0037](../adr/0037-overlay-mount-new-api.md) | `uname -r` |
| **`br_netfilter`** loaded, `net.bridge.bridge-nf-call-iptables=1` | namespace isolation is enforced in nftables `forward` chains; without this module traffic between two containers on the same bridge never reaches them, and isolation is silently inert | `delonix system doctor` |
| **KVM** (`/dev/kvm`) | only for microVMs — see [09](09-microvm-setup.md) | `test -w /dev/kvm` |

The 6.5 requirement has **no preflight check**: an older kernel fails when the first container
rootfs is mounted, not at startup (ADR-0037 records this as a deliberate choice).

On older Debian kernels unprivileged user namespaces are switched off by
`kernel.unprivileged_userns_clone=0`; the installer sets it to `1`.

## Host packages

The source of truth is [`scripts/install.sh`](../../scripts/install.sh), which is also the
official installer (it is published as a release asset). It detects the package manager through
`/etc/os-release` and supports **apt** (Debian, Ubuntu and derivatives), **dnf** (Fedora, RHEL,
CentOS Stream, Rocky, AlmaLinux), **zypper** (openSUSE, SLES) and **pacman** (Arch and
derivatives). Prebuilt binaries exist only for **x86_64**; on other architectures build from
source.

To run containers the engine needs:

| Command | Package (apt / dnf names) | Why |
|---|---|---|
| `slirp4netns` | `slirp4netns` | rootless networking and published ports — without it `run -p` fails |
| `newuidmap` / `newgidmap` | `uidmap` / `shadow-utils` | setuid helpers that map more than one uid into the user namespace; without them images with a non-root user fail in `chown()` |
| `nft` | `nftables` | SDN firewall, isolation and port DNAT |
| `ip` | `iproute2` / `iproute` | veth, bridge and netns plumbing |
| `conntrack` (optional) | `conntrack` / `conntrack-tools` | cleaning up connections when a port is unpublished |

For VMs, `qemu-img`, `cloud-localds` (`cloud-image-utils`), `virsh` (libvirt) and/or
Cloud Hypervisor with its firmware — see [09](09-microvm-setup.md). For building VM images,
`libguestfs-tools` (`install.sh --with-image-build`).

You also need a **subordinate uid/gid range** for your user in `/etc/subuid` and `/etc/subgid`;
without it the user namespace can map only one uid.

### The quickest way to a working host

You do not have to replicate the installer by hand. To install only the host dependencies and
configuration, keeping the binary you build yourself:

```bash
bash scripts/install.sh --no-binary
```

Read the flag list at the top of the script first: some flags change host-wide security settings
(`--low-ports` lets any local program bind ports from 80, `--with-image-build` makes
`/boot/vmlinuz-*` world-readable), and `--no-tune` skips the kernel modules and sysctls, including
`br_netfilter`.

### Memory and disk

There is no fixed minimum. What costs resources is what you run: container images and layers,
VM disks, and the Rust `target/` directory itself (debug builds of the whole workspace take
several gigabytes). Keep an eye on free disk space — kubelets in a local cluster start evicting
pods on disk pressure, which then looks like an engine problem.

## Diagnosing the host

Build the binary (see [02](02-build-and-test.md)) and ask it. The commands below are
read-only unless you pass `--delegate`:

```bash
./target/debug/delonix system doctor          # is every prerequisite met? says how to fix each
./target/debug/delonix system info            # rootless?, cgroup delegation, network infra, counts
./target/debug/delonix system setup           # diagnose cgroup delegation
./target/debug/delonix system resources       # which controllers are delegated, which flags are ignored
```

`delonix system doctor --strict` exits non-zero when a check fails, which is useful in a
provisioning script. If you run these on a machine that already has a Delonix installation in use,
isolate the state first (see [Isolating the engine's state](02-build-and-test.md#isolating-the-engines-state)).

## Known host traps

### Ubuntu 23.10+: AppArmor blocks user namespaces for your dev binary

Recent Ubuntu sets `kernel.apparmor_restrict_unprivileged_userns=1`. A binary without an AppArmor
profile then cannot create a user namespace, and the engine dies at `unshare()` with `EPERM` —
which reads like an engine bug.

`install.sh` installs a profile (`/etc/apparmor.d/delonix`, `flags=(unconfined)` with `userns`),
but that profile is **bound to one path**: `<install dir>/delonix` (`/usr/local/bin/delonix` by
default, `~/.local/bin/delonix` with `--user`). A binary you just built at
`target/debug/delonix` — or copied to `/tmp` — is **not** covered.

Options, from least to most invasive:

1. Add a second profile for your development path (for example your worktree's
   `target/debug/delonix`), following the same shape as the one the installer writes, and load it
   with `sudo apparmor_parser -r <file>`. This touches nothing that is already running.
2. Install your build to the profiled path (`sudo install -m 0755 target/debug/delonix /usr/local/bin/`)
   — **only on a machine where no Delonix workload is in use**. The installed binary is what boot
   units (`ExecStart=<exe> container start …`) and the servers re-execute, so on a host with live
   workloads a debug build would silently become the production engine.
3. Set `kernel.apparmor_restrict_unprivileged_userns=0` — this lowers a host-wide boundary; only
   on a machine you own.

### cgroup delegation: some limits are refused, others are not enforced

Resource limits only reach the kernel if the shell you run the engine from sits in a **delegated**
cgroup. This is a cgroup v2 rule, not a Delonix limitation — rootless Podman has the same
requirement. Without delegation the engine does two different things, depending on the flag:

- `-m`/`--memory`, `-c`/`--cpus` and `--cpu-weight`: `container run` **refuses** before creating
  anything, with an error that names the fix, and exits **69** (`Error::Unavailable`, the
  `EX_UNAVAILABLE` class — `preflight_resource_limits` in
  `bins/delonix-runtime-bin/src/cmd/container.rs`). `DELONIX_ALLOW_UNENFORCED_LIMITS=1` runs the
  container anyway, unlimited, with a warning.
- `--cpuset`, `--io-weight` and the `--device-read-bps`/`--device-write-bps`/`--device-read-iops`/
  `--device-write-iops` family are **not** checked by that probe: they are accepted and applied
  best-effort, so without the `cpuset`/`io` controllers they have no effect and nothing fails.

There is no `--pids-limit` flag; the pids ceiling is a property of the engine's cgroup group, not
of `container run`.

The common case is an **SSH session**: its `session-N.scope` is a *sibling* of
`user@<uid>.service`, and moving a process between them requires writing to a cgroup owned by root.
The per-command fix needs no root:

```bash
systemd-run --user --scope -p Delegate=yes -- ./target/debug/delonix container run -d -m 128M alpine sleep 60
```

For long-lived workloads, use a systemd **user** unit with `Delegate=yes`. Some hosts delegate only
`cpu memory pids` to user sessions; `cpuset` and `io` may never be available rootless, and
`delonix system resources` names the flags that will be ignored. `delonix system setup --delegate`
writes a system-wide drop-in (needs root, takes effect at the next login) when the `cpu`
controller itself is missing.

### A stale `delonix` on your `PATH`

If Delonix is installed on the machine, `delonix` on your `PATH` is the installed release, not your
tree. Always run `./target/debug/delonix` (or `target/release/delonix`) when you test a change.
`--version` shows the commit and the distance from the last tag
(`commit: <hash> (+N commits since vX.Y.Z)`), because between releases two builds share the same
version number.

### Other traps you may hit

- **Ports below 1024** fail rootless with `slirp_add_hostfwd failed`: the port is bound by
  `slirp4netns`, an unprivileged process. Use a high port, or opt in with `install.sh --low-ports`.
- **Hosted CI runners** (GitHub-hosted) block unprivileged user namespaces. The chaos workflow
  detects this and reports `skipped`, not `success`; to exercise the live paths you need a real
  host, a self-hosted runner or a VM.
- **VM firmware and image-building traps** (the Cloud Hypervisor firmware choice, an old `passt`
  in libguestfs builds, `/boot/vmlinuz-*` permissions) are covered in [09](09-microvm-setup.md).
