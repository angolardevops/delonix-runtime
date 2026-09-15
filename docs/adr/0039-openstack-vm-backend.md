# ADR-0039: An OpenStack `VmBackend`, gated on a spike against a live cloud

- **Status:** Proposed — becomes Accepted or Rejected by the spike result below, not by review
- **Date:** 2026-09-15
- **Deciders:** Walter Angolar

## Context

`delonix vm` and `kind: VirtualMachine` serve two hypervisors today: libvirt and one
Proxmox VE node (ADR-0008). An OpenStack target does not exist in the engine, and
that is now measured rather than assumed:

- `delonix vm create --backend openstack` answers
  `unknown VM backend: 'openstack' (use 'cloud-hypervisor', 'libvirt')`, leaves no
  record, and does not fall back to libvirt. Same for a manifest with
  `backend: openstack`. (`delonix-provider-lifecycle` executor, 2026-09-15, 9 PASS / 1 SKIP.)
- Nothing named OpenStack exists under `crates/`. What exists is an **appliance image**,
  `openstack:2026.1` (#247, #253): kolla-ansible pinned and its container images pulled,
  **not installed**. The installation is the `openstack_aio` role of `delonix-deploy`.

### What already decided the ORDER, in another repository

`delonix-paas` ADR 0019 (provider ports) §9 says the second adapter proves the port, and
its done criterion is *the same `kind: VirtualMachine` runs on Proxmox and on libvirt
without changing the manifest* — "OpenStack comes after that and not before". §3 says a
platform adapter **wraps** the engine backend and never moves or duplicates it.

That criterion is about the PaaS ports (`delonix-provider-*`). What this repository can
state is the engine-side half, which is what those ports wrap, and it was proved on
2026-09-15 with one executor and one manifest shape against both hypervisors, acting only
through the `delonix` CLI and verifying through read-only provider access:

| Backend | Commit | Result |
|---|---|---|
| libvirt | `58fdc4e3` (incl. #312) | 52 / 0 — incl. a real Ubuntu 24.04 guest reachable through `delonix vm ssh` |
| Proxmox VE 9.2 | `a9c41cb9` (incl. #314) | 51 / 0 — create, same-VM stop/start, back-to-back restart, snapshot, destroy, field refusal, `kind: VirtualMachine` |

This ADR does **not** claim the PaaS §9 criterion is met. It claims the engine precondition
for an OpenStack backend is.

### Guardrails this touches

- **#2 (no tenant).** OpenStack is multi-tenant by construction (projects, domains). The
  engine backend must address ONE project in ONE region with ONE credential, exactly as
  `delonix-proxmox` addresses one node. Which project a customer maps to is the PaaS's
  (ADR 0019 §8: provider credentials are platform secrets, per cell).
- **#4 (engine crates dependency-clean).** An HTTP client does not enter `delonix-vm`.
- **#5 (spike before the boundary).** A remote API is not a privilege boundary on this
  host, but the rule that forbade shipping `delonix-proxmox` before it was seen booting a
  VM applies unchanged (ADR-0008 phase 2).
- **#6 (no silent failure).** Every `VmConfig` field Nova cannot honour is refused by name.

## Decision

1. **Spike first, in this order, and nothing lands in `crates/` before it is GO.**
   Stand a cloud up with `delonix-deploy`'s `openstack_aio` (substrate, via Ansible), then
   drive Keystone v3 + Nova + Glance + Neutron over plain HTTP from a throwaway program and
   record, for each question below, the request, the response and the timing:

   | # | Question the spike must answer with a measurement | Why it decides the shape |
   |---|---|---|
   | S1 | An **application credential** scoped to one project authenticates and survives token expiry | the `kind: Secret` shape, and whether re-auth is needed (Proxmox needed it) |
   | S2 | `POST /servers` from a Glance image + Neutron network → `ACTIVE`, and how long the `BUILD` wait is | the `boot` wait loop and its ceiling |
   | S3 | `os-stop` → `SHUTOFF` keeps the server and its root disk; `os-start` resumes the **same** server ID | `stop` ≠ `destroy`, the defect `delonix-proxmox` already paid (`vm start` created a 2nd VM) |
   | S4 | Two lifecycle actions back-to-back: is a `409 Conflict` (task_state not null) returned, or queued? | the lock defect `delonix-proxmox` paid in #314 — must be waited on, never raced |
   | S5 | `pause`/`unpause` are native | the first backend where `vm pause` could be real remotely |
   | S6 | Snapshot: Nova `createImage` (a Glance image) vs Cinder volume snapshot — which can be **restored onto the same server** and **removed** | whether `vm snapshot restore`/`rm` are honest or must be refused |
   | S7 | The IP comes from `addresses` of the server (observed), for a tenant network without floating IP | `ip()` and `ip_is_predicted()` |
   | S8 | Cloud-init: `user_data` + `key_name`/injected key reach the guest; config drive vs metadata service | `hostname`/`sshKeys`/`userData` intent mapping |
   | S9 | `DELETE /servers/{id}` leaves no port, volume or floating IP behind | `destroy` without orphans |

   **GO** requires S1–S4, S7–S9 answered positively. S5/S6 negative is not a NO-GO; it
   decides which verbs are refused.

2. **If GO, the backend mirrors `delonix-proxmox`, and deliberately nothing more:**
   - crate `delonix-openstack`, depending on `delonix-vm`; registered by the `-bin` from
     environment (`DELONIX_OPENSTACK_AUTH_URL`, `_REGION`, `_PROJECT_ID`,
     `DELONIX_OPENSTACK_SECRET` → a `kind: Secret` with an application credential);
     nothing connects until the backend is selected (the ADR-0008 registry closure);
   - `manages_own_storage() = true`, `auto_selectable() = false`;
   - `--disk` addresses the cloud: `image:<glance-id-or-name>` (with a flavor chosen from
     `vcpus`/`memory` by exact match — **refused** when no flavor matches, never rounded);
   - the handle in the record is `openstack:<region>:<server-id>`;
   - every wait reads Nova's `status`/`OS-EXT-STS:task_state`, never a sleep.

3. **The acceptance gate is the existing executor**, extended with an `openstack` provider
   case that acts only through `delonix` and observes with read-only Nova/Neutron GETs, and
   must reach the libvirt/Proxmox bar: imperative cycle, `kind: VirtualMachine` with zero
   drift, back-to-back operations, field refusal, and a real guest answering `delonix vm ssh`.

## Alternatives considered

- **Do nothing; declare libvirt + Proxmox the supported set.** Cheapest, and honest today.
  Rejected as the end state because IaaS customers with an existing OpenStack are a stated
  target; kept as the explicit NO-GO outcome.
- **Write the backend now, test later.** Rejected: `delonix-proxmox` shows what a remote
  backend written from documentation carries — a duplicated form key that made every create
  fail (#314), a `stop` that deleted the disk, a `start` that created a second VM. All were
  found only against a real node.
- **Shell out to the `openstack` CLI.** Rejected: it interpolates into a process the way
  achado CRÍTICO #1 did, adds a Python toolchain to the host, and fails the executor's
  anti-hack rule (the provider CLI may not be on the path of a managed VM operation).
- **Build it in `delonix-paas` as a `ComputeProvider` directly.** Rejected by ADR 0019 §3:
  the adapter wraps an engine backend; putting the lifecycle in the control plane would make
  the public engine unable to serve the target at all.
- **A generic multi-cloud crate** (libcloud-style). Rejected by ADR 0019 §1 in spirit — one
  focused backend per provider, native inside.

## Consequences

- **Easier:** once GO, the same `kind: VirtualMachine` reaches a third provider, and the PaaS
  can wrap it per ADR 0019 without touching the engine.
- **Harder:** the spike needs a running OpenStack. `openstack_aio` recommends 16 GiB; the
  development host (31 GiB, shared with the `delonix-dev` cluster and the `pve92` lab) cannot
  run it next to both. The spike is scheduled after the Proxmox guest-OS validation, with the
  other labs stopped. That is a resource constraint stated here, not hidden.
- **Debt accepted:** one project, one region, one network per VM; floating IPs, security
  groups, volumes as extra disks and multi-NIC are refused by name in v1.
- **Not settled here:** anything the PaaS port adds (discovery, capacity, placement, health),
  and whether the PaaS §9 criterion is met on its side.

## Spike result

_Not run yet. This section is filled with the measurements, and the Status line changes
with it — GO → Accepted, NO-GO → Rejected._
