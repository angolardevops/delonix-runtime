# ADR-0007: The manifest schema is GENERATED from the code (`schemars`)

- **Status:** Accepted (implemented)
- **Date:** 2026-08-10
- **Deciders:** Walter (owner) + Chief Runtime Architect review
- **Related:** `docs/kinds.html` (hand-written today), `docs/gen.py`, `cmd/manifest.rs`
  (`*_SPEC_FIELDS`), ADR-0005 (`-o json` contract), `docs/discovery/47_IAC_REVISAO.md` §F12,
  `AGENTS.md` "Output: … Sem dependências novas".

## Context

The manifest schema — 18 Kinds, `ContainerSpec` alone with ~35 fields — exists in **two
independent places**: the Rust structs, which are the truth, and `docs/kinds.html`, written and
maintained by hand. Nothing links them.

This repo has already paid for that shape three times, all found in the v0.32.2 sweep: the docs
described `serve docker-api` as read-only (it had been full lifecycle since v0.26.0),
`cluster kubeadm` as having no HA (the automatic HAProxy landed in v0.13.0), and `network` as
realizing only `bridge` physically (`overlay` had been realized for several versions). Each was
written correctly once and then silently became a lie.

The IaC review (`47_IAC_REVISAO.md`) named it as the documentation gap that most directly
contradicts the stated goal — *«a documentação dá-lhe sustento para trabalhar sem ficar
travado»* — because the person writing a manifest has no way to check a field short of reading
Rust. There is also no `delonix explain`, the one command a `kubectl` user reaches for first.

Forces:
- **More manual review does not fix it.** The three divergences above were not caused by
  carelessness; they were caused by two sources of truth. A fourth review pass buys one more
  correct snapshot, not a property.
- **A JSON Schema is worth more than a doc page.** With `# yaml-language-server: $schema=…`, the
  editor gives completion, type checking and inline docs while the manifest is being written —
  which is a strictly better answer to "don't make me memorize anything" than any page.
- **`AGENTS.md` forbids new dependencies** for a container runtime, and that rule has already
  refused a table formatter. It has exactly one documented exception (`ratatui`, for `delonix
  dash`), taken explicitly by the owner and recorded so a later audit would not treat it as
  accidental.

## Decision

**Generate the schema from the spec structs with `schemars`, and make the generated schema the
single source for `delonix schema`, `delonix explain` and the field tables in `kinds.html`.**

This is the **second deliberate exception** to the no-new-dependency rule, taken under the same
discipline as `ratatui`:

- **Confined to `delonix-runtime-bin`.** The eight engine crates stay dependency-clean —
  verified, not assumed: `cargo tree -e normal` for each of `delonix-runtime`, `delonix-net`,
  `delonix-image`, `delonix-volume`, `delonix-vm`, `delonix-runtime-core`, `delonix-cri` and
  `delonix-mgmt` shows zero occurrences of `schemars`.
- **Measured footprint: 6 crates** (`schemars`, `schemars_derive`, `serde_derive_internals`,
  `dyn-clone`, `ref-cast`, `ref-cast-impl`). The derive half is proc-macro only, so it is a build
  dependency of the bin and reaches no shipped artifact of the engine.
- **The derive is the point.** The alternative that avoids the dependency — hand-writing a schema
  emitter next to the `*_SPEC_FIELDS` constants — reproduces the exact defect being fixed: a
  second place to keep in sync, with the type information duplicated by hand.

## Alternatives considered

- **Hand-written schema emitter.** Rejected: it is the same two-sources-of-truth shape, one level
  down. The `*_SPEC_FIELDS` constants already prove the pattern is survivable but not free — they
  need a test to stay aligned, and they only carry NAMES, not types, defaults or docs.
- **Keep maintaining `kinds.html` by hand, more carefully.** Rejected on evidence: three
  divergences, none from carelessness.
- **Derive the schema from a `Default` instance via serde round-trip.** Rejected: gives field
  names and rough types, but no enums, no required/optional distinction and no doc text — i.e.
  none of what makes editor completion useful.
- **Do nothing and document the gap.** Rejected: the gap is the stated goal of the work.

## Consequences

**Easier:** a manifest gets completion and validation in VS Code/Neovim from one `$schema`
comment; `delonix explain container.spec.ports` answers from the same source; `kinds.html` field
tables stop being hand-maintained; and a schema diff between releases becomes a real artifact
(the schema-changelog of ADR-0008's companion work).

**Cost / debt:** 6 crates of supply-chain surface, and the doc-comments on the spec structs
become **user-facing** — a note written for a maintainer now shows up in someone's editor. That
is a net gain, but it is a change in what those comments are for, and it should be said out loud
rather than discovered.

**Guardrail audit:** daemonless ✅ (a command that prints and exits) · PaaS boundary ✅ (schema of
the public manifest, no tenant) · engine crates untouched ✅ (measured) · no silent failure ✅
(a Kind without a typed spec is listed as such rather than omitted) · **new dependency ⚠️ —
accepted deliberately, documented here, second exception after `ratatui`.**
