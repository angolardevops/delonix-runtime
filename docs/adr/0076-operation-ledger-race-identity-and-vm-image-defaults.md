# ADR-0076: The operation ledger races on its own staging file, idempotency keys ignore request content, and the node API's VM defaults diverge from the CLI's

- **Status:** Accepted — D1, D2 and D3 implemented and merged (see "Implemented", below)
- **Date:** 2026-10-09
- **Deciders:** diagnostic review handoff; D1/D2/D3 implemented the same day
- **Reviewed at:** `2b9087b06e914fcc1698707fd13fd925fd94e425` (`origin/main`, tag `v5.0.0`)
- **Relates to:** ADR-0042 (One engine API — step E, "the operation record"; D2 "Idempotency": *"the same request sent again is answered with that operation, whatever its state"*), ADR-0040 D4 (one node contract, idempotency as a published guarantee), ADR-0044 (the VM use-case layer and its ports: `LocalDiskImages`, `SeedBuilder`, `VmNetwork` in `delonix_compute::ports`/`vm_registry`), the engine's own layering rule (an interface crate never depends on a bin crate)

## Context

Three findings from a line-by-line review of `delonix-node-api`, each reproduced against the
real code (no copies, no hooks) in isolated `tests/` added to the crate for this review:
`achado_a_operation_race.rs`, `achado_b_idempotent_identity.rs`, `achado_c_vm_defaults.rs`.
Full test transcripts are in the review's evidence log, summarized below.

### A. `operations::begin()`'s exclusive-creation staging file is not unique per attempt

`operations::begin()` (`crates/interfaces/delonix-node-api/src/operations.rs:206`) answers
every mutation (`CreateNetwork`, `DeleteNetwork`, `CreateVirtualMachine`, …) with a persisted
`Operation`, keyed by `request_id` for idempotent retries (ADR-0042 D2). Two requests for the
same `request_id` are meant to resolve to exactly one executor and one `Operation`.

The exclusive-creation path (`operations.rs:232-249`) is:

```rust
let staged = dir(root).join(format!(".{id}.{pid}.new"));      // line 236
std::fs::write(&staged, bytes).map_err(|e| io("writing", e))?; // line 237
let linked = std::fs::hard_link(&staged, path(root, &id));     // line 238
let _ = std::fs::remove_file(&staged);                          // line 239
```

`id` is derived only from `request_id`; `pid` is the SERVER PROCESS's pid, not a per-thread or
per-attempt value — this crate runs every mutation via `tokio::task::spawn_blocking`
(`service.rs:63`), so two concurrent requests for the *same* `request_id` run on two OS
threads of the *same* process, and compute the *identical* staged path. There is no lock
between `replay()`'s initial check (line 213) and this sequence, and `std::fs::write` opens an
**existing** path in place (truncate, not create-a-fresh-inode) — so two racing threads share
one inode until whichever thread finishes first calls `remove_file`.

The sibling function this crate already has for exactly this class of bug,
`delonix_node::write_atomic_mode` (`crates/contexts/delonix-node/src/atomic.rs:21-33`), names
it in its own comment: *"Unique per WRITER (pid + sequence): a fixed temp name lets two
processes — or two threads of the CRI server — interleave their bytes in the same temp, and
then `rename` faithfully publishes the corruption."* `operations::begin()`'s staging path does
not follow that discipline.

**Measured** (`achado_a_operation_race.rs`, two racing threads per iteration, 1500 iterations,
32-core host, `tokio` not involved — the race is reproduced directly against `begin()`):

| run | New | Replay | hard `Err` (should have been `Replay`) |
|---|---|---|---|
| 1 | 1500 | 911 | **589 (39.3%)** |
| 2 | 1500 | 999 | **501 (33.4%)** |

Every `Err` carries the identical message: `operation record: creating: No such file or
directory (os error 2)` — the exact ENOENT this hypothesis predicted (the staged file is
removed by the thread that wins the `hard_link` race before the losing thread's own
`hard_link` call runs against it). A companion test with 12 threads racing one `request_id`
gets `New=1, Replay=7, Err=4` (33%) — the "exactly one executor" invariant never breaks (no
run ever produced two `New`s or zero), but roughly a third of legitimate concurrent retries
surface an opaque `Internal` gRPC status instead of ADR-0042's promised replay.

