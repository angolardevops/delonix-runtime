# ADR-0013: Routed topologies — external gateway/DNS, subnets and VLANs, without leaving rootless

**Status: Accepted (2026-08-12).** Nothing here is implemented yet. It exists so the first line of
code is written against a decided boundary rather than discovered halfway. Tier B's GO/NO-GO spike
has been run — see "Spike result" — and its answer changed what tier B is.

## Decision taken

Split the request into three tiers by the PRIVILEGE each one needs, and ship them in that order,
because the first is free, the second turned out to be an exemption rather than a dataplane (the
spike below), and the third is not rootless at all:

| Tier | What it buys | Privilege | Verdict |
|---|---|---|---|
| **A — the address space becomes real** | arbitrary CIDRs, several subnets, a declared gateway/DNS | none (rootless) | **GO**, unblocked |
| **B — routing between networks** | subnet ↔ subnet, namespace ↔ namespace through a declared path | none (all inside the holder's netns) | **GO** — spike done, see below |
| **C — 802.1Q VLANs on a physical NIC** | trunk to a real switch, `macvlan`/`ipvlan` realized | `CAP_NET_ADMIN` in the **host's** init-netns | **opt-in privileged**, never the default |

Tier C is the one that cannot be made rootless, and saying so up front is the point of this ADR.

## Context — five things measured, not assumed (2026-08-12)

1. **A network's registry entry is ONE OCTET.** `~/.local/share/delonix/networks/kaeso-net`
   contains the single line `210`. Everything else — the bridge name, the `10.<n>.0.0/16` space,
   the `.0.1` gateway, the IPAM range — is DERIVED from it. There is no CIDR stored anywhere.
2. **`--subnet` only started meaning anything in v0.47.0**, and only to pick that octet: the
   accepted space is `10.<200-254>.0.0/16` and nothing else. `172.20.0.0/16` or a `/20` are
   refused, naming the form that works.
3. **`--gateway` is already fail-closed**, and correctly: a bridge network's gateway is the first
   address of its subnet, and passing a different one is refused rather than ignored. So
   "point this network at MY gateway" is **new semantics**, not the relaxation of a check.
4. **DNS is a thread inside the holder** (`infra::dns_server_main`), answering A records for
   `<name>.<ns>.delonix.internal` and forwarding the rest. There is no per-network resolver, no
   forwarder list, and no way to say "this subnet resolves against 10.0.0.53".
5. **There is no 802.1Q anywhere in the tree.** `macvlan`/`ipvlan` exist in the declarative store
   and `network create` WARNS that they were not realized (`Realized=False`,
   `reason=DriverNotImplemented`) rather than pretending. The reason is privilege, not missing
   code, and that distinction is why tier C is separate.

And one fact about the layer above: cross-namespace isolation today is **nftables chains
per workload** (`fwcont`, verdict map by IP). It decides whether a packet is allowed. It does not
route, and it has no notion of a path between two subnets.

## Decision

### Tier A — the address space becomes real

The registry stops being an octet and becomes a record with a CIDR. This is the change everything
else waits on, and it is the one with no privilege question at all: the holder already owns its
netns, and giving its bridge `172.20.4.0/22` instead of `10.210.0.0/16` needs nothing it does not
already have.

* `spec.subnet` accepts any private CIDR that does not overlap an existing network, `/16` to `/28`.
* `spec.gateway` may name an address inside that subnet that is NOT the derived `.1` — which is
  what "an external gateway" means for a rootless SDN: the holder keeps routing, and the named
  address is where it forwards to.
* `spec.dnsServers: []` — the resolvers handed to workloads on this network, replacing the
  holder's forwarder for them. The internal `.delonix.internal` zone keeps being answered locally;
  the ADR's rule is that a declared external resolver **adds** a forwarder and never removes the
  internal zone, because losing service discovery is not a networking option.
* **Migration is the hard part, and it is decided:** a record holding a bare octet is read as
  `10.<octet>.0.0/16` — which is exactly what it always meant — and rewritten on the next write.
  No flag day. This is the same promotion the `base=<n>` line already does.

### Tier B — routing between networks

A `kind: Network` gains no routing field. Instead a new document type describes the PATH, because
a route is a relationship and belongs to neither end:

```yaml
kind: NetworkRoute        # name provisional
spec:
  from: frontend          # network or namespace
  to: backend
  via: gateway-vm         # optional: a workload that forwards (a firewall appliance)
```

Rationale: putting `routes:` inside a `kind: Network` makes the same relationship expressible from
both sides, and two documents that disagree about one route is the class of bug this repo already
paid for with `FirewallPolicy` (two policies for the same target/direction are REFUSED for exactly
this reason).

**Composition with what exists is the whole risk**, and the rule is: routing decides where a packet
MAY go, the existing `fwcont` chains decide whether it goes. A route never implies an allow. A
namespace boundary crossed by a route still needs a `kind: Dependency` or an explicit policy — the
same way a container on a shared bridge is still isolated by namespace today.

**The spike this gated on has been run** — see "Spike result" below. Its answer is that the
forwarding ALREADY exists and an explicit pairwise drop is what closes it, so tier B never needed
the dataplane this section was written to justify. What is left of the design is the document shape
above and the composition rule; the mechanism it compiles to is one line of `nft`.

### Tier C — 802.1Q and physical NICs

`kind: Network` gains `vlan: <1-4094>` and `parent: <nic>`, realized as `ip link add link <nic>
name <nic>.<vlan> type vlan`. **This needs `CAP_NET_ADMIN` in the host's init-netns and there is no
rootless path to it** — the same wall `macvlan`/`ipvlan` already hit, and the same one `vm bridge`
already crossed deliberately.

So it follows the `vm bridge` precedent exactly, and that precedent is the decision:

* a separate, explicitly privileged command — never a flag that silently escalates;
* **dry-run by default**, printing the plan; `--apply` to execute;
* refuses under an unprivileged uid with the reason, rather than degrading;
* a `delonix-runtime-sec` pass before merge, because it puts the host's physical NIC in reach.

## Spike result (2026-08-12) — tier B is a **GO**, and smaller than this ADR assumed

Run rootless on a live host, against two throwaway bridge networks (`10.252.0.0/16` and
`10.244.0.0/16`) with one container on each, alongside the host's production workloads.

**Isolation between networks is not the absence of a route. It is an explicit pairwise drop.**
Inside the holder's netns:

* `ip_forward` is **1**;
* the routing table already has `10.252.0.0/16 dev dlxn…` and `10.244.0.0/16 dev dlxn…`;
* every `forward` chain (`fwguard` -20, `fwdeny` -10, `fwcont` -5, policy 0) is `policy accept`;
* and there is one `iifname "<bridgeA>" oifname "<bridgeB>" drop` per ORDERED PAIR of bridges.

The blocker was isolated without mutating anything, by comparing two destinations from the same
container: A → **B's gateway** (`10.244.0.1`, which is the holder itself, so an `input` path)
answered with **0% loss**, while A → **a container on B** (a `forward` path between the two
bridges) lost **100%**. Routing works; the pair is dropped.

**Consequence for tier B: opening a path is EXEMPTING a pair, not building a dataplane.** No new
forwarding, no privilege, no second mechanism — which is why tier B stays rootless and does not
descend to tier C. The `kind: NetworkRoute` document compiles to «do not install (or bypass) the
drop for this ordered pair», and the composition rule already stated holds unchanged: the
`fwcont` chains still decide whether the packet is allowed.

**Two defects found on the way, both worth their own fix and neither blocking:**

1. **The pairwise drops carry no `counter`.** Every other rule this engine emits does — that was
   the point of `fw_rule_tail` and of the PACKETS/BYTES columns in `ingress ls`. Here the one
   question worth asking, «did this pair ever try to talk», cannot be answered at all. It is also
   why the spike had to prove the blocker by comparing two destinations instead of just reading a
   number.
2. **The rules are O(n²) and unmanaged.** Measured on this host: **8 bridges, 73 rules**. Every new
   network rewrites a mesh against every existing one. This is the same shape the per-container
   dispatch had before it became a verdict map (`@fwmap`, 2 rules regardless of container count),
   and the same fix applies — an `nft` set/map keyed on the interface pair. Tier B should land on
   top of that, not on top of the mesh, or it will be adding exceptions to a structure that is
   already the wrong one.

## Alternatives considered

**Give the holder `CAP_NET_ADMIN` on the host and do everything in one tier.** Rejected: it makes
the engine privileged by default to serve the minority of installs that need a trunk port, and
the daemonless/rootless default is the product.

**Model VLANs as another `driver:` value on `kind: Network`.** Rejected: `driver` already means
"how the dataplane is built" (bridge/macvlan/ipvlan/overlay), and a VLAN is orthogonal — an
802.1Q tag on a bridge network is a coherent thing to want. `vlan:` is a property, not a driver.

**Keep the octet and add a CIDR beside it.** Rejected for the reason this repo keeps rediscovering:
two fields that must agree eventually disagree. The octet becomes derived from the CIDR, or it goes.

## Consequences

* The IPAM stops being arithmetic on one octet and becomes allocation inside an arbitrary prefix —
  including overlap detection between networks, which does not exist today.
* `network ls`/`inspect` gain CIDR, gateway and resolvers; the JSON contract only ever ADDS fields
  (ADR-0005), so this is additive.
* The reconciler already compares `subnet` (`RECONCILED_NETWORK_FIELDS`), so a widened subnet field
  is comparable on day one — but changing a live network's CIDR is a COLD field and must land in
  the recreate path, never a silent in-place edit.
* Tier C splits the product's promise: most of `delonix network` stays rootless, one command is not.
  That must be visible in `--help` and in `docs/cli-stability.md`, not discovered at runtime.

## What this ADR does NOT decide

The `via:` forwarding appliance. Sending a subnet's traffic through a workload (an OPNsense VM, a
firewall container) changes where masquerade happens — today everything leaves under one
`oifname "tap0"` masquerade, and a gateway appliance that sees every packet with a single source
address is a gateway whose own per-source rules are useless. That is its own ADR, and it depends
on tier B being real first.

