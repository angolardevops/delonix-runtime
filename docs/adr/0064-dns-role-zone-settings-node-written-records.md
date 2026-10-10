# ADR-0064: The DNS role sets a zone's DNS settings; the node writes the records

- **Status:** Accepted (2026-10-06, by the owner). D1–D5 are implemented in the ADR-0059 F5c PR; D6 is implemented
  (2026-10-09, see «Implementation of D6» below — Sprint 2.4 of `docs/discovery/66_CONTINUITY_PLAN.md`).
- **Date:** 2026-10-02
- **Deciders:** Walter Angolar
- **Relates to:** ADR-0059 (network providers by role; F5c is its DNS slice), ADR-0063 D1 (the
  engine does not create a controller that carries a third-party credential), ADR-0049 D3
  (cluster administration is not the engine's), guard-rail 6 of `delonix-adr` (no silent failure).

## Context

ADR-0059 D1 names `DnsProvider` as the DNS role's port and Proxmox SDN DNS (`powerdns`) as its
first implementation. What that role can mean depends on what the node does, and none of it was
measured when ADR-0059 was written. Everything below was measured on the lab cluster (PVE 9.2.2,
two nodes, 2026-10-02) against a real PowerDNS 4.9.17, or read in the node's code
(`/usr/share/perl5/PVE/…`, PVE 9.2.2). Each fact says which.

### What the node does with a zone's DNS settings

- A zone carries `dns` (a DNS controller id), `dnszone` (the domain) and `reversedns` (the
  controller the PTR records go to). They are staged like every SDN change (measured).
- **The engine cannot write a record through the node.** No API route takes a record. The node
  writes them itself, from the IPAM path (`Network/SDN/Subnets.pm`, read):
  - a **guest** (QEMU or LXC) gets an A and a PTR named after the guest when the IPAM gives it an
    address from a DHCP range (`add_next_free_cidr` returns early unless the zone has both `ipam`
    and `dhcp`, `Network/SDN/Vnets.pm`, read). Measured: a container `f5cguest` on the vnet got
    `f5cguest.f5c.lab A 10.84.0.100` and the PTR; destroying it removed both;
  - a **subnet's gateway** gets `<vnet>-gw.<domain>` A and PTR when the subnet is created, before
    any apply (measured);
  - a **reservation** made through `POST /cluster/sdn/vnets/{vnet}/ips` gets nothing: the API
    passes an empty hostname and the plugin returns early (read and measured).
- Creating a subnet makes the node verify the domain AND a reverse zone it derives, on the DNS
  server; a missing one fails the subnet with the server's 404 (measured: `can't read zone
  10.in-addr.arpa.`). For a private network the reverse zone is fixed per block —
  `10.in-addr.arpa.`, `168.192.in-addr.arpa.`, `16-31.172.in-addr.arpa.` — whatever the prefix
  (read, `Dns/PowerdnsPlugin.pm`). For a public one, `if ($mask <= 24)` is tested before
  `<= 16`, so a /16 is given a /24 zone: an upstream defect, read and not measured.
- `PUT /cluster/sdn/zones/{zone}` with `dnszone` and without `dns` in the same request is refused
  ("dnszone: missing dns server") even when the zone already has a `dns` (measured).
- `GET /cluster/sdn/dns` returns each controller's API key **in clear** (measured). A 400 from
  `POST /cluster/sdn/dns` did not echo the key in the two cases tried (bad URL, bad `ttl`).

### Records the node leaves behind (measured, PVE 9.2.2)

1. **A deleted subnet keeps its gateway's A and PTR.** The subnet plugin's `on_delete_hook` does
   nothing.
2. **A changed gateway keeps the old address in `<vnet>-gw`**: the gateway is re-added under the
   same name (the plugin appends to the rrset) and the old one is deleted without a hostname, so
   only its PTR goes (read, `Network/SDN/SubnetPlugin.pm`).
3. **A renamed guest keeps its old A**, and a destroy afterwards looks the record up under the
   new name, so the old A outlives the guest (measured; a destroy without a rename removed both).

No node API removes these records. Removing them needs the DNS server's own API.

## Decision

### D1 — The role is the zone's settings; the provider's controllers are the administrator's

`kind: NetworkZone` gets `spec.dns: { server, zone, reverseServer? }`. `server` and
`reverseServer` name DNS controllers **the cluster's administrator registered**. The engine never
creates one — it holds a credential to a third-party server, ADR-0063 D1's reasoning for IPAM
controllers. A controller the cluster does not have is refused before any write
(`RemotePrerequisiteMissing`, reason `invalid_intent`, DX-1380). The port is `DnsProvider` with
`controllers`, `prepare_zone` and `observe`; it is served by the provider that serves the zone,
from its own registry (ADR-0059 D1 rule 2).

### D2 — `dns` without a DHCP range is refused

The node registers only guests that get an address from a range, so a zone with `dns` and no
`dhcpRange` would register nobody. Accepting it is the accept-and-ignore failure guard-rail 6
forbids.

### D3 — `dns` is a hot field read from the provider

A change converges live: `prepare_zone` writes only what differs, in the segment transaction,
before the subnets (the node registers a gateway when the subnet is created). The `dns` field a
plan compares is read from the node (the running zone), so settings changed by hand read as a
hot change and the next apply converges them. Every zone of a provider with the role is read,
declared or not, and the plan digest covers that same reading. An apply that changes the
settings of an existing zone says that the records already written stay as they are.

A cold field was rejected: a different domain or reverse server would replace the zone — its
subnets, its IPAM and its reservations — taking every guest's network away to change a setting
the node accepts in place.

### D4 — What the node leaves behind is said out loud

A teardown of a zone with `dns` names each gateway record left on the DNS server
(`<vnet>-gw.<domain> (A <ip>)` and its PTR). The live case asserts the leak; when a PVE release
removes the records, that assertion fails and the warning goes.

### D5 — The engine never holds a controller's credential

The provider reads a controller's id and type and drops the rest of the row. The client's parser
for the SDN DNS and IPAM controller routes never quotes the body in an error (the general parser
quotes 160 characters, which would carry the key). The engine does not reuse the key the node
returns to talk to the DNS server.

### D6 — Cleaning the leaked records (implemented 2026-10-09)

When the engine has to remove what the node leaves (D4), it talks to the DNS server with a
credential the operator gives **the engine** in `providers.yaml` (a `dns:` entry with the
server's URL and a `keyFile`), never the one the node returns. The cleanup removes only
`<vnet>-gw.<domain>` records whose address is the gateway of a subnet this engine owned, and
their PTR. It needs its own live case, including the guest-rename leak (3).

## Alternatives considered

- **A `DnsProvider` that writes records itself (a PowerDNS adapter).** Rejected for the Proxmox
  zone: the node already writes the zone's records through its plugin, and a second writer to the
  same zone makes ownership ambiguous. It stays possible for a segment provider without DNS of
  its own; there is no consumer today.
- **Reusing the key `GET /cluster/sdn/dns` returns.** Rejected: the administrator gave it to the
  node, and an engine that reads a credential off one API to use it on another has widened its
  own reach without anyone deciding so.
- **Records for reservations.** Not possible through the node (the API passes no hostname).
  Revisited with ADR-0063 D3.3 (reservations that name a workload).
- **Hiding the leaked records.** Rejected: a teardown that leaves records in someone's DNS and
  says nothing is guard-rail 6's failure.

## Consequences

- Easier: a zone's guests get A and PTR records with no engine-side DNS code; a hand-made change
  converges on the next apply; the settings change without recreating the zone.
- Harder: every plan of a zone on a provider with the role reads the running zones once more.
- Known limits: the leaked gateway records (D4) are removed only where the operator gave the
  engine a `type: powerdns` entry (D6), and a renamed guest's A is not a gateway record and stays;
  no records for reservations; the reverse zone for a private network is the node's fixed one, not
  configurable; the public-prefix defect is read, not measured; IPv6 is out of scope.

## Implementation of D6 (2026-10-09)

Built and measured on the lab cluster (PVE 9.2.2, two nodes) against the lab's PowerDNS 4.9.17
(`pdnslab`, the controller F5c already registered), through the CLI (`stack apply`/`delete
networkzones` of a `NetworkZone` with `dns:`, a `/24` with a DHCP range) and through
`crates/providers/delonix-powerdns/tests/live.rs`, unless a line says otherwise.

**The pieces.**

- `crates/providers/delonix-powerdns` — a minimal client of the PowerDNS HTTP API: `connect` proves
  the server URL and the key with `GET <server>`; `remove_record(zone, name, type, content)`
  removes ONE record and rewrites the rest of the rrset (`REPLACE` with its TTL) or deletes an
  emptied one — the read-filter-rewrite the node's plugin does itself (`del_a_record`,
  `Dns/PowerdnsPlugin.pm`, read). It creates nothing. Its own error converts into the shared
  class with the ADR-0059 D5 reason: a refused key exits 77, a server that does not answer 69.
- `providers.yaml` gets `type: powerdns` (`url` = the server's `/api/v1/servers/<id>`,
  `controllers` = the cluster's DNS controller ids this server is, `auth.keyFile`, `tls`,
  `allowPlainHttp`). The key is only by file, refused unless only its owner reads it; an inline
  `key:` is refused by name. `http://` is refused unless `allowPlainHttp: true` — PowerDNS's own
  webserver has no TLS, and the key would travel in the clear.
- `delonix_networking::dns::gateway_records` names each record with the content a cleanup must
  match: `<vnet>-gw.<domain>.` A `<gateway>`, and the PTR of the gateway's address pointing back
  at that name, in the reverse zone `DnsProvider::reverse_zone` gives — for Proxmox,
  `delonix_proxmox::node_reverse_zone`, the plugin's `get_reversedns_zone` (read): the fixed zone
  of an RFC 1918 block, the /24 zone of a public prefix up to /24 (the upstream defect mirrored on
  purpose), none past /24.
- `cmd::dns_cleanup` (the composition root) removes them with the entry's credential; a record
  whose controller no entry names, or whose server refuses or does not answer, is NOT removed and
  is said, with the reason. A cleanup never fails the apply or the teardown that called it. What
  could not be removed is kept on the zone's record (`dns_left`) and retried on every apply and on
  the teardown; it is said again only when it moves, or on the teardown.

**The two triggers, and what was measured at each.**

- **A teardown** (`delete networkzones`, `--prune`, `stack destroy`, `--replace`): the gateway
  records of the subnets the teardown deleted — never those of a vnet left as someone else's (by
  the code: such a vnet keeps its subnets; not exercised live).
  Measured: the subnet's delete leaves `v6d-gw.f5c.lab. A 10.87.0.150` and
  `150.0.87.10.in-addr.arpa. PTR v6d-gw.f5c.lab.` (leak 1, again); with the engine's key, both are
  `removed`, and the zone and vnet are gone from the cluster. With no `powerdns` entry, both are
  named `left` (D4) and stay — measured on the same run.
