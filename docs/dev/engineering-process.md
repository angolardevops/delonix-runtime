# Engineering process (canonical)

This page is the **single source of truth** for how work flows through this repository: which
branch a change starts from, how it is built in isolation, how it is validated, how it lands, and
how its traces are cleaned up. AGENTS.md, [CONTRIBUTING.md](../../CONTRIBUTING.md), the
[contribution workflow](contributing-workflow.md) and the session skills all defer here for the
branch model and lifecycle — they do not restate it. When they disagree with this page, this page
wins; fix the other place rather than forking the rule.

> **This is a Delonix decision, not a claim about other projects.** The branch model below is
> GitFlow-shaped by the owner's decision (2026-10-23). Docker/Moby and Podman are PR-first,
> trunk-based projects; we borrow their *contribution* and *review* discipline (tests before merge,
> live validation of kernel-facing changes, DCO-style focused commits, CODEOWNERS review) — not
> their branch topology. Do not assert they use this flow.

## The two long-lived branches

- **`main`** is the released line. It only ever moves forward by a release or a hotfix. Its `HEAD`
  is what a user who installs from source gets.
- **`develop`** is the integration line. Everyday work is cut from it and lands back on it. A
  release promotes `develop` into `main`.

Both are fetched fresh before use; never branch from a stale local ref, and never from another
task's branch for convenience.

## Branch policy (mandatory)

Classify by the **nature of the problem**, not by the prefix someone suggested. The prefix follows
the classification; it does not drive it.

| Kind of work                | Branch prefix          | Starts from (fresh remote) | Normal destination            |
| --------------------------- | ---------------------- | -------------------------- | ----------------------------- |
| Feature                     | `feat/`                | `origin/develop`           | `develop`                     |
| Chore / maintenance         | `chore/`               | `origin/develop`           | `develop`                     |
| Refactor                    | `refactor/`            | `origin/develop`           | `develop`                     |
| Performance                 | `perf/`                | `origin/develop`           | `develop`                     |
| Documentation               | `docs/`                | `origin/develop`           | `develop`                     |
| Test / CI / build           | `test/` `ci/` `build/` | `origin/develop`           | `develop`                     |
| **Bug fix / hotfix**        | `fix/` `bug/` `hotfix/`| **`origin/main`**          | **`develop` and, when it affects the released line, `main`** |

Names are short and descriptive, with the task identifier when one exists:
`feat/DX-1234-http-route-timeouts`, `fix/compose-secret-lazy-read`.

### The fix/bug/hotfix rule, and its one real trap

A fix branches from `main` because a fix is, by default, a correction to what is published. The
same correction must also reach `develop` so the next release does not regress it.

The trap, which this policy forbids: **a defect that exists only on `develop`**. Do not branch it
from `main` (it will not reproduce there), and do not merge `develop` into `main` to make it
reproduce. A develop-only defect is corrected on a branch cut from `develop` and lands only on
`develop` — record that condition explicitly when you classify it. If this policy ever blocks a
coherent fix, surface the concrete conflict to the owner for a decision; do not silently bend the
rule.

### Carrying a fix to both destinations

When a fix targets the released line, it lands on `main` through the release gates and the same
change reaches `develop`. **Never merge `develop` into `main`** to carry an isolated fix across.
Choose merge, back-merge or `cherry-pick` per the linear-history convention below, record the
commit equivalence, and apply the change once — not twice. Authorisation to integrate into
`develop` does **not** authorise cutting a release, a tag, or publishing artefacts; those follow the
release process.

## One worktree per task

Several sessions write to this clone at once (measured: `main`'s `HEAD` moved four times during one
session, eight live worktrees, two regenerating the same file minutes apart). So every task gets its
own worktree and branch, and nothing is edited directly in the `main` or `develop` checkout.