Also undecided: IPv6. The engine currently DISABLES it per container by design (v0.37.1, it was a
complete bypass of the policy model). A routed topology is where v6 stops being avoidable, and
re-enabling it is a security decision, not a networking one.


## Onde isto está (2026-08-12) e o que falta para «tudo funcional»

Implementado nesta série, com validação ao vivo em root isolado:

| | estado |
|---|---|
| isolamento entre redes (set + verdict map, com contador) | **feito** — era uma malha O(n²) sem contadores; medido, 8 bridges → 73 regras → **2** |
| camada B — `network route` + `kind: NetworkRoute` | **feito** — dirigida, e o retorno flui por `established` |
| camada C — `network vlan` (802.1Q) | **feito**, privilegiado e contido; falta uma corrida real com `sudo` |
| camada A — o tipo `Cidr` e a sua aritmética | **fundação feita**, IPAM por ligar |

Falta exactamente isto, e nada mais, para o que o utilizador chamou «tudo
funcional a nível de rede»:

### 1. Ligar o IPAM ao `Cidr` (camada A, 2.ª fatia) — rootless

O tipo existe e está provado; o que ainda recebe uma string `"10.X"` e assume um
`/16` são três funções (`derive_ip_in`, `valid_ip_in_subnet`, `probe_free`) e o
registo, que continua a guardar um octeto. Enquanto isso não mudar, `--subnet`
só escolhe QUAL `10.<200-254>.0.0/16`, e `--gateway` só aceita o derivado.

