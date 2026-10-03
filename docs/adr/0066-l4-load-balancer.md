# ADR-0066: An L4 load balancer in the engine — a VIP per `Service`, nftables DNAT in the holder, readiness-gated backends

- **Status:** Proposed (2026-10-02). Nothing implemented; the evidence is the spike in
  «Measurements», run in a throwaway unprivileged namespace. The owner's answers of 2026-10-02 are
  recorded as decided (D2, D8, D9, «Owner decisions»).
- **Date:** 2026-10-02
- **Deciders:** Walter Angolar
- **Relates to:** ADR-0032 (extends it: the VIP it deferred), the maturity plan
  (`docs/discovery/65_PLANO_MATURIDADE.md`, decision **D3** approved: «L4 load balancing in the
  engine»; and **D5**, refuse what `br_netfilter` does not enforce), the capability-matrix cells
  `net.lb.l4` and `net.lb.health-check` (today `not-implemented` on the `linux` provider),
  guard-rails 1 (daemonless), 5 (spike before privilege) and 6 (no silent failure) of
  `delonix-adr`.
- **Review copy:** `0066-l4-load-balancer.pt-AO.md` (Portuguese, internal review). This file is
  canonical; the two carry the same decisions.

## Context

ADR-0032 gave `kind: Service` a backend set resolved by DNS (several `A` records, order rotated)
and deferred the VIP until there was a concrete need: a long-lived connection DNS cannot
rebalance, or a client that keeps the address. Plan decision D3 is that need: the owner approved
L4 load balancing **in the engine**, not in an external component. The constraints that do not
move:

- **Daemonless.** No resident process by default; a new daemon needs its own ADR with evidence of
  what the alternative does not solve. The holder (pin + control plane) and slirp are already
  persistent infrastructure, and exist only while there is network work.
- **Rootless.** The dataplane lives in the holder's netns (`unshare --user --net`), nftables only,
  no `CAP_NET_ADMIN` on the host.
- **No consumer.** The engine does not know who asks it for a VIP.

### What already exists (read in the code, `origin/main` `b269c46e`)

1. **An `lbset`/`lbclear` pair with no caller** — `crates/adapters/delonix-sdn/src/infra.rs`:
   `do_lbset`/`do_lbclear` (control-socket verbs) and the public functions `set_service_lb`,
   `set_service_lb_algo`, `clear_service_lb`. `grep` over the workspace: zero callers. Three
   defects, all **by reading**:
   - **They cannot work with the VIP the crate itself computes.** `do_lbset` refuses a VIP
     outside `is_ingress_ip` (the workload space, `10.200`–`10.254`), and
     `delonix_net_rules::service_vip` always returns `10.90.a.b` — outside that space *on
     purpose*, says its doc comment, because a VIP inside the subnet would be delivered directly.
     The pair only accepts the VIP that does not work.
   - **Not atomic.** `do_lbset` calls `do_lbclear` (one `nft list` + one `nft delete` per handle)
     and then an `nft add rule` in another invocation: in between the VIP has no rule, and its
     traffic follows the holder's default route (`tap0`, out).
   - **`numgen inc` by default** — measured below (E10), a per-rule counter restarts at 0 on every
     rewrite and, with frequent rewrites, skews towards the first indices.
2. **`service_vip` (16-bit FNV hash into `10.90.0.0/16`)** — no record, so no collision
   detection. With *k* services the collision probability is ≈ 1 − e^(−k²/2·65536): **7 % at
   100 services, 50 % at 300**. Two services with the same VIP silently swap traffic.
3. **The DNS index** (`build_dns_index`) already resolves each `Service` selector against the live
   containers (same namespace, `matches_labels`), inside the holder's control process, with a 2 s
   TTL.
4. **The health monitor** (`health_monitor_loop`, `bins/delonix-runtime-bin/src/cmd/container.rs`)
   runs in the supervisor every `run -d` already has, executes `--health-cmd` (or the image's
   `HEALTHCHECK`) inside the container, writes `health_state` with `Store::update`, and emits the
   `container/health_status` event **only on transitions**.
