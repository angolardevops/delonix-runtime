# ADR-0036: macOS/Windows support is a guest-VM launcher, not a port — and only the Windows half is buildable without a real Mac

- **Status:** Proposed
- **Date:** 2026-09-06
- **Deciders:** Walter (owner)
- **Related:** ADR-0006 (`type: microvm`, the existing VM-as-workload precedent this reuses),
  ADR-0008 (Proxmox backend — the phased "registry now, backend blocked on a real host" pattern
  this ADR copies), the "Imagens de appliance"/"Imagem VM dourada" sections of `AGENTS.md` (the
  `delonix-vm-base` images this design runs unmodified), the "Regra de ouro" section of `AGENTS.md`
  (engine crates stay dependency-clean and daemonless — this ADR explains why a guest VM does not
  violate that).

## Context

Requested last, deliberately, and in its own ADR — the queue this session worked through put
every other improvement ahead of it. That ordering turns out to be correct for a reason beyond
"do the easy things first": every other item extended a machine this engine already runs on.
This one asks whether the engine can run somewhere it structurally cannot.

**Confirmed by exhaustive `git grep` against `origin/main`, not assumed**: zero
`#[cfg(target_os = ...)]` in the tree, zero `#[cfg(windows)]`/`#[cfg(macos)]`, zero
platform-specific Cargo dependencies, zero prior ADR mentioning either platform. This is not a
partially-started, abandoned effort — it is a clean question, asked for the first time.

**Why "port the engine" is not on the table.** Every load-bearing primitive in
`delonix-runtime`/`delonix-net`/`delonix-cri` is a Linux kernel interface with no equivalent on
the other two kernels:

- `clone(2)`/`unshare(2)` with `CLONE_NEWUSER|NEWPID|NEWNET|NEWNS|NEWUTS|NEWIPC` — Linux
  namespaces. macOS (XNU) and Windows (NT) have no namespace model shaped like this; Windows has
  *Windows Containers* (job objects + a completely different isolation model, no relation to
  runc-style namespaces), and macOS has no container primitive at all below the VM layer.
- cgroups v2 — Linux-only resource control. This engine's entire limits story
  (`memory.max`/`cpu.max`/the rootless-delegation machinery in "Delegação de cgroup" above) has no
  target on the other two kernels.
- nftables — Linux-only. Every firewall/NAT/DNAT primitive in `delonix-net` (`fwcont`, `fwdeny`,
  `@dlxall`, the isolation sets) is `table ip` nft syntax with no port to `pf` (macOS) or Windows
  Filtering Platform without rewriting the entire dataplane from scratch.
- `pivot_root(2)` — Linux-only.
- Rootless as this engine defines it (subuid/subgid ranges, a user namespace mapping a real uid to
  a fake root) is a Linux user-namespace concept end to end.
- The VM backends are no better off: Cloud Hypervisor needs KVM (Linux-only); libvirt/QEMU can run
  on macOS/Windows but not through the KVM accelerator this engine assumes throughout
  (`delonix_vm::CloudHypervisorBackend`, `LibvirtBackend`'s `qemu:///session` usage).

Porting this would not be "add a few `#[cfg]` blocks" — it would be a second engine, sharing
nothing load-bearing with the first, for the platforms that need it. Nobody has asked for that,
and nothing in this repo's guardrails (`AGENTS.md`'s "Regra de ouro") makes that trade worth
proposing.

## What every real prior-art project actually does (verified, not assumed)

Docker Desktop, Podman (`podman machine`), Colima and Lima all solve "run a Linux container
engine from macOS" the same way, because there is only one way: **run a real Linux kernel in a
VM on the host, and put the engine inside that VM.** The native macOS/Windows binary is a thin
client — it does not run containers itself, it drives the VM and/or forwards a socket into it.
This is public, well-documented behavior of all four projects, not a novel insight; the question
this ADR answers is not *whether* to do that, but how to do it with what this engine already has
instead of building a second stack.

## Decision (proposed)

**Ship a `delonix machine` launcher — a new, host-native, macOS/Windows-only surface — that boots
the SAME `delonix-vm-base` image already published to `ghcr.io/angolardevops/delonix-vm-appliances`
(see the "Imagens de appliance"/imagem dourada sections of `AGENTS.md`), and give the user a
generic pass-through to run any real `delonix` command inside it.** No new guest image, no new
Linux-side code, no new RPC protocol for 245 CLI commands — the guest runs the unmodified engine
this repo already ships, and the host launcher's job is narrow:

```
delonix machine init [--memory 4G --cpus 2]   # provision the guest, once
delonix machine start / stop / rm / list
delonix machine ssh                            # a real shell inside the guest
delonix machine exec -- <any delonix command>  # e.g. `delonix machine exec -- container run -d nginx`
```

**This is not a new architectural precedent for this engine — it is the existing `type: microvm`
workload pattern (ADR-0006), pointed at the engine's own binary instead of a tenant's.** A
`delonix machine` guest is exactly a `kind: Vm` created from a published image; the launcher is a
thin, host-native wrapper around "boot this VM, then reach it," which is the same shape as `vm
create` + `vm console`/`vm ssh` already have on Linux. It does not violate the daemonless
guardrail: the guest VM is not a new persistent Linux-side daemon this repo runs everywhere — it
is the substrate a non-Linux HOST needs to run the engine at all, no different in kind from a
Proxmox/libvirt hypervisor being the substrate any `kind: Vm` already needs.

### The two platforms are NOT equally hard, and the ADR says so instead of averaging them

- **Windows: buildable soon, low new-code cost.** Windows 10 (2004+)/11 already ships **WSL2** — a
  real Linux kernel under a lightweight, Microsoft-maintained Hyper-V VM, present on the host with
  no new hypervisor integration required from this project. `delonix machine init` on Windows is
  `wsl --import delonix <install-dir> <rootfs-from-the-published-image>`; `exec`/`ssh` are
  `wsl -d delonix -- delonix ...`. The engineering surface is a thin Rust wrapper shelling out to
  `wsl.exe`, not a hypervisor client.
- **macOS: the hard half, genuinely blocked on hardware this session cannot reach.** There is no
  WSL2 equivalent — Apple's **Virtualization.framework** (the same one Podman/OrbStack use, macOS
  11+, no HyperKit needed since Big Sur) has to be driven from Rust, either through a community
  binding or a small native Swift/Objective-C helper this project would have to write and
  maintain. That is new, real engineering, and this ADR does not pretend otherwise by naming an
  unverified crate as a fact — a spike has to pick and prove the binding before any code ships.

**No hypervisor-facing code is written in this PR, on either platform.** This sandbox is Linux —
it cannot compile, run, or validate a single line of WSL2- or Virtualization.framework-facing
Rust, and this repo's own rule against affirming what wasn't measured applies exactly here: an
untestable claim of "it works" would be worth nothing. What ships here is the decision and the
phasing; the code is Phase 1/Phase 2 work, gated the way ADR-0008 gated the Proxmox backend on a
real host.

## Alternatives considered

- **Port the isolation primitives themselves** (namespaces→Windows job objects, nftables→WFP/`pf`,
  cgroups→Job Object resource limits/macOS `sandbox-exec`). Rejected: this is not a compatibility
  shim, it is rewriting the two riskiest, most security-sensitive subsystems in the codebase
  (`delonix-runtime`'s `spawn()`, already flagged elsewhere in `AGENTS.md` as a ~405-line function
  needing care, and the entire `delonix-net` dataplane) for kernels with different security models
  end to end. The "GO/NO-GO spike before any new privilege boundary" guardrail would require
  proving this safe on TWO new kernels before writing a single container primitive — a
  multi-quarter security-research project, not a CLI feature.
- **A remote-only client, no local guest** (native `delonix` CLI on macOS/Windows that only ever
  talks to a Linux engine running elsewhere over the network). Rejected as the DEFAULT: it solves
  "control a remote node," which `delonix serve api`/`serve docker-api` already do over a unix
  socket (reachable through SSH port-forwarding today, with no code changes) — it does not solve
  "run Delonix on my laptop," which is what "macOS/Windows support" means to everyone who has
  asked for it in the prior art. `machine ssh`/`exec` already give a remote-control story for free
  once the local guest exists; a *pure* remote client with no local guest is a smaller, already
  half-solved problem this ADR doesn't need to re-decide.
