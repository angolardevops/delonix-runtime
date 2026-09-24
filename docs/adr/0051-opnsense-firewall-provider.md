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
- **Write path, completed after the owner cleared this session's tool
  policy.** `add_item`/`add_rule` both want the FLAT shape the docs'
  worked example shows — `{"alias": {"name":…, "type":…, "content":…}}`,
  `{"rule": {"description":…, "source_net":…, "protocol":…,
  "destination_net":…}}` — never the verbose form `get`/`get_item` return
  back; the two are asymmetric by design (write a few fields, read
  everything). Both answer `{"result":"saved","uuid":"<uuid>"}` on
  success — the SAME envelope for an alias and a rule, one shared
  convention across `ApiMutableModelControllerBase` resources.
- **`apply()` is SYNCHRONOUS, confirmed by wall-clock timing, not
  assumed.** `POST firewall/filter/apply` returned in ~0.85s with
  `{"status":"OK\n\n"}` — literally the captured stdout of the underlying
  reload, not a task id. There is no Proxmox-style UPID/polling loop to
  build for OPNsense; a `GatewayProvider::commit()` for this backend can
  just make the call and read the body. `firewall/alias/reconfigure` is
  the alias table's own equivalent (`{"status":"ok"}`), and is a SEPARATE
  call from `filter/apply` — an alias used by a rule needs the alias
  table reconfigured too, not just the filter reloaded, confirmed by
  creating an alias-referencing rule, applying, and reading it back via
  `search_rule` (see below) before either was reconfigured.
- **Validation failures are HTTP 200**, not 400/422 — the docs table's
  own "GET, POST" note for these routes undersold it: the ERROR channel
  is the JSON body, not the status line. A bad `add_rule` answered
  `{"result":"failed","validations":{"rule.protocol":"Option [] not in
  list.","rule.source_net":"not-an-ip is not a valid source IP address or
  alias."}}` — a dict keyed by `<record>.<field>`, at HTTP 200. A client
  that only branches on HTTP status will treat this as success.
- **No credentials at all is HTTP 302 (a redirect), not 401.** Wrong
  Basic-Auth credentials (a real GUI account, `root:opnsense`, used the
  wrong way) DO answer `401 {"status":401,"message":"Authentication
  Failed"}` — measured earlier in this same session. But an entirely
  missing `Authorization` header answers `302`, presumably a redirect
  toward the session-based GUI login this route also serves. Two
  different "you are not authenticated" answers for two different ways
  of not authenticating; ADR-0043's mapping needs both, and neither is
  the generic 401 the docs' own wording implies.
