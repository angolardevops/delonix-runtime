# ADR-0035: `kind: App` connects the existing CNB scaffolding to a real build path — and the registry stops being loopback-only to make that possible

- **Status:** Accepted, implemented and validated live 2026-09-06
- **Date:** 2026-09-06
- **Deciders:** Walter (owner)
- **Related:** `crates/delonix-image/src/{buildpack,detect,internal_registry}.rs` (the existing,
  disconnected scaffolding this ADR wires up), `cmd/build.rs` (`resolve_or_pull_platform`/
  `prepare_rootfs_flat`/`ensure_container`/`runtime::exec` — the Dockerfile-build machinery this
  reuses), ADR-0001 (`kind: Workload`, the precedent for a thin lowering-layer Kind), ADR-0007
  (generated manifest schema — a new Kind needs a typed spec from day one).

## Context

`crates/delonix-image` already carries three pure, unit-tested, **zero-caller** modules for Cloud
Native Buildpacks — confirmed by exhaustive `git grep` against `origin/main`, not assumed from the
module doc-comments alone:

- **`buildpack.rs`** — `CnbPlan` (builder/run image pair, `creator_args()` building the
  `/cnb/lifecycle/creator -app=/workspace -layers=/layers -cache-dir=/cache -run-image=<run>
  <output_ref>` invocation, `mounts()` pairing source→`/workspace` and a named cache volume→
  `/cache`) and `builder_images(family)` (two families: `"heroku"` → `heroku/builder:24`/
  `heroku/heroku:24`, everything else — including a typo — silently falls through to
  `paketobuildpacks/builder-jammy-base`/`run-jammy-base`).
- **`detect.rs`** — `detect(dir) -> Option<Detected>`, a real, tested, priority-ordered stack
  detector (go/rust/java/dotnet/node[spa|express]/python[django|fastapi|flask]/ruby[rails]/
  php[wordpress|laravel]/static, in that order) that already names a suggested Paketo builder per
  stack — but returns data, not a plan; nothing turns a `Detected` into a `CnbPlan`.
- **`internal_registry.rs`** — argument-builders for running the real `registry:2` (OCI
  Distribution) image as a Delonix container, bound to **`127.0.0.1` only, by explicit design**
  ("never exposed to the network").

None of the three has a caller outside its own module and tests. There is no `kind: App` in the
manifest dispatch (`cmd/manifest.rs`'s `filled_spec` — confirmed, the Kind list has no `APP`
entry), no `--buildpacks` flag on `BuildArgs` (`cmd/build.rs` — confirmed, its fields are
`context`/`file`/`tag`/`build_arg`/`no_cache`/`secret`/`platform`, nothing CNB-shaped), and the
lifecycle assumes only the single-phase `creator` binary — the separate `detector`/`analyzer`/
`builder`/`exporter` phases of the full CNB platform spec are never mentioned anywhere in the repo.

**The one genuine, unsolved problem, found by tracing the actual data flow rather than trusting
the doc-comments:** `creator_args()`'s `output_ref` positional is the export target, and the CNB
`creator` in this configuration (no `-daemon` flag present) exports **over the network to an OCI
registry** — there is no Docker socket for it to fall back to, matching this engine's daemonless
model. But `internal_registry.rs`'s registry is bound to `127.0.0.1` on the **host**. A container
running the CNB builder lives in its **own network namespace** (this engine's rootless SDN model —
one veth/slirp per container, exactly like every other container this engine creates). A builder
container has no route to the host's loopback by default; the two pieces of scaffolding, as
written, cannot reach each other. This is not a gap in effort — it is a real, structural mismatch
that nobody had reached yet because nothing calls either module.

**The machinery to actually RUN a CNB build already exists, proven, in the Dockerfile build path**
(`cmd/build.rs`): `resolve_or_pull_platform` (pull-if-absent, arch-aware) + `prepare_rootfs_flat`
(materialize the image) + `ensure_container` (`RunSpec { detach: true, userns: rootless, .. }`,
`runtime::create_with`, a `sleep infinity` placeholder) + `runtime::exec` (run a command inside
it) is the exact shape `CnbPlan.mounts()` + `creator_args()` need — a builder image is not
structurally different from a Dockerfile build stage's base image.

## Decision (proposed)

**Ship `kind: App` as a lowering layer, the same shape ADR-0001's `kind: Workload` already
established**: a thin declarative object that resolves (via `detect.rs`, or an explicit override)
to a `CnbPlan`, then reuses the Dockerfile-build container machinery verbatim to run the `creator`
inside a builder container. No new execution engine, no new container lifecycle code.

```yaml
apiVersion: delonix.io/v1
kind: App
metadata: { name: shop }
spec:
  source: .                 # defaults to the manifest's own directory
  builder: auto             # "auto" | "heroku" | an explicit builder image ref
  image: shop:latest        # the resulting tag, in the LOCAL ImageStore after export+re-pull
```

