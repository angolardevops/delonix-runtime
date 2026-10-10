# Delivery plan — two independent changes (review 2026-10-09)

This is the handoff plan for ADR-0076, once accepted. Two waves, each self-contained, each
preserving everything that passes today (all 63 pre-existing `delonix-node-api` unit tests, and
the CLI's `resolve_vm_defaults_*` tests, were run unmodified during this review and are
unaffected by either wave).

## Wave 1 — the operation ledger (D1 + D2)

Scope: `crates/interfaces/delonix-node-api/src/operations.rs`, plus one call-site change per
mutation (`network_ops.rs`, `vm_ops.rs`) to pass a fingerprint into `begin`/`replay`.

1. **D1 first, alone** (smallest, zero contract/behaviour change beyond fixing the bug):
   - Add a per-attempt-unique suffix to the staged path (`operations.rs:236`). Prefer promoting
     a tiny `pub fn unique_suffix() -> String` out of `delonix_node::atomic`'s existing
     `TMP_SEQ`/pid discipline (`crates/contexts/delonix-node/src/atomic.rs:11,32`) over adding a
     second private counter — this crate already depends on `delonix-node`.
   - Fold `achado_a_operation_race.rs`'s two tests into `operations.rs`'s own `#[cfg(test)]`
     module (or keep them as an integration test — the review's files are not necessarily the
     final home, just a working reproduction against the real code).
   - Verify: 0 hard errors across the same 1500-iteration and 12-thread scenarios that reproduced
     33–40% today.
2. **D2 second, on top of D1** (touches `Record`'s shape and every mutation's call site):
   - `Record` gains `#[serde(default)] fingerprint: String`.
   - `begin`/`replay` take a `fingerprint: &str` parameter; the refusal condition in
     `replay()` (`operations.rs:267`) gains the third clause.
   - One fingerprint function per mutation that currently calls `begin`/`replay`:
     `CreateNetwork`/`DeleteNetwork` (`network_ops.rs:209,251`), `CreateVirtualMachine`/
     `DeleteVirtualMachine`/`StartVirtualMachine`/`StopVirtualMachine`/snapshot create-restore-
     delete (`vm_ops.rs:345,385,469,559` and the three snapshot call sites) — ADR-0076's "What
     was not measured" already flags that start/stop/snapshot likely have nothing to fingerprint
     beyond the target name itself (no per-call content to diverge on); confirm this per
     function rather than assume it, since that is exactly the kind of gap this review found by
     checking rather than assuming.
   - Assign the new refusal's `DX-` code from the existing dictionary process (ADR-0043) — not
     guessed in this plan.
   - Update `scripts/contract_gate.py`'s baseline if the new error surfaces through
     `codes::problem` the same way other refusals do.
3. Battery: the existing `achado_b_idempotent_identity.rs` scenarios become regression tests
   asserting REFUSAL instead of silent replay; `the_same_request_id_is_answered_once` and the
   two `the_same_create/delete_sent_twice_*` tests must still pass unchanged (same content, same
   key, real replay).

Risk notes for whoever picks this up: D1 is safe to ship alone and should not wait on D2 — it
fixes a measured, high-frequency production bug with no contract change. D2 is a judgment call
on exactly which fields belong in each mutation's fingerprint; get that wrong narrow and
Finding B reopens partially, get it wrong wide (including fields that should not distinguish
requests) and legitimate retries start being refused. Write the fingerprint function for each
mutation next to that mutation's own `config_from_spec`-equivalent, not as one generic function
guessing at every mutation's semantics.

## Wave 2 — VM image defaults (D3)

Scope: a new `ImageDefaults` port in `crates/contexts/delonix-compute/src/ports.rs`, its
`OnceLock` wiring in `vm_registry.rs` (mirroring `set_network`/`network()`,
`vm_registry.rs:410-429`), its implementation in `crates/adapters/delonix-vm/src/
local_ports.rs`, and the one call-site change in `vm_ops.rs::config_from_spec`.

1. Define the port and its return type (three optional fields: `vcpus`, `memory`, `backend`).
   Do **not** reuse or wrap `bins/delonix-runtime-bin`'s `VmImage`/`VmImageStore` — read the same
   three JSON keys directly, independently (ADR-0076 names the duplication cost and why it is
   accepted).
2. Wire `set_image_defaults`/`image_defaults()` the same shape as the existing `VmNetwork` pair.
   Decide where it gets seeded — `delonix-vm`'s own lazy `seeded()` is the natural place if this
   port should be available with zero explicit wiring from either `bin`'s or `node-api-bin`'s
   `main.rs` (the same reason `VmBackends`/`LocalDiskImages`/`SeedBuilder` need no explicit call
   from node-api today) — but `VmNetwork` is NOT seeded that way (it needs an explicit
   `set_network` call from `bin`'s `main.rs`, which `delonix-node-api` never makes — see ADR-0076's
   "Adjacent, not in scope" note). Confirm which pattern fits before copying either one verbatim.
3. `config_from_spec` calls `vm_registry::image_defaults()`, falls back to today's `(1, "1G")`
   when absent, exactly mirroring `resolve_vm_defaults`'s precedence (explicit request value
   wins; then the image; then the fixed default).
4. `achado_c_vm_defaults.rs`'s divergence test flips to asserting the CLI's answer; the other two
   scenarios in that file are already regression-safe and should pass unmodified.
5. Verify `cargo tree -e normal -p delonix-node-api` and `-p delonix-vm` carry no edge to
   `delonix-runtime-bin` (`scripts/arch_fitness.py` is the automated version of this check).

This wave does not touch `operations.rs` and can ship independently of Wave 1, in either order.

## What this plan does not schedule

- The adjacent `delonix_vm::set_network` gap named in ADR-0076's Context (not wired for
  `delonix-node-api`/`delonix-node-api-bin`) — needs its own, separately-scoped look at whether
  node-api's `main.rs`/`serve_blocking` should call it, and what `HostVmNetwork`'s dependencies
  (`delonix-sdn::vm_network`) look like from that binary.
- D3's unresolved `backend` resolution trace (ADR-0076 Known limitations) — the port carries the
  field; whether any OTHER divergence besides vcpus/memory exists needs its own check before
  Wave 2 is called complete.
