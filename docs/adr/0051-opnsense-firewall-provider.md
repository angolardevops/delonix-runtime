# ADR-0051: A pluggable `GatewayProvider`, and OPNsense as its first external backend

- **Status:** Accepted — Option B (pluggable backend), scoped to a design this
  repo already runs in production for `VmBackend`, not a new context crate
- **Date:** 2026-09-24
- **Deciders:** Walter (owner)
- **Related:** ADR-0008 (the `VmBackend` registry this mirrors almost
  verbatim); ADR-0009 (TrueNAS — the "configure an external appliance over
  its REST API" client shape this still reuses for the OPNsense client
  itself); ADR-0043 (the `DX-CDNN` error dictionary the client maps into);
  ADR-0044 (the phase-tagged exception mechanism this decision reuses, and
  the P4 migration this rides alongside rather than blocks on); ADR-0050
  (the capability catalog this adds `Domain::Network` entries to)

## Context (updated after the first draft — see the correction below)

The owner asked for an "OPNsenseProvider" to serve as the interface for the
firewall used in the network layer. The first draft of this ADR (see git
history) framed this as "Option A: a provisioning client (TrueNAS shape)"
vs. "Option B: a pluggable backend, which requires a `delonix-networking`
context crate that does not exist yet." The owner picked Option B. **That
framing of Option B's cost was wrong, caught before any implementation
code was written**, and the correction is why this ADR looks the way it
does:

`crates/proxmox → crates/vm` (`delonix-proxmox` depending on the
`delonix-vm` ADAPTER, where `trait VmBackend` and its registry live today)
is not the target architecture — it is a **named, phase-tagged exception**
in `scripts/arch_fitness.py`'s `EXCEPTIONS` table:

```
("dep", "delonix-proxmox", "delonix-vm"): (
    "P4",
    "the VmBackend port lives in the same crate as the Cloud Hypervisor and "
    "libvirt adapters; P4 moves the port into the compute context and each "
    "backend into its own provider crate",
),
```

