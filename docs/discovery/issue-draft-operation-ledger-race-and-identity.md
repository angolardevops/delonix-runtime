<!--
Draft GitHub issue — NOT published. Ready to paste into
`gh issue create --repo angolardevops/delonix-runtime --title "..." --body-file ...`
once the owner approves. Everything above the title line is this file's own
header and is not part of the issue body.
-->

# Title

`delonix-node-api`: the operation ledger loses legitimate retries to a race, and idempotency keys ignore request content

# Body

## Summary

Two related defects in `crates/interfaces/delonix-node-api/src/operations.rs`, found by a
line-by-line review and reproduced against the real code (commit `2b9087b0`, v5.0.0, `tests/
achado_a_operation_race.rs` and `tests/achado_b_idempotent_identity.rs` in the review branch):

1. **Concurrency**: `operations::begin()`'s exclusive-creation staging file is not unique per
   *attempt* (only per `(request_id, pid)`), so two threads racing the same `request_id` —
   ordinary behaviour of any client retry under `tokio::spawn_blocking`, which every mutation
   goes through with zero serialization above it — share one staging inode. Measured: **33–40%
   of racing retries get a hard `Internal`/ENOENT error instead of the clean replay ADR-0042
   promises** ("the same request sent again is answered with that operation").
2. **Identity**: `operations::replay()` decides "is this the same request?" by comparing only
   `(verb, target)`, where `target` is just the resource's bare name
   (`format!("VirtualMachine/{name}")` / `format!("Network/{name}")`). A `request_id` reused
   across two requests that differ in `spec` (image, vcpus, memory, namespace, …) is silently
   answered with the FIRST request's `Operation` — the second request's distinct content never
   reaches validation or the work closure.

Full analysis, line numbers, and the alternatives considered: `docs/adr/0076-operation-ledger-
race-identity-and-vm-image-defaults.md`, Context sections A and B, Decision D1/D2.

## Reproduction

### Finding 1 (race)

```
cargo test -p delonix-node-api --test achado_a_operation_race -- --nocapture --test-threads=1
```

Two threads call `begin(root, "create", "Network/lab", &same_request_id)` behind a
`std::sync::Barrier`, repeated 1500 times with a fresh `request_id` each iteration. Expected:
every pair resolves to exactly one `Begun::New` and one `Begun::Replay`. Observed (two
independent runs): `New=1500, Replay=911, Err=589` and `New=1500, Replay=999, Err=501` — i.e.
33–40% of the non-executing thread in each pair gets:

```
status: Internal, message: "operation record: creating: No such file or directory (os error 2)"
```

### Finding 2 (identity)

```
cargo test -p delonix-node-api --test achado_b_idempotent_identity -- --nocapture --test-threads=1
```

`create_with_fn` (the crate's own injectable test seam) called twice with the same
`request_id="same-key"`, `name="web"`, but request 1 is `image="alpine:a", vcpus=2,
memory=2048M` and request 2 is `image="ubuntu:b", vcpus=16, memory=65536M`. The work closure —
which would realize the VM — is recorded as called exactly **once**, with request 1's content;
`op1.id == op2.id`. The client of request 2 is told "succeeded" describing a VM it did not ask
for.

## Root cause

**Finding 1**: `operations.rs:236`, `std::fs::write(&staged, bytes)` opens the existing path
`.{id}.{pid}.new` in place (truncate, same inode) rather than creating a fresh one, because the
staged name has no per-attempt uniqueness. `delonix_node::write_atomic_mode`
(`crates/contexts/delonix-node/src/atomic.rs:21-33`) already names this exact failure mode in
its own doc comment and fixes it with a `pid + sequence` suffix; `operations::begin()`'s staging
path predates or missed that discipline.

**Finding 2**: `operations.rs:267`, `found.verb != verb || found.target != target` is the entire
identity check. `target` carries only the resource name (`network_ops.rs:209,251`,
`vm_ops.rs:345,385,469`) — nothing from the request body.

## Proposed fix

See ADR-0076, Decision D1 and D2:

- **D1**: stage at `.{id}.{pid}.{seq}.new` with a process-wide atomic sequence (reusing or
  mirroring `delonix_node::atomic`'s discipline). One-line change to the staged path, no change
  to the surrounding `hard_link`/`AlreadyExists`/`replay` logic.
- **D2**: `Record` gains `#[serde(default)] fingerprint: String`; each mutation computes a
  canonical, versioned fingerprint of its semantically-relevant fields (map-order-independent,
  omitted-equals-default) and passes it to `begin`/`replay`, which refuse a `(verb, target)`
  match whose fingerprint disagrees (a new, distinct error — not silently replayed, not silently
  treated as a second operation). An empty stored fingerprint (legacy records, or a mutation not
  yet updated) is not compared, bounded by `RETENTION_SECS` (7 days).

## Acceptance criteria

- [ ] `achado_a_operation_race.rs`'s two tests pass at 0 hard errors across at least 1500
      iterations and a 12-thread wide race (or the equivalent coverage is folded into
      `operations.rs`'s own `#[cfg(test)]` module — this review's files are a reproduction, not
      necessarily the final test shape).
- [ ] A new regression test proves: two `CreateVirtualMachineRequest`s sharing a `request_id`
      and resource name but differing in `spec.image`/`vcpus`/`memory_bytes` are **refused**
      (distinct error, not a silent replay, not a second operation) rather than one being
      silently answered with the other's result.
- [ ] A reused `request_id` with the SAME content (genuine retry) continues to replay cleanly —
      no regression on `operations::tests::the_same_request_id_is_answered_once` or
      `network_ops::tests::the_same_create_sent_twice_does_the_work_once` /
      `vm_ops::tests::the_same_create_sent_twice_boots_once`.
- [ ] A legacy on-disk `Record` with no `fingerprint` field still deserializes and still replays
      against a matching `(verb, target)` (no `#[serde(default)]` regression).
- [ ] `scripts/contract_gate.py` / `buf breaking` stay green (no wire-breaking change —
      `fingerprint` is server-internal state in `Record`, not a new proto field, unless the
      delivery decides the refusal needs a new `DX-` code surfaced in the response, which is an
      additive contract change).
- [ ] E2E battery green.

## Severity / scope

High for Finding 1 (production-reachable under ordinary client retry behaviour, no fault
injection needed); Medium-High for Finding 2 (requires a client-side key-reuse bug, but the
engine gives no signal when it happens and the result looks like success).

Not in scope for this issue (see ADR-0076 Context, "Adjacent, not in scope"):
`delonix_vm::set_network` is never called by `delonix-node-api`/`delonix-node-api-bin`, so a VM
create naming a network attachment fails today — tracked separately.