- **An unknown route is a clean 404** (`{"errorMessage":"Endpoint not
  found"}`), and a non-POST request to a POST-only action route is
  **200 with a silent no-op** (`{"result":"failed"}` with no
  `validations`), not a routing error — confirmed by re-running
  `search_rule` immediately after a `GET` to `add_rule` and finding the
  rule count unchanged. Matches the source read earlier
  (`addApiKeyAction`'s own `if ($this->request->isPost())` guard):
  several of these controllers accept the wrong HTTP verb at the
  transport level and refuse it themselves, in the body, not the status.
- **Alias references resolve inline, and `search_rule` proves it**: a
  rule created with `source_net` set to the test alias's NAME (not its
  UUID) came back from `search_rule` carrying
  `"alias_meta_source_net":[{"value":"adr0051spike","isAlias":true,
  "description":"…<br/>10.99.99.99"}]` — the alias's current content,
  denormalized into the rule's own read representation. A `Gateway
  Provider` reading current state back does not need a second call to
  resolve what an alias means at the time a rule was read.
- **Cleaned up after measuring**: the test rule and alias were deleted
  (`del_rule`/`del_item`), `filter/apply` and `alias/reconfigure` run
  again, and `filter/get` confirmed empty
  (`{"filter":{"rules":{"rule":[]},…}}`) — the appliance is back to a
  clean, unmodified firewall config.
- The VM (`opnsense-adr0051-spike`, `192.168.122.103`, root/opnsense,
  LAN reconfigured to DHCP) is left running for whoever continues this —
  tearing it down would throw away the one thing this spike exists to
  produce: a reachable, credentialed, real appliance to measure against.
  **Phase 0 is done**: auth, both read shapes, the write shape, `apply()`
  semantics, and both classes of error are all measured. Phase 2 can
  start from fact, not guess.

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

**Phase 3 — declarative wiring, decided and built.** Neither of the two
candidates above, exactly. A new `kind: NetworkGateway` (`spec.provider`
names a registered `GatewayProvider` by id, `spec.aliases`/`spec.rules`
are the trait's own `GatewayAlias`/`GatewayRule`) rather than a block
bolted onto `NetworkPolicy` — a `NetworkPolicy` already means "the
native nftables answer for this workload," and folding an unrelated
appliance's config into it would be the exact `Domain::Firewall` shape
mismatch this ADR rejects below. And rather than the full `VmBackend`
reconciler-selector shape either: the ownership problem it named —
"an OPNsense alias/rule carries none of this engine's labels" — turned
out to have the same answer `kind: Service` already gave it
(ADR-0032): a Kind with no natural object to stamp gets its **own**
registry (`Presence::Registry`, a `delonix_state::JsonStore` under
`<root>/network-gateways`) instead of trying to carry `delonix.io/stack`
on something external. `stack apply` still dispatches by Kind through
the same `desired()`/`actual()`/`converge_and_stamp` machinery every
other converging Kind uses — just backed by its own store rather than a
target container's labels.

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
  "applied." Phase 0 is now done (2026-09-24): auth (two distinct
  unauthenticated answers, 401 vs. 302), both read shapes, the write
  shape, `apply()`'s synchronous confirm-by-body semantics, and the
  validation/routing error shapes are all measured against the live
  appliance, not assumed.
- This decision deliberately rides alongside, not ahead of, P4b's
  `VmBackend`-to-`delonix-compute` migration. If that migration's shape
  changes once it actually lands, `GatewayProvider` moves with it in the
  same commit shape — a second, smaller instance of the same mechanical
  move, not a redesign.
- **Phase 3 is done (2026-09-25).** `kind: NetworkGateway` exists and
  goes through `stack apply`/`plan`/`destroy` like any other converging
  Kind — see the Phase 3 note above for the shape. Applying ensures
  every alias before any rule; removing (on `--prune` or `destroy`) goes
  rules-first, aliases-last — the order Phase 2's live test proved is
  required, because the appliance validates a rule's `source` against
  its alias table on write, so a rule outliving the alias it names
  fails removal with "not a valid source IP address or alias." A spec
  change has no update-in-place: it is a full remove-then-reapply, the
  same honest "no live update path" `converge_and_stamp` already gives
  several other Kinds rather than pretending to reconcile in place.
- **Phase 2 is done (2026-09-24).** `crates/providers/delonix-opnsense`
  implements `GatewayProvider` for real: `ensure_alias`/`ensure_rule`/
  `remove_alias`/`remove_rule`/`commit`, built exactly to what Phase 0
  measured — the flat write shape, `redirect::Policy::none()` so a 302
  is classified rather than followed, and the `{"result":"failed",
  "validations":{...}}`-at-200 shape read on every 2xx body. One thing
  Phase 0's manual `curl` spike had not exercised and only the Rust
  client caught: `reqwest` does not send `Content-Length: 0` on a
  bodyless POST the way `curl -X POST` (no `-d`) does, and the
  appliance's web server answers `411 Length Required` without it —
  `del_item`/`del_rule`/`apply`/`reconfigure` all needed an explicit
  empty body attached. 16 failure-injection tests against a TLS mock,
  and a `tests/live.rs` run end-to-end against `opnsense-adr0051-spike`
  (create, idempotency-check, commit, remove, commit, and a
  removal-was-real proof), appliance confirmed clean afterward.
  `cmd::gatewayproviders` registers it from the environment, mirroring
  `cmd::vmbackends` exactly.
