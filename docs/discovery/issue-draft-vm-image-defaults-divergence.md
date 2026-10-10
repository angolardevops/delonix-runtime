<!--
Draft GitHub issue — NOT published. Ready to paste into
`gh issue create --repo angolardevops/delonix-runtime --title "..." --body-file ...`
once the owner approves.
-->

# Title

`delonix-node-api`: `CreateVirtualMachine` ignores the named image's recorded vCPU/memory defaults, unlike the CLI

# Body

## Summary

`vm_ops::config_from_spec` (`crates/interfaces/delonix-node-api/src/vm_ops.rs:176-184`) resolves
`spec.vcpus == 0` / `spec.memory_bytes == 0` to a fixed `(1 vCPU, 1 GiB)`, never consulting the
named image's own recorded defaults (`default_vcpus`/`default_memory` in `<root>/vm-
images/<name>.json`). This contradicts the contract's own field comment
(`proto/delonix/node/v1/compute.proto:550-551`):

```proto
int32 vcpus = 2;        // 0 = image default, then 1
int64 memory_bytes = 3; // 0 = image default, then 1 GiB
```

and diverges from `bins/delonix-runtime-bin/src/cmd/vm.rs::resolve_vm_defaults`, which DOES
consult the image for the identical "nothing given" case. The module's own doc comment already
names this as a known, deliberate gap, blocked on ADR-0040's layering rule (an interface crate
cannot depend on a bin crate, where `VmImageStore` lives today).

Full analysis: `docs/adr/0076-operation-ledger-race-identity-and-vm-image-defaults.md`, Context
section C, Decision D3.

## Reproduction

```
cargo test -p delonix-node-api --test achado_c_vm_defaults -- --nocapture --test-threads=1
```

A fixture `<root>/vm-images/golden.json` is written in the exact shape and location
`VmImageStore::save` produces, recommending `default_vcpus: 4, default_memory: "4G"`.
`CreateVirtualMachine` with `spec.image="golden", vcpus=0, memory_bytes=0` resolves, through the
node API, to `vcpus=1, memory="1G"` — the fixture is never read. The CLI's own existing,
unmodified test for the identical scenario corroborates the divergence:

```
cargo test -p delonix-runtime-bin resolve_vm_defaults_cai_para_a_imagem_quando_o_cli_nao_diz_nada
```

resolves the same input to `(4, "4G")`.

Explicit non-zero `vcpus`/`memory_bytes` are honoured identically by both paths (not the gap);
an image reference with no recorded metadata at all falls back to `(1, "1G")` in both (also not
the gap) — the divergence is specifically: image exists, has its own recorded defaults, request
says 0.

## Proposed fix

See ADR-0076, Decision D3: a minimal `ImageDefaults` port in `delonix_compute::ports` (three
fields: `vcpus`, `memory`, `backend` — not `VmImage`'s full build/registry metadata), wired via
a process-wide `OnceLock` the same way `delonix_compute::vm_registry` already wires
`VmNetwork` (`set_network`/`network()`), implemented in `delonix-vm::local_ports` by reading
`<root>/vm-images/<name>.json`'s three optional fields directly — independent of `bins/delonix-
runtime-bin`'s `VmImageStore`, so no `interfaces`→`bins` dependency is introduced.

## Acceptance criteria

- [ ] `achado_c_vm_defaults.rs`'s `zero_vcpus_and_memory_ignore_the_named_images_own_recorded_
      defaults` is updated to assert the NEW behaviour (resolves to the image's `4`/`"4096M"`,
      not the fixed `1`/`"1G"`) and passes.
- [ ] `explicit_vcpus_and_memory_are_honored_regardless_of_the_image` and
      `no_image_metadata_falls_back_to_one_vcpu_one_gig_like_the_cli_does` continue to pass
      unchanged (no regression on the two scenarios that were never the gap).
- [ ] `cargo tree -e normal -p delonix-node-api` and `-p delonix-vm` show no new dependency on
      `delonix-runtime-bin` (the guardrail this fix exists to respect).
- [ ] `scripts/arch_fitness.py` stays green (no new disallowed crate-direction edge).
- [ ] A regression test proves `backend` resolution (the module doc comment's other named
      "nothing given" field) is traced the same way vcpus/memory were, and either also converges
      with the CLI or is explicitly scoped out with a measured reason (ADR-0076's "Known
      limitations" flags this as unverified by this review).
- [ ] E2E battery green.

## Severity / scope

Medium: silent to the API caller (nothing in the response signals the image's recommendation was
ignored), and the only contract a caller has (the proto field comment) promises the opposite.
Requires no concurrency or adversarial input — reproduces on the first call with an image that
has recorded defaults.

Not in scope for this issue (see ADR-0076 Context, "Adjacent, not in scope"):
`delonix_vm::set_network` is never called by `delonix-node-api`/`delonix-node-api-bin` — a
`CreateVirtualMachine` naming a network attachment fails today regardless of this fix; tracked
separately.
