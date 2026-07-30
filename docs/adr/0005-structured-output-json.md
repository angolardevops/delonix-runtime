# ADR-0005: Structured output (`-o json`) for listing commands

- **Status:** Accepted (contract + first slice; remaining commands are follow-up)
- **Date:** 2026-07-30
- **Deciders:** Walter (owner) + Chief Runtime Architect review (DevOps / SRE / Platform lens)
- **Related:** `AGENTS.md` ("Por fazer, deliberadamente: `--format json`… merece desenho próprio"),
  `cmd/output.rs` (the `Table` formatter), `dash --json` / `container inspect` (existing JSON).

## Context

Today every listing command (`container ls`, `vm ls`, `workload ls`, `network ls`, `volumes ls`,
`image ls`, `pod ls`, `secret ls`, …) prints only a human-aligned `output::Table`. The column
headers are **i18n'd** (translated per `--l18n`), and widths are measured from content — great for
humans, unusable for machines. `inspect` gives full per-object JSON, and `dash --json` gives a
snapshot, but there is no list-as-JSON.

Through the three operational lenses this is the single highest-leverage in-bounds gap:

- **Platform Engineering** — machine-readable list output is the foundation of the runtime's "API":
  no JSON, no GitOps/catalog/self-service tooling can build on top without brittle table scraping.
- **DevOps** — CI/CD pipelines consume JSON; parsing aligned tables breaks the moment a column grows
  or a value is translated.
- **SRE** — dashboards/alerts/runbooks need **stable field names**, not `awk` over columns.

Crucially, unlike the other open ADRs (0002 Phase 2b, 0003, 0004), this has a **real consumer now**
(any script/pipeline the user writes) — it is not building ahead of a hypothetical consumer.

Why an ADR (not just a flag): it is a **new API surface across ~10 commands**, and its value
depends on being **consistent** (same flag, same field-naming rules) — a contract worth fixing once.

## Decision

Add an opt-in `-o, --output <FORMAT>` flag (FORMAT ∈ `table` | `json`; default `table`) to listing
commands, emitting a JSON **array of typed rows**. Default behaviour (table) is byte-identical when
the flag is unused — zero change for existing users.

**Contract (the part that must stay consistent across all ~10 commands):**

1. **Flag:** `-o, --output <table|json>`, default `table`. (`-o` is the kubectl/platform standard;
   supersedes the older `--format` wording in `AGENTS.md` — Docker's `--format` means Go templates,
   which we deliberately do NOT implement.) A shared `output::OutputFormat` (clap `ValueEnum`) so
   every command uses the same enum, not ad-hoc strings.
2. **Shape:** a **JSON array** of objects, one per listed resource — even for zero (`[]`) or one.
   `to_string_pretty` (same as `dash --json`/`inspect`), newline-terminated.
3. **Field names are STABLE and language-independent** — lowercase `snake_case`, **never** the
   i18n'd table headers. `workload ls -o json` → `[{"type","name","status","info"}]`. The table
   headers may be translated; the JSON keys never are. This is the whole point (SRE: stable fields).
4. **Summary, not full detail.** `ls -o json` is the *list view* as JSON (the same columns the table
   shows). Full per-object detail stays `inspect` (already JSON). The two do not merge.
5. **`--output json` suppresses all human chrome** — no headers, no color, no warnings on stdout
   (warnings go to stderr as today). stdout is pure JSON, safe to pipe into `jq`.
6. **Errors stay errors.** `-o json` changes success output only; a failure is still a non-zero exit
   + stderr message (fail-closed), not a JSON error blob (unless a future ADR adds one deliberately).

**First slice (this PR):** `workload ls -o json` — the unified, most platform-facing surface.
`WorkloadRow` derives `Serialize` (its fields are already the stable names). The `OutputFormat`
enum + the emit helper land in `output.rs` so the remaining commands adopt them mechanically.

**Rollout status:**
- **Done:** `workload ls`, `container ps`, `vm ls`, `pod ls`, `network ls`, `volumes ls`,
  `secret ls` (key names only — never values), `storage ls`, `sharevolume ls`.
- **Remaining:** `image ls` only. Its `ImageCmd::Ls` is **dual-purpose** — it also serves
  `image --vm ls` (mapped to `VmImageCmd::Ls`). Adding `-o json` there must cover the vm-image
  path too, otherwise `image --vm ls -o json` would silently ignore the flag (violating the
  no-silent-failure guardrail). Deferred to a dedicated slice that touches both `cmd::image` and
  `cmd::vmimage` together — tracked, not a silent gap.

## Alternatives considered

- **`--format '{{json .}}'` (Docker Go-template style).** Rejected: a template engine in Rust is a
  large surface for little gain; `-o json` covers the real need. (This is why `AGENTS.md`'s
  "`--format json`" wording is superseded.)
- **`--json` boolean (like `dash --json`).** Rejected as the cross-command standard: not extensible
  (yaml later), and `-o` is what platform/SRE users reflexively type. (`dash --json` stays as-is —
  not worth a breaking change for one command.)
- **Reuse the i18n'd table headers as JSON keys.** Rejected hard: keys would change with `--l18n`,
  breaking every consumer in another locale — the exact fragility this ADR removes.
- **Emit full `inspect`-level objects from `ls`.** Rejected: `ls` is the summary view; conflating it
  with `inspect` bloats output and couples two surfaces. Keep them distinct.
- **Do nothing.** Rejected: it is the documented gap, has a present consumer, and blocks the
  platform/GitOps story.

## Consequences

**Easier:** the runtime becomes automatable — `workload ls -o json | jq` works; GitOps/monitoring
build on stable fields; CI stops scraping tables. The shared enum keeps the ~10 commands honest.

**Cost / debt:** ~10 commands to convert (each trivial but must land to be consistent — a half-done
rollout is a worse API than none, so the follow-up is tracked, and each command's JSON keys become a
**stability commitment** once shipped — renaming a key is a breaking change). Each command needs a
`Serialize` row struct mirroring its table.

**Guardrail audit:** daemonless ✅ · PaaS boundary ✅ (output format, no tenant) · no new dependency ✅
(`serde_json` already in the tree) · engine crates untouched ✅ (all in `-bin`) · no silent failure ✅
(default table unchanged; errors stay errors; follow-up commands documented, not silently missing).

## Note for the owner (DevOps hygiene, separate)

Observed this session: `main` has **no required status checks** — `gh pr merge --auto` merges
immediately instead of gating on CI (safe here only because we ran `sentinela` to confirm green
first). A DevOps/SRE improvement worth making (your call, it's a GitHub repo setting): enable branch
protection requiring `ci.yml` to pass before merge. Not done here — it changes repo settings.