5. **IPAM** (`ipam::allocate`/`reserve`/`release`, per prefix, under `IpamLock`) and its reaper
   `reap_orphan_leases`, whose liveness comes from `prune::lease_owners`. A lease keyed by
   something other than a container is reclaimed if `lease_owners` does not know it — the lesson
   already paid for with pods (`pod-<name>`).

## Measurements (spike, 2026-10-02)

Kernel 7.0.0-34, nftables 1.0.9. Everything inside
`unshare --user --map-root-user --net --mount --pid --fork --mount-proc` (which is exactly what the
holder is), without touching the host. Topology: `br0` `10.233.0.1/16` with three backends
(`b1..b3`, `10.233.0.11-13`, a Python TCP server answering `<name> <peer-ip>` per connection and per
line), a client `c1` **on the same bridge** (`10.233.0.50`), a client `c2` on another bridge
(`br1`, `10.234.0.50`, routed through the «holder»), and the VIP `10.90.0.10:80` (a lab address;
see D2). The scripts (`setup.sh`, `backend.py`, `client.py`, `probe.py`, `e0`–`e4.sh`) stayed in the
session scratchpad; the essentials are here.

The rule measured:

```
table ip lb {
  chain pre { type nat hook prerouting priority -100;
    ip daddr 10.90.0.10 tcp dport 80 dnat ip to numgen random mod 3 map { 0 : 10.233.0.11 . 80, 1 : 10.233.0.12 . 80, 2 : 10.233.0.13 . 80 }
  }
  chain out { type nat hook output priority -100;   # the same, for clients in the holder itself
    ...
  }
}
```

| # | What | Result |
|---|---|---|
| E0 | `nft` builds the rule inside the userns | `rc=0`. `nft_numgen` **was not loaded** on the host and the kernel autoloaded it at the userns's request — it stayed loaded; this was the only effect outside the namespace. `jhash` (`nft_hash`) **was deliberately not measured**, to avoid loading another module |
| E1 | `numgen random`, `c2`, 5 × 300 connections | `99/97/104`, `114/86/100`, `112/100/88`, `106/101/93`, `102/88/110` (worst deviation 14 % of 100) |
| E1 | `numgen random`, 3000 connections | `996/975/1029` (±2.5 %) |
| E2 | `numgen inc`, 300 connections | `100/100/100` |
| E3 | Hairpin: client `c1` on the backends' bridge, `bridge-nf-call-iptables=1` | `20/20/20`, and the backend sees the **client's real IP** (`10.233.0.50`) |
| E3 | The same with `bridge-nf-call-iptables=0` | **60/60 `TimeoutError`** (60 s): the backend answers straight over the bridge with its own IP, the client expected the VIP |
| E3 | `=0` + `iifname br0 oifname br0 ct status dnat masquerade` | `20/20/20`, but the backend sees **`10.233.0.1`** (the gateway) — the client's IP is lost |
| E3 | The sysctl is per netns and writable by the userns root | `sysctl -w net.bridge.bridge-nf-call-iptables=0` worked; a fresh netns has `1` |
| E4 | Client in the holder itself (`nat output` chain), no route to the VIP | `Errno 101 Network is unreachable` (the routing decision precedes output NAT) |
| E4 | With a default route (the real holder has one, via `tap0`) | `2/2/2`, source `10.233.0.1` |
| E5 | A `drop` in `forward` (priority −5) on the b1 `daddr` | b1 gets **0**; 20 of 60 connections time out, b2/b3 answer 20/20; rule counter `packets 20` — the per-container filter sees the real backend, **after** the DNAT |
| E6 | Backend b2 dead but still in the map, 300 connections | `ConnectionRefused` 100, b1 100, b3 100 — one third fails |
| E7 | Eject b2: rewrite the rule in one `nft -f` | 18 ms; 300 connections → `150/150`, zero errors. Five rewrites: 26/19/25/23/24 ms (includes starting the `nft` process) |
| E8 | A long connection whose backend leaves the map | Landed on b1; b1 removed; the next 5 lines **on the same connection** answered `b1` (conntrack keeps the DNAT); 200 new connections → `b2:100, b3:100` |
| E9 | Published port: a simulated `tap0` (`10.0.2.100`) with `ip daddr 10.0.2.100 tcp dport 8080 dnat … map` to the same set | `30/30/30` |
| E10 | 50 atomic rewrites (`flush chain svc` + `add rule`, one `nft -f`) **during** 3000 connections, alternating 3 and 2 backends, with `numgen inc` | **zero errors**; `b1:1266, b2:486, b3:1248` — `inc`'s skew under rewrites |
| E11 | One transaction rewriting 200 services × 3 backends (30 KB script) | 44/47/48 ms; 200 rules in the chain |
| E12 | A transaction with an invalid step | `rc=1` and nothing applied (the valid rule in the same script did not land) |
| E13 | TCP readiness probe from the holder netns, 500 ms timeout, 200 rounds | listening backend: 200 × `ready`, < 0.1 ms each; process gone: 200 × `ECONNREFUSED`, < 0.1 ms; port dropped by a filter in the backend's netns: `timeout`, 500 ms each |
| E14 | VIP with **no** DNAT rule (no ready backend), filter `ip daddr <pool> meta l4proto tcp reject with tcp reset` + `reject with icmp type port-unreachable` at forward −20 | TCP: 5/5 `ConnectionRefused` (first one 166 ms, including neighbour resolution); UDP: `ConnectionRefused` in 0 ms |
| E14 | The same without the filter, holder with a default route (a dummy standing in for `tap0`) | client `TimeoutError` after 3 s; the leak counter on the default route: **3 SYN packets left the holder** |
| E15 | A served VIP port next to the reject filter | port 80 (DNATed): 3/3 answered; port 81 (not served): 3/3 `ConnectionRefused` |