- **A gateway changed in place** (ADR-0063 D2): the old address only. Measured leak 2 on the
  node: `.1 → .254` turns `v6d-gw` into `A 10.87.0.1, 10.87.0.254` and the node removes `.1`'s PTR
  itself (only `254.0.87.10.in-addr.arpa.` remains). So this trigger removes the old A and does
  not name a PTR — naming the PTR «left» there would be false, and the first version did, until
  the run showed it.

**What the live run showed, step by step** (one zone, gateway `.1 → .254 → .200 → .150`):

| step | providers.yaml | engine says | DNS server after |
|---|---|---|---|
| `.1 → .254` | no `powerdns` entry | `v6d-gw … A 10.87.0.1` left, with how to fix | `A .1, .254` |
| `.254 → .200` | entry with a wrong key | `… A 10.87.0.254` left: `refused the API key (401)` | `A .1, .200, .254` |
| `.200 → .150` | entry with the engine's key | `.1`, `.254` (kept from before) and `.200` removed | `A .150` |
| hand-added `.77` in the same rrset | — | — | `A .150, .77` |
| teardown | engine's key | `A .150` and its PTR removed | `A .77` (not the engine's: kept) |

- `stack plan --detailed-exitcode` was 0 after each apply; the zone record's `dns_left` held
  `.1` and `.254` after the third step and was empty after the fourth.
