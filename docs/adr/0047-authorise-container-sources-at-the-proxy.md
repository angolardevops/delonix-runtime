# ADR-0047: the L7 proxy authorises container sources, so containers can reach it

## Status

Accepted (2026-10-10, by the owner). The spike is done (2026-10-11, below): **GO for D1, but not
D1 as first written.** The measurements changed three parts of it: how the proxy tells the host
from a container, which checks it repeats, and how it learns which workload the backend is. D1,
D2 and the phases below are the revised ones. Nothing is implemented yet. It follows from
ADR-0046 phase 1b, where `hosts: containers` was built, measured, and withdrawn because of the
control described below.

## Context

`kind: HTTPRoute` publishes a service by name, and a container cannot use it. The proxy listens in the
holder netns; a container on the SDN reaches the holder only through its bridge address, and
`dlxinput` (the holder's input chain) drops that on purpose. The reason is written next to the chain
in `delonix-sdn/src/infra.rs` and was measured (`docs/discovery/46_GAPS_ENCONTRADOS.md` §4.2): a
container that reached the proxy on its gateway was relayed to ANY registered backend, across
namespaces, and past an `ingress policy deny` on the backend, because the proxy→backend leg starts in
the holder and never meets the per-container `fwcont` chain.

So the container-to-route path is closed by a control that is correct, and the only way through is to
make the proxy enforce what the dataplane would have enforced. Opening `dlxinput` on the listener
ports without that is the exploit again. The spike reproduced it on this tree and found it wider than
§4.2 recorded: the proxy also bypasses the path between networks and the CLIENT's own outbound policy
(S3, S7 below).

What ADR-0046 phase 1b already proved: the holder DNS can answer a route host with the bridge address
of the querying client's network (three unit tests, and a container resolved `app.example.pt` to
`10.234.0.1` live). That DNS code was removed from the tree. It was in commit `d7245765`, which no
ref holds any more (checked 2026-10-11: `git for-each-ref --contains d7245765` is empty), so the next
`git gc` can delete it. Its code part is kept as `0047-spike/d7245765-holder-dns.patch`.

## Spike (2026-10-11)

Kernel 7.0.0-38-generic, nftables 1.0.9, slirp4netns 1.2.1, the engine built from `b1eab234`
(`cargo build -p delonix-runtime-bin`, debug). Everything ran in an isolated root
(`DELONIX_ROOT`/`DELONIX_NET_RUNTIME_DIR` under `/tmp/dlx17b`), with its own holder, slirp and proxy.
No engine code was changed. Where the spike needed the D2 opening, it inserted one rule by hand into
the isolated holder's `dlxinput`, in the shape D2 proposes. The scripts and their output are in
`docs/adr/0047-spike/`: `env.sh` pins the isolated root and refuses any other, `s0`–`s7` are the
steps, and `s9-teardown.sh` removes the bench.

The bench (S0): two networks (`netA`, `netB`), two namespaces (`teamA`, `teamB`), and a backend
`be-a` (teamA, netA). The clients cover every combination: `cl-a` (teamA/netA), `cl-b` (teamB/netA),
`cl-a2` (teamA/netB), `cl-b2` (teamB/netB), `cl-m` (teamB, netB plus netA, multi-homed), and a pod
`po` (teamB/netA). A manual `HTTPRoute` on port 18471 sends `Host: app.test` to `be-a:8080`. A
peer probe (`peer_probe.py`) listens on 18472 in the same holder netns. It does the same `accept()`
the proxy does (`let (stream, _peer) = listener.accept()`) and reports the peer address and the local
address.