**Severity:** High. This is not a corner case requiring adversarial timing — it is the
ordinary behaviour of any client that retries a mutation under a timeout while the first
attempt is still in flight, against a server that dispatches every mutation to its own OS
thread with zero `request_id`-keyed serialization anywhere above `operations::begin()`.

### B. `operations::replay()` identifies a request by `(verb, target)` only

`replay()` (`operations.rs:254-274`) decides whether a `request_id` already answered *this*
request by comparing only `found.verb != verb || found.target != target`
(`operations.rs:267`). `target` is built per mutation from the resource's **name alone**:

```rust
let target = format!("Network/{}", req.name);        // network_ops.rs:209, 251
let target = format!("VirtualMachine/{}", req.name);  // vm_ops.rs:345, 385, 469
```

Nothing about `spec` (image, `vcpus`, `memory_bytes`, `cloud_init`, labels, …) or `namespace`
enters the comparison. A client that reuses one `request_id` across two **materially
different** requests that happen to name the same resource gets the first request's recorded
`Operation` back — silently, with the second request's distinct content never reaching
`config_from_spec`'s validation, never reaching the work closure, and the caller told
"succeeded" for an operation that does not describe what they asked for.

Input-shape validation (`spec.image` non-empty, CIDR well-formed, …) does run before `replay()`
in both `network_ops.rs` and `vm_ops.rs` — consistent, not a second inconsistency — but
**state-dependent** checks (`already_exists`, `not_found`) run *after* `replay()`, matching the
ADR-0042 text quoted above. The gap is that `replay()`'s own equality test has no field finer
than the resource's own name.

**Measured** (`achado_b_idempotent_identity.rs`, against the real `vm_ops::create_with_fn`
with an injected work closure that records what `VmConfig` it was called with — the crate's
own test seam, not a copy):

- Request 1: `request_id="same-key"`, `name="web"`, `image="alpine:a", vcpus=2,
  memory=2048M`. Recorded work call: `disk="alpine:a", vcpus=2, memory="2048M"`.
- Request 2: **same** `request_id="same-key"`, **same** `name="web"`, but `image="ubuntu:b",
  vcpus=16, memory=65536M` — a 16× larger VM of a different image.
- Result: `calls recorded = 1`. The work closure is **never invoked a second time**;
  `op1.id == op2.id == "r-same-key"`. A client that asked for `ubuntu:b`/16 vCPU/64 GiB is told
  the operation succeeded, describing what was in fact a 2-vCPU `alpine:a` VM.
- The same holds across two different **namespaces** sharing one `request_id` and the same
  name (`team-a`/`team-b`): the second namespace's create never runs.

**Severity:** Medium-High. Requires a client bug (idempotency-key reuse across logically
distinct requests) or a confused/malicious client to trigger; the engine-side defect is that
nothing detects or rejects it — the contract's own refusal path
(`a_request_id_that_answered_something_else_is_refused`, `operations.rs:516`) only fires on a
`verb`/`target` mismatch, never on a content mismatch for the same resource name.

### C. The node API resolves "0 = image default" differently than the CLI, for the identical input

`VirtualMachineSpec`'s own contract comment (`proto/delonix/node/v1/compute.proto:550-551`)
says:

```proto
int32 vcpus = 2;        // 0 = image default, then 1
int64 memory_bytes = 3; // 0 = image default, then 1 GiB
```

`vm_ops::config_from_spec` (`vm_ops.rs:176-184`) does not honour the first half of either
comment:

```rust
let vcpus = match spec.vcpus {
    0 => 1,                              // never looks at the image's recorded default
    n if n > 0 => n as u32,
    _ => return Err(...),
};
let memory = match mib_ceil(spec.memory_bytes, "spec.memory_bytes")? {
    Some(mib) => format!("{mib}M"),
    None => "1G".to_string(),            // same — fixed, never image-derived
};
```

This is **named, not hidden**, in the module's own doc comment (`vm_ops.rs:15-21`): *"Known
gap... resolving '0 vcpus/memory = this image's own recorded default' ... needs
`VmImageStore`, which lives in the CLI's bin crate... unreachable from an INTERFACE crate under
ADR-0040's layering. This module falls back to a fixed default (1 vCPU, 1 GiB) instead."* The
CLI's own resolver, `resolve_vm_defaults`
(`bins/delonix-runtime-bin/src/cmd/vm.rs:2657-2671`), *does* consult the image:

```rust
let vcpus = vcpus.or_else(|| image_meta.and_then(|m| m.default_vcpus)).unwrap_or_else(default_vcpus);
let memory = memory.or_else(|| image_meta.and_then(|m| m.default_memory.clone())).unwrap_or_else(default_memory);
```

**Measured** (`achado_c_vm_defaults.rs`): a fixture `<root>/vm-images/golden.json` written in
the exact shape and location `VmImageStore::save` produces, recommending `default_vcpus: 4,
default_memory: "4G"`. `CreateVirtualMachine` with `spec.image="golden", vcpus=0,
memory_bytes=0` resolves to `vcpus=1, memory="1G"` through the node API — the fixture sitting
on disk, in the exact format and path the CLI reads, is never consulted. Corroborated by
running the CLI's own existing test for the identical scenario
(`cmd::vm::tests::resolve_vm_defaults_cai_para_a_imagem_quando_o_cli_nao_diz_nada`, unmodified,
`cargo test -p delonix-runtime-bin resolve_vm_defaults`: **ok**), which resolves the same input
to `(4, "4G", backend)`. Explicit non-zero values are honoured identically by both paths
(not the gap); a reference with no recorded metadata at all falls back to `(1, "1G")` in both
(also not the gap) — the divergence is specifically "image exists, has defaults, request says
0".

**Severity:** Medium. Silent in the sense that matters to a caller: nothing in the response
says the image's own recommendation was ignored, and the proto comment — the only contract a
caller has — promises the opposite behaviour.

**Adjacent, not in scope here:** `delonix_vm::set_network(...)` (called from
`bins/delonix-runtime-bin/src/main.rs:603`, the only call site in the tree) is never called by
`delonix-node-api` or `bins/delonix-node-api-bin`, even though `delonix-node-api` links
`delonix-vm` directly. A `CreateVirtualMachine` request that names a network attachment would
fail at `delonix_compute::vm_registry::network()`'s `Err("no VM network provider is registered
in this process")` today. Flagged for its own, separately-scoped follow-up — fixing it is not
part of what D1–D3 below propose, and folding it in here would widen this ADR past what was
reviewed.

## Decision

### D1. Give `begin()`'s staging file a unique name per attempt, not per `(request_id, pid)`

Add a process-wide atomic sequence counter to `operations.rs` (the same discipline
`delonix_node::atomic::TMP_SEQ` already uses, re-derived locally since that counter is private
to its module — or, preferably, promote a small `delonix_node::atomic::unique_suffix() ->
String` helper so this becomes the **second** user of one counter instead of a second private
copy of the same idea) and stage at `.{id}.{pid}.{seq}.new`. Every concurrent attempt then
writes to its OWN inode; the existing `hard_link`-wins-the-race / `AlreadyExists`-falls-to-
`replay` logic is otherwise unchanged — this is a one-line change to the staged path plus the
counter, not a redesign. `hard_link` (not `rename`, which would silently clobber) stays,
because the "exactly one creator" guarantee the contract needs is `hard_link`'s refuse-if-
destination-exists semantics, which `rename` does not have.

### D2. A canonical, versioned fingerprint joins `(verb, target)` in `replay()`'s identity check

`Record` gains `#[serde(default)] fingerprint: String` (empty = "no fingerprint on this
record", so records written before this field existed are not retroactively invalidated — see
Consequences). Each mutation computes its own fingerprint from the fields that distinguish *this
request's effect* from another sharing the same resource name — `spec.image`, `vcpus`,
`memory_bytes`, `networks`, `cloud_init`, `labels`/`annotations` for a VM create; the
`request_id`'s equivalent for a network create — using a representation that:

- **treats map key order as irrelevant** (sort before hashing — a `BTreeMap`, or an
  equivalent canonical-JSON pass over sorted keys);
- **treats a field's own default (proto3 zero value) as equal to that field being omitted** —
  proto3 cannot distinguish "unset" from "set to the zero value" for a scalar anyway, so the
  fingerprint must not invent a distinction the wire format does not carry;
