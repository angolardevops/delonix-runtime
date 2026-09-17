# 10. Contribution workflow

This page is the "how we work" part of the handbook: where to make changes, which rules the gates
enforce and why, when a change needs a written decision first, and how to send it. The gates
themselves, and how to run them, are in [02 — Clone, build and test](02-build-and-test.md).

For anything non-trivial — a new command, a new manifest Kind, a change to namespace or cgroup
setup, a new backend — open an issue first and agree on the approach. It saves a rewrite.

## Start from the latest tag, not from memory

Before you write code:

```bash
git fetch --tags origin
git describe --tags --abbrev=0 origin/main          # the newest release
git log --oneline "$(git describe --tags --abbrev=0 origin/main)"..origin/main | wc -l   # how far main is past it
git log --oneline -- <path you will touch>          # what was already decided, fixed or removed there
```

Reading the history of the area you touch is not ceremony: a lot of this codebase is a record of
things that were tried, measured and changed. What was already decided or removed is not redone
out of unawareness. The long-form record lives in [`AGENTS.md`](../../AGENTS.md) (organised by
area) and in [`docs/adr/`](../adr/README.md).

## One worktree per task

Several people and tools often work on the same clone at the same time. Editing in a shared
checkout has already cost real work here: edits absorbed into someone else's commit, `HEAD`
switching branches in the middle of a task, and a green `cargo test` on a dirty tree that did not
prove `HEAD` compiled. So every task gets its own git worktree and branch, created from
`origin/main`:

```bash
git fetch origin
git worktree add -b <topic>/<task> <workspace>/.worktrees/delonix-runtime/<task> origin/main
cd <workspace>/.worktrees/delonix-runtime/<task>
```

- **Never put a worktree in `/tmp`.** Many systems empty `/tmp` on boot; a reboot mid-task takes the
  uncommitted work, and can leave half-written objects in the shared `.git`. Use a persistent
  directory outside the repository (the convention here is a `.worktrees/` directory next to the
  repositories), so no `grep -r`, `docker build` context or gate script picks it up.
- **Commit and push early**, at every step that passes its checks. A persistent worktree survives a
  reboot; a pushed branch survives everything else.
- **Stage files by name**: `git add <file> <file>`, never `git add -A`, `-u` or `.`. Check
  `git branch --show-current` and recognise every entry of `git status --short` as yours before
  committing.
- **Never `git checkout -- <path>`** in a tree someone else may be using: it reverts to `HEAD`
  without a stash and destroys their uncommitted work.
- **Rebase, do not merge**, when your push is rejected for divergence: `git pull --rebase`. The
  history is linear.
- **When the task is done, remove both** the worktree and the branch — the branch survives
  `worktree remove`, and that is how stale branches pile up:

  ```bash
  git worktree remove <path>
  git branch -D <topic>/<task>
  git worktree list
  ```

## Version alignment

The `version` in the root `Cargo.toml` is load-bearing: it selects which release `delonix-cri` is
downloaded from, it is reported as the Docker API `ServerVersion`, it is recorded in every backup,
and the release workflow compares it with the built binary. `scripts/version_gate.py` (CI job
`version`) allows exactly two states:

1. **Equal to the newest tag the commit contains** — all ordinary work. Do not bump the version in
   a feature PR, and do not use a `-dev` suffix (it would send the `delonix-cri` download to a
   release that does not exist).
2. **Greater, only in the release commit**, together with `docs/releases/v<version>.md`.

It also fails a branch that **does not contain the newest tag**: the branch started before that
release, and merging it as-is would undo what the release published. Rebase onto `origin/main`.

Between releases, `delonix --version` tells builds apart by commit and distance
(`commit: <hash> (+N commits since vX.Y.Z)`), because two builds with the same version number are
not the same build.

## Language: English in the code (LANG-01)

Identifiers, comments and user-facing messages are written in **English**. Portuguese reaches the
operator only through the translation catalogue:

- `bins/delonix-runtime-bin/src/cmd/po.rs` — `po::t("…")` for fixed strings, `po::tf("… {name} …",
  &[("name", value)])` for interpolated ones (named placeholders, because a translation may reorder
  them). CLI `--help` text is translated at runtime by `po::translate_help`.
- `bins/delonix-runtime-bin/data/pt.po` — the Portuguese entries, embedded in the binary and
  selected with `--l18n pt` or `DELONIX_L18N=pt`.

A missing catalogue entry degrades to English; a Portuguese string written directly in the code is a
bug. `scripts/lang_ratchet.py` (CI job `lang`) counts the Portuguese still left in identifiers,
comments and messages against `scripts/lang_baseline.json`. It is a **ratchet**, not a ceiling: it
fails when a count rises (new Portuguese entered) **and** when it falls without the baseline being
lowered. When you translate something, run `python3 scripts/lang_ratchet.py --update` and commit the
new baseline **in the same commit** as the translation. Tests in the CLI crate also check that
command help has a Portuguese entry — add one when you add a command or flag.

## Architecture rules the gates enforce

`scripts/arch_fitness.py` (CI job `arch`) enforces the structure decided in
[ADR-0040](../adr/0040-engine-restructuring-layers-ports-node-contract.md). The layer of each crate
and the allowed direction are listed in [05 — Architecture](05-architecture.md). What it means for
a change:

- **The engine knows no consumer.** No name of any product, platform, control plane, console or
  agent that uses the engine — nor tenant, account, plan or billing — in `crates/`, `bins/`,
  `proto/` or the manifests, comments included. A requirement that comes from a consumer is written
  as the generic capability it is, in the engine's own vocabulary, and only enters if it makes sense
  for any client. History that needs external names lives in `docs/`, never in code.
- **Dependencies point inward.** Foundation depends only on foundation; contexts do not depend on
  adapters; adapters and providers do not depend on interfaces; a binary composes **one** interface.
- **The directory is the layer.** A crate lives under `crates/<layer>/` matching its entry in the
  `LAYERS` table. A new crate enters `LAYERS` and the right directory in the same commit.
- **Dependency versions live only in the root** `[workspace.dependencies]`; a member crate writes
  `{ workspace = true, features = [...] }` and nothing else.
- **Exceptions name the phase that removes them.** An exception without a phase fails, and so does
  an exception that no longer applies.
- **Debt ratchets** — for example `self_exec_sites` (a library re-running the engine's own binary
  instead of calling a function), `library_prints` (`println!`/`eprintln!` in a library crate —
  libraries emit `tracing`, interfaces print), `env_writes` (`env::set_var`/`remove_var`) and
  `shared_error_imports` (an adapter or provider using the shared `Error` as its own instead of a
  crate error that converts into it). The current list is generated:

<!-- dev-docs:begin ratchets -->
`scripts/arch_fitness.py` keeps **5 debt ratchets** (baseline in `scripts/arch_baseline.json`):

- `self_exec_sites`
- `library_prints`
- `env_writes`
- `shared_error_imports`
- `raw_error_variant_matches`
<!-- dev-docs:end ratchets -->
Same semantics as the language ratchet (`--list`, `--update`).

Beyond what the gate can see, three principles decide reviews: **daemonless** (a new resident
process needs an ADR with evidence of what systemd units, timers or socket activation could not
do), **rootless-first** (privilege is an explicit, announced opt-in, never a silent default), and
**no silent failure** (a flag that is accepted and ignored is worse than a flag that does not exist
— refuse it with a clear error instead).

## When to write an ADR

Write an Architecture Decision Record in `docs/adr/` **before** the code when a change moves a
structural boundary, for example:

- a new backend or provider (a hypervisor, a storage system), or a new port;
- a new external dependency in an engine crate, or a new daemon or resident process;
- a new privilege boundary — which also needs a GO/NO-GO spike first;
- a change to the node contract, the manifest schema's stability, or the layer structure.

A routine feature inside an existing boundary does not need one. Format and the current list are in
[`docs/adr/README.md`](../adr/README.md): one file per decision, `NNNN-title.md`, in English.
**Accepted ADRs are never rewritten** — a new ADR supersedes them.

## Adding or changing a CLI command

- **Wire every entry point.** Several commands are reachable by more than one path (for example
  `delonix vm pull` and `delonix image vm pull`). Change all of them, then check each one with the
  binary you built — including shell completion, because the `clap` declaration you edited may
  not be the one the user's path parses. The completion engine can be probed directly;
  `_CLAP_COMPLETE_INDEX` is the position of the word being completed:

  ```bash
  COMPLETE=bash _CLAP_COMPLETE_INDEX=3 ./target/debug/delonix -- delonix image vm ''
  ```
- **Validate against the binary**, not the source: `./target/debug/delonix <group> <command> --help`
  and a real run with the state roots isolated (see [02](02-build-and-test.md#isolating-the-engines-state)).
- **Update the CLI baseline** (`scripts/cli-tree.sh --update`) in the same commit when you add or
  remove a leaf, and regenerate the site (`python3 docs/gen.py` with a release build) when help text
  changes.
- **Unit-test every new pure function** — parsers, validators, argument builders. This codebase has
  a long record of real bugs caught exactly there.
- **Classify errors.** Exit codes carry a class (not found, conflict, …) decided in one place from
  the error type; return the right `Error` variant rather than a generic one.

## Commits and pull requests

- One logical change per commit; say **why** in the message — the diff already shows what.
- Reference the issue when there is one.
- Open the PR against `main` and fill in [the template](../../.github/PULL_REQUEST_TEMPLATE.md):
  what you ran, and for runtime, namespace, cgroup or network code, what you ran **live** on a real
  host, with the command and its output.
- "It compiles" and "the command returned 0" do not close a change. Say what was proved and, just
  as explicitly, what was not validated and why.
- Every change is reviewed by the code owners listed in [`.github/CODEOWNERS`](../../.github/CODEOWNERS).

## Security-sensitive changes

Call it out explicitly in the PR when a change crosses a privilege or namespace boundary: user
namespace mapping, the network holder or its control socket, `setns`/`unshare`, capability or
seccomp handling, or path handling driven by user or manifest input. These get extra review.

If you found a **vulnerability** rather than a bug — privilege escalation, namespace escape, command
injection, path traversal — do not open a public issue or PR. Follow [`SECURITY.md`](../../SECURITY.md)
(GitHub Private Vulnerability Reporting).