What the spike feeds into the decisions: (a) the mechanism works entirely without host privilege;
(b) hairpin **depends** on `br_netfilter` or loses the source IP; (c) a full atomic rewrite costs
tens of milliseconds, so it need not be incremental; (d) `numgen inc` and frequent rewrites do not
mix; (e) a dead backend in the map costs 1/N of the connections, which is why readiness is part of
L4 and not an extra; (f) a VIP with nothing behind it must be refused explicitly, or its traffic
leaves the holder through the default route.

## Owner decisions (2026-10-02)

Recorded as decided, and applied below:

1. **The VIP pool is configurable.** `10.90.0.0/16` appears only as a lab example, never as an
   automatic universal reservation. The configuration and its validation are D2.
2. **`Starting` is out of rotation.** Admission depends on readiness; being alive is not being
   ready (D8).
3. **No probe defined → an explicit minimum criterion**, never «healthy because the process
   started». The TCP readiness probe moves from «conditional phase 4» into the main plan (D8).
4. **No ready backend → predictable unavailability**: TCP reset / ICMP port-unreachable, never
   forwarding to containers still starting (D3).

## Decision

### D1 — Not a new Kind: `kind: Service` gains `spec.type`

```yaml
apiVersion: networking.delonix.io/v1alpha1
kind: Service
metadata: { name: web, namespace: teamA }
spec:
  selector: { matchLabels: { app: web } }
  port: 8080            # the container port AND the VIP port (v1; open question 1)
  type: VirtualIP       # DNS (default) | VirtualIP
  vip: 10.90.4.20       # optional; must be inside the configured pool; without it IPAM picks
  publish: [18080]      # optional; host ports leading to the VIP (D6)
  readiness:            # optional; the TCP check is always on, these are its knobs (D8)
    tcp: { intervalSeconds: 2, timeoutMilliseconds: 500, successThreshold: 1, failureThreshold: 2 }
```

- `type: DNS` (default) is ADR-0032 **byte for byte**: no existing manifest changes.
- `type: VirtualIP`: the name `<svc>.<ns>.delonix.internal` resolves to **one** `A`, the VIP, with
  ADR-0032's namespace rule.
- **Why not a `LoadBalancer` Kind:** selector, namespace, DNS name and `delonix.io/stack` ownership
  would be the same; two Kinds with the same selector are two readings of the same `matchLabels`
  drifting apart — what ADR-0032 already refused for `FirewallPolicy`. `type` changes **how** the
  set is served, not **which** set.