- **Reuse an existing VM-manager wholesale (Lima on macOS, WSL's own UI on Windows) instead of a
  `delonix machine` subcommand.** Rejected as the shipped default: it would make "install Delonix
  on a Mac" mean "install Delonix, then separately learn and configure Lima" — worse first-run
  experience than `podman machine init` offers today, which is the bar this engine is measured
  against throughout `AGENTS.md`'s own Docker/Podman comparison table. Nothing here stops a user
  from running the published `delonix-vm-base` image under Lima by hand; the built-in launcher is
  what makes it a supported path instead of a workaround.
- **One shared Rust crate abstracting "boot a guest and exec into it" for both platforms up
  front.** Rejected for v1: the two backends have almost nothing in common yet (`wsl.exe`
  shell-out vs. a framework binding) — abstracting before there are two real implementations to
  compare would be guessing at the seam, the same mistake ADR-0002 (`ComputeDriver`) explicitly
  avoided by waiting for a second real consumer before extracting a trait.

## Consequences

- A new crate, `delonix-machine`, compiled **only** for `target_os = "windows"` and
  `target_os = "macos"` (`[target.'cfg(...)'.dependencies]` in its own `Cargo.toml`) — the existing
  Linux engine crates gain zero new dependencies and zero new `#[cfg]` branches; `cargo tree -e
  normal` on every Linux crate stays exactly as clean as the "Regra de ouro" section already
  requires.
- `delonix-runtime-bin`'s `Cmd` enum gains a `Machine` variant compiled only on those two targets,
  so a Linux build of the CLI (the only build this repo can currently test) is entirely unaffected
  — `delonix machine` does not exist as a command on Linux, and does not need to.
- The published `delonix-vm-base` images need **zero changes** — they already have `delonix`
  installed and boot fast (7.8s measured for `ubuntu-24.04` under Cloud Hypervisor, per the
  "Imagem VM dourada" section). WSL2 import needs the rootfs in the tar-based format `wsl --import`
  expects, which is a packaging step, not an image rebuild.
- CI cannot validate either platform today — `.github/workflows/ci.yml` runs on `ubuntu-*` runners
  only. Phase 1 (Windows) adds a `windows-latest` job that can at least confirm the crate compiles
  and `wsl --import`/`exec` round-trip against a real (GitHub-hosted) WSL2, which is the honest
  bar: a compiled-but-never-run binary is not "supported." Phase 2 (macOS) needs the equivalent on
  a `macos-latest` runner — GitHub-hosted macOS runners do **not** support nested virtualization
  for third-party VMs as of this writing, so validating Virtualization.framework in CI is an open
  question the Phase 2 spike has to answer, not something this ADR can assume works.

## Not done here, and why

- **Implementation, on either platform.** This ADR is the design; Phase 1 (Windows/WSL2) and
  Phase 2 (macOS/Virtualization.framework) are separate, later commits, each gated the way
  ADR-0008 gated its Proxmox backend: **Phase 1 (Windows) is GO once someone with a real Windows
  10/11 machine can validate the `wsl --import`/`exec` round-trip** — no exotic hardware needed,
  just a host this sandbox doesn't have. **Phase 2 (macOS) needs its own spike, on a real Mac,
  before any code ships**, to pick and prove a Virtualization.framework binding — naming one now
  without testing it would be exactly the kind of asserted-not-measured claim this repo's own
  doctrine exists to catch.
- **A GUI.** Docker Desktop's UI is a large, separate product decision; this ADR is scoped to the
  CLI parity every other platform already has.
- **Networking parity with the Linux SDN from outside the guest** (e.g., a container inside the
  `delonix machine` guest getting a routable IP on the host's LAN). The guest is reached by
  `ssh`/`exec` and published ports the same way any `kind: Vm` is reached today — no new
  host↔guest dataplane is proposed here.
- **Apple Silicon vs. Intel Mac, or ARM vs. x86 Windows, as separate concerns.** Virtualization.
  framework and WSL2 both already handle the host-architecture question themselves; this ADR
  doesn't need to re-decide it, only note that the Phase 2 spike should confirm the published
  guest image has (or gains) an `arm64` build — today's `delonix-vm-base` images are x86_64-only
  (see ADR-0018), which is itself a real, separate gap this ADR surfaces but does not fix.