- The key file's content appeared in no output of the run (grepped).
- `delonix-powerdns/tests/live.rs` against the same server: one record of a two-record rrset
  removed and the other kept, a second removal `Absent`, the last removal deletes the rrset, an
  unknown zone `ZoneAbsent`, a wrong key exit 77 without the key in the message.
- Measured on PowerDNS 4.9.17: an unknown zone and a wrong server id answer the same bare
  `404 Not Found`, which is why `connect` checks the server first; the `rrset_name`/`rrset_type`
  filter of `GET …/zones/<zone>` is honoured.

**Leak 3, measured and left out by D6's own rule.** A container created on the vnet got
`dlxd6a.f5c.lab. A 10.87.0.100` and its PTR; `PUT …/lxc/<vmid>/config hostname=dlxd6b` changed
nothing on the DNS server; the destroy removed the PTR and left the A. It is a guest's record,
not `<vnet>-gw`, so the cleanup never touches it: an engine that removed every A pointing into a
subnet it owned would remove records it never wrote (measured: a hand-added `.77` in the gateway's
own rrset survived the teardown).

**Not validated.** A TLS proxy in front of PowerDNS (`https://`, `tls.caFile`) — the lab server
is plain http, so the client's TLS path is the same `reqwest` stack the other providers use, not
measured here; a reverse controller on a different server from the forward one (the lab has one
server; the code looks the PTR's controller up separately); a public prefix (the reverse-zone
rule for it is read, not measured); the race between the cleanup's read and its write with
another writer on the same rrset (the API has no conditional write; the node's plugin has the
same window).