- In the reconciler, `type`, `vip`, `publish`, `selector`, `port` and `readiness` are **hot**
  fields: changing any of them rewrites the dataplane and the record, with no state to lose.
  Changing `vip` says so out loud (whoever kept the old address stops reaching it).

### D2 — The VIP comes from IPAM, from a pool the operator configures

**No default pool.** On a node without one, `type: VirtualIP` is refused with class 69
(`EX_UNAVAILABLE`, a host precondition), and the message names the command that sets it. A
universal reservation would silently take a range away from every host this engine runs on.

**Where it is configured** — a node setting, not a field of each `Service` (every Service on the
node draws from the same address space):

- `delonix network vip-pool set <cidr>[,<cidr>...]` / `clear` / `show`, persisted in
  `<root>/ingress/vip-pool` (one CIDR per line, written atomically) — the same shape as
  `vm default-backend`'s `<root>/vm-default-backend`.
- `DELONIX_SERVICE_VIP_CIDR` (comma list) overrides it for a process (CI, labs). `show` says which
  source is in effect.
- Lab example: `delonix network vip-pool set 10.90.0.0/16`.

**What a pool must be:** IPv4, inside RFC 1918 or `100.64.0.0/10`, prefix between `/16` and `/29`
(a public range would shadow real Internet addresses for every container).

**What it must not overlap, and how each is detected** (checked at `vip-pool set`, at every VIP
allocation or reservation, and on every `stack plan`/`apply` that holds a `VirtualIP` Service):

| Must not overlap | Detected by |
|---|---|
| Declared engine networks | the `NetworkStore` records (`base=`, `cidr=`) and the holder's `NetDef` registry under `<root>/ingress/` — files, no holder needed |
| Pod networks | the CNI configs the CRI reads (the same conf dir `cni::readiness` reads): `ipam.subnet` and every `ipam.ranges[][].subnet`; and each kind-mode cluster's `podSubnet` in `<root>/clusters/<name>/kubeadm.conf` |
| Service subnets | each kind-mode cluster's `serviceSubnet` in the same file; the other Services' VIPs are already excluded by the pool's own IPAM |
| The host's addresses and routes | `ip -4 -o addr show` and `ip -4 route show table all`, run by the CLI in the **host** netns before it talks to the holder; every address and route prefix except the default route. This catches the LAN, `docker0`/`virbr0`, and any VPN that installs routes |
| VPN / overlay ranges | the engine's overlays from the `NetworkStore` (`wg_ip` networks and peer node IPs); host WireGuard interfaces by `ip -4 -o addr show type wireguard` and the routes through them (what `wg-quick` installs for `AllowedIPs`) |
| The holder's own plumbing | the slirp subnet `10.0.2.0/24` and the holder's bridge addresses |

Known limit of the detection, written down: a WireGuard peer whose `AllowedIPs` are **not** routed
(`Table = off`) is invisible — `wg show allowed-ips` needs `CAP_NET_ADMIN` on the host, which the
engine does not have. Kubernetes clusters bootstrapped over SSH (`mode: ssh`) keep their subnets in
the manifest, not on this node; they matter here only if routed here, which the route check sees.

**When an overlap appears later** (a VPN brought up after the VIP was allocated): the next
`plan`/`apply` refuses with a conflict (exit 5) naming the VIP and the overlapping source. The
dataplane is **not** withdrawn silently — removing a VIP under its clients is a decision the
operator makes by moving the pool or the VIP.

**Allocation:** the lease is keyed `svc:<namespace>/<name>`, kept in `ServiceDef.vip`, released on
`delete`/`--prune`/`destroy`. An explicit `spec.vip` is **reserved** (`ipam::reserve`) or refused:
outside the pool → a new DX of class *invalid*; taken by another service → `Conflict` (exit 5).
`prune::lease_owners` counts declared services as owners — without it the IPAM reaper reclaims the
VIP and hands it to the next service.