The host load was 30 to 112 during the runs (another session's writeback, 3–4.6 GB dirty). This
matters for the latency numbers only.

### S1 — today's tree, unchanged

The host reaches the route (`WIN-be-a`). Every container times out on the proxy port of its own
gateway: `dlxinput` counted 24 dropped packets, 4 SYNs from each of 6 clients. Directly, only `cl-a`
reaches `be-a`. The control is closed, as §4.2 left it.

### S2 — the source address the proxy would see

| Client | Dialled | Peer at `accept()` | Local address |
|---|---|---|---|
| host, through `127.0.0.1:18472` | slirp hostfwd | `10.0.2.2` (slirp gateway) | `10.0.2.100` (tap0) |
| host, through `192.168.122.1:18473` | slirp hostfwd | `192.168.122.1` (the host's real address) | `10.0.2.100` |
| `cl-a`, `cl-b` (netA) | netA gateway | own SDN address | `10.251.0.1` |
| pod member `po-c0` | netA gateway | the pod address `10.251.0.2` | `10.251.0.1` |
| `cl-a2` (netB) | netB gateway | own address | `10.230.0.1` |
| `cl-a2` (netB) | **netA's** gateway | own address | `10.251.0.1` |
| `cl-m` (multi-homed) | netB gateway | primary `10.230.108.135` | `10.230.0.1` |
| `cl-m` | netA gateway | **extra** `10.251.108.135` | `10.251.0.1` |

- A container's address reaches the proxy unchanged on a custom bridge, from a pod netns, and from a
  multi-homed container. The address is the one on the network it dialled through.
- The host is NOT always `10.0.2.2`. libslirp replaces only a loopback source. A published port bound
  to a routable address delivers the client's real address. A LAN client numbered inside an SDN range
  would therefore look like a workload if the proxy classified by peer address alone.
- The local address does not tell the client's own network apart: a container may dial any holder
  address, including another bridge's gateway (`cl-a2` → `10.251.0.1`).

### S3 — the exploit through the real proxy (D2 opening only, no D1)

| Case | Direct to `be-a:8080` | Through the proxy |
|---|---|---|
| `cl-a`, teamA/netA (control) | `WIN` | `WIN` |
| `cl-b`, other namespace | timeout | **`WIN`** |
| `po-c0`, pod in another namespace | timeout | **`WIN`** |
| `cl-a2`, same namespace, other network, no route | timeout | **`WIN`** |
| `cl-b2`, other network and namespace | timeout | **`WIN`** |
| `cl-m`, multi-homed, through netA | timeout | **`WIN`** |
| `cl-a`, `be-a` with `ingress policy deny` | timeout | **`WIN`** |
| `cl-a` with its OWN `egress policy deny` | timeout | **`WIN`** |

The counters attribute every direct refusal (S3b, counters zeroed per client):

- `cl-b` and `cl-m`: 4 packets on `be-a`'s namespace rule (`ip saddr @dlxall ct state new … drop`).
- `cl-a2` and `cl-b2`: 4 packets on the inter-bridge drop (`iifname @dlxbr oifname @dlxbr`).
- `ingress policy deny`: 4 packets on `be-a`'s default drop.
- `egress policy deny`: 4 packets on `cl-a`'s `ip saddr … drop`.

Through the proxy, the only counter that moves is the hand-made `dlxinput` accept (5 packets). No
forward-hook rule sees the relayed leg, because it starts in the holder (output, not forward).

What the proxy knows about the backend at request time is the composed config:
`{"host": "app.test", "path": "/", "backend": "10.251.153.125:8080"}`. That is an address, with no
workload identity.

### S4 — mapping the source back to a workload, and trusting it

- **The index D1 named misses addresses.** The holder DNS places a client by `ns_of_ip`, which is built from each container record's primary `ip`. `cl-m` asking through its primary address resolves `cl-b2.teamB.delonix.internal` (`10.230.108.102`). Asking through its extra address gives `NXDOMAIN`: it is unplaceable, so it is scoped to `default`. The dataplane maps both addresses (`fwmap`: `10.230.108.135` and `10.251.108.135` both `jump fw888ae54f`). D1 as first written treats an unplaceable source as the host, so the extra address would have had the host's unrestricted access.
- **The pod address IS placed**: `po-c0` resolves teamB names, because the member record carries the pod address.
- **The source address is trustworthy, and only because of the bridge anti-spoof.** `cl-x` has `NET_ADMIN` and sat in teamB on netA. With its own address it reached the probe. When it sent with `cl-a`'s address, nothing arrived, and `table bridge dlxspoof` counted 3 packets on `ether type ip … drop`. D1 depends on `dlxspoof` staying where it is.

### S5 — cost (8 containers, load 88, so treat as an order of magnitude)

- **Index rebuild.** A DNS query through the real holder server took 3.78 ms median (max 5.99) after 2.5 s idle, when the index is rebuilt from disk. Right after, with the index cached, it took 2.01 ms (max 3.55). The rebuild costs about 1.8 ms for 8 records.
- **Proxy today.** host → proxy → backend took 4.47 ms median, p90 7.96 ms, over 50 requests.
- **Per-request decision.** `delonix_net_rules::policy::evaluate` walks a workload's rule list once. It was not timed on its own (no engine code was written).

### S6 — can a container pass for the host?

A container that dials the holder's tap address (`10.0.2.100`) is delivered on local `10.0.2.100`,
the same local address the host's traffic has (`cl-b`: peer `10.251.212.68`, local
`10.0.2.100:18472`). Neither peer nor local address alone separates the two sides. With the
destination guard below in the D2 rule (`iifname @dlxbr ip daddr != 10.0.2.100 …`), that
connection is dropped (4 packets on the final `ct state new drop`). The bridge-gateway path and the
host path both still work. Behind that rule, local `10.0.2.100` means the connection came in on tap0
(or lo), and a bridge gateway means it came from the SDN.

### S7 — the network path is a third control

`cl-a2` is in `be-a`'s namespace but on netB: refused directly. `network route netB netA` opens it
(`WIN`). `cl-b2` stays refused: the route opens the path, and the namespace still decides.
`network route … --rm` closes it again. A proxy that checks namespace and the backend firewall only
would let `cl-a2` through with no route.

### Mechanisms compared

| Mechanism | What it costs | What it cannot see |
|---|---|---|
| **D1 revised (below)**: the proxy repeats the dataplane's decisions from the persisted records, through the policy IR | an index of every SDN address (≈1.8 ms rebuild per 8 records, per TTL), one `evaluate` per direction per request, the proxy reading the container, VM, network and route stores | changes inside the index TTL; a backend address that no longer belongs to a workload (refused, D5) |
| D1 as first written: namespace (`dns_client_ns`/`allow_in`) plus the backend firewall | as above | the network path (S7), the client's egress policy (S3), the extra addresses of a multi-homed container (S4). `allow_in` is a second, partial reading of the firewall: allows only, no ports, no deny |
| Make the relayed leg meet the dataplane (connect with the client's source address through `IP_TRANSPARENT`, and dispatch `fwmap` in an `output` hook) | policy routing and marks so the backend's reply comes back to the proxy; a second dispatch path in the ruleset | `iifname`/`oifname`, which the inter-bridge and anti-spoof rules depend on, do not exist in `output`. Not measured; rejected on the asymmetric return path alone |
| PROXY protocol | the backend must speak it | It carries the client address to the backend; it authorises nothing. Orthogonal (an `X-Forwarded-For`/`Forwarded` header does the same for HTTP) |
| Per-route allowed sources | a second policy language on the route | the backend's own `policy deny` and the namespaces. As the authority it drifts from `NetworkPolicy`. Acceptable later as an extra narrowing on top of D1, never instead of it |
| Resolve the host straight to the backend (alternative below) | none | TLS termination and path routing. It only works when the route port equals the backend port |

**Conclusion: GO, with D1 revised.** The source address survives on every SDN shape measured. It
is trustworthy because of `dlxspoof`. Everything the dataplane decides for a direct packet can be
recomputed from persisted records through functions that already exist, are pure, and are pinned
against the nft lowering:

- `delonix_networking::policy::from_container_fw` turns a record into the policy IR;
- `delonix_net_rules::policy::evaluate` decides a packet against it;
- the golden table in `delonix-networking` pins the two together, and
  `policy_nft::the_rendered_chain_gives_every_golden_verdict` checks the nft rendering against it.

The binary already depends on `delonix-networking`, `delonix-net-rules` and `delonix-sdn`, so nothing
of the holder enters the bin. Classifying by «known workload or not» is NO-GO: S2, S4 and S6 each
broke it.

## Decision

### D1 — the proxy decides from the connection's two addresses, with the dataplane's own functions

At `accept()` the proxy keeps the peer address and the local address. Today it discards both.

1. **The side comes from the local address, never from the peer.** Local `10.0.2.100` (`SLIRP_IP`) is
   the host side and keeps today's behaviour; that is what an ingress is for. A local address that
   is a bridge gateway is the SDN side. Anything else is refused. This depends on the D2 destination
   guard (S6).
2. **On the SDN side, the peer must be a workload.** The index maps EVERY address the dataplane
   maps: each container's primary `ip` and every `extra_networks` address, the pod address
   (through the member records), and Cloud Hypervisor VM addresses. For each address it keeps the
   workload, its namespace, its labels, its network (bridge) and its persisted `ContainerFw`. It is
   NOT `ns_of_ip` (S4). A peer the index cannot place gets one forced rebuild (the DNS server's
   200 ms rule), then a refusal. It is never treated as the host.
3. **The proxy computes the same decisions the dataplane would take for a direct TCP SYN from the peer
   to the backend's address and port**, in the dataplane's order, and the first refusal wins:
   1. **Path between networks** (`fwdeny`): the same bridge, or a `NetworkRoute` pair from the
      client's network to the backend's (the persisted `RouteDef` registry).
   2. **The client's outbound policy** (`fwout`): `evaluate(egress)`, with the backend as peer.
   3. **The backend's inbound policy** (`fwcont`): `evaluate(ingress)`, with the client as peer.
      Its namespace and labels go into `Packet.peer_namespace` and `peer_labels`.

   Namespace isolation (ADR-0011) is NOT a separate check: it is the guardrail `from_container_fw`
   already appends. That is why the IR is used and not `dns_client_ns`/`allow_in`. Those read only
   the allows, with no ports and no deny, and would be a second implementation.
4. **A refusal is a `403`.** The body names the control that refused: «no path between the networks»,
   «the client's outbound policy», or «the backend's inbound policy». It never names another
   namespace or workload. The full decision (client, backend, rule) goes to the proxy log.

### D2 — open the listener ports to the bridges, and only those

`dlxinput` gains one accept, placed before its final drop:
`iifname @dlxbr ip daddr != <SLIRP_IP> tcp dport @<route ports> accept`.

- **The destination guard is part of the rule, not an option.** Without it, a container dials
  `10.0.2.100` and lands on the host side of D1 (S6).
- **The port set is opt-in.** With no route that opted in, the set is empty and `dlxinput` is exactly
  as it is today.
- **The set is written by the same `rebuild` that owns the composed proxy config**, so the chain and
  the proxy cannot disagree about what is listening. The concern the current comment raises against
  enumerating ports is answered by generating the list from the single owner instead of keeping two.
- **`delonix0` is not in `@dlxbr` today**, so a workload on it does not reach the proxy. That fails
  closed, and is stated rather than fixed here.

### D3 — opt-in, per route

`hosts: [containers]` (ADR-0046 D4) is the switch. A route without it is not reachable from
containers even after D1/D2 land: the set of ports is built only from routes that asked, and a route
that did not ask is refused by D1 for an SDN-side connection regardless of the listener being shared.

### D4 — the holder DNS answers the host

Reinstate the phase 1b DNS answer (`0047-spike/d7245765-holder-dns.patch`): for a client on the SDN,
the host resolves to the bridge address of the client's own network, and only when the route opted
in. AAAA is NODATA. The DNS answer stays a convenience: S2 shows a container can dial any bridge
gateway, so D1 never relies on which gateway was dialled.

### D5 — the route names its backend's workload, and a stale address is refused

The composed config carries an address only (S3). Each route backend gains the identity it was
resolved from (kind, name, namespace). At request time the proxy checks that the backend address
still belongs to that workload in the index. If it does not (the workload is gone, or the address
was leased to another one; this is the §4.1 class), the request from the SDN side is refused, with
no evaluation against whoever holds the address now. The host side keeps today's behaviour, which is
a separate question (§4.1).

## Alternatives considered

- **Resolve the host straight to the backend's address, skipping the proxy.** Traffic then meets
  `fwcont`, so isolation holds without new code. It loses TLS termination and path routing, and only
  works when the route port equals the backend port. Rejected as the general answer; it may still be a
  useful narrow mode for a route with one backend.
- **Leave `dlxinput` as it is and document that containers cannot use routes.** Safe, and it is where
  ADR-0046 stands today. It leaves the route feature unusable for the most obvious consumer.
- **A second proxy per network, outside the holder.** Meets `fwcont` naturally but is a process per
  network, which is the daemon-shaped cost principle 2 asks a justification for.
- **The relayed leg with the client's source address, PROXY protocol, per-route source lists**: see
  the spike's comparison table.

## Consequences

- **The proxy learns to read the stores.** Today it deliberately does not ("the proxy knows no
  containers or stores"); with D1 it reads the workload, firewall, network and route stores. That is
  the cost of D1 and the main reason this is an ADR. The authorisation is computed through the policy
  IR the nft chain is lowered from, so it cannot drift from the chain without a golden test failing.
- **The index and the decision live in `delonix-sdn` and `delonix-networking`, not in the bin.** The
  decision is a pure function over the facts; the bin only wires `accept()` to it.
- **Per-request cost.** One map lookup and at most two `evaluate` calls. The rebuild cost is about
  1.8 ms per 8 records under load (S5). It is paid by the first request after the TTL unless the
  rebuild is moved off the request path. Phase 1 measures it at 100 records before it ends.
- **A namespace, firewall or route change reaches the proxy within the index TTL, not instantly.**
  State it in the docs; do not promise more.
- **D1 inherits the anti-spoof as a dependency.** A change that weakens `dlxspoof` widens the proxy
  too. The S4 forged-source case belongs in the regression gate.

## Phases

1. **D1 and D5, without any `dlxinput` change.**
   - Build: the address index (all SDN addresses, D1.2) and the decision (D1.3) as pure, tested
     functions, and the `accept()` wiring with the `403`.
   - Unit gate: a table test of every S3 row, plus:
     - the S4 multi-homed extra address placed in its namespace;
     - an unplaceable SDN peer refused;
     - local `SLIRP_IP` treated as the host;
     - a stale backend address refused;
     - a `NetworkRoute` pair opening S7's case.

     The test asserts that the decision for a backend record agrees with
     `policy::golden::cases()` cell by cell.
   - Live gate: the S3 matrix, as a check in `scripts/e2e.sh`, in an isolated root with
     `DELONIX_ALLOW_HOLDER_INGRESS=1` (the escape hatch that exists today). Every row that is
     «timeout» directly must be `403` through the proxy. The controls stay `WIN`, and the host stays
     `WIN`. On today's tree that check is red: every row is `WIN`, as S3 measured.
   - Cost: the rebuild at 100 records, measured.
2. **D2 and D3 behind the opt-in, with the nft set.**
   - Unit gate: the ruleset test asserts the accept comes before the drop and carries the destination
     guard.
   - Live gate:
     - a route without `hosts: [containers]` times out from a container, with the drop counter
       moving;
     - a container dialling `10.0.2.100` is dropped (S6);
     - the S3 matrix again, now without the escape hatch.
3. **D4, the DNS answer, and the live proof.** A container resolves the name and gets the response,
   and a container in another namespace gets the `403`.

## Not validated

- **VM sources and VM backends.** A Cloud Hypervisor VM's source address through a tap, and a VM as
  backend, were not measured: no VM was booted in this spike. VMs are in `fwmap` and the DNS index
  reads their records, so D1.2 includes them, but by reading only.
- **macvlan/ipvlan**: not measured, because they are not realised in rootless (no address on the SDN).
- **The `IP_TRANSPARENT` alternative**: rejected by reasoning about the return path, not measured.
- **Scale.** The rebuild cost at more than 8 records, and the cost of `evaluate` on its own: not
  measured.
- **Spike provenance.** The spike's D2 opening was a hand-written rule in an isolated holder, not
  code.
