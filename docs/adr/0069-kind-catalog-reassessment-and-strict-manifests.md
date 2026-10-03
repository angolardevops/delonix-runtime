# ADR-0069: The Kind catalog after reassessment, and manifests that refuse what they do not understand

- **Status:** Accepted (2026-10-03, by the owner's mission brief) — D1–D5 implemented; D6–D8 are decisions with their work still pending, listed in «Pending»
- **Date:** 2026-10-03
- **Deciders:** Walter Angolar
- **Relates to:** ADR-0020 (CLI restructuring, Kind names), ADR-0028 (NetworkAccessRule), ADR-0032 (Service), ADR-0035 (App), ADR-0044 (provider ports), ADR-0046 (HTTPRoute, IPPool), ADR-0058 (system containers), ADR-0059 (network providers by role)

## Context

A static review of commit `681d3de8` proposed a smaller Kind set and listed defects. Every claim
was re-checked against that commit's code before acting (read-only audits with file:line
evidence); the review is a hypothesis, this ADR records what the code showed.

Confirmed at `681d3de8`:

| Claim | Result |
|---|---|
| Unknown manifest fields only warn on `plan`/`apply` | TRUE — only `stack validate --strict` failed |
| The unknown-field check recurses into Pod `containers[]` | FALSE — `readinessProbe:` on a member vanished without a word |
| `App` rebuilds on every apply | TRUE — `desired()` said `converges: false`, no input fingerprint |
| CNI plugins run without deadline or output cap | TRUE — `wait_with_output`, nothing bounded |
| `Service` membership is readiness-aware | FALSE — `Starting`/`Unhealthy` containers were handed out |
| Plan treats an unreadable store as «nothing there» | TRUE for VM, share volumes, and the presence probe |
| Same pod name in two namespaces | `create_pod` answered «already exists, nothing to do» for the second: success for a pod never created |
| Policy Kinds apply after workloads, no quarantine | TRUE (`run_layers`) — see D6 |
| VM `actual()` reads the registry only | PARTIAL — it refreshes status per VM best-effort but compares recorded fields, and a VM only the provider knows is invisible |
| `Ingress`, `Workload`, `Dependency` have their own executors | FALSE — all three already lower to `HTTPRoute`, `Container`/`Pod`/`VirtualMachine`, `NetworkPolicy` at load; there is no second executor to remove |
| OpenStack backend | does not exist (ADR-0039 *Proposed*; only an appliance image) |
| `Gateway` is a tunnel | TRUE — one local port exposed through a third-party provider; `NetworkGateway` is the perimeter-appliance Kind |

## Decisions

**D1. A manifest the engine does not fully understand is refused before any effect.**
`manifest::load` fails when any field was ignored, on every path that applies, plans, diffs or
drifts. The warnings are still printed (they say which field), and the error says nothing was
applied. `stack validate` keeps reporting (it uses `load_lenient`), and `--strict` keeps turning
the report into an exit code. `DELONIX_MANIFEST_LENIENT=1` is the explicit escape for a manifest
written for a newer binary. The check now covers each `spec.containers[]` item of a Pod-shaped
spec (`POD_CONTAINER_FIELDS`): probes, `lifecycle`, `initContainers`-style keys and typos are
refused instead of dropped. The count is per thread, so concurrent loads do not see each other's
warnings.

**D2. `App` converges on its inputs.** The catalog row said `converges: true`, the Kind's
`desired()` said `false`. Both now say true, honestly: `desired()` computes a SHA-256 over the
builder choice and the source tree (paths, sizes, bytes; symlinks by target; `.git`,
`node_modules`, `target`, `.delonix` skipped), `actual()` reads the fingerprint recorded when the
image under the tag was built — trusted only while the tag still points at the digest recorded —
and `apply` skips the build when both match. A retagged or hand-made image reads as «inputs
unknown» and plans a rebuild. Build remains an operation that produces an image; the record is
`<state>/apps/<name>.json`.

**D3. CNI plugins are bounded.** Deadline (60 s, `DELONIX_CNI_TIMEOUT_SECS`), 1 MiB cap per
stream (the plugin keeps being drained so it never blocks on a full pipe), stdin written from a
thread, and a timed-out plugin is killed and reaped before the error returns.