**Rejected:** the hash-derived VIP (`service_vip`) — 50 % collision at 300 services, with no record
to see it in.

### D3 — Dataplane: a `svc` chain, rewritten whole and atomically; an explicit reject for the rest

- The holder's base ruleset gains `chain svc` (nat, no hook), a `jump svc` in `pre`, and an `out`
  chain (`type nat hook output priority -100; jump svc`) for clients in the holder itself (the L7
  proxy may have a `Service` as a backend). The holder already has a default route through
  `tap0`, which E4 showed is needed.
- Per VIP and port **with at least one ready backend**:
  `ip daddr <vip> tcp dport <port> dnat ip to numgen random mod <N> map { … }` over the ready
  backends only. **`numgen random`, not `inc`**: `random` has no state, `inc` restarts on every
  rewrite (E10). The measured distribution (E1) is a uniform generator's.
- **No ready backend → predictable unavailability.** The VIP has no DNAT rule, and a filter chain
  `vipguard` (`type filter hook forward priority -20` and the same at `output`) rejects every
  address of the configured pools that no DNAT rewrote: `meta l4proto tcp reject with tcp reset`,
  everything else `reject with icmp type port-unreachable`. Measured (E14): the client gets
  `ConnectionRefused` at once, TCP and UDP. **Never** a backend that is still starting, never a
  timeout, and never the default route: without this filter the SYNs leave the holder through
  `tap0` (E14, 3 packets counted) and the client waits for a timeout.
- The same filter rejects a VIP port that is not served (E15), for the same leak reason.
- **The rewrite is always whole**: `flush chain ip dlxing svc` + every rule in one `nft -f` (E10:
  zero errors under traffic; E12: an invalid script applies nothing). No incremental map edits, so
  no `@netpair`-style ordering bugs. Cost: ~50 ms for 200 × 3 (E11).
- TCP only in v1. A UDP VIP needs a readiness signal the TCP probe cannot give (D8), and enters
  when there is a concrete use.

### D4 — Reachability and isolation do not change

DNAT runs at `prerouting`; the per-container chains (`fwout` −6, `fwcont` −5) run at `forward`,
**after**, on the real backend (E5). Namespace isolation, `NetworkPolicy`, `Dependency` and
`NetworkAccessRule` apply as if the client had used the backend's IP. A VIP crosses no boundary:
a client from another namespace reaches the VIP and is refused by each backend's chain, exactly as
it would be at the direct IP.

### D5 — Hairpin requires `br_netfilter`, and is refused without it

With the client on the backends' bridge — the common case — hairpin only works with
`bridge-nf-call-iptables=1` (E3: 60/60 timeouts without it). The alternative, a `masquerade` of
DNAT traffic returning to the same bridge, works but makes the backend see the gateway instead of
the client (E3), taking the source IP away from the application. Decision: **no masquerade**;
`type: VirtualIP` is refused with class 69 when the holder does not have
`/proc/sys/net/bridge/bridge-nf-call-iptables` at `1` — the same precondition and class as plan
D5. The holder sets it to `1` in its own netns (writable by the userns root, E3); it refuses only
when the module is absent from the host.

### D6 — Publishing the VIP on the host: the existing `-p` path

`spec.publish: [<hostPort>]` uses the usual `slirp_add_hostfwd` (bound to `127.0.0.1` by default,
`DELONIX_PUBLISH_ADDR` to widen) and puts in the `svc` chain
`ip daddr 10.0.2.100 tcp dport <hostPort> dnat ip to numgen random mod N map { … }` — the same set,
another entry (E9). **No double DNAT** (to the VIP, then to the backend): a DNAT at `prerouting` is
terminal, so the rule points straight at the backends. With no ready backend the published port
gets the same reject. The source reaches the backend intact for routable clients and as `10.0.2.2`
for loopback ones (the rule already in AGENTS.md).

### D7 — The set stays current without a daemon

- **One function, two consumers:** `service_backends(def, containers)` becomes the only evaluation
  of the selector (same namespace, `matches_labels`, workload live by pid + `starttime`, with an IP
  on a network). The DNS index and the dataplane rewrite use it; readiness (D8) narrows its output
  for the dataplane. There is no second reading of `matchLabels`.
