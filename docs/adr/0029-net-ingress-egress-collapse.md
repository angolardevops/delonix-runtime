# ADR-0029: The three open questions of B4's `net` collapse, resolved

## Status

Accepted.

## Context

Block B4 of the CLI restructuring plan (`docs/discovery/52_CLI_PLANO_MIGRACAO.md`)
promised −41 leaves from collapsing `net ingress`/`net egress` into a
declarative Kind; only 4 genuinely-duplicated leaves were ever cut
(`net tunnel ls/describe/rm` → `get/describe/delete gateways`, `net httproute
ls` → `get httproutes`). `ADR-0028` closed the structural blocker — `kind:
NetworkAccessRule` gives per-rule, independently-applied-and-removed
incremental firewall state, which `FirewallPolicy`'s whole-direction-replace
semantics could never express — but explicitly left the actual leaf-count
question for later: "whether removing 17 imperative leaves in favor of
authoring `NetworkAccessRule` manifests is a net UX improvement needs its own
measurement, not an assumption."

That measurement surfaced three separate open questions, not one. This ADR
answers all three, so B4 has a settled direction instead of an open blocker.

## Decision 1: `net ingress/egress allow/deny` write through the SAME
bookkeeping `NetworkAccessRule` uses — but the CLI verbs themselves are NOT removed

**What changes**: `firewall.rs::add_rule` currently mutates
`ContainerFw.rules` directly — find the rule with the same `(dir, proto,
port, src)` match, remove it, push the new one (`origin: None` always, ufw
"last command wins" semantics, `firewall.rs:499-532`). This is a second,
independent code path from `network_access_rule.rs::set_rule_by_origin`,
which does the identical shape of operation (remove-then-push) but keyed by
`origin` instead of by match tuple.

`add_rule` moves to `network_access_rule::set_rule_by_origin`, passing a
**deterministic synthetic origin** derived from the match tuple itself:
`format!("cli:{dir}:{proto}:{port}:{}", norm_any(&src))`. This preserves the
exact existing behavior — the same `(dir, proto, port, src)` invocation
always maps to the same origin, so a second `net ingress allow <c> 8069`
still replaces the first one-for-one, byte-identical to today's UX — while
making every CLI-added rule visible to the generic Kind machinery for free:
`get networkaccessrules <container>`, `describe networkaccessrules
cli:in:tcp:8069:`, and eventually `stack apply --prune` (once a stack is
taught to own CLI-added rules, which it is not today — see "Not decided
here").

**Why this is the right grain and not a bigger refactor**: it does not touch
the two things that make `net ingress`/`net egress` easy to use today — the
UX (`net ingress allow <c> 8069`, no manifest authoring required) and the
shadow-detection warning (`field_overlaps`, unrelated to `origin`, unchanged).
It only unifies which Rust function owns the "replace-by-key" mutation, so
there is exactly one implementation of "find this rule and replace it,
leaving everything else on the container untouched" instead of two that
must be kept in sync by hand. `set_rule_by_origin` becomes `pub(crate)` (it
already lives in `network_access_rule.rs`); nothing about its signature
needs to change — a CLI-synthesized origin is a `String` exactly like a
manifest document's name.

**A CLI-added rule is `Adopt`-able, never pre-owned.** Unlike a manifest
`NetworkAccessRule` document (which a `stack apply` can stamp with
`owner`/`last-applied` via `stamp()`), a CLI-added rule has no stack backing
it — `owner` stays absent, exactly like a container created by hand today
reads as `Adopt` rather than belonging to a phantom stack. No new state:
`stamp()` is simply never called for these origins, which is already what
happens for any origin no `stack apply` names.

**What does NOT change**: `net ingress ls`/`net egress ls` keep printing
port/proto/from/action columns exactly as today — the origin is bookkeeping,
not a new column users need to learn. `field_overlaps`'s shadow-detection
warning is untouched (it reasons about dir/proto/port/src overlap, never
about origin). No CLI leaf is removed by this decision.

**Why this doesn't collapse `net ingress`/`net egress`'s own CLI leaves**:
that was never what this decision was about. `ADR-0028` already settled that
question — collapsing the imperative surface into manifest-only
`NetworkAccessRule` authoring is a UX regression for the common case (nobody
wants to write a YAML file to open one port on one container), and nothing
measured since has changed that conclusion. This decision closes the
*persistence* half of B4's first open question (one code path, one source of
truth) without reopening the *CLI surface* half, which stays decided.