- **carries an explicit version prefix** (`"fp1:"...`), so a later change to what gets
  fingerprinted cannot collide with an old stored value by accident — an old record simply
  never matches a new-scheme fingerprint, which is handled the same way as "no fingerprint on
  this record" (below).

`begin()`/`replay()` take the fingerprint as a parameter (mirroring how `verb`/`target` are
already passed in by each mutation) and the refusal in `operations.rs:267` gains a third
condition: `found.verb != verb || found.target != target || (!found.fingerprint.is_empty() &&
found.fingerprint != fingerprint)`. A record with an **empty stored fingerprint** (legacy, or a
mutation that has not been updated to compute one yet) is **not** compared — this is
deliberately permissive for the transition, not a loophole: `RETENTION_SECS` (7 days) bounds
how long an un-fingerprinted record can exist to collide against, and every mutation this ADR
touches gains a fingerprint in the same change.

A mismatch is refused distinctly from "answered something else" — a new error naming that the
`request_id` was reused with different content (`ALREADY_EXISTS` is already how a name
collision is reported; this needs its own `DX-` code and message, assigned when this lands, not
guessed here) — never silently replayed and never silently treated as a second, independent
operation.

**Exactly-once is not claimed for provider-side effects.** The fingerprint makes "this is a
genuine retry" distinguishable from "this reuses a key by mistake or by attack" at the
engine's own ledger; it says nothing about whether the underlying provider (libvirt, Proxmox,
the SDN) itself guarantees exactly-once execution of the work closure if the SAME fingerprinted
request is legitimately retried after an `Interrupted` record (ADR-0042's own existing,
unresolved case) — that remains "retry and let the provider's own idempotent create/resolve
handle it", as today.

### D3. A minimal, context-level port for an image's recorded defaults — not a bin dependency, not a second rich struct

Add `trait ImageDefaults` to `delonix_compute::ports` (alongside `LocalDiskImages`,
`SeedBuilder`, `NetworkProvider`): one method, `fn defaults(&self, reference: &str) ->
Option<ImageDefaults>` returning `{ vcpus: Option<u32>, memory: Option<String>, backend:
Option<String> }` — the three fields `config_from_spec` actually needs, nothing from
`VmImage`'s build/registry metadata (`ubuntu_release`, `kernel_version`, `packages`,
`built_by`, …), which stays exactly where it is (`bins/delonix-runtime-bin`'s own concern:
`build`/`push`/`pull`/`ls`/`describe`). Wire it the same way `vm_registry` already wires
`VmNetwork` (`set_network`/`network()`, `crates/contexts/delonix-compute/src/vm_registry.rs:
410-429`): a `OnceLock<Box<dyn ImageDefaults>>`, `set_image_defaults`/`image_defaults()`.
Implement it in `delonix-vm::local_ports` (where `QemuImgDisks`/`CloudLocaldsSeed` already
live) by reading `<root>/vm-images/<sanitized-name>.json`'s three optional fields directly —
independent of, and not a dependency on, `bin`'s `VmImageStore`/`VmImage`. `config_from_spec`
calls `delonix_compute::vm_registry::image_defaults()` (an `Option`, absent gracefully falls
back to today's fixed `(1, "1G")`) before applying the `0 => 1` fallback. This closes the gap
for the common case named in the review (nothing given, the image has its own recorded
defaults) without creating the `interfaces`→`bins` dependency the module's own comment already
refuses, and without duplicating `VmImage`'s full, CLI-only metadata into a second struct a
second place has to keep in sync.

## Alternatives considered

**D1 — lock `request_id`s with an in-process mutex table instead of fixing the staged path.**
Rejected: the server is socket-activated and can exit between a lock being taken and the record
being durable, which is exactly the failure class ADR-0042's `Interrupted` handling already
exists to recover from *on disk* — adding an in-memory lock re-introduces a window the design
deliberately avoided. The cheaper fix (unique staging per attempt) keeps the existing,
already-correct `hard_link`-based exclusivity and removes only the one path that was not
unique.

**D1 — `flock` the whole `operations/` directory around `begin()`.** Rejected: serializes every
concurrent create/delete across *every* `request_id`, not just colliding ones, for no gain over
the one-line fix.

**D2 — leave `request_id` collision across different content undefined, document it as a
client responsibility.** Rejected per the engine's "no silent failure" guardrail and ADR-0042's
own stated guarantee ("answered with that operation, whatever its state") — a wrong answer that
looks like success is worse than a clear refusal, and the CLI reviewer did not need to guess
this is reachable: it reproduces on the first try with ordinary struct literals, no fault
injection.

**D2 — compare the full request message (serde round-trip equality) instead of a fingerprint.**
Rejected: brittle to catalog growth. A later field added to `VirtualMachineSpec` with a default
would make an old stored record (missing the field entirely) compare unequal to a new request
that explicitly sends the default — the exact false-positive a *canonical* fingerprint
(default-equals-omitted, by construction) is built to avoid.

**D3 — move `VmImageStore`/`VmImage` wholesale into `delonix-compute` or `delonix-vm`.**
Rejected: `VmImage` carries build-provenance fields (`ubuntu_release`, `kernel_version`,
`packages`, `packages_sha256`, `built_by`, `base_sha256`) that exist solely for `image vm
build`/`push`/`pull`/`ls`/`describe` — none of which `config_from_spec` or any context/adapter
code needs. Moving the whole struct widens a context crate's concerns to cover CLI build
tooling it has no business knowing about, for the sake of three fields.

**D3 — have `delonix-node-api` depend on `bins/delonix-runtime-bin` directly.** Rejected
outright: this is the exact dependency direction ADR-0040's layering forbids (an interface
depending on a bin), already named as out of bounds by the reviewed code's own comment. Not
reconsidered here.