- **Who rewrites:** the holder's control process, in a new `svcsync` verb — the same process that
  already builds the DNS index from the same records. It exists only while the infrastructure
  exists, which is exactly when there are VIPs to serve.
- **When:** (1) `apply`/`delete` of a `Service` (CLI → `svcsync`); (2) at the end of `do_attach` and
  `do_detach` (already inside the holder — a direct call); (3) when the supervisor records a
  container's death, since a death without `detach` would leave the IP in the map; (4) on readiness
  transitions (D8); (5) at control-plane start — covers a control restart and a full rebuild.
- **No periodic reconcile of membership.** A change none of the five paths sees (a record edited
  by hand) waits for the next event. Written down, not hidden.

### D8 — Readiness: what admits a backend into rotation

**Admission = (a) AND (b):**

- **(a) The declared port accepts a TCP connection from inside the holder** — always, for every
  backend, probe or no probe. This is the explicit minimum criterion: it proves something listens
  where the VIP will send the traffic. It is **not** called «healthy»: `get services` reports it as
  `ready (tcp)`, and a container is never reported healthy because its process started.
- **(b) When the container has a `HealthConfig`** (`--health-cmd` or a monitored `HEALTHCHECK`):
  `health_state.health == Healthy`. `Starting` is out; `Unhealthy` is out.

A container without a health probe is admitted on (a) alone; one whose port does not accept is
out, whatever its process state. With nothing admitted, D3's reject applies.

**Who runs (a):** a probe thread in the holder's control process — the process that already runs
the DNS, RA and DHCP servers as threads and lives exactly as long as the infrastructure. It starts
only when at least one `VirtualIP` Service exists. No new process, no lifecycle of its own. It is,
written down plainly, a new **periodic** activity inside an existing resident process; that is
what the owner's decision 3 requires, and it is the least resident way to satisfy it.

- Defaults: every 2 s, 500 ms timeout, 1 success to admit, 2 failures to eject; overridable per
  Service in `spec.readiness.tcp`.
- Probes run in parallel with a cap, so a filtered backend's 500 ms timeout (E13) does not delay
  the others; a refused or accepted connect costs < 0.1 ms (E13).
- State lives in memory. **After a control restart every backend starts NOT ready** and is admitted
  on its first success (fail-closed): the cost is up to one interval of `ConnectionRefused` on the
  VIP — measured in the chaos scenario, not assumed.
- The probe originates in the holder (output hook), so the per-container forward chains do not
  filter it: it measures «the application listens», not «this client is allowed» — which D4 leaves
  to the backend's own chain, unchanged.

**Who runs (b):** the existing supervisor monitor. On a transition, `health_monitor_loop` sends
`svcsync` right after emitting the `health_status` event (best effort: holder down = nothing to
rewrite).

**Open connections to an ejected backend continue** (E8): conntrack keeps the DNAT and an
in-flight request finishes. Conntrack is not flushed on ejection. On a container's death there is
nothing to flush.

| Option evaluated | Cost | Verdict |
|---|---|---|
| Supervisor's health monitor alone | Zero new activity; but a container without a probe would be admitted «because it started», which decision 3 forbids | Kept as (b), not sufficient alone |
| **TCP probe thread in the holder's control process** | A periodic task in an existing resident process; state lost on control restart (fail-closed); tests «listens», not «ready» | **Kept as (a), in the main plan** |
| systemd timer per Service | Depends on a user systemd session; a process per tick; coarse default granularity (`AccuracySec=1min`); different root/rootless paths | Rejected |
| Probe only on events (`stack`/`container`) | No process; a hung backend is never ejected | Rejected as sole mechanism (it is D7) |
| A supervisor process per Service | A daemon per service, with its own lifecycle, pid-identity guard and reaping | Rejected: what guard-rail 1 forbids without proven need |
| Nothing | E6: 1/N of connections refused while a dead backend is in the map | Rejected |

### D9 — The `lbset`/`lbclear` pair and `service_vip` go

