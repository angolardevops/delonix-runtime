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

**D6. Policy before activation (implemented for containers).** A container that some
`NetworkPolicy`, `NetworkAccessRule` or (lowered) `Dependency` in the manifest names is created
**closed**: `RunOpts.policy_hold` makes `cmd_run` install a default-deny chain for its address
right after the network attach and before the process exists, and the record carries the same
state (`delonix.io/policy-hold`) so a restart cannot reopen it. While the annotation is there,
`apply_firewall_everywhere` writes what the policy documents say to the RECORD and keeps the
closed chain on the dataplane, so a policy that has applied its default but not yet its rules
opens nothing. After the policy layers succeed, `release_policy_holds` removes the annotation
and applies the record: a direction the manifest declared keeps its policy, one it did not
returns to the open default. If a policy layer fails the workload stays closed, the apply says
so, and the next apply releases it; a second apply of an unchanged manifest holds nothing. The
hold is written with the policy fields the dataplane has always understood (`deny`/`deny`), so a
holder from before this change enforces it too. **Pods are covered** (see D6b). Not covered: VMs and
system containers (their firewall is the provider's, `scope: vm`/`systemcontainer`, and a hold there is
provider-specific). Lab: `scripts/chaos.sh policy_hold` — 8
checks pass, and with the hold disabled 3 fail, including the ping that gets through after a
failed policy.

**D6b. A Pod is one policy target.** A `NetworkPolicy` / `NetworkAccessRule` / `Dependency` naming a
Pod used to pass `stack validate` and fail at apply with `no such container: <pod>`, after the layers
before it had created things — five shipped examples did it. `update_locked` and `load_governed`
(`cmd/firewall.rs`) now resolve a name that is no container to the pod's **view**
(`pod::pod_view`): the head member's record wearing the shared netns as identity and the pod's
address, so every policy path (default policy, rules, origin-keyed rules, `fromWorkload`) runs
unchanged on it and only `firewall` and `annotations` are written back, under the head's lock.
`apply_pod_namespace_isolation` reapplies the persisted firewall (or the hold) when a pod's netns is
recreated, so a holder respawn does not reopen it. A VM named by a container-scope policy is now
refused by `stack validate` (use `scope: vm`). The imperative `net ingress|egress <pod>` verbs
still answer `no such container`. Also fixed on the way: a Pod on `network: appnet` planned a
replace on every second apply, because `actual()` read the network back from the member's record
(`--net host`); the declared network now travels on the members (`delonix.io/pod-network`).
Lab: `scripts/chaos.sh policy_hold_pod` — 7 checks pass (closed after a failed policy, marked, opened
by the next apply, no drift in the following plan, `deny` closes the pod, a member restart does not
reopen it); with the pod hold disabled 2 fail.

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

1. D6 for VMs and system containers (provider firewalls); a holder-respawn test for a held or governed pod; `net ingress|egress <pod>` on the CLI.
2. Plan identity as `(kind, scope, name)` for Pod/Service/Container/VM (only share volumes are
   scoped today); `destroy_one` takes `(kind, name)`. Needs a `ResourceKey` through
   `reconcile.rs` and the destroy path. D5 removes the silent false success but not the
   plan-level homonym limit.
3. DONE: `plan` asks the provider (`VmBackend::holds_vm`, Proxmox implements it) before a VM `Create`; a VM the provider holds and the registry does not is a `Conflict`. Other backends cannot enumerate and keep trusting the registry.
4. DONE: every change carries a `planDigest` and `apply --plan-digest` refuses a stale one; each apply layer writes `apply-started`/`apply-done`/`apply-failed` to the node event log.
5. Pod standalone: `emptyDir` on disk, `network: host|none` that mean what they say, `requests`
   admission semantics, probes/init containers. D1 makes the unsupported ones refuse; it does not
   implement them.
6. DONE: a Secret carries a `version` the store assigns on save (1 on creation, +1 when the values change); `secret apply` reports `created`/`unchanged`/`rotated to version N (changed: KEYS)` by key name only. `stack plan` now converges a Secret that carries its values (`stringData`), comparing key names and a KEYED fingerprint (never a value; the node's master key is in it, and a node without one plans a `Create` and creates nothing). A Secret that reads `fromEnv`/`fromEnvFile` stays ensure-present, because a plan that read the environment would differ by who ran it. A container records the version of each `--secret` it started with (`delonix.io/secret-versions`), and `container describe` says when it has been rotated since.
7. OpenStack spike (ADR-0039) — requires a lab cloud.
8. CNI `VERSION`/`GC`, persisted netconf for `DEL` after a conflist change.
9. OCI/CRI/CNI/CSI conformance suites — not run in this change; no conformance is claimed.

## Consequences

- A manifest that used to apply with a warning now fails. Fix the spelling, or set
  `DELONIX_MANIFEST_LENIENT=1`. Examples and templates are covered by the existing «examples
  raise no warning» tests.
- A second `stack apply` of an unchanged `App` prints `up to date` and builds nothing.
- Rollback: each decision is a separate commit on `kinds/refactor-canonical`.