**D3 — leave the gap as documented debt.** Considered, since the gap is already named rather
than hidden *in the source*. Rejected because the contract's own field comment ("0 = image
default, then 1") is what a caller of this API actually reads, and that promise is violated
with no signal to the caller that it was — the documentation-in-code does not reach the API's
actual consumer.

## Consequences

**Easier:** a concurrent idempotent retry of a network or VM mutation behaves the way ADR-0042
already promises it does (D1); a reused `request_id` across two different requests is refused
loudly instead of answered with the wrong resource's state (D2); `CreateVirtualMachine` with
`vcpus: 0`/`memory_bytes: 0` against a named image with its own recorded recommendation matches
what the CLI would have done for the identical input and what the contract's own comment says
(D3).

**Harder / debt assumed:** D2 adds a field to every `Record` and a fingerprint-computation
function per mutation that must be kept in sync with that mutation's own semantically-relevant
fields — a fingerprint that fingerprints too little re-opens exactly this ADR's finding B; one
that fingerprints request metadata that should NOT distinguish requests (e.g. `trace_id`, were
it ever added to the request) would start refusing legitimate retries. D3 adds a second, narrow
reader of `vm-images/*.json`'s on-disk shape (`delonix-vm::local_ports`, alongside `bins`'s
richer `VmImageStore`) — two readers of one file format is real duplication, accepted here as
smaller than the alternatives rejected above, but it means a future field rename in that JSON
shape has two call sites to update, not one.

**Known limitations, stated rather than hidden:**

- D2's "empty stored fingerprint is not compared" is a deliberate transition allowance, not a
  permanent exemption — it means a `request_id` collision against a record written **before**
  this lands (or by a mutation this pass did not reach) is not detected for up to
  `RETENTION_SECS` (7 days) after this ships. Not retroactively fixable: there is nothing to
  compare against for a record whose request content was never fingerprinted.
- D2 does not claim exactly-once execution of the underlying provider work on a retried,
  `Interrupted`-then-resubmitted request with a matching fingerprint — only that the engine's
  own ledger correctly recognizes it as the same request. This was already the state of the
  world before this ADR; it is named here so the fingerprint is not mistaken for closing that
  separate, larger question.
- D3 closes the gap only for VM creation through the node API's `spec.vcpus`/`spec.memory_bytes`
  fields. The module doc comment's "known gap" also named `backend` resolution; `ImageDefaults`
  carries a `backend` field for the same port to close that too, but this review did not trace
  every caller of backend selection to confirm no other divergence exists there — left to the
  implementing pass to verify, the same discipline this review applied to vcpus/memory.