`do_lbset`, `do_lbclear`, the socket verbs `lbset`/`lbclear`, `set_service_lb`,
`set_service_lb_algo`, `clear_service_lb`, `delonix_net_rules::service_vip` and
`Error::InvalidLbSpec` are retired. They have no callers, the pair does not accept the VIP the hash
produces, and the rewrite is not atomic. This breaks users of the `delonix-sdn` **library** (the
same note the removal of `Net` left); it goes in the release notes. Whether they go at once or
after one release with `#[deprecated]` is open question 2.

## Alternatives considered

- **DNS only (ADR-0032).** Does not rebalance a long connection nor serve a client that keeps the
  IP, and the approved D3 asks for L4.
- **IPVS.** Needs the `ip_vs` module and, for most of its configuration, `CAP_NET_ADMIN` in the
  initial user namespace; **by reading, not measured**. Adds nothing `numgen` does not give for a
  node's small sets, and puts a second dataplane next to nftables.
- **Userspace proxy** (extending the L7 proxy to TCP). Copies bytes, a resident process per port,
  and the source IP arrives as the proxy's. L7 stays the answer for HTTP; L4 belongs to the kernel.
- **eBPF/XDP.** `CAP_BPF` does not exist in an unprivileged userns; the engine already degrades
  `net flow` for the same reason.
- **`jhash ip saddr` affinity by default.** Proposed as an optional `sessionAffinity: ClientIP`
  field (phase F2b), not the default: it remaps whenever N changes, and was not measured in this
  spike (to avoid loading `nft_hash` on the host).
- **Masquerading hairpin** instead of requiring `br_netfilter` — rejected in D5 (loses the source
  IP, E3).
- **A default pool** (`10.90.0.0/16` reserved everywhere) — rejected by the owner (decision 1).
- **No reject; let an empty VIP fall through** — rejected: E14 measured the leak through the
  default route and the client's timeout.

## Phased plan

| Phase | What | Files |
|---|---|---|
| F1 — model and cleanup | `ServiceDef.{type, vip, publish, readiness}` (`#[serde(default)]`); `network vip-pool` command and `<root>/ingress/vip-pool`; the overlap detection of D2 as a pure function over the collected sources plus a thin collector; VIP allocation in IPAM; `lease_owners` counts services; schema and `explain`; `hot_fields` for `Service`; D9 retirement; new DX codes and `pt.po` | `crates/adapters/delonix-sdn/src/{infra.rs,ipam.rs,cni.rs,error.rs}`, `crates/foundation/delonix-net-rules/src/lib.rs`, `crates/foundation/delonix-model/src/codes.rs`, `bins/delonix-runtime-bin/src/cmd/{service.rs,network.rs,prune.rs,schema.rs}`, `crates/contexts/delonix-stack/src/reconcile.rs`, `docs/schema/v1/delonix.json`, `data/pt.po` |
| F2 — dataplane | `svc`/`out` chains and `vipguard` in the base ruleset (also created by a `svcsync` against an older holder, without duplicating the `jump`); `service_backends` shared with the DNS index; the `svcsync` verb and D7's five triggers; DNS of a `VirtualIP`; `publish` (D6); refusal without `br_netfilter` (D5) | `crates/adapters/delonix-sdn/src/infra.rs`, `bins/delonix-runtime-bin/src/cmd/{service.rs,container.rs}`, `crates/adapters/delonix-linux/src/supervise.rs` |
| F3 — readiness | the TCP probe thread in the control process (D8 a); the health gate (D8 b) and `svcsync` from `health_monitor_loop`; `get/describe services` with `READY/TOTAL`, `ready (tcp)` vs `healthy` per backend, and the reject state | `crates/adapters/delonix-sdn/src/infra.rs`, `bins/delonix-runtime-bin/src/cmd/{container.rs,service.rs}` |

The matrix moves `net.lb.l4` to `partial` at the end of F2 and to `supported` when the checks below
exist and pass; `net.lb.health-check` likewise at the end of F3. The evidence cited is the check's
title, as ADR-0050 requires. **No phase ships a VIP without F3's readiness gate enabled**: F2 alone
behind a hidden flag for the battery, so a released VIP never forwards to a container still
starting.

