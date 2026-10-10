# ADR-0065: IPv6 in the SDN dataplane — one `inet` table, addresses derived from the v4 lease, anti-spoof in the `bridge` family

- **Status:** Accepted (2026-10-10, by the owner); P1–P6 are completion plan Sprint 17. Nothing implemented yet; the spike is done and recorded below.
  The owner decided D2 (prerequisite), D7 (port publishing) and D8 (opt-in) on 2026-10-02.
- **Date:** 2026-10-02
- **Deciders:** Walter Angolar
- **Relates to:** decision D4 of the maturity plan (`docs/discovery/65_PLANO_MATURIDADE.md`,
  PR #658: «IPv6 in the dataplane — implement»); the IPv6 refusal of v0.37.1 (Block 0 of plan 33,
  `AGENTS.md`); the `net.ipv6` cell of the capability matrix (ADR-0050); guard-rails 1, 5 and 6 of
  `delonix-adr` (daemonless, spike before a privilege boundary, no silent failure).
- A Portuguese review copy with the same decisions is `0065-ipv6-dataplane.pt-AO.md`; this file is
  the canonical one.

## Context

### What exists today (read in the code, `origin/main` b269c46e)

- **The whole firewall is `table ip dlxing`** (`ingress_table_ruleset`, `crates/adapters/delonix-sdn/src/infra.rs`):
  `@dlxall`, the `@dlxns<hash>` sets, the verdict map `@fwmap` (`ipv4_addr : verdict`), the chains
  `fwguard` (-20), `fwdeny` (-10), `fwout` (-6), `fwcont` (-5), `dlxinput` and `forward` (0, `policy
  drop`). Each workload chain's body comes from `policy_nft::chain_body`, all of it anchored on
  `ip daddr`/`ip saddr`. There are 59 references to `INGRESS_TABLE` in `infra.rs` alone, nearly all
  with the family argument `"ip"`.
- **IPv6 is refused in two layers** since v0.37.1: `disable_ipv6` in each container's netns
  (`disable_ipv6_argv`, in `do_attach` and `attach-extra`) and `table ip6 dlxing` with `forward
  policy drop` in the holder (`ingress_v6_refusal_ruleset`). `DELONIX_ENABLE_IPV6=1` restores the
  old path, with no policy at all. The reason is measured and written down: the ULA
  `fd00:<o2>::<o3>:<o4>` bypassed every policy, because all of it is `table ip`.
- **The old address scheme is not worth reusing**: `fd00:<group>::/64` with the second v4 octet
  written in DECIMAL inside a hexadecimal field, no random Global ID (RFC 4193 §3.2), and broken for
  CIDR networks (`172.20.4.0/22` has no «second octet» that identifies the network). It also sits
  next to slirp's fixed `fd00::/64` (see S5).
- **v6 leftovers still on with the refusal active** (read, not measured live): `ensure_net_bridge`
  puts `fd00:<group>::1/64` on every bridge and turns on `ipv6/conf/all/forwarding`, and
  `ra_sender_main` (a control-plane thread) sends RAs for that prefix on every bridge,
  unconditionally. Containers do not see them (v6 off); a Cloud Hypervisor VM on the SDN, which does
  not go through `disable_ipv6`, gets a SLAAC address. `dlxinput` is `table ip`: v6 traffic from a VM
  TO the holder has no policy at all. Whether any holder service listens on v6 was not measured.
- **DNS**: `dns_action_owned` answers NODATA to an `AAAA` for one of our names — correct today,
  because «there is no AAAA» is the truth.
- **Control line**: `attach <netns> <ip> <bridge> <gw> [<ns>]`, `attach-extra …`, `vmtap …`;
  `validate_control_tokens` requires strict IPv4 (`control_ipv4_ok`) in every IP.
- **Egress**: the holder's slirp runs without `--enable-ipv6`.

### The spike (2026-10-02, kernel 7.0.0-34-generic, nftables 1.0.9, slirp4netns 1.2.1/libslirp 4.7.0)