## Decision 2: `publish`/`unpublish` (DNAT) get NO `NetworkAccessRule`
equivalent — they are a different grain and stay purely imperative

`NetworkAccessRule.spec` is `{target, direction, action: allow|deny, proto,
port, from}` — an accept/drop decision on already-routed traffic. A publish
is not that: it is a `hostPort:containerPort` DNAT mapping, allocating a
**host-side** resource (the published port itself, tracked in
`port_owner`/`reap_orphan_hostfwds`'s domain) that has no `allow`/`deny`
axis at all — there is nothing to flip a publish between two states of.
Forcing it into `NetworkAccessRuleSpec` would mean either inventing a second,
unrelated `action: publish` variant with entirely different required fields
(`hostPort`, no `proto/port/from` triple in the same sense), or leaving half
the spec's fields meaningless for that variant — the exact "two grains
wearing one Kind's clothes" problem `ADR-0024` already flagged for
`FirewallPolicy` itself.

**Decision**: `net ingress publish/unpublish` (and the `netns
publish/unpublish` twin used internally) stay imperative-only, permanently,
not "pending a future Kind." This also corrects B4's own leaf-count target:
the plan's original "−41" implicitly counted these as collapsible; they are
not, and should be subtracted from any future B4 leaf-count measurement
alongside `l4guard`'s already-excluded 3 leaves (`ADR-0028`).

If a genuine declarative need for DNAT surfaces later (e.g. GitOps-managed
port publishing, `ADR-0021`'s pull reconciler), it gets its OWN Kind sized to
its own grain — not a bolt-on to `NetworkAccessRule`.

## Decision 3: `net netns` stays a visible, first-class subcommand

`netns`'s own module doc already states its purpose plainly: "exposing it
helps debug the network path directly (attach a netns, publish a port,
inspect state)." The truly internal plumbing — the `netns holder`/`netns run
<spec>` re-execs — is **already** hidden, intercepted in `main()` before
`clap` parsing even runs (`net.rs`'s own doc comment: "the hidden subcommand
... doesn't appear here"). What remains visible (`up`/`status`/`down`/
`attach`/`detach`/`exec`/`publish`/`unpublish`/`firewall`) is operator-facing
diagnostic tooling analogous to `ip netns` itself, not internal wiring other
subsystems merely happen to call.

**Decision**: no change. Hiding it would remove real, documented debugging
capability (inspecting holder/slirp/bridge state directly) for zero benefit
— nothing else in this plan needs it hidden, and B4's leaf count never
counted `netns`'s 9 leaves as collapsible in the first place (they were
never duplicated by anything; the CLI restructuring's own `AGENTS.md`
section on the root reorganization already explains why `net netns` groups
existing plumbing rather than exposing a leaner surface for it).

## Consequences

- `firewall.rs::add_rule` shrinks to build a `FwRule` + a synthetic origin
  and delegate to `network_access_rule::set_rule_by_origin` — one fewer
  independent mutation path to keep behaviorally identical to the other by
  hand.
- `get networkaccessrules <container>`/`describe networkaccessrules
  <origin>` become useful against CLI-added rules for the first time, with
  zero new CLI surface.
- B4's leaf-count target corrects from "−41" to reflect that `publish`/
  `unpublish` (4 leaves: `net ingress publish/unpublish`, `netns
  publish/unpublish`) and `netns`'s 9 leaves were never collapsible — the
  real remaining opportunity, if any, is smaller than the plan's original
  number implied.
- `docs/discovery/52_CLI_PLANO_MIGRACAO.md`'s B4 section needs updating to
  reflect that all three open questions are now closed decisions, not open
  blockers.

## Not decided here, and why

- **Teaching `stack apply --prune` to own CLI-added rules.** Decision 1 makes
  CLI-added rules *visible* to `get networkaccessrules`; it does not make
  them *ownable* by a stack that never declared them. That is a separate,
  smaller follow-up (`stamp()`/`desired()` already assume a manifest
  document exists) and not required for B4's own goal.
- **Collapsing `net ingress`/`net egress`'s CLI leaves onto manifest-only
  authoring.** Explicitly re-affirmed as the wrong move (see Decision 1) —
  not merely deferred.
- **A DNAT-shaped declarative Kind.** No concrete consumer exists yet
  (Decision 2) — inventing one speculatively would repeat the mistake
  `ADR-0010` already named and rejected for the management API: build for a
  consumer that exists, not one that might.