**D4. `Service` membership is readiness-aware.** A backend is `Running`, and when it declares a
health check, `healthy`. `Starting`, `Unhealthy`, `Paused` and `Created` are out. A container
with no health check is ready when it runs. Name resolution of the container itself is
unchanged. This is discovery, not connection draining.

**D5. Observation errors are errors.** VM, scoped-volume and container-presence listings
propagate their failure; an unreadable store no longer plans `Create` for every declared
resource. A pod name is unique on the node, so the same name in another namespace is a
`Conflict`, not «already exists».

**D6. Policy before activation (decided, not implemented).** Today `FirewallPolicy`,
`NetworkAccessRule` and `NetworkGateway` apply after `VM`/`Container`/`Pod` because they need the
target's address. The fix is a prepare/activate split — attach the workload's network
administratively blocked, install the policy, verify, then open — not a reordering of Kinds. It
changes the dataplane attach path (`delonix-sdn`) and needs a traffic test during create; it is
the first item of the follow-up.

**D7. Catalog.** Keep: `RuntimePolicy`, `Secret`, `Network`, `NetworkRoute`, `Volume`, `Image`,
`App` (as the build operation's declarative front), `VirtualMachine`, `Pod`, `Service`,
`NetworkPolicy`, `NetworkAccessRule`, `NetworkGateway`, `IPPool`, `HTTPRoute`, `Gateway`,
`Stack`, and the two optional-infrastructure Kinds `NetworkZone` and `SystemContainer`.
Not merged, with reasons:

- `NetworkAccessRule` into `NetworkPolicy`: `NetworkPolicy` declares the whole state of a
  (target, direction); `NetworkAccessRule` owns one rule by `origin` and retracts only that
  one. Merging would either lose that retraction or make `NetworkPolicy` two things. They share
  the dataplane (`FwRule`, one `nft -f`), which is where the sharing belongs.
- `NetworkZone` into `Network`: a zone is a provider-side segment realised through a
  `SegmentProvider` (Proxmox today), with no native implementation and no update-in-place;
  `Network` is the native rootless SDN. Different realisers, different lifecycles.
- `Gateway` rename: the review proposed `Tunnel`. The contract did not change, so a rename is
  cosmetic and costs every template, example and doc; `Tunnel` already resolves as an alias. Kept.
- `Container` stays a `Sunset(Pod)` Kind. A `Container` lowered to a one-member `Pod` would get a
  shared netns holder it never had; the sunset is announced, not rewritten (ADR-0020).
- `KubernetesCluster` stays outside `stack apply` (`in_stack: false`): it is a remote SSH
  procedure, documented as bootstrap, not a convergent resource.
- `Ingress`, `Workload`, `Dependency`: already sugar over one executor each (see Context); no
  duplicate lifecycle exists to remove.

**D8. Providers.** One selection mechanism already exists (configured target + capability
catalog); no `Provider` Kind is added. OpenStack stays *Proposed*: no backend, no claim.

## Pending (real, with the prerequisite)

1. D6 prepare/activate network attach — needs `delonix-sdn` attach path work and a live traffic test.
2. Plan identity as `(kind, scope, name)` for Pod/Service/Container/VM (only share volumes are
   scoped today); `destroy_one` takes `(kind, name)`. Needs a `ResourceKey` through
   `reconcile.rs` and the destroy path. D5 removes the silent false success but not the
   plan-level homonym limit.
3. Operational refresh in `plan` for VMs a provider knows and the registry does not.
4. Saved plans with a digest for every Kind (today only network documents) and a journal for
   container/pod/volume/VM applies (networking and Proxmox have ledgers).
5. Pod standalone: `emptyDir` on disk, `network: host|none` that mean what they say, `requests`
   admission semantics, probes/init containers. D1 makes the unsupported ones refuse; it does not
   implement them.
6. Secret rotation by version without plaintext in the diff.
7. OpenStack spike (ADR-0039) — requires a lab cloud.
8. CNI `VERSION`/`GC`, persisted netconf for `DEL` after a conflist change.
9. OCI/CRI/CNI/CSI conformance suites — not run in this change; no conformance is claimed.

## Consequences

- A manifest that used to apply with a warning now fails. Fix the spelling, or set
  `DELONIX_MANIFEST_LENIENT=1`. Examples and templates are covered by the existing «examples
  raise no warning» tests.
- A second `stack apply` of an unchanged `App` prints `up to date` and builds nothing.
- Rollback: each decision is a separate commit on `kinds/refactor-canonical`.