**Fixing the network mismatch: the internal registry moves onto a per-build SDN network — not
loopback, and not the general SDN either.** `App::apply` creates a throwaway `kind: Network`
(bridge driver, same primitive `kind: Network` always uses) scoped to the build, starts the
registry container on it (dropping the `127.0.0.1` bind — the network boundary itself is the
isolation, the same model every other inter-container reachability question in this engine already
uses), and starts the builder container on the **same** network. Both containers are torn down
with the build; the registry never outlives one `App` apply and is never reachable from any
container outside that build's network. This is strictly narrower than the loopback design's
apparent intent (the registry was never meant to be reachable by arbitrary local processes either)
and fixes the one thing loopback actually got wrong: a container's loopback is its own, not the
host's.

**The builder reaches the registry by its SDN IP, not by this engine's internal DNS name — found
only by running a real build, not by reasoning about the design.** The design as first written
here pointed `output_ref` at `<registry-container-name>.<namespace>.delonix.internal:5000/<tag>`
(this engine's existing DNS resolver). It resolves fine; the CNB lifecycle's registry client
(go-containerregistry) then refuses it anyway — `Get "https://….delonix.internal:5000/v2/": http:
server gave HTTP response to HTTPS client`. That client only auto-selects plain HTTP for an
RFC1918 address or a literal `localhost:<port>`; any other hostname — a DNS name included — gets
HTTPS, and there is no flag on `/cnb/lifecycle/creator` (checked against its real `-h`, this
lifecycle version 0.21.18) to override it. This engine's SDN subnets are always RFC1918 per
network, so pointing `output_ref` at the registry container's own SDN IP instead satisfies that
heuristic for free — no config file, no extra flag, no registry-auth plumbing.

**After the `creator` succeeds, the built image comes back into the LOCAL `ImageStore`** via the
same `pull_from_registry_with_creds` path `resolve_or_pull_platform` already uses, pointed at the
throwaway registry before it's torn down — the manifest's `spec.image` names it in the store the
same way `kind: Image` always does. The throwaway registry is *transport*, not a place images
live.

**Scope of v1**: auto-detected single-buildpack-group builds via the `creator` (single-phase)
lifecycle only — the shape the existing scaffolding already assumes. `spec.builder` accepts
`"auto"` (uses `detect.rs`), `"heroku"`, or an explicit builder image reference (bypassing
`builder_images()`'s family match entirely, for anyone who names an unsupported family and would
otherwise silently get Paketo without knowing it — **this ADR treats that silent fallback as a bug
to fix in the same change**, not a v1 feature to keep: an unrecognized `family`/`builder` value
should refuse, naming the two it knows, exactly like every other allowlist-shaped input in this
codebase).

## Alternatives considered

- **Export via a Docker-socket-shaped `-daemon` flag instead of a registry.** Rejected: there is no
  Docker daemon here, by design (guardrail #1). Emulating one well enough for the CNB `exporter`
  phase to talk to it is a much larger surface than running one more instance of a registry image
  this engine already knows how to run.
- **Bind the internal registry to the general SDN (every container's default bridge) instead of a
  per-build network.** Rejected: that makes an unauthenticated OCI push/pull endpoint reachable
  from every container on the node for the registry's entire lifetime, not just the one build that
  needs it — a real, avoidable widening of the attack surface for a component whose own doc-
  comment already says it should never be network-exposed.
- **Run the registry on the HOST's network namespace directly (root-mode container, or a
  host-network flag) so the builder reaches it via the host's real IP.** Rejected: this engine's
  containers reach the host over the SDN's gateway already (documented pattern elsewhere in
  `AGENTS.md` — `vm reach`, the ingress proxy's own hostfwd) but doing that here would mean binding
  a registry to a real interface, which is a bigger exposure than a network only the build's two
  containers are ever attached to.
- **Implement the full 4-phase CNB lifecycle (`detector`/`analyzer`/`builder`/`exporter`
  separately) instead of `creator`.** Rejected for v1: the existing scaffolding already committed
  to `creator` (single-phase) — nothing in `buildpack.rs` builds the other three binaries'
  arguments — and the 4-phase split exists mainly to let a platform cache/rebase between phases,
  which is exactly the kind of optimization to add once `creator` is proven working, not before.
- **Skip `detect.rs` and require `spec.builder` always explicit.** Rejected: `detect.rs` is
  already written, tested, and the entire point of "git push and get a running app" — requiring an
  explicit builder for every app would make this no better than `kind: Image` with `build:`
  already is.

## Consequences

- `cmd/manifest.rs` gains `k::APP` in its Kind dispatch; `cmd/kinds.rs`'s `KindFacts` gains a row
  (same wiring class `kind: Service` needed: `hot_fields`, presence, teardown — an `App` converges
  like `kind: Image` does, since re-running a build is the update path, not a field-level patch).
- A new `cmd/app.rs` (lowering + orchestration), reusing `cmd::build`'s container/exec helpers
  (promoted to `pub(crate)` where they currently are not) and `delonix_image::{buildpack, detect,
  internal_registry}` verbatim — no changes needed to the three existing modules' pure logic, only
  to who calls them.
- `internal_registry.rs`'s loopback bind is REMOVED as the default — this is the one behavior
  change to existing (if never-yet-executed) scaffolding, and it is the fix this ADR exists to
  make, not a side effect to explain away.
- `builder_images()`'s silent fallback on an unknown family becomes a refusal — a small, deliberate
  breaking change to code that has zero callers today, so it breaks nothing real.

## Validated live, and what it found

Three real defects, none visible from reading the code, all found by actually running
`delonix apply -f` against a real Node/Express app (`package.json` + `Procfile`) with the
`heroku` builder family, against real registries (Docker Hub for the builder image, the
throwaway one for export) — no mocks, no stub buildpacks:

1. **`source_dir` was never canonicalized.** `spec.source` defaults to `.`, and the builder
   container is created on a custom network — the `--net <custom>` re-exec path this engine
   already uses for every container on a non-default network. That path is a different process
   with a different CWD; a relative `.` surviving to `CnbPlan::mounts()` resolved there instead of
   here, and the builder's `/workspace` bind mount silently became `/` (this host's `mount(2)`
   returned `ENOENT` trying to prepare the rootfs against a mount source that didn't mean what it
   looked like). Fixed by canonicalizing `source_dir` immediately after the is-a-directory check,
   before it becomes a mount-source string.
2. **`CNB_PLATFORM_API` is required, not optional** — `/cnb/lifecycle/creator` refuses to run at
   all without it ("failed to get platform API version"). Fixed to `"0.7"`: both known builder
   families (`auto`/Paketo-jammy-base and `heroku`) embed the identical lifecycle version
   (0.21.18, confirmed against their published `io.buildpacks.builder.metadata` labels) and
   declare this exact value as their own default, inside a range that reaches `"0.15"` — this is
   the lifecycle's own preferred choice for both currently-supported families, not a guess.
3. **The DNS-name `output_ref` from the original design didn't work** — see the "Fixing the
   network mismatch" section above; fixed by addressing the registry via its SDN IP.

**A separate, pre-existing, out-of-scope defect was found and NOT fixed here**: `paketobuildpacks/
builder-jammy-base` has 91 layers, and this engine's overlay-mount option string
(`lowerdir=<91 paths>:...`) comes to ~8.8 KB — over double the ~4 KB a single classic `mount(2)`
syscall's `data` argument can carry, which silently truncates rather than erroring, and the
truncated last lower path then fails to resolve (`ENOENT`, the exact symptom, reproduced with a
plain `container run paketobuildpacks/builder-jammy-base sleep 5` — no App-kind code involved).
This is a limitation of `delonix-runtime`'s `mount_overlay_if_marked` (`crates/delonix-runtime/src/
lib.rs`) for ANY image with enough layers, affecting every container/App build that resolves to
that specific builder image, not something `kind: App` introduced or can fix from `cmd/app.rs`.
Properly fixing it means moving that one mount to the newer `fsopen`/`fsconfig`/`fsmount` API
(which accepts `lowerdir+=` incrementally, with no such length ceiling) — a `delonix-runtime`-wide
change deserving its own ADR and its own validation, not a one-line patch bundled into this one.
**Until fixed, `builder: auto` may fail on this specific defect for a real app**; `builder: heroku`
does not hit it (23 layers, ~2 KB) and is what full live validation exercised end-to-end: detect →
Heroku Node.js + Procfile buildpacks → Node 24.20.0 installed → export → push (by IP, plain HTTP)
→ pull-back → tagged `app-live-test:latest` locally → **run the resulting image and it served
"hello from kind:App" over the port it declared**, proving the built artifact is a real, working
container image, not just a successful exit code.

## Not done here, and why

- **Implementation.** This ADR is the design; `kind: App` schema, `cmd/app.rs`, the per-build
  network lifecycle, and the fallback-to-refusal fix are the next, separate commit(s).
- **Custom buildpack authoring/composition beyond auto-detected groups** (CNB's `buildpacks:`
  list, `extension:` images) — v1 is "detect a stack and build it," not a buildpack development
  tool.
- **A shared/warm builder cache across separate `App` builds** — `CnbPlan.cache_volume` is already
  named per-app (`cnb-cache-{name}`) and persists as a normal named volume across re-applies of the
  SAME app; sharing a cache ACROSS different apps is a separate, later optimization.
- **Registry authentication for the throwaway per-build registry.** It is not reachable outside
  its own build's private network, so authentication would add complexity without closing a real
  exposure — revisit only if the per-build-network model itself changes.
- **`kind: App` targeting a REMOTE registry directly** (skipping the local re-pull, publishing to
  `ghcr.io`/similar) — v1's `spec.image` always lands in the local `ImageStore`, matching every
  other Kind's "this node's state" scope; a remote-publish flag is a natural, separate follow-up.