```bash
git fetch origin --prune
# base is origin/develop for feat/chore/..., origin/main for fix/bug/hotfix
git worktree add -b <prefix>/<task> \
    <workspace>/.worktrees/delonix-runtime/<task> origin/<base>
cd <workspace>/.worktrees/delonix-runtime/<task>
export CARGO_TARGET_DIR=<repo>/target   # reuse the build cache safely
```

- **Never in `/tmp`** — it is cleared on boot and this host reboots; a reboot mid-task takes the
  uncommitted work. Use the persistent `.worktrees/` beside the repositories (gitignored by the
  workspace harness). Commit and push the branch **early and at every step with value**.
- **Isolate state, not just code.** A worktree isolates the files; it does **not** isolate the one
  shared Delonix engine on this host, nor ports, volumes or test fixtures. Do not share state files,
  ports, volumes or test resources between parallel tasks. Build caches may be shared only when the
  tool guarantees it is safe; never run a binary from another worktree without checking its
  identity (`target/release/delonix --version` + commit distance).
- **Stage by name** (`git add <file>`, never `-A`/`-u`/`.`), recognise every `git status --short`
  entry as yours, and **never `git checkout -- <path>`** in a shared tree — it destroys another
  session's uncommitted work without a stash.
- **Rebase, do not merge** on a rejected push (`git pull --rebase`): history is linear, no merge
  commits.
- **WIP cap: at most 3 open worktrees/sessions at once.** Opening a fourth forces closing or parking
  one, with a written handoff.

## The lifecycle of a prompt

Every new request runs this loop. A small task may be a single sprint — do not manufacture
ceremony; but do not confuse "implementation ready" with "done" either.

1. Recover the instructions, decisions and checkpoint (the session contract, `AGENTS.md` for the
   area, `docs/adr/` for the decisions).
2. Define objective, scope and acceptance criteria.
3. Classify the work and pick the base branch from the table above.
4. Identify the specialists needed (see "Delegation").
5. Build the dependency graph; sequence dependent fronts, parallelise independent ones.
6. Organise execution in short sprints sized to the work.
7. Create the worktree(s).
8. Implement, review, validate.
9. Integrate the finished work into `develop`.
10. Clean up the resources and update the checkpoint.

### Each sprint's contract

A sprint delivers a **concrete, usable change** — no stubs or fictional paths presented as complete,
and never a state that leaves `develop` functionally broken. It names: the functional result; the
tasks and their dependencies; the responsible specialists; acceptance criteria; risks; the tests;
the evidence collected; and the integration condition.

## Delegation to specialist agents

When the environment offers specialist agents, pick them by the problem (Rust/systems, OCI
lifecycle, CRI/CNI/Kubernetes, networking/isolation, virtualization/providers, cloud images,
CLI/DX, security, functional testing, CI/release). For each, state: objective, specialty, scope,
branch and worktree, the files/contracts it owns, dependencies, acceptance criteria, validation
commands, and delivery format. Independent fronts run in parallel; dependent fronts run in sequence
from an already-integrated contract. **Two agents never edit the same worktree at once.** If the
environment offers no agents, say so and run the responsibilities sequentially — do not simulate
delegation that did not happen.

The principal agent owns the architecture, the common contracts and the integration. When one front
depends on another: stabilise and validate the first, integrate it into `develop`, cut the next
front from the updated `develop`, and re-validate the consumed contract. Do not start parallel
implementations on incompatible assumptions about a schema, trait or API. Control build/test
concurrency to avoid saturating CPU, RAM and I/O.

## Quality before integration

Before a task is considered validated:

- Confirm the requirements and the relevant sources; review the diff.
- Run the gates the repository requires — `make ci` (or the subset the change touches, with the
  `rc=0` quoted). Zero clippy warnings is enforced, not advisory.
- Run tests proportional to the risk; a change that depends on the kernel, a provider or a protocol
  is validated **live**, and the PR says what was tested (command, expected vs. actual).
- Update the documentation and contracts the change affects
  (`scripts/dev_docs.py --check`, `scripts/dev_docs_site.py --check`).
