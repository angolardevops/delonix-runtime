# ADR-0046: `VirtualMachine.spec.expose`, `kind: IPPool`, and hosts injection

## Status

Accepted 2026-09-20. Phase 1 in progress. Decisions of scope were taken with the owner on 2026-09-20
(below); the open questions at the end need an answer before phase 2.

## Context

A service listening inside a VM (HTTP/S on 80/443/8080…) cannot be published by name today:

1. **`HTTPRoute` only resolves containers.** `httproute.rs::container_ips()` reads the container
   store; a `backend.service` naming a VM fails with "has no IP on the SDN". The L7 proxy runs
   inside the holder netns, so it only reaches workloads on the SDN.
2. **A VM is on the SDN only with the Cloud Hypervisor backend.** A libvirt VM lives on `virbr0`
   in the host netns, a different L2 the holder does not see (same constraint documented for
   `vm bridge`).
3. **There is no address reservation.** The only allocator is the per-network IPAM (private
   leases keyed by workload id). Nothing reserves an address that a route or service can claim,
   and nothing tells a name where to resolve.
4. **Names are not propagated.** The internal DNS answers `<name>.<ns>.delonix.internal`, but a
   host declared in an `HTTPRoute` (`app.example.pt`) resolves nowhere unless the operator edits
   `/etc/hosts` by hand — a manual step on the normal path.

## Decisions taken with the owner (2026-09-20)

| Question | Choice |
|---|---|
| libvirt VMs | Covered, through **a proxy in the host netns** (not only `vm bridge`) |
| `/etc/hosts` | **Three targets, each opt-in**: containers, operator host, VM guest |
| Address announcement | **Reservation + local bind by default; L2 announcement opt-in and privileged** |

## Decision

### D1 — `VirtualMachine.spec.expose` lowers to `HTTPRoute`

```yaml
kind: VirtualMachine
metadata: { name: web01 }
spec:
  expose:
    - host: app.example.pt
      port: 8080          # port INSIDE the guest
      path: /             # default
      tls: { mode: selfSigned }   # or secretRef, as in HTTPRoute
      pool: edge          # optional IPPool (D3)
```

It is **sugar that lowers at `manifest::load`** into a synthetic `kind: HTTPRoute` (the
`Workload` and `Dependency` precedent). The VM does not gain a second proxy path: `apply`,
`plan`, drift and `--prune` see the child. The child is owned by the stack via the same label.
The backend resolves through a new `vm_ips()` next to `container_ips()`, with the VM record's
IP (observed for libvirt, predicted for CH — see D2).

Refused, by name and before creating anything: `expose` on a VM whose IP is only predicted and
whose reachability cannot be probed (the `--wait` lesson: a computed lease is not a live VM).

### D2 — libvirt reachability: a per-route-set proxy in the host netns

The existing proxy is launched inside the holder (`nsenter … ingress-proxy`) because its
backends live on the SDN. libvirt VMs need a proxy **in the host netns**, where `virbr0` is
routable. It is the same binary and the same config format, launched **without** `nsenter`, as a
detached process with a pidfile and the ownership proof already used for the L7 proxy
(`--config` token, `DELONIX_ROOT` pinned). One process per route set, owned by the systemd
unit or the invoking `apply`; **no resident daemon** (principle 2). The proxy config gains a
`netns: holder|host` field so a route set is served where its backends are reachable; a mixed
set is split in two config files, never one proxy with two personalities.

This needs an ADR-level acknowledgement of the exception: a per-workload detached process with
a clear owner is allowed by principle 2; a process in the host netns binding ports is the new
part. Ports below `ip_unprivileged_port_start` are still refused by the existing preflight.

### D3 — `kind: IPPool` (networking group)

```yaml
kind: IPPool
metadata: { name: edge }
spec:
  addresses: ["203.0.113.10-203.0.113.20", "10.50.0.0/28"]
  announce: local        # local (default) | l2
  interface: eno1        # only for announce: l2
```

- **Allocation is persisted and idempotent**, keyed by the claimant (`HTTPRoute/<name>`,
  `Gateway/<name>`), in the same style and under the same lock discipline as the IPAM: a claim is
  a lease; a lease is only reclaimed by a reaper that respects a grace window and the same
  liveness set the IPAM reaper uses. **No reaper ships before the ledger is observable**
  (`ippool ls`), following the IPAM lesson (391 leases, 47 live, no reaper).
- **`announce: local`** (default, rootless): the reserved address is the bind address of the
  publish (`publish_bind_addr` already accepts `[hostIp:]hostPort:contPort`). Nothing is added to
  an interface. The address must already be present on the host; if it is not, `apply` says so
  and stops, and does not pretend.
- **`announce: l2`** (opt-in, privileged, EXPERIMENTAL, like `vm bridge`): `ip addr add` on
  `interface` plus a gratuitous ARP; `--apply` gate and dry-run by default; removed on release.
  It is the MetalLB-L2 equivalent and the only place root is needed.
- **`ipv6` and BGP are out of scope.** The dataplane is `table ip` (v4).
- The pool is a **capability, not a consumer name**: nothing here mentions a platform.

### D4 — hosts injection, three targets, each opt-in

`spec.hosts` on the route (or `expose[].hosts`), default **none**:

| Target | Mechanism | Privilege |
|---|---|---|
| `containers` | the existing per-container hosts file, re-applied on route apply/remove | none |
| `host` | a **delimited managed block** in `/etc/hosts` (`# BEGIN delonix:<stack>` … `# END`), rewritten atomically, removed on `--prune`/`destroy` | root (refused otherwise, with the command to run) |
| `guest` | cloud-init `write_files`/`manage_etc_hosts` at create; qemu-guest-agent `guest-file-write` afterwards | none, but **refused** on an appliance image (`cloud_init: false`) and when no agent answers |

Rules that do not bend: the block is the only thing this feature touches in `/etc/hosts`; a name
that already exists outside the block is **refused**, not overwritten; every target reports what
it wrote and what it could not.

## Phases

1. `expose` for **Cloud Hypervisor** VMs (D1, `vm_ips`) + `hosts: containers`. Provable end to
   end on the SDN, no privilege.
2. Host-netns proxy for **libvirt** VMs (D2) + `hosts: host`.
3. `IPPool` with `announce: local` and the ledger + `ippool ls` (D3).
4. `announce: l2` and `hosts: guest`.

Each phase closes with a run against the real thing, and reports what was not validated.

## Consequences

- `kinds.rs` gains `IPPool` (one table row, plus the five satellite tables found by the `Service`
  work: `hot_fields`, `converge_and_stamp`, `NAMESPACE_SOURCES`, `TYPED_KINDS`, the published
  schema). `VirtualMachine` gains an `expose` field, so the published schema changes.
- `HTTPRoute` gains `netns` and a VM-aware resolver. Existing manifests are unchanged.
- The single-owner rule holds: an address has one claimant; a second claim is `Conflict` (5).

## Open questions

1. Should `IPPool` addresses also be usable by `kind: Service` (DNS round-robin today, no VIP)?
   Leaning yes, as a later claimant, not in v1.
2. ~~`hosts: host` with several stacks~~ — answered in phase 1b: one block per node.
3. libvirt `expose` when the guest IP is not yet known at apply time: fail, or register the route
   and let the proxy retry? Leaning fail, with `--wait`.

## Phase 1 — what was built and what was measured (2026-09-20)

Built: `VmSpec.expose` lowering to `HTTPRoute/<vm>-expose` (`cmd/vm_expose.rs`); `httproute::vm_ips()`
(Cloud Hypervisor VMs only; a libvirt VM is refused BY NAME with the reason); `validate_graph` accepts
a VM as a route backend. Measured against a real Cloud Hypervisor VM in an isolated root: a request
with `Host: app.example.pt` through the proxy reached a server inside the guest, and an unknown host
answered 404. That run used a hand-written `HTTPRoute` on port 18080, because port 80 was taken on the
host; the `expose:` sugar itself was validated by `validate` and `--dry-run`, not by traffic.

Found only by running it: `validate_graph` rejected every VM backend ("not a declared or existing
Container"), which no unit test had reached.

**`hosts: containers` is not in phase 1.** A container's `/etc/hosts` is written once at creation,
so the first attempt (phase 1b) answered the route host from the holder DNS with the bridge address
of the client's network. The name resolved, and the container could not connect: `dlxinput` drops a
container's connection to any holder-resident listener ON PURPOSE, because a container that reaches
the proxy is relayed to any backend, across namespaces and past `ingress policy deny`. That control
is not weakened here. The question is now ADR-0047. The DNS code is kept in commit `d7245765` for
that ADR to reuse; it is not in the tree.

## Phase 1b — `hosts: host` (2026-09-20)

`spec.hosts: [host]` on an `HTTPRoute` (and `expose[].hosts`, which must agree across entries)
publishes each rule host as `127.0.0.1 <host>` in ONE delimited block of the operator host's
`/etc/hosts` (`cmd/hosts_file.rs`). Loopback is where the slirp forward binds by default; the port is
the route's entrypoint, which a hosts file cannot carry.

- **One block per node, not per stack** (resolves open question 2): the proxy composer is collective
  across documents, so the block is rewritten whole from the composed config and a name disappears
  when no route asks for it.
- **Nothing outside the block is touched.** A name that already has an entry outside it is refused,
  with the offending line.
- **Root is required.** Without it the apply stops and prints the exact block to add; `/etc/hosts` was
  measured byte-identical after that refusal. Removing a route without root only warns, so `rm`
  is never blocked by it. No write happens when nothing would change, so a manifest that does not
  use `hosts:` never needs root.
- `DELONIX_HOSTS_FILE` redirects the file, so an isolated run never touches the real one.
- The reconciler compares `hosts` (desired from the spec, actual from the composed config). A route to
  a VM also had permanent drift, because `actual()` mapped addresses back to names for containers
  only; fixed.

Measured against the real proxy in an isolated root: the block is written after the other lines and
removed by `httproute rm`, leaving the file as it was; a second apply changes nothing; a conflicting
line is refused.

## Not validated (phase 1b)

A client resolving a name from the written block and reaching the backend was NOT observed: writing
the real `/etc/hosts` needs root, and the traffic step of that run failed on my test backend (a
container whose command never came up, not investigated further). The block content, the refusals
and the removal were measured; the name-to-traffic step was not. The `expose:` sugar with `hosts:` was
covered by unit tests and `--dry-run`, not by traffic.

## Not validated (design)

Everything above is design. Not measured yet: that a host-netns proxy reaches a libvirt guest on
`virbr0` from a rootless user (the `qemu:///session` mode has no `virbr0` at all — user-mode
SLIRP — so this may only hold for `qemu:///system`); that the guest agent is present in the
published images; and the L2 announcement on any real network.