So the *eventual* target for `VmBackend` — port in the `delonix-compute`
context, each backend its own provider crate — is real, but it is **P4b's
own unfinished migration**, still in progress for VM alone (this session's
own memory: "próxima fatia = P4b.2, VmConfig/VmBackend/registo para o
compute"). Building a *second* pluggable-backend abstraction straight into
that not-yet-proven target shape, before the first one (VM) has even
finished moving there, would mean redoing this work if P4b's migration
turns up problems the VM case does not have yet.

**The decision: build `GatewayProvider` exactly where `VmBackend` lives
today** — trait + closure registry inside the adapter that owns the native
implementation (`delonix-sdn`, which already owns the nftables dataplane
`FirewallPerWorkload`/`FirewallDefaultDeny`/`FirewallSourceFiltering`/
`FirewallEgressPolicy` answer to) — with `delonix-opnsense` (a new
`crates/providers/` crate) taking the exact same kind of named,
phase-tagged exception `delonix-proxmox → delonix-vm` already has. When
P4b eventually moves `VmBackend`'s port into `delonix-compute`, this one
moves the same week, by the same mechanical pattern, tagged to the same
phase. This is materially cheaper than the first draft's "stand up
`delonix-networking` first" framing, ships now, and throws nothing away.

**What the OPNsense API actually looks like (read from the docs, not
assumed).** `https://<host>/api/<module>/<controller>/<command>`, `GET` to
read and `POST` to create/update/act, JSON bodies, HTTP Basic Auth with a
`key`/`secret` pair generated per account (`System ‣ Access ‣ Users ‣ API
Keys`) — no password fallback the way Proxmox and TrueNAS document. The
firewall module (`firewall/alias/*`, `firewall/filter/*`,
`firewall/d_nat/*`) is a standard `ApiMutableModelControllerBase` CRUD
surface: administrative calls **stage** changes, and a separate
`firewall/filter_base/apply` (or `firewall/filter/apply`) call **activates**
them — a create-then-verify shape close to what `delonix-proxmox`'s task
polling and `delonix-truenas`'s job polling already handle, minus
Proxmox's asynchronous UPID. Whether `apply` blocks or needs its own poll
is **unmeasured** — first thing the live spike (Phase 0 below) settles.

**This repo already has OPNsense, and already knows one hard limit about
it.** `scripts/appliances/build-opnsense.sh` builds and publishes an
OPNsense 26.1.2 `nano` image (`ghcr.io/angolardevops/
delonix-vm-appliances`), and `AGENTS.md` records a measured NO-GO:
**OPNsense does not boot under Cloud Hypervisor** (`rust-hypervisor-fw`
and the EDK2 `CLOUDHV.fd` both stuck), so it only runs under `libvirt`, on
`virbr0` — outside the netns the native SDN programs. The only measured
path from an OPNsense VM's data plane into the container SDN's L2 is
`vm bridge` (root-privileged, `EXPERIMENTAL`, the one deliberate exception
to daemonless/rootless-first this repo has made so far). **Nothing in
this ADR solves that.** It designs how the engine talks to OPNsense's
*control plane* (its API); OPNsense's *data plane* placement relative to
container traffic is a separate, already-documented limit.

**The domain fit, and why `GatewayProvider` is a new trait and not an
extension of the existing `NetworkProvider`/`Domain::Firewall`.**
`crates/contexts/delonix-compute/src/ports.rs` already has a
`trait NetworkProvider` with `apply_firewall(&self, id: &str, ip: &str, fw:
&ContainerFw) -> Result<()>` — the read port `container run` resolves
through when a workload needs per-container nftables rules applied at
creation. That signature is **per container, per IP** — exactly the shape
`AGENTS.md` already flags as a mismatch for an appliance built to be a
perimeter device: making OPNsense answer it would mean one alias+rule pair
per container, on hardware/VM meant to sit at a boundary, not inside a
per-workload micro-segmentation loop. `Domain::Firewall`'s four catalog
entries (`FirewallPerWorkload`/`FirewallDefaultDeny`/
`FirewallSourceFiltering`/`FirewallEgressPolicy`) describe exactly what
native nftables already gives for free, rootless, and nothing about an
external REST API improves on that. OPNsense's actual strengths — NAT,
multi-WAN, VPN, perimeter filtering — belong to `Domain::Network`
(already exists: "Networks, addressing, isolation and exposure"), under
new capability names this ADR adds, not the four `Firewall` ones.

## Decision

Build `GatewayProvider` — a trait for **node-egress / perimeter gateway**
policy, distinct from the existing per-workload `NetworkProvider` — with a
`BackendFactory`/`register_gateway_provider` registry mirroring
`delonix-vm`'s mechanism method-for-method (`BackendRegistration`,
`auto_selectable`, idempotent-by-id registration, the same refusal rules).
`delonix-sdn` gets one built-in, always-registered implementation
(`"native"`, wrapping the existing masquerade/forward dataplane, mostly a
pass-through given that path is unconditional today — see Phase 1).
`delonix-opnsense` (new `crates/providers/` crate) is the first external
one, registered from `-bin` the same way `DELONIX_PROXMOX_URL` registers
the Proxmox `VmBackend`.

### Phased rollout

**Phase 0 — live spike against a real OPNsense (blocks everything below
it).** Boot the `delonix-vm-appliances` OPNsense image under `libvirt` on
an isolated, clearly-named VM (never touching another session's running
work — this host already carries other sessions' VMs:
`delonix-dev-cp-*`/`delonix-dev-w-1`, `pve-lab-475`), generate an API
key/secret, and confirm against the real appliance: whether
`firewall/filter/apply` is synchronous or needs polling; the exact JSON
shape `add_rule`/`set_rule` accept and `search_rule` returns; whether an
alias is required before a rule can reference it or can be created inline;
and what a malformed/unauthorized call actually answers with (status code
and body), to map into the ADR-0043 dictionary correctly instead of
guessing. **This step needs the owner's go-ahead before it runs** — it
means standing up a VM on a shared host, which this repo's own operating
rule treats as an action to do deliberately, not casually.