- The adjacent `delonix_vm::set_network` gap (Context, above) is **not** addressed by D1–D3 and
  is called out so it is not conflated with D3's scope.

## What was not measured

- Whether a real deployment's REST/gRPC transport layer (axum/tonic, ahead of
  `operations::begin()`) adds its own per-connection buffering or retry behaviour that would
  change the 33–40% empirical collision rate measured directly against `begin()` — the review
  deliberately reproduced the race at the layer where it lives (`operations.rs`) rather than
  through a live socket, per this phase's scope (diagnosis, not a full integration harness).
- D2's exact wire shape for the new refusal (HTTP status, `DX-` number, gRPC code) — proposed in
  prose above, not assigned a number, since that is implementation work for the delivery phase,
  not this diagnostic pass.
- Whether any other mutation besides `Create`/`Delete` for `Network` and `VirtualMachine`
  (volumes, snapshots, start/stop) shares achado B's gap — `vm_ops.rs`'s `start`/`stop`/
  snapshot operations use `target = format!("VirtualMachine/{name}")` with **no** per-call
  content to diverge on (their "spec" is just the target name), so B likely does not apply to
  them structurally, but this was not traced function-by-function the way create was.

## Implemented

D1 and D2 landed together (`fix/operation-ledger-staging-race-and-fingerprint`); D3 landed
separately (`feat/vm-image-defaults-port`) — independent waves, no dependency between them.

- **D1**: `delonix_node::atomic::unique_tmp_suffix()` (`<pid>.<sequence>`, a process-wide
  `AtomicU64`) replaces the bare pid in `begin()`'s staged path
  (`.{id}.{unique_tmp_suffix()}.new`). Regression test folded into the existing concurrency
  test (`concurrent_begins_of_the_same_request_id_never_produce_a_hard_error`, 500 iterations):
  `total_new == ITERATIONS`, zero hard errors.
- **D2**: `operations::fingerprint_of(parts: &[(&str, &str)]) -> String` — versioned `fp1:`
  prefix, sorts parts, filters empty values (default-equals-omitted), unit-separator joined,
  `DefaultHasher`. `begin()`/`replay()` take a `fingerprint: &str` parameter; a stored,
  non-empty fingerprint that disagrees with the current request's is refused as
  `already_exists`, carrying the new dictionary entry **DX-5001**
  (`api_idempotency_key_reused`) in the response metadata — the `DX-` number this ADR left
  unassigned. An empty stored fingerprint is never compared, exactly as D2 specifies.
  `network_ops::create_fingerprint` covers `NetworkSpec` (deliberately excluding `topology`,
  documented in-line); `vm_ops::create_fingerprint` covers every field of `VmConfig` plus
  labels/annotations. Delete/stop/start/snapshot calls pass `""` (no content to diverge on,
  matching "What was not measured" above). New tests:
  `a_request_id_that_answered_the_same_verb_and_target_with_different_content_is_refused`,
  `an_empty_stored_fingerprint_is_never_compared`.
- **D3**: `delonix_compute::ports::ImageDefaults` (the struct) and `ImageDefaultsReader` (the
  trait — named distinctly from the struct, the one departure from this ADR's literal
  `trait ImageDefaults` wording) land in `delonix-compute::ports`, wired through
  `vm_registry::{set_image_defaults, image_defaults}` the same `OnceLock` shape `VmNetwork`
  already uses. Implemented by `delonix-vm::local_ports::VmImageJsonDefaults`, reading
  `<root>/vm-images/<sanitized-name>.json`'s three optional fields — a second, narrow reader of
  that JSON shape, the duplication named as accepted debt above. `vm_ops::config_from_spec`
  gained a `root: &Path` parameter and calls `delonix_vm::image_defaults(root, &spec.image)`
  before falling back to the fixed `(1, "1G")`. New test:
  `zero_vcpus_and_memory_resolve_the_named_images_own_recorded_defaults` (writes a real
  `vm-images/golden.json` fixture with `default_vcpus: 4, default_memory: "4G"`, asserts the
  resolved config matches, unnormalized).
- The `backend` resolution gap D3's "Known limitations" flagged for the implementing pass, and
  the adjacent `delonix_vm::set_network` gap, remain open — neither was in D3's scope.
