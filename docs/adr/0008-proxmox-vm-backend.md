# ADR-0008: Add a Proxmox VE backend as a separate crate, and make backends registrable

- **Status:** Proposed
- **Date:** 2026-08-10
- **Deciders:** Walter Angolar

## Context

The engine can now build and run a Proxmox VE appliance image
(`proxmox-ve:9.1`, see the appliance work in `AGENTS.md`), which makes the next
question concrete: a `delonix vm create` should be able to place the VM **on**
a Proxmox host through its API, not only on the local libvirt/Cloud Hypervisor.

Measured before proposing anything:

- **There is no Proxmox or TrueNAS API code anywhere in the workspace** (grep
  over `crates/` for `/api2/json`, `pve.?api`, `proxmox.*api`: no hits). This is
  greenfield, not a wiring job.
- **`delonix-vm` has four dependencies**: `delonix-runtime-core`,
  `delonix-net`, `tracing`, `libc` (`crates/delonix-vm/Cargo.toml`). It is one
  of the cleanest crates in the tree.
- **`VmBackend` is public and implementable from outside**
  (`crates/delonix-vm/src/lib.rs:499`), but **backend selection is not**:
  `fn backend_for(vm: &Vm) -> Box<dyn VmBackend>` (line 644) is a private
  `match` over two string literals with `_ => CloudHypervisorBackend`. Nothing
  outside the crate can add a third.

Guardrails in play (see the `delonix-adr` skill):

- **#2 (boundary with the private PaaS).** A Proxmox *driver* with
  multi-cluster inventory, a scheduler, or tenant↔resource mapping belongs to
  `delonix-paas`. A **single-node backend with no notion of tenant** is
  explicitly named as something that can live here.
- **#4 (engine crates stay dependency-clean).** Talking to a REST API needs an
  HTTP client. `reqwest` pulls in tokio, hyper and a TLS stack; putting that
  into a four-dependency engine crate is a supply-chain decision for a
  container/VM runtime, not a detail.

## Decision

1. **A new crate `delonix-proxmox`** implements `delonix_vm::VmBackend` against
   the Proxmox VE REST API (`/api2/json`). It depends on `delonix-vm` (for the
   trait and `VmConfig`), `delonix-runtime-core` and an HTTP client.
   `delonix-vm` gains **no** new dependency.

2. **Backend selection becomes registrable.** `backend_for` is replaced by a
   registry the caller populates, so a backend can be added without editing
   `delonix-vm`. The default registration (libvirt, Cloud Hypervisor) stays
   inside `delonix-vm`, so nothing changes for existing callers.

3. **Scope is one Proxmox node, addressed explicitly** — `--backend proxmox`
   plus an endpoint and an API token. No inventory, no scheduling, no choosing
   a node for the user. If a decision would need to know *who the customer is*,
   it is out of scope by guardrail #2.

4. **Credentials come from `kind: Secret`**, never from a flag or a manifest
   literal (the `Tunnel` `tokenSecretRef` precedent).

5. **An unknown backend name becomes an error.** Today `_ =>` silently falls
   through to Cloud Hypervisor; `valid_backend_name` makes that hard to reach
   from the CLI, but the registry must not re-create a silent default
   (guardrail #6).

## Alternatives considered

- **Put `reqwest` in `delonix-vm` and write the backend there.** Simplest
  diff, and rejected: it takes an engine crate from 4 dependencies to a tokio +
  hyper + TLS tree, for a feature most users never enable. Guardrail #4 exists
  for exactly this.
- **Implement the backend in `delonix-runtime-bin`**, where `reqwest` already
  lives (via `delonix-image`). Tempting, and rejected because `backend_for` is
  private: the bin cannot inject a backend without the registry change anyway,
  and putting a compute backend in the CLI crate puts it out of reach of
  `delonix-cri` and `delonix-mgmt`.
- **Shell out to `qm`/`pvesh` over SSH**, the way `cluster apply` drives
  `kubeadm`. Rejected: it needs shell access to the Proxmox host rather than an
  API token, and this repo has already paid for command injection through
  interpolated remote shell commands (audit finding #1, `AGENTS.md`).
- **Do nothing** — keep Proxmox as an appliance you run, not a place you run
  things. Legitimate, and the right answer if the registry change proves to
  cost more than the backend is worth; this ADR should be rejected rather than
  half-implemented in that case.

## Consequences

**Easier.** A third backend stops being a special case: Firecracker, or a
single-node Proxmox, become "implement the trait and register it". The
`ComputeDriver` direction of ADR-0002 gets its second real consumer, which is
the bar that ADR set for promoting the trait.

**Harder.** Backend selection stops being a two-line `match` a reader can take
in at a glance. The registry must not become a plugin system — it is a map
populated at startup, nothing more.

**Debt accepted.** A REST backend cannot honour the whole of `VmConfig`:
hugepages, CPU affinity, 9p volumes and the raw libvirt XML escape hatches have
no Proxmox equivalent. Every one of those must be **refused with a message
naming the field**, never accepted and dropped — the failure mode this repo
treats as its worst.

**Testing.** There is no Proxmox host in this sandbox. The API client is
testable against recorded responses; the backend is not testable end-to-end
here, and that limit must be stated in the release notes rather than implied
away. The appliance image built in this same series (`proxmox-ve:9.1`) is the
obvious test target and should be used as one.