**Phase 0, started 2026-09-24 — read-side findings measured, write-side
blocked by this session's own tool policy.** `delonix vm create
opnsense-adr0051-spike --disk opnsense:26.1 --backend libvirt` (the
`opnsense:26.1` image was already local from earlier appliance-building
work — no pull needed). Confirmed and corrected on the way:

- **A single NIC on the appliance is assigned as LAN, not WAN**, and LAN's
  factory default is a STATIC `192.168.1.1/24` — a subnet this host's
  `virbr0` (`192.168.122.0/24`) cannot route to, so `delonix vm ls` showed
  `<none>` for its IP even though the VM had booted fine. Reconfigured LAN
  to DHCP via the appliance's own console menu (option 2), which put it on
  `192.168.122.103` — reachable. A real deployment giving OPNsense a WAN
  interface (the second NIC) would not hit this; a single-NIC spike does.
- **Username/password Basic Auth is refused** — confirmed empirically, not
  just read from the docs: `curl -u root:opnsense …/api/core/firmware/status`
  → `401 {"status":401,"message":"Authentication Failed"}`. Only a
  generated key/secret pair authenticates.
- **There is no bootstrap-safe way to mint the FIRST API key** — no CLI
  command, no console-menu option, nothing short of the web GUI or a root
  shell. Read the actual source
  (`OPNsense\Auth\FieldTypes\ApiKeyField::add()`,
  `/usr/local/opnsense/mvc/app/models/OPNsense/Auth/FieldTypes/
  ApiKeyField.php`) rather than guessing: `key` and `secret` are each
  `base64(random_bytes(60))`, the secret is shown exactly once, and what
  persists in `config.xml` is `key|crypt(secret, '$6$')` — a newline-joined
  text blob, not per-item XML nodes (`ApiKeyField::setValue` only accepts
  that richer XML shape when convert-importing an existing config). Minted
  one from a root console shell by calling
  `(new OPNsense\Auth\User())->getUserByName("root")->apikeys->add()`
  directly and saving the model — the same call
  `OPNsense\Auth\Api\UserController::addApiKeyAction` makes, just without
  going through HTTP session auth. This is the exact trap this ADR's
  Context section already named for Proxmox tasks, now confirmed for
  OPNsense too: guessing this shape instead of reading the source would
  have produced a client that could not bootstrap its own credential.
- **The key/secret pair authenticates** — confirmed:
  `curl -u "$key:$secret" …/api/core/firmware/status` → `200`, a real
  `CORE_ABI`/`CORE_HASH`/`CORE_NICKNAME` body.
- **`GET firewall/filter/get` and `GET firewall/alias/get` are NOT the same
  shape**, and a client that assumes one schema for both will misparse one
  of them. `filter/get` returns a clean, minimal `{"filter":{"rules":
  {"rule":[]}, "snatrules":…, "npt":…, "onetoone":…}}` — empty arrays,
  easy to walk. `alias/get` returns the full Phalcon form-widget
  representation of EVERY existing alias (including the six built-in ones:
  `__lan_network`, `__lo0_network`, `bogons`, `bogonsv6`, `virusprot`,
  `sshlockout`), keyed by NAME at the top level, where a field like `type`
  or `proto` is not a string but an object listing every possible option
  with a `selected: 0|1` flag — the "shared model classes… will look quite
  similar" the docs' own introduction warns about, and precisely the case
  where it does not hold. Reading an alias needs its own parser; reading a
  filter rule does not.
- **What is still unmeasured**: `add_item`/`add_rule`/`apply` themselves —
  this session's own tool policy classified the POST calls that would
  create an alias or a rule as "Modify Shared Resources" and refused them,
  even against this isolated, purpose-named VM. No workaround was
  attempted; per that refusal's own instructions, this is surfaced to the
  owner rather than bypassed. **Phase 2 cannot start until this finishes**
  — the write-path shape (does `add_item` want the flat form the docs'
  worked example shows, or the verbose form `get` returns back;
  synchronous or polled `apply`) is exactly what Phase 2's client needs and
  is exactly what is not yet known.
- The VM (`opnsense-adr0051-spike`, `192.168.122.103`, root/opnsense,
  LAN reconfigured to DHCP) is left running for whoever continues this —
  tearing it down would throw away the one thing this spike exists to
  produce: a reachable, credentialed, real appliance to measure against.

**Phase 1 — the trait, the registry, the native implementation. Zero
behavior change.** `crates/adapters/delonix-sdn`: `trait GatewayProvider`,
`BackendFactory`/`ReportFactory`/`BackendRegistration`/
`register_gateway_provider`, seeded with one builtin (`"native"`,
`auto_selectable: true`). New `Domain::Network` capability entries for
what a gateway provider answers (NAT, multi-WAN, perimeter filtering, VPN
termination — named precisely once Phase 0's findings are in, not
guessed now). `scripts/arch_fitness.py` gains the
`("dep", "delonix-opnsense", "delonix-sdn")` exception, tagged the same
phase and reason as `delonix-proxmox → delonix-vm`. This phase does not
touch `NetworkPolicy` convergence or any live dataplane behavior — it is
the same kind of pure scaffold P2/P3 already did for `VmBackend` before
`delonix-proxmox` existed.

**Phase 2 — `delonix-opnsense`, built against Phase 0's findings.** The
`Auth::ApiKey { key, secret }` / `Target { base_url, insecure_tls,
ca_cert_pem }` / `Client` shape `delonix-proxmox`/`delonix-truenas` already
establish, an `Error` enum mapped into the ADR-0043 dictionary, and a
`GatewayProvider` impl wrapping `firewall/alias/*` + `firewall/filter/*` +
`apply`. Depends on `delonix-sdn` (Phase 1's exception) for the trait,
`delonix-model`/`delonix-compute` for the rest — the same dependency
footprint `delonix-proxmox` has today.

**Phase 3 — declarative wiring.** How a manifest names "use OPNsense for
this" is deliberately undecided until Phase 0/1 exist: candidates are an
*optional* `spec.provision.opnsense` block on `kind: NetworkPolicy` (the
TrueNAS shape, cheapest, no reconciler change) or a `desired()`/
`converge()` provider selector the stack reconciler dispatches through
(the full `VmBackend` shape, which reopens the ownership/identity problem
`NetworkAccessRule`/`NetworkRoute` already solved for themselves — an
OPNsense alias/rule carries none of this engine's labels). This ADR does
not pick between them yet; Phase 1/2 do not require the answer.

## Alternatives considered

- **Do nothing, stay nftables-only.** Rejected — the owner has a concrete
  need to interface with a real OPNsense appliance.
- **Stand up `delonix-networking` as a new context crate first** (the
  first draft's framing of Option B). Rejected on correction: the
  established, currently-used pattern for exactly this situation
  (`delonix-proxmox → delonix-vm`) is a named exception against an
  adapter, not a new context crate, and duplicating P4b's unfinished
  target shape before it is proven for VM is premature.
- **The TrueNAS-only shape (no registry at all, one client called
  directly from a Kind's `provision` block).** Considered and folded into
  Phase 3 as one of two undecided declarative-wiring candidates rather
  than rejected outright — it is still the cheapest way to reach
  OPNsense from a manifest, independent of whether the *trait* is
  pluggable underneath.
- **Answering `Domain::Firewall`'s existing four capabilities instead of
  adding new ones under `Domain::Network`.** Rejected: those four are
  answered correctly and for free by native nftables already; forcing
  OPNsense to answer them per-container is the shape mismatch this ADR's
  Context section measures.

## Consequences

- OPNsense's *data-plane* placement relative to container traffic remains
  unsolved by this ADR (it cannot boot under Cloud Hypervisor; only
  `libvirt` + `vm bridge`, privileged and `EXPERIMENTAL`, reaches the SDN).
  This ADR is entirely about the *control-plane* interface — configuring
  OPNsense over its REST API — not about wiring its data plane in.
- Phase 1 ships with zero behavior change and no dependency on Phase 0's
  findings beyond the capability names, so it can land and be reviewed on
  its own, the same way `VmBackend`'s scaffold predates any real backend.
- Phase 2 cannot be built responsibly without Phase 0: guessing the
  `apply()` semantics or the write-path JSON shape risks the exact trap
  ADR-0008's own module doc names for Proxmox tasks — treating "staged" as
  "applied." Auth, and the read-path shape of `filter/get`/`alias/get`,
  are now measured (see Phase 0 above, started 2026-09-24); the write path
  (`add_item`/`add_rule`/`apply`) is the piece still blocked on the
  owner's decision about this session's tool policy.
- This decision deliberately rides alongside, not ahead of, P4b's
  `VmBackend`-to-`delonix-compute` migration. If that migration's shape
  changes once it actually lands, `GatewayProvider` moves with it in the
  same commit shape — a second, smaller instance of the same mechanical
  move, not a redesign.
- Phase 3 (declarative wiring) is explicitly left open. Closing it before
  Phase 0/1/2 exist would be designing a manifest field for an appliance
  behavior nobody has measured yet.