### Gates

Pure tests (no holder): the `nft` script render (no ready backend → no DNAT and the pool in
`vipguard`; N backends → map of N; `publish` → second rule; names and IPs validated before the
argv); the pool validation against each source of D2 (one fixture per source, including a host
route listing and a WireGuard interface); `service_backends` with readiness (`Starting` out, no
probe → TCP only, namespace); the VIP refusals (outside the pool, duplicate); `lease_owners` with an
`svc:` lease.

Battery (`scripts/e2e.sh`, new section «kind: Service type VirtualIP», isolated root with both
roots):

- «without a VIP pool, type VirtualIP is refused with class 69»;
- «a VIP pool overlapping a declared network, a CNI pod range or a host route is refused, naming
  the source»;
- «a VIP spreads connections over every ready backend» — 300 connections from a container on
  another network, each backend at least 20 %;
- «a client on the backends' network reaches the VIP and the backend sees its IP» (hairpin, D5);
- «the service name resolves to the VIP»;
- «a removed backend (`container rm`) gets no new connections» — zero of 200, no `stack apply`;
- «a backend whose port does not accept stays out of rotation» (no probe defined: the TCP minimum);
- «a starting backend gets no traffic until its health probe passes» (`--health-cmd` reading a
  file; `Starting` → out, file present → in, file removed → out);
- «a VIP with no ready backend refuses at once» (`ConnectionRefused` in < 1 s, nothing on the
  holder's default route);
- «an unserved port of the VIP is refused»;
- «a client from another namespace does not get past the VIP» (D4);
- «the published port leads to the same set»;
- «without `br_netfilter` type VirtualIP is refused with class 69» (only where the module is
  missing; otherwise an audible `SKIP`);
- «the pool refuses a duplicate VIP (exit 5) and one outside it».

Chaos (`scripts/chaos.sh`, new scenario `service_lb`): three backends and a client in a continuous
loop; (1) `kill -9` of a backend with `--restart no` — the client sees errors at most until the
supervisor records the death, then zero; (2) a failing health probe on a second backend — ejected
within `interval × retries` + 2 s; (3) a backend that closes its port while its process stays up —
ejected within the TCP probe's `failureThreshold × interval` + 1 s; (4) `kill -9` of the holder's
control plane — the pin keeps the ruleset, traffic **does not stop** for the backends already in
the map until the restarted control rewrites; the scenario measures the refusals in the window when
every backend restarts not ready; (5) `netns down`/`up` (the pin dies) — the VIP comes back with
the same address (from the record). The scenario compares per-backend counts, not just «the client
got answers», because a VIP that served one backend would answer too.

## Consequences

- `Service` has two ways of being served under one Kind; the default does not change.
- The holder gains a responsibility (rewriting the `svc` chain) and a periodic task (the TCP
  readiness probe), with no new process.
- A node without a configured pool has no VIPs, by design; the error says how to configure one.
- The pool's overlap check depends on what the engine can see without host privilege; the
  unrouted-WireGuard gap is documented.
- Ejection speed is the probe's: 2 × 2 s for the TCP check by default, `interval × retries` for a
  health command (Docker's defaults: 90 s; the `Service` documentation recommends short values).
- A public API of the `delonix-sdn` library goes (D9).
- IPv6 is out (plan D4 handles `table inet`); the VIP is IPv4 like the rest of the dataplane.

## Open questions for the owner

1. **A single `port` in v1, or `ports: [{port, targetPort, protocol}]` straight away?**
   Recommendation: the single `port` in v1 (one rule, one probe target, the battery above), with
   `ports[]` in its own phase — the reject, the readiness state and the publish all become per
   port, and that deserves its own measurements.
2. **D9 — remove `set_service_lb*`/`service_vip` at once, or one release with `#[deprecated]`?**
   Recommendation: remove at once. They have no caller in the workspace and cannot work (the pair
   refuses the hash's VIP), so a deprecation period would only keep a broken API visible; the break
   is named in the release notes.