**É a única peça que destrava CIDR e gateway à escolha**, e é a mais perigosa das
que restam: um erro de máscara aqui não dá um erro, dá containers com endereços
sobrepostos. Merece a sessão inteira, com o laboratório isolado que esta série
deixou a funcionar.

### 2. Realizar o `macvlan` (camada C) — privilegiado

«Uma rede que recebe IP da rede do host por DHCP e serve de gateway às VMs e
containers» tem nome nesta base: é o driver `macvlan`. Hoje é REGISTADO e o
`network create` avisa alto que **não foi realizado**
(`Realized=False reason=DriverNotImplemented`) em vez de fingir — medido a
2026-08-12. Bate na mesma parede que a VLAN: `CAP_NET_ADMIN` na init-netns do
host.

Portanto entra pelo MESMO caminho que o `network vlan` já abriu — comando
privilegiado à parte, dry-run por omissão, recusa clara sem root — e o trabalho
é o plano `ip link add … type macvlan` mais o encaminhamento que a torna gateway
das redes internas. O `network vlan` é o precedente pronto a copiar; não há
modelo de privilégio novo a inventar.

**Aviso que já está no código e tem de continuar visível**: uma rede macvlan põe
os containers DIRECTAMENTE na LAN física, FORA da firewall, do anti-spoof e do
isolamento deste motor. Realizá-la não pode calar esse aviso.

### 3. O que continua fora, por decisão e não por esquecimento

**IPv6.** Desligado de propósito desde a v0.37.1 — a SDN dava ULA a cada
container e a firewall inteira é `table ip`, ou seja um segundo caminho de dados
sem política nenhuma, que contornava `ingress`/`egress`, isolamento de namespace
e `kind: Dependency`. Foi medido na altura: com a firewall a negar em IPv4, o
mesmo alvo respondia pela ULA. Reactivá-lo é trabalho de segurança com o seu
próprio ADR, não o alargamento de um campo.

**O `via:`** (mandar uma subnet por um appliance) continua onde este ADR já o
deixou: depende de mover o masquerade para o appliance, senão ele vê todos os
pacotes com um só endereço de origem.
