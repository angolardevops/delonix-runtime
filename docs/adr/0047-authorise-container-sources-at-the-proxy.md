# ADR-0047: the L7 proxy authorises container sources, so containers can reach it

## Status

Proposed. Nothing is implemented. It follows from ADR-0046 phase 1b, where `hosts: containers` was
built, measured, and withdrawn because of the control described below.

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
ports without that is the exploit again.

What ADR-0046 phase 1b already proved: the holder DNS can answer a route host with the bridge address
of the querying client's network (three unit tests, and a container resolved `app.example.pt` to
`10.234.0.1` live). The DNS half is done and was removed from the tree; it is in commit `d7245765`.
What is missing is only the authorisation.

## Decision

### D1 — the proxy decides by the SOURCE address of the connection

A container reaching the proxy on its bridge address arrives with its own SDN address as the peer, not
the slirp gateway, so the proxy can tell who is asking. For each request from a source that is a
known workload, the proxy applies the SAME rules the dataplane applies to a direct connection, using
the SAME functions, not a second implementation:

1. **Namespace (ADR-0011).** The client may reach a backend in its own namespace or in `default`. A
   backend in another namespace is refused unless an explicit inbound allow opened it. This is the
   rule the DNS already applies (`dns_client_ns` / `allow_in`), so name resolution and reachability
   cannot disagree.
2. **The backend's inbound firewall.** The client address is evaluated against the backend's
   persisted `ContainerFw` ingress rules: `default: deny` plus the allows, exactly as `fwcont` would.
   The proxy reads the persisted firewall, not a copy.

A refusal is a `403` with the reason, not a connection drop, and the reason names the rule.

Sources that are NOT a known workload (the slirp gateway, i.e. traffic that came from the host through
`tap0`) keep today's behaviour unchanged: the host reaching the proxy is what an ingress is for.

### D2 — open the listener ports to the bridges, and only those

`dlxinput` gains an allow for the proxy's TCP listener ports on `iifname @DLXBR_SET`, populated from
the composed proxy config (an nft set of ports, rewritten when `rebuild` changes the listeners). It is
NOT open by default: with no route that opted in, the set is empty and `dlxinput` is exactly as it is
today. The port set is written by the same `rebuild` that owns the config, so the chain and the proxy
cannot disagree about what is listening — the concern the current comment raises against enumerating
ports, answered by generating the list from the single owner instead of keeping two.

### D3 — opt-in, per route

`hosts: [containers]` (ADR-0046 D4) is the switch. A route without it is not reachable from
containers even after D1/D2 land: the set of ports is built only from routes that asked, and a route
that did not ask is refused by D1 for a container source regardless of the listener being shared.

### D4 — the holder DNS answers the host

Reinstate the phase 1b DNS answer (commit `d7245765`): the host resolves, for a client on the SDN, to
the bridge address of the client's own network, and only when the route opted in. AAAA is NODATA.

## Alternatives considered

- **Resolve the host straight to the backend's address, skipping the proxy.** Traffic then meets
  `fwcont`, so isolation holds without new code. It loses TLS termination and path routing, and only
  works when the route port equals the backend port. Rejected as the general answer; it may still be a
  useful narrow mode for a route with one backend.
- **Leave `dlxinput` as it is and document that containers cannot use routes.** Safe, and it is where
  ADR-0046 stands today. It leaves the route feature unusable for the most obvious consumer.
- **A second proxy per network, outside the holder.** Meets `fwcont` naturally but is a process per
  network, which is the daemon-shaped cost principle 2 asks a justification for.

## Consequences

- The proxy learns to read the workload and firewall stores, which today it deliberately does not
  ("the proxy knows no containers or stores"). That is the cost of D1 and the main reason this is an
  ADR: the authorisation has to be computed from the same source as the dataplane, or it drifts.
  The decision is to share the functions in `delonix-sdn`, not to copy them into the bin.
- Per-request cost: one index lookup (the DNS index is already cached with a 2 s TTL) and one firewall
  evaluation. To be measured before the phase ends, not assumed.
- A namespace or firewall change is visible to the proxy within the index TTL, not instantly. State
  it in the docs; do not promise more.

## Phases

1. **Spike, measure first.** Reproduce the exploit with a container against the current proxy (it is
   already measured in the discovery notes; reproduce it on this tree). Then implement D1 alone and show
   it refuses the three measured cases: cross-namespace, backend `policy deny`, and an unregistered
   backend. No `dlxinput` change yet.
2. D2 and D3 behind the opt-in, with the nft set.
3. D4, the DNS answer, and the live proof: a container resolves the name and gets the response, and a
   container in another namespace gets the 403.

## Not validated

Everything above is design. In particular: that a container's source address survives to the proxy
unchanged on every network shape (custom bridge, pod netns, macvlan-like); that the firewall evaluation
can be shared without pulling the whole holder into the bin; and the per-request cost.