- Confirm there are no secrets or accidental artefacts; check migration and compatibility where they
  apply.
- For relevant changes, a **reviewer agent different from the implementer** hunts for defects,
  regressions, test gaps and contract violations — approving the author's description is not review.

Do not change assertions, reduce isolation or ignore failures to get a green. Do not declare
integration validated when the material tests are missing.

## Integration into `develop` (mandatory)

Finished, validated work is integrated into `develop` in the **same work sequence**, when
permissions and gates allow. Integration is by **pull request, squash-merged, with the CI suite
green** — matching the linear history and Conventional Commits already in use. Before integrating:

1. `git fetch origin --prune` and check whether `develop` moved.
2. Resolve conflicts on the work branch (`git pull --rebase origin develop`).
3. Re-run the affected tests.
4. Confirm the exact candidate to integrate.
5. Obtain the required review.
6. Open/complete the PR against `develop`; let CI finish before merging.

Respect PRs, checks and protections. Never bypass a control with a direct push or a force-push. If a
protection blocks integration, leave the PR and the evidence ready and explain the exact blocker —
do not mark the work integrated.

CI runs the gate suite on push to `main` and `develop` and on pull requests targeting either (see
[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml)). A pushed branch with no PR does not,
by itself, prove "CI green".

## Cleanup after integration

After integration is **confirmed** — not assumed — verify: the destination branch locally and
remotely; that the change is effectively included; the required checks; no local changes left; no
un-preserved commits; and no agent/process still using the worktree. Then:

1. `git worktree remove <path>` (never `--force` to hide pending changes).
2. `git branch -d <branch>` (never `-D`).
3. Delete the remote branch if it belongs to the task and is no longer needed.
4. `git worktree prune` for stale references.
5. Update the task record / checkpoint.

A squash or cherry-pick means **commit identity changes** — prove inclusion by the PR, the diff and
`git cherry develop <branch>`, not by ancestry alone. Never remove someone else's branch or
worktree. A blocked resource stays identified with its reason and next step — no abandoned branches
without state.

## Definition of done, and the states on the way

A task is **CONCLUÍDA / DONE** only when: acceptance criteria met; review finished; the required
tests pass; documentation and contracts updated; integration into `develop` confirmed; any
additional integration into `main` resolved when it is in the authorised scope; the temporary
worktree and branch removed safely; and the checkpoint updated.

Use the explicit intermediate states in the session contract:

`EM IMPLEMENTAÇÃO` → `EM REVISÃO` → `BLOQUEADO NA VALIDAÇÃO` →
`VALIDADO, A AGUARDAR INTEGRAÇÃO` → `INTEGRADO, LIMPEZA PENDENTE` → `CONCLUÍDO`.

## The harness that enforces this

`scripts/gitflow.py` turns this page into runnable controls. It validates the repo and remote before
any mutation, supports `--dry-run`, is idempotent, preserves data, and propagates exit codes.

```bash
python3 scripts/gitflow.py classify "fix a crash in compose config"   # -> prefix + base
python3 scripts/gitflow.py start  <prefix>/<task>  [--dry-run]          # fetch, base, worktree, contract
python3 scripts/gitflow.py preflight                                    # remote, base freshness, stale worktree refs, WIP cap
python3 scripts/gitflow.py gates                                        # run the gates, collect evidence
python3 scripts/gitflow.py integrate  [--dry-run]                       # rebase, re-gate, push, open PR to develop
python3 scripts/gitflow.py done                                         # prove inclusion, checks, clean tree
python3 scripts/gitflow.py cleanup  [--dry-run]                         # remove worktree + branches safely
```

`make gitflow ARGS="<subcommand> ..."` is the same thing through the Makefile. Local hooks assist the
person at the keyboard; the CI checks and (when the owner enables them) the branch protections are
what hold the important rules. Instruction files are not a substitute for an executable control.
