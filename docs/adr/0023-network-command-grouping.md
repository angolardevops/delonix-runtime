# ADR-0023: `network` and `net` stay two groups, and the line between them is the RESOURCE/PLUMBING one

**Status: Accepted (2026-08-25).** A naming decision, not an implementation one. What it costs is
one alias and two `--help` sentences.

## Decision taken

**Keep both groups. Do not merge them, and do not move anything between them.** Instead, make the
line they already draw legible, because today it is real and invisible:

| | Decision |
|---|---|
| `delonix network` | the **resources a person declares** — the things with a `kind:` and a registry record |
| `delonix net` | the **plumbing that carries them** — netns, flow, the firewall verbs, the proxy, tunnels, boot |
| `network route` | **stays** under `network` (it is a `kind: NetworkRoute` — a resource, not a verb) |
| `net ingress`/`net egress` | **stay** under `net`, and their `--help` gains the sentence below |
| an alias | `delonix net network` → `delonix network`, so the wrong guess lands somewhere |

The v0.30.0 reorganization was a clean break with no aliases, on purpose, and this ADR does not
reopen that. It adds one alias in the direction people actually guess wrong, and nothing else.

## Context — measured on `origin/main` (b465300), 2026-08-25

The two trees today:

```
delonix network   create  rm  route  vlan  describe  inspect  ls  apply  dash  node
delonix net       flow  egress  ingress  l4guard  httproute  tunnel  boot  netns
```

Eighteen leaves across two groups whose names differ by three characters. The obvious reading is
that one is an abbreviation of the other, and it is not.

**The line is already there and it is a good one.** Everything under `network` has a Kind and a
registry record — `Network`, `NetworkRoute`. Everything under `net` is either a verb against the
live dataplane (`flow`, `netns`, `l4guard`, `boot`) or a Kind whose CLI is a verb rather than a
resource (`ingress`, `egress`, `httproute`, `tunnel`). That is the same split the engine makes
everywhere else: `container` is a resource group, `system` is a verb group.

**The two-question design is the reason `route` must not move.** `AGENTS.md` states it and the
chains enforce it:

```
fwdeny (-10)  ← NetworkRoute:   do the two networks have a path?   otherwise: drop
fwcont  (-5)  ← FirewallPolicy: does this workload accept this?    otherwise: drop
```

A route says the packet MAY cross; it never says it is ALLOWED. Filing `route` next to `ingress`
under `net` would put the two answers to two different questions side by side, which is precisely
the merge this engine refuses — the same reason they are two Kinds and not one field.

## Why not merge them into one group

Because the merged group would have eighteen leaves and no internal structure, and the thing a
person is looking for is not "network stuff" — it is either "declare me a network" or "why is this
packet dropped". Those are different tasks with different urgencies, and a flat list serves
neither. The v0.30.0 report that motivated the last reorganization said exactly this about the
26-command root, and the answer then was to group, not to flatten.

## Why not move `ingress`/`egress` under `network`

Tempting, because `FirewallPolicy` is a Kind. Refused for a measured reason: **the CLI verbs and
the Kind are not the same grain.** `net ingress allow <container> <port>` acts on ONE container's
chain; `kind: FirewallPolicy` declares the whole state of one direction. Putting the imperative
verb next to `network create` would suggest they operate on the same kind of thing. They do not,
and the day someone writes `network ingress allow` expecting it to act on a network is the day
this costs more than it saved.

## What actually gets fixed

The confusion is real; it is just not a grouping problem. Two sentences, in the two `--help` texts:

- `delonix network` — "the networks and routes you declare. For the firewall, the proxy and the
  live dataplane, see `delonix net`."
- `delonix net` — "the plumbing that carries the networks: firewall, proxy, tunnels, namespaces.
  To create a network, see `delonix network`."

Plus the alias, because a person who types `delonix net network ls` has guessed wrong in a way that
a bare "unrecognized subcommand" does not help with.

## What this ADR does NOT decide

- **`network node` and `network vlan`.** Both sit under `network` and neither is a Kind — `node`
  is WireGuard key material and `vlan` is tier C of ADR-0013, which is not rootless. They are the
  two entries that genuinely do not fit the rule this ADR just drew, and they deserve their own
  look rather than being tidied away in a naming ADR.
- **Anything about `net flow`.** It is live traffic, it belongs where it is, nobody has confused
  it with anything.

## Consequences

- No breaking change. Every command keeps its path.
- The `--help` of two groups changes, which means two `pt.po` entries.
- The alias is the only new surface, and it points from the wrong guess to the right place rather
  than creating a second way to do the same thing.