All in throwaway namespaces (`unshare --user --map-root-user --net [--mount]`), without root,
without touching the host's sysctls or modules, and without the engine. The scripts and each run's
output are in `docs/adr/0065-ipv6-spike/` (run from that directory, e.g.
`unshare --user --map-root-user --net --mount sh ./s2.sh`; `s4.sh`/`s5.sh` run without `unshare`,
create the namespace themselves, and need a short path for slirp's socket, so they use
`mktemp -d /tmp/dlx65.XXXX` and remove it at the end). The host has `br_netfilter` loaded.

**S1 — `nft` accepts a `table inet` with today's structures, in a userns** (`s1.nft`, rc=0). In
one table: `ipv4_addr` and `ipv6_addr` sets, one verdict map per family (`fwmap4`, `fwmap6`)
jumping to the SAME workload chain, the `netpair` (`ifname . ifname : verdict` with `counter`),
`ct state`, `icmpv6 type …`, `ip6 daddr fe80::/10`, and a `nat` chain with a `masquerade` usable for
both families (`ip6 saddr fd65:1::/64 oifname "tap0" masquerade`). One map per family because a map
has one key type: `ipv4_addr` and `ipv6_addr` do not fit in the same map. The `bridge` family also
loads in a userns, with `icmpv6 taddr` (an NA's target) — the `@th,64,128` form is not accepted by
this version's parser.

**S2 — the `bridge-nf` path for v6 is per netns and on by default** (`s2.sh`). In a new netns,
`/proc/sys/net/bridge/bridge-nf-call-ip6tables` and `-iptables` are **1**, and they can be written
from inside the userns (0 and 1, both «ok»). At 0, the `table inet` does not see traffic between two
ports of the same bridge (all counters at 0; a ping a→b passes). At 1 it does (`a2b-seen: 2`,
`ra-seen: 1`, `nd-seen: 2`). Traffic routed between two bridges is seen in both cases
(`inter-bridge-drop` counts in both). **And with `bridge-nf` at 1 the `iifname` of a bridged packet
is the BRIDGE (`br0`), not the port**: `spoof6-seen-iif-bridge: 2`, `spoof6-seen-iif-veth: 0`.

**S3 — measured consequence for today's IPv4: the per-veth anti-spoof is inert** (`s3.sh`). The
production form (`insert rule ip dlxing fwdeny iifname <veth> ip saddr != <ip> drop`,
`antispoof_rule_args`) counted **0 packets**, and a container forging its source reached a
neighbour on the same bridge (3/3) and a container on another bridge by route (3/3). The two facts
of S2 explain it: for bridged traffic the `iifname` is the bridge, and for routed traffic the packet
enters the IP stack through the bridge too. The same holds for a VM's `tap` (it is a bridge port),
i.e. the `tap` anti-spoof fix of audit #3 is written and does not filter. This finding is about the
IPv4 in production and does not wait for this ADR: the owner decided to fix it now, in a separate
security PR (see D2).

**S6 — namespace isolation works on v6 with the same shape** (`s6.sh`, `bridge-nf` at 1). A
workload chain with the v4 and v6 rules side by side, reached through `fwmap4` and `fwmap6`:

```
v4 a(X)->b(Y): BLOCKED      v6 a(X)->b(Y): BLOCKED
v4 c(Y)->b(Y): OPEN         v6 c(Y)->b(Y): OPEN
v4 b(Y)->a(X) (return via established): OPEN     v6 same: OPEN
ip6 daddr fd65:1::b ip6 saddr @dlxall6 ct state new counter packets 2
```

Neighbour discovery passed without a rule of its own (the NS goes to the solicited-node multicast,
which does not match `ip6 daddr <workload>`).

**S8 — anti-spoof in the `bridge` family works without `br_netfilter`** (`s7.nft` + `s8.sh`, both
`bridge-nf-call-*` at **0**). A `bridge … prerouting` chain with `ifname . ether_addr`,
`ifname . ipv4_addr` and `ifname . ipv6_addr` sets (assigned address + the port's link-local):

```
legit v4 a->b: replies   legit v6 a->b: replies   legit v6 a->holder gw: replies
spoof v4 a->b: no reply  spoof v4 a->holder: no reply
spoof v6 a->b: no reply  spoof v6 a->holder: no reply
b got rogue RA prefix: 0
```

In the `bridge` family's `prerouting` the filter also catches what goes TO the holder (`forward`
would not see it). In S2, without this chain, an RA forged by container `a` gave neighbour `b` a
`fd77::/64` prefix; with it, `ra-drop` counted 1 and `b` got nothing. The forged NA (`icmpv6 taddr`
outside the port's addresses) has its rule loaded and **was not exercised** with traffic.

**S4/S5 — what `slirp4netns --enable-ipv6` gives** (`s4.sh`, `s5.sh`):

- An RA on `tap0` with the **fixed** prefix `fd00::/64` and default `via fe80::2`; the gateway
  `fd00::2` answers ping. The prefix is not configurable in this version (`--cidr` is v4 only; no v6
  prefix option in `--help`).
- With `ipv6/conf/all/forwarding=1` (the holder routes), `tap0` only accepts the RA with
  `accept_ra=2` — measured with 2; the value 1 was not measured, it is the kernel's documented
  behaviour.
- **A source outside `fd00::/64` does not pass through slirp** (`container fd65:1::a -> fd00::2
  WITHOUT nat66: FAIL`); with `oifname "tap0" ip6 saddr … masquerade` in a `table inet` it does
  (`masquerade packets 1`). v6 egress needs NAT66 in the holder, as v4 already masquerades.
- slirp's v6 DNS (`fd00::3`) **did not answer** (timeout); `10.0.2.3` answered an `AAAA` query with
  two answers. The holder's resolver keeps using the v4 path.
- **slirp4netns 1.2.1's API `add_hostfwd` is IPv4 only**: a v6 `guest_addr` and `host_addr` `::1`
  are refused (`bad arguments.guest_addr`, `bad arguments.host_addr`). Publishing a port on v6 on the
  host cannot be done through this slirp.
- **v6 egress to the Internet: not measurable on this machine** — the host has no global v6 address
  and no v6 default route (`ip -6 route` only has `fe80::/64`), and slirp opens its sockets on the
  host side: `tcp6 … 443` gave `Network is unreachable`. The same request on v4 connected.

## Decision

### D1 — One `table inet dlxing`, not a parallel `table ip6`

Policy moves to a `table inet dlxing` that replaces today's two (`ip dlxing` and the `ip6 dlxing`
refusal). Inside it: `@dlxall4`/`@dlxall6`, the namespace sets in pairs
(`dlxns4<hash>`/`dlxns6<hash>`), `@fwmap4`/`@fwmap6` jumping to the same workload chain, and the
chains that do not compare addresses (`fwdeny` with the `netpair`, `forward`, `fwguard`) stay single,
because `ifname`, `ct state` and the `drop` verdict apply to both families.

Reason: the engine has already paid twice for two copies of one format (`fw_rule_tail`, the six
Kind lists). With a parallel `table ip6` every rule would have two generators, and the v0.37.1
bypass would come back the day one of them fell behind. In one table `policy_nft::chain_body` emits,
for each rule, the `ip` line and the `ip6` line in the same place, and the test that already pins
the body pins both.

The `fwmap`/`dlxall` names change (they get a family suffix), and with them the parsers that read
`nft list map ip dlxing fwmap` (`parse_fwmap_elements`, `ingress ls`).

**Migration**: none while live. The table family is decided when the infra netns is BUILT. A new
control plane reattaching to an old pin (table `ip dlxing` present) keeps v4 mode as it is and
**refuses** a network with IPv6, naming the remedy (`delonix net netns down` + `up`, which restarts
the workloads — the operator's decision, the same rule as `stale_holder_message`). Converting the
table under live containers is out: the state of `@fwmap` and the sets would have to be rebuilt from
the records in one transaction, and an error there leaves the node with no policy.

### D2 — Anti-spoof in the `bridge` family; v6 extends the v4 fix that lands separately

**Decided by the owner (2026-10-02):** the inert v4 anti-spoof (S3) is fixed NOW, in a separate
security PR, not inside this ADR. That fix is a prerequisite of P1 and does:

- a `table bridge` with a `prerouting` chain, ahead of everything IP;
- allowed sources PER PORT (`ifname . ether_addr`, `ifname . ipv4_addr` sets), on the three attach
  paths (container veth, extra veth, a VM's `tap`);
- a Kind node may only emit its own address and its own PodCIDR;
- a router container may only emit explicitly authorised prefixes;
- a full opt-out only when an administrator authorises it, and auditable.

v6 **extends the same table and the same sets**, with no second structure: an `ifname . ipv6_addr`
set per port (the address assigned by D4 and the port's link-local), the same exceptions with v6
prefixes (a Kind node's v6 PodCIDR, a router's authorised v6 prefixes), the same administrative
opt-out covering both families at once, and the rules that only exist in v6, measured in
`s7.nft`/`s8.sh`: `ip6 saddr` outside the set (except `::`), `icmpv6 type { nd-router-advert,
nd-redirect }`, an NA with `icmpv6 taddr` outside the port's addresses, and `udp sport 547` (a
container is not a DHCPv6 server). It does not depend on `br_netfilter` (S8). The table and set
names are whatever the security PR sets; this ADR adds elements and rules to them, it does not
rename them.

### D3 — `bridge-nf-call-ip6tables` set by the holder, and verified

Isolation within one network (namespace, `NetworkPolicy`, `Dependency`) is decided in `forward`,
and for bridged traffic that only exists with `bridge-nf-call-ip6tables=1` in the holder's netns
(S2). The holder writes `bridge-nf-call-iptables=1` and `bridge-nf-call-ip6tables=1` in its own
netns when it builds the infra (it is per netns and writable in a userns — measured), and READS
them back. A value that did not stay at 1, or a missing `/proc/sys/net/bridge` (`br_netfilter`
module not loaded on the host), refuses creating a network with IPv6 with class 69 (host
precondition). It is the maturity plan's D5 applied to v6, and stricter: without `bridge-nf`, v6
does not turn on at all, not «on with a warning».

### D4 — v6 addresses derived from the v4 lease: one lease store

- **Node prefix**: a random 40-bit Global ID (RFC 4193), generated once and stored in
  `<state root>/ipam/ula-global-id`. That gives `fdXX:XXXX:XXXX::/48` for the node.
- **Network prefix**: a `/64` inside the `/48`, with a 16-bit subnet id picked at `network create`
  (the first free one) and PERSISTED in `NetDef` (`ipv6_prefix: Option<String>`,
  `#[serde(default)]`; `None` is «network without v6», which is what every existing record means).
  A declared `--ipv6-subnet <prefix>/64` is accepted if it is a ULA or GUA `/64`, does not overlap
  another network on the node and is not `fd00::/64` (slirp's, S4).
- **Workload address**: the network prefix plus the 32 bits of the leased v4 address in the `/64`
  (`10.200.0.7` = `0x0ac80007` → `<prefix>::ac8:7`). The gateway is derived the same way from the v4
  gateway.

Why not a second IPAM: the engine already has a lease store whose reaper only arrived after 88 % of
the entries were orphans (`network ipam prune`, 2026-09-15). A second store would be a second reaper
and a second way for the two to disagree. Derived from v4, the v6 address is born, changes and dies
with the lease that already exists, a fixed `--ip` also gives a fixed v6, and anti-spoof and policy
know the address before the workload starts.

Accepted consequence: **dual-stack** only. A v6-only network is outside this ADR.

### D5 — Static assignment in containers; no RA on networks with policy

At `attach`, v6 is set like v4: `ip -6 addr add … nodad`, `ip -6 route add default via <gw6>`, and
`accept_ra=0` in the container's netns (defence in depth with D2). `ra_sender_main` stops sending
RAs on bridges with policy; and a bridge only gets a v6 address when the network has an
`ipv6_prefix` (today it always does, see Context). VMs come later (P5) with v6 written in the
seed's `network-config`, which already matches the NIC by MAC: a SLAAC address with
privacy/stable-privacy in the guest is not predictable from the host, and anti-spoof would refuse
it. An RA carrying only the default route (A=0) remains an alternative if the `network-config` is
not enough.

### D6 — DNS `AAAA` in the holder

The DNS index keeps, per name, the v4 address and the network's v6 prefix, and `AAAA` answers with
the derived address (D4). A `Service` (ADR-0032) returns one `AAAA` per backend, with the same
rotation and the same namespace isolation as the `A`. One of our names on a network without v6
stays NODATA. Forwarding of external queries keeps the v4 path (S4: `10.0.2.3` answered an `AAAA`,
`fd00::3` did not).

### D7 — Egress: slirp with `--enable-ipv6`, NAT66 in the holder; v6 port publishing by measured capability

The holder's slirp moves to `--enable-ipv6`, `tap0` to `accept_ra=2`, and the `table inet` gets
`oifname "tap0" ip6 saddr <node prefixes> masquerade` (S5: without NAT66 nothing passes). A
container's v6 egress only exists on a host with v6 egress; on a host without it, `connect` gives
`Network is unreachable` — the same as today. Per-network and per-container `egress` (`fwdeny`,
`fwout`) applies to both families through D1's `ip6` lines. `fwguard` gets `fe80::/10`, `::1`,
`fd00::/64` (slirp's network, where its DNS and gateway live) and any v6 metadata address a provider
publishes, at the same priority -20 as v4.

**Port publishing — decided by the owner (2026-10-02).** v6 publishing is restricted on the
`slirp4netns` 1.2.1 backend, because that version's `add_hostfwd` refuses v6 (S4). It is not a
blanket ban: the decision comes from the MEASURED capability of the backend that publishes,
identified by name and version.

- A publishing capability table, per backend and version, where v6 only appears as supported for a
  version where a test proved it (an allowlist, not a denylist). Today it has one entry:
  `slirp4netns` 1.2.1, v4 yes, v6 no. An unknown version counts as «v6 not proven».
- The version is read from the binary the engine will actually use (`slirp4netns --version`), once
  per invocation, and the decision is taken BEFORE anything is created.
- A v6 request (`-p [::1]:8080:80`, `-p [::]:…`, a v6 `--publish-addr`, a port published in a
  `kind: Container`/compose that names a v6 address) on a backend without the capability **fails**
  with an error that names the backend, the version and what is missing. It is never ignored and
  never silently converted into an IPv4 publish.
- A backend that proves support (a newer slirp, `pasta`, another) enters the table with its proof,
  and v6 publishing works on it without changing the surface.
- What a backend supports appears in the capability matrix (ADR-0050) as that provider's v6
  publishing cell, with the reason and the version.

### D8 — Opt-in per network; the default changes only in a major, after the gates

**Decided by the owner (2026-10-02):** IPv6 is opt-in per network, now. `delonix network create
--ipv6` (and `spec.ipv6: true` in `kind: Network`) turns v6 on for that network. Without the flag,
everything stays as today: `disable_ipv6` in the netns, and the `inet` table with a `meta nfproto
ipv6 drop` rule on bridges without `ipv6_prefix` (it replaces `table ip6 dlxing` as the second
layer). `DELONIX_ENABLE_IPV6=1` is removed at the end of the plan: it was the door to the
policy-less path, and with real v6 it has no reason to exist; until then it refuses to be on next to
an `--ipv6` network (the two together would be v6 with and without policy on one node).

Changing the default (the maturity plan's «the current refusal becomes opt-out») is considered only
in a **major release**, with a breaking-change note, and only after ALL of D10's named gates are
green: isolation, DNS, routing, firewall, port publishing and provider compatibility.

### D9 — Control line: a new verb, never an extra token on the old verbs

A workload's v6 goes in its own verb, `addr6 <netns> <ifname> <ip6> <gw6>`, sent after
`attach`/`attach-extra` (and `vmtap6` for VMs in P5). An old holder answers `invalid control
command`: the client undoes the v4 attach it just made and fails with the reason («the control plane
predates IPv6: `delonix net netns down` + `up`»). A workload is never left v4-only on a network
declared `--ipv6`. `validate_control_tokens` gains `control_ipv6_ok` (strict parse, no `%` zone, no
v4-mapped). The existing verbs and the shape of their lines do not change: an old client against a
new holder keeps working (the same rule as the 5/6-token `attach`).

### D10 — The gates: every v4 property proved on v6 too

The `net.ipv6` cell only moves from `unsupported-by-provider` to `supported`, and the default can
only be discussed (D8), when the six gates below exist and are green in a release. Every check sends
packets and measures at the destination or on a counter; reading the ruleset does not count (S3 is
exactly that mistake). Every check must fail with its piece reverted, verified. Battery checks live
in `scripts/e2e.sh`, a new section «network: IPv6», on an `--ipv6` network.

**G1 — Isolation.**
1. same network, same namespace: open on v4 and v6;
2. same network, different namespaces: blocked on v4 and v6, in both directions of initiation; the
   return traffic of an established connection passes;
3. `kind: Dependency` a→b: opens on v6 as on v4, and b→a stays closed;
4. multi-homing (`network connect`): the extra v6 address is under the same firewall and in the same
   namespace set (the regression of sweep #2, now on v6);
5. anti-spoof: a forged v6 source reaches neither a neighbour nor the holder; a forged RA gives a
   neighbour no prefix; an NA with someone else's target does not change the gateway's neighbour
   entry; a Kind node only emits its v6 PodCIDR; a router only its authorised v6 prefixes;
6. network without `--ipv6`: the container has no v6 address at all (not even link-local), and a
   `--privileged` container that turns v6 back on forwards nothing;
7. chaos (`scripts/chaos.sh`, scenario `ipv6_isolation_respawn`): two namespaces on an `--ipv6`
   network, kill the control plane and then the pin, and repeat 1, 2 and 5 after each recovery,
   comparing the workloads' PIDs (like `control_restart`).

**G2 — DNS.**
8. `AAAA` of the workload's own name and of a name in the same namespace;
9. `AAAA` of a name in another namespace: NXDOMAIN; one of our names on a network without v6:
   NODATA;
10. `AAAA` of a `Service`: one record per backend, rotated, with the `A`'s namespace isolation.

**G3 — Routing.**
11. two `--ipv6` networks without `NetworkRoute`: blocked on v6; with it, open; removed, closed;
12. v6 egress through NAT66 on a host with v6 egress (the source the destination sees is the
    holder's); on a host without v6 egress, `Network is unreachable` and not a hang.

**G4 — Firewall.**
13. `ingress policy deny` + `ingress allow <port> --from <v6 cidr>`: only the allowed source passes;
14. `egress policy deny` and `egress deny <v6 cidr>`: v6 egress closes, and the `ip6` rules'
    counters count;
15. `fwguard`: `fe80::`, `::1` and `fd00::/64` not reachable by forward.

**G5 — Port publishing.**
16. on the `slirp4netns` 1.2.1 backend: a v6 publish request fails with the error naming backend
    and version, creates nothing, and no v4 publish appears in its place (`container port` and `ss`
    show nothing);
17. on a backend that declares the capability: the v6 publish answers, and G4's `ingress` rules
    apply to it. Until such a backend exists, this check is SKIP with a reason, and gate G5 is green
    on the proven refusal alone.

**G6 — Provider compatibility.**
18. every network and compute provider in the capability matrix answers the v6 question with a
    measured state: the native SDN `supported`; Cloud Hypervisor VMs on the SDN with v6 and
    anti-spoof (P5); libvirt VMs (on `virbr0`, outside the SDN) and the remote providers with the
    reason written;
19. the CRI (a node with a `kubelet`) and a Kind node on an `--ipv6` network: the pod gets v6 and
    anti-spoof lets the node's v6 PodCIDR through and nothing else;
20. old holder: a `network create --ipv6` against a control plane without the `addr6` verb fails
    loudly and does not leave the workload v4-only;
21. precondition: with `bridge-nf-call-ip6tables` at 0 (forced in the holder's netns) the `--ipv6`
    network is refused with class 69.

## Alternatives considered

- **A `table ip6` parallel to `table ip`.** Cheaper to introduce (does not touch v4). Rejected: two
  generators and two readers for every rule, and the failure of either is v0.37.1's failure again,
  silently. See D1.
- **Converting `ip dlxing` → `inet dlxing` live on reattach.** Rejected: the state of `@fwmap` and
  the sets would have to be rebuilt from the records in one transaction; an error leaves the node
  with no policy, and the case only exists in an in-place upgrade, which has an explicit remedy.
- **Anti-spoof in the `table inet` with `meta iifname`/`physdev`.** `inet` sees the bridge as the
  input interface (S2) and nftables has no `physdev` in the IP families. Rejected by measurement.
- **A second, v6-only IPAM (its own leases).** Rejected: a second reaper, a second source of truth
  (D4). The cost is no v6-only networks.
- **Keeping the `fd00:<o2>::<o3>:<o4>` scheme.** No random Global ID, decimal in a hexadecimal
  field, broken for CIDR networks, and next to slirp's `fd00::/64`. Rejected.
- **SLAAC by RA for containers.** The address is no longer known before start and depends on the
  guest's privacy extensions; anti-spoof would have to learn addresses. Rejected for containers;
  kept as an alternative to D5 for VMs.
- **Stateful DHCPv6 in the holder.** One more server per bridge to hand out an address the engine
  already knows. Rejected for this phase.
- **On by default now.** It is what the maturity plan ends up asking for. Rejected by the owner
  (2026-10-02): the starting point is a measured policy bypass, and the default is considered only in
  a major after D10's six gates (D8).
- **Banning v6 publishing on every backend.** Rejected by the owner: the refusal belongs to the
  measured backend and version, and a backend that proves support offers it (D7).
- **Publishing on IPv4 when the request is v6 and the backend does not support it.** Rejected: it is
  a silent conversion (guard-rail 6).
- **Doing nothing (keep the refusal).** It stays the behaviour for whoever does not pass `--ipv6`.
  As an end state, the owner refused it (D4 of the maturity plan).

## Consequences

- Possible: dual-stack v6 per network, with the same policy, isolation and DNS as v4, and the same
  anti-spoof the security PR puts on v4.
- Harder: every `nft` call in `infra.rs` changes family, and map and set names change — whoever reads
  the holder's ruleset by hand (runbooks, the `ingress ls` doc) has to update them. A node with the
  old pin needs `netns down`/`up` (restarts the workloads) to get v6.
- Accepted debt: no v6-only networks; v6 port publishing refused on `slirp4netns` 1.2.1 until a
  backend proves support; v6 egress depends on the host; VMs after containers; the VXLAN/WireGuard
  overlay between nodes stays v4 (the tunnel); v6 `allowed-ips` over it are not in this ADR.
- `br_netfilter` goes from recommendation to a hard precondition for an `--ipv6` network (D3).
- Matrix: `net.ipv6` in the `linux` column moves to `partial` once P1–P3 are merged (with the
  evidence of G1 checks 1–6) and to `supported` with D10's six gates; `net.nat.npt` stays
  `unsupported-by-provider` (NAT66 by masquerade is not NPTv6), with the reason updated.

## Implementation plan

| Phase | What lands | Files | Proof |
|---|---|---|---|
| **P0** | **Prerequisite, outside this ADR:** the v4 anti-spoof security PR (`table bridge` in prerouting, per-port sources, Kind nodes' PodCIDR, routers' authorised prefixes, an opt-out that is admin-only and auditable). P1 does not start until it is merged | whatever that PR touches (`delonix-sdn/src/infra.rs`: `antispoof_rule_args`, `clear_antispoof`, the three attaches) | that PR's proof; this spike's S3 is the case that must be red on today's code |
| **P1** | `table inet dlxing` with `fwmap4`/`fwmap6`, `dlxall4/6`, `dlxns4/6`; v6 still off; reattach to an `ip`-table pin refuses v6 networks | `infra.rs` (59 `INGRESS_TABLE` sites, `ingress_table_ruleset`, `fw_dispatch_*`, `ns_set_join/leave`, `parse_fwmap_elements`), `policy_nft.rs` (`chain_body` emits `ip` and `ip6`) | the whole v4 network battery with unchanged results; chain-body tests with both families |
| **P2** | Global ID, `NetDef.ipv6_prefix`, `network create --ipv6`/`--ipv6-subnet`, `spec.ipv6`; pure v4→v6 derivation | `delonix-net-rules` (`Cidr6`, derivation), `delonix-sdn/src/infra.rs` (`NetDef`), `ipam.rs` (Global ID), `bins/delonix-runtime-bin/src/cmd/network.rs`, `delonix-stack` (compared field) | pure tests of the derivation and of the overlap/`fd00::/64` refusal; `stack plan` with no drift |
| **P3** | `addr6` verb, `control_ipv6_ok`, `accept_ra=0`, v6/RA/NA anti-spoof in P0's `bridge` table (D2), `bridge-nf` set and verified (D3), end of the unconditional RA and `fd00:` address | `infra.rs` (`validate_control_tokens`, `handle_control`, `do_attach*`, `ensure_net_bridge`, `ra_sender_main`), `cmd/container.rs` (attach rollback) | G1 (1–6), G4, 20, 21 |
| **P4** | DNS `AAAA` (names and `Service`) | `infra.rs` (`dns_action_owned`, `handle_dns`, `build_dns_index`, `dns_resolve*`) | G2 |
| **P5** | Egress: slirp `--enable-ipv6`, `accept_ra=2` on `tap0`, NAT66, v6 `fwguard`; the publishing capability table and D7's refusal; CH VMs with `vmtap6` and v6 in the `network-config` | `infra.rs` (slirp spawn, base ruleset, `vmtap_line`, `parse_publish_addr`/`publish_bind_addr`), `cmd/container.rs`/`cmd/compose.rs` (v6 publish request), `delonix-vm`/`cmd/vm.rs` (seed) | G3, G5, and G6 18 for CH VMs |
| **P6** | D10's six gates complete, including chaos (G1 7) and CRI/Kind (G6 19); matrix `net.ipv6` → `supported`; `DELONIX_ENABLE_IPV6` removed; docs (`AGENTS.md`, `docs/gen.py`) | `scripts/e2e.sh`, `scripts/chaos.sh`, `crates/adapters/delonix-sdn/src/provider_report.rs`, `docs/providers/capability-matrix.md` | the six gates green in a release; only then, in a major, is the default discussed (D8) |

Every phase goes through `delonix-runtime-sec` before merging (guard-rail 5): P1, P3 and P5 touch
isolation boundaries.

## Owner decisions and open questions

Decided on 2026-10-02:

- **v4 anti-spoof (S3):** fixed now, in a separate security PR; it is P0 and a prerequisite of this
  ADR, and D2 extends it to v6.
- **Default:** opt-in per network now; the default is considered only in a major, after D10's six
  gates (D8).
- **v6 port publishing:** restricted on `slirp4netns` 1.2.1 by the measured capability; other
  backends once they prove support; an unsupported request fails with a clear error, never ignored
  and never converted to IPv4 (D7).

Open, with the recommendation:

1. **Dual-stack only (D4)** — acceptable without v6-only networks? Recommendation: yes for this
   phase; a v6-only network needs its own v6 IPAM and is another ADR when someone asks for it.
2. **v6 egress** can only be proved on a host with v6 — does the maturity plan's D1 nightly lab
   have v6 egress? Recommendation: give it v6 egress; without it G3's check 12 is SKIP with a reason
   and G3 cannot be green.
3. **Reattaching to an old pin refuses v6**, and the remedy (`delonix net netns down` + `up`)
   restarts the workloads — acceptable as a one-time upgrade cost? Recommendation: yes; the
   alternative is converting the table under live workloads (rejected in D1).
4. **ADR number:** 0065 was not checked against other open PRs.
