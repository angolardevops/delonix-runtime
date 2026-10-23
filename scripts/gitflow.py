#!/usr/bin/env python3
"""Delonix engineering-process harness.

Turns docs/dev/engineering-process.md into runnable controls: classify work,
start a task in its own worktree off the right base, check preconditions, run
the gates, prepare integration into `develop`, prove a task is done, and clean
up — validating the repo and remote before any mutation.

Design contract (see docs/dev/engineering-process.md):
  * validate repo + remote before mutating;
  * `--dry-run` previews; outward/destructive steps need `--execute` to act;
  * idempotent (re-running a completed step is a no-op, not an error);
  * preserve data (never -D / --force / git add -A / checkout --);
  * actionable errors, propagated exit codes, no fixed paths, no secrets.

stdlib only; Python 3.8+.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

ORIGIN_SLUG = "delonix-runtime"
REMOTE = "origin"
INTEGRATION = "develop"
RELEASED = "main"
WIP_CAP = 3

# Classification: prefix -> base branch. Order matters (first match wins on the
# keyword scan below). fix/bug/hotfix branch from the released line; everything
# else from the integration line.
PREFIX_BASE = {
    "hotfix": RELEASED,
    "fix": RELEASED,
    "bug": RELEASED,
    "feat": INTEGRATION,
    "chore": INTEGRATION,
    "refactor": INTEGRATION,
    "perf": INTEGRATION,
    "docs": INTEGRATION,
    "test": INTEGRATION,
    "ci": INTEGRATION,
    "build": INTEGRATION,
}

# Keyword -> prefix, scanned to suggest a classification from free text. A fix
# wins over a feature when both signals are present, because a correction to
# existing behaviour is the higher-stakes classification.
# The `\w*` after each group lets truncated Portuguese stems (corrig-, documenta-,
# manuten-, reestrutur-, otimiz-) match their inflected forms (corrigir,
# documentação, …). Advisory only: on no match classify falls back to feat.
KEYWORD_PREFIX = [
    (r"\b(hotfix|urgent|regression|regress[aã]o)\w*\b", "hotfix"),
    (r"\b(fix|bug|crash|broken|falha|corrig|defect|defeito|panic|leak)\w*\b", "fix"),
    (r"\b(refactor|reestrutur|rename|renomea)\w*\b", "refactor"),
    (r"\b(perf|performance|slow|lento|optimi[sz]|otimiz)\w*\b", "perf"),
    (r"\b(doc|docs|documenta|handbook|readme)\w*\b", "docs"),
    (r"\b(test|teste|coverage|cobertura)\w*\b", "test"),
    (r"\b(ci|pipeline|workflow|release)\w*\b", "ci"),
    (r"\b(build|compile|toolchain|makefile)\w*\b", "build"),
    (r"\b(chore|maintenance|manuten|cleanup|bump|deps|dependenc)\w*\b", "chore"),
    (r"\b(feat|feature|add|new|funcionalidade|support|suporte)\w*\b", "feat"),
]


class GitFlowError(Exception):
    """A user-actionable failure. Message is printed; exit code is 1."""


# --------------------------------------------------------------------------- #
# process plumbing
# --------------------------------------------------------------------------- #
def run(cmd, *, cwd=None, check=True, capture=True):
    """Run a command. Returns CompletedProcess. Raises GitFlowError on failure
    when check=True, with the stderr folded into an actionable message."""
    proc = subprocess.run(
        cmd,
        cwd=cwd,
        text=True,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.PIPE if capture else None,
    )
    if check and proc.returncode != 0:
        err = (proc.stderr or proc.stdout or "").strip()
        raise GitFlowError(f"command failed ({proc.returncode}): {' '.join(cmd)}\n{err}")
    return proc


def git(args, *, cwd=None, check=True, capture=True):
    return run(["git", *args], cwd=cwd, check=check, capture=capture)


def git_out(args, *, cwd=None):
    return git(args, cwd=cwd).stdout.strip()


# --------------------------------------------------------------------------- #
# repo / remote discovery and validation
# --------------------------------------------------------------------------- #
def repo_root(cwd=None) -> Path:
    """The MAIN worktree root (parent of the shared git dir), not the current
    worktree — so task state lives in one place regardless of where we run."""
    common = git_out(["rev-parse", "--path-format=absolute", "--git-common-dir"], cwd=cwd)
    p = Path(common)
    # common dir is <main-root>/.git for a normal/linked worktree; its parent is
    # the root. A custom/bare gitdir is its own root.
    return p.parent if p.name == ".git" else p


def validate_remote(cwd=None):
    """Confirm we are in the delonix-runtime repo with the expected origin,
    before any mutation. Fail closed with an actionable message otherwise."""
    try:
        url = git_out(["remote", "get-url", REMOTE], cwd=cwd)
    except GitFlowError:
        raise GitFlowError(
            f"no '{REMOTE}' remote here — run inside the delonix-runtime clone."
        )
    if ORIGIN_SLUG not in url:
        raise GitFlowError(
            f"origin '{url}' is not the {ORIGIN_SLUG} repo; refusing to mutate "
            f"the wrong repository."
        )
    return url


def worktrees_dir(root: Path) -> Path:
    """Where task worktrees live. Never /tmp. Honour DELONIX_WORKTREES_DIR;
    else the nearest ancestor that already holds a `.worktrees/`; else the
    repo's parent. No fixed absolute paths."""
    env = os.environ.get("DELONIX_WORKTREES_DIR")
    if env:
        return Path(env)
    for ancestor in [root, *root.parents]:
        cand = ancestor / ".worktrees"
        if cand.is_dir():
            return cand / ORIGIN_SLUG
    return root.parent / ".worktrees" / ORIGIN_SLUG


def registry_path(root: Path) -> Path:
    # .claude/ is gitignored in this repo — correct home for local task state.
    return root / ".claude" / "gitflow" / "registry.json"


def load_registry(root: Path) -> dict:
    p = registry_path(root)
    if p.exists():
        try:
            return json.loads(p.read_text())
        except json.JSONDecodeError:
            return {"tasks": {}}
    return {"tasks": {}}


def save_registry(root: Path, reg: dict):
    p = registry_path(root)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(json.dumps(reg, indent=2, sort_keys=True) + "\n")


def integration_ref(root) -> str:
    """The ref to prove inclusion against: prefer origin/<develop> (the real
    integration tip), fall back to local develop only if the remote-tracking ref
    is absent. Callers must fail CLOSED if `git cherry` against it errors."""
    remote_ref = f"{REMOTE}/{INTEGRATION}"
    if git(["rev-parse", "--verify", "-q", remote_ref], cwd=root, check=False).returncode == 0:
        return remote_ref
    return INTEGRATION


def pending_commits(root, branch, *, target) -> list:
    """Commits of `branch` not yet in `target`, by content (squash-safe). Raises
    GitFlowError when inclusion cannot be determined — never silently "none"."""
    cherry = git(["cherry", target, branch], cwd=root, check=False)
    if cherry.returncode != 0:
        raise GitFlowError(
            f"cannot prove whether {branch} is in {target} "
            f"(git cherry failed: {(cherry.stderr or '').strip()})."
        )
    return [l for l in cherry.stdout.splitlines() if l.startswith("+")]


def branch_prefix(branch: str) -> str:
    return branch.split("/", 1)[0] if "/" in branch else branch


def base_for_branch(branch: str) -> str:
    prefix = branch_prefix(branch)
    if prefix not in PREFIX_BASE:
        raise GitFlowError(
            f"branch '{branch}' has no recognised prefix. Expected one of: "
            f"{', '.join(sorted(PREFIX_BASE))} (see the branch policy table)."
        )
    return PREFIX_BASE[prefix]


def list_worktrees(cwd=None):
    """[(path, branch)] for every worktree, branch without refs/heads/."""
    out = git_out(["worktree", "list", "--porcelain"], cwd=cwd)
    entries, path, branch = [], None, None
    for line in out.splitlines():
        if line.startswith("worktree "):
            path = line[len("worktree ") :]
        elif line.startswith("branch "):
            branch = line[len("branch ") :].replace("refs/heads/", "")
        elif line == "":
            if path is not None:
                entries.append((path, branch))
            path, branch = None, None
    if path is not None:
        entries.append((path, branch))
    return entries


def task_worktrees(cwd=None):
    """Worktrees that are NOT the main/develop checkout — i.e. real task WIP."""
    return [
        (p, b) for p, b in list_worktrees(cwd=cwd)
        if b not in (None, RELEASED, INTEGRATION)
    ]


# --------------------------------------------------------------------------- #
# subcommands
# --------------------------------------------------------------------------- #
def cmd_classify(args):
    text = " ".join(args.text).lower()
    if not text.strip():
        raise GitFlowError("classify needs a description of the work.")
    prefix = "feat"  # safe default: additive, lands on develop
    for pattern, pfx in KEYWORD_PREFIX:
        if re.search(pattern, text):
            prefix = pfx
            break
    base = PREFIX_BASE[prefix]
    print(f"prefix: {prefix}")
    print(f"base:   origin/{base}")
    print(f"suggest: {prefix}/<task>")
    if base == RELEASED:
        print(
            "note: fixes branch from main; a defect that exists ONLY on develop "
            "must instead be cut from develop (see engineering-process.md)."
        )
    return 0


def _fetch(root, dry):
    if dry:
        print(f"DRY-RUN: git fetch {REMOTE} --prune")
        return
    git(["fetch", REMOTE, "--prune"], cwd=root, capture=False)


def cmd_start(args):
    root = repo_root()
    validate_remote(cwd=root)
    branch = args.branch
    base = base_for_branch(branch)
    task = branch.split("/", 1)[1] if "/" in branch else branch
    task = re.sub(r"[^A-Za-z0-9._-]", "-", task)
    wt = worktrees_dir(root) / task
    dry = args.dry_run

    # idempotent: a worktree already on this branch is success, not an error.
    for p, b in list_worktrees(cwd=root):
        if b == branch:
            print(f"already started: {branch} at {p}")
            return 0

    n_wip = len(task_worktrees(cwd=root))
    if n_wip >= WIP_CAP and not dry:
        raise GitFlowError(
            f"WIP cap reached ({n_wip}/{WIP_CAP} open task worktrees). Close or "
            f"park one (with a written handoff) before starting another."
        )

    _fetch(root, dry)
    base_ref = f"{REMOTE}/{base}"

    if dry:
        print(f"DRY-RUN: git worktree add -b {branch} {wt} {base_ref}")
        print(f"DRY-RUN: register task '{task}' (base {base}) and write contract")
        return 0

    if wt.exists():
        raise GitFlowError(
            f"path {wt} already exists but is not a worktree on {branch}; "
            f"resolve it by hand (refusing to clobber)."
        )
    git(["worktree", "add", "-b", branch, str(wt), base_ref], cwd=root, capture=False)

    reg = load_registry(root)
    reg["tasks"][branch] = {
        "task": task,
        "base": base,
        "worktree": str(wt),
        "state": "EM IMPLEMENTAÇÃO",
    }
    save_registry(root, reg)

    contract = wt / ".claude" / "sessions" / f"{branch.replace('/', '-')}.md"
    contract.parent.mkdir(parents=True, exist_ok=True)
    if not contract.exists():
        contract.write_text(
            f"---\nbranch: {branch}\nbase: {base}\nworktree: {wt}\n"
            f"status: EM IMPLEMENTAÇÃO\n---\n\n# Objectivo\n\n<um objectivo>\n\n"
            f"## Done when (observável)\n\n- [ ] <critério>\n\n## Fora de âmbito\n\n- <...>\n"
        )
    print(f"started {branch} (base {base}) at {wt}")
    print(f"contract: {contract}")
    return 0


def cmd_preflight(args):
    root = repo_root()
    validate_remote(cwd=root)
    problems, notes = [], []

    _fetch(root, args.dry_run)

    for base in (INTEGRATION, RELEASED):
        local = git(["rev-parse", "--verify", "-q", base], cwd=root, check=False)
        remote = git(["rev-parse", "--verify", "-q", f"{REMOTE}/{base}"], cwd=root, check=False)
        if local.returncode == 0 and remote.returncode == 0:
            if local.stdout.strip() != remote.stdout.strip():
                notes.append(f"local {base} differs from {REMOTE}/{base}; branch from the remote ref.")

    n_wip = len(task_worktrees(cwd=root))
    if n_wip > WIP_CAP:
        problems.append(f"WIP cap exceeded: {n_wip}/{WIP_CAP} open task worktrees.")
    else:
        notes.append(f"WIP: {n_wip}/{WIP_CAP} open task worktrees.")

    for p, b in task_worktrees(cwd=root):
        if not Path(p).exists():
            problems.append(f"worktree record points at missing path {p} (run: git worktree prune).")

    for line in (notes):
        print(f"note: {line}")
    for line in problems:
        print(f"PROBLEM: {line}")
    if problems:
        return 1
    print("preflight: ok")
    return 0


def cmd_gates(args):
    cwd = os.getcwd()
    # Non-mutating; run everything where the caller is (inside their worktree),
    # so a worktree's own uncommitted changes are what gets gated.
    steps = [
        ("fmt-check", ["cargo", "fmt", "--all", "--check"]),
        ("gates", ["make", "gates"]),
        ("gitflow-selftest", [sys.executable, str(Path(cwd) / "scripts" / "test_gitflow.py")]),
    ]
    if args.full:
        steps.append(("ci", ["make", "ci"]))
    evidence = []
    failed = False
    for name, cmd in steps:
        print(f"\n=== {name}: {' '.join(cmd)} ===")
        proc = run(cmd, cwd=cwd, check=False, capture=False)
        evidence.append({"step": name, "cmd": " ".join(cmd), "rc": proc.returncode})
        if proc.returncode != 0:
            failed = True
            if not args.keep_going:
                break
    ev_path = Path(cwd) / ".claude" / "gitflow" / "last-gates.json"
    ev_path.parent.mkdir(parents=True, exist_ok=True)
    ev_path.write_text(json.dumps({"evidence": evidence}, indent=2) + "\n")
    print(f"\nevidence: {ev_path}")
    return 1 if failed else 0


def _current_branch(cwd):
    return git_out(["rev-parse", "--abbrev-ref", "HEAD"], cwd=cwd)


def _tree_clean(cwd):
    return git_out(["status", "--porcelain"], cwd=cwd) == ""


def cmd_integrate(args):
    cwd = os.getcwd()
    root = repo_root(cwd=cwd)
    validate_remote(cwd=cwd)
    branch = _current_branch(cwd)
    if branch in (RELEASED, INTEGRATION):
        raise GitFlowError(f"on '{branch}' — integrate is run from a task branch, not {branch}.")
    base = base_for_branch(branch)
    act = args.execute and not args.dry_run

    if not _tree_clean(cwd):
        raise GitFlowError("working tree has uncommitted changes; commit or stash before integrating.")

    print(f"branch {branch} (base {base}) -> integrate into {INTEGRATION}")
    _fetch(root, not act)

    plan = [
        ["git", "pull", "--rebase", REMOTE, INTEGRATION],
        ["python3", "scripts/gitflow.py", "gates"],
        ["git", "push", "-u", REMOTE, branch],
        ["gh", "pr", "create", "--base", INTEGRATION, "--head", branch,
         "--fill", "--label", "gitflow"],
    ]
    if not act:
        print("PLAN (pass --execute to run):")
        for c in plan:
            print("  " + " ".join(c))
        print("note: merge the PR squash + CI green; never force-push.")
        return 0

    git(["pull", "--rebase", REMOTE, INTEGRATION], cwd=cwd, capture=False)
    if cmd_gates(argparse.Namespace(full=False, keep_going=False)) != 0:
        raise GitFlowError("gates failed after rebase; not pushing.")
    git(["push", "-u", REMOTE, branch], cwd=cwd, capture=False)
    pr = run(["gh", "pr", "create", "--base", INTEGRATION, "--head", branch,
              "--fill", "--label", "gitflow"], cwd=cwd, check=False, capture=False)
    if pr.returncode != 0:
        print("note: branch pushed; open the PR to develop by hand (gh unavailable or PR exists).")
    return 0


def cmd_done(args):
    cwd = os.getcwd()
    branch = _current_branch(cwd)
    problems = []
    if not _tree_clean(cwd):
        problems.append("working tree not clean.")
    # inclusion by content (squash-safe), not ancestry, against the fetched
    # integration tip so a stale local ref doesn't hide a landed change.
    git(["fetch", REMOTE, INTEGRATION], cwd=cwd, check=False)
    target = integration_ref(cwd)
    pending = pending_commits(cwd, branch, target=target)
    if pending:
        problems.append(f"{len(pending)} commit(s) not yet in {target} "
                        f"(git cherry {target} {branch}).")
    for line in problems:
        print(f"PROBLEM: {line}")
    if problems:
        return 1
    print(f"done: {branch} content is present in {INTEGRATION}; tree clean.")
    return 0


def cmd_cleanup(args):
    root = repo_root()
    validate_remote(cwd=root)
    branch = args.branch
    base = base_for_branch(branch)
    act = args.execute and not args.dry_run
    reg = load_registry(root)
    wt = None
    for p, b in list_worktrees(cwd=root):
        if b == branch:
            wt = p
            break

    # Safety: prove inclusion before removing anything. Fail CLOSED — when
    # inclusion cannot be determined (e.g. the integration ref is missing), refuse
    # rather than tear down the worktree. --force-unmerged is the explicit override.
    git(["fetch", REMOTE, INTEGRATION], cwd=root, check=False)
    if not args.force_unmerged:
        target = integration_ref(root)
        pending = pending_commits(root, branch, target=target)
        if pending:
            raise GitFlowError(
                f"{len(pending)} commit(s) of {branch} are not in {target}; "
                f"refusing to clean up (would lose work). Integrate first, or pass "
                f"--force-unmerged only if you have proven inclusion another way."
            )
    if wt and not _tree_clean(wt):
        raise GitFlowError(f"worktree {wt} has uncommitted changes; refusing to remove it.")

    plan = []
    if wt:
        plan.append(["git", "worktree", "remove", wt])
    plan.append(["git", "branch", "-d", branch])
    if args.remote:
        plan.append(["git", "push", REMOTE, "--delete", branch])
    plan.append(["git", "worktree", "prune"])

    if not act:
        print("PLAN (pass --execute to run):")
        for c in plan:
            print("  " + " ".join(c))
        return 0

    if wt:
        git(["worktree", "remove", wt], cwd=root, capture=False)
    git(["branch", "-d", branch], cwd=root, capture=False)
    if args.remote:
        git(["push", REMOTE, "--delete", branch], cwd=root, capture=False)
    git(["worktree", "prune"], cwd=root, capture=False)
    if branch in reg.get("tasks", {}):
        reg["tasks"][branch]["state"] = "CONCLUÍDO"
        save_registry(root, reg)
    print(f"cleaned up {branch}.")
    return 0


def build_parser():
    p = argparse.ArgumentParser(prog="gitflow", description=__doc__.split("\n")[0])
    sub = p.add_subparsers(dest="cmd", required=True)

    s = sub.add_parser("classify", help="suggest prefix + base from a description")
    s.add_argument("text", nargs="+")
    s.set_defaults(func=cmd_classify)

    s = sub.add_parser("start", help="create the worktree + branch off the right base")
    s.add_argument("branch", help="<prefix>/<task>, e.g. fix/compose-secret")
    s.add_argument("--dry-run", action="store_true")
    s.set_defaults(func=cmd_start)

    s = sub.add_parser("preflight", help="check remote, base freshness, WIP cap")
    s.add_argument("--dry-run", action="store_true", help="do not fetch")
    s.set_defaults(func=cmd_preflight)

    s = sub.add_parser("gates", help="run the gates here and record evidence")
    s.add_argument("--full", action="store_true", help="also run `make ci`")
    s.add_argument("--keep-going", action="store_true")
    s.set_defaults(func=cmd_gates)

    s = sub.add_parser("integrate", help="prepare/execute integration into develop")
    s.add_argument("--execute", action="store_true", help="actually push + open PR")
    s.add_argument("--dry-run", action="store_true")
    s.set_defaults(func=cmd_integrate)

    s = sub.add_parser("done", help="prove the branch content is in develop")
    s.set_defaults(func=cmd_done)

    s = sub.add_parser("cleanup", help="remove worktree + branch(es) safely")
    s.add_argument("branch")
    s.add_argument("--remote", action="store_true", help="also delete the remote branch")
    s.add_argument("--execute", action="store_true")
    s.add_argument("--dry-run", action="store_true")
    s.add_argument("--force-unmerged", action="store_true",
                   help="allow removal when inclusion cannot be proven by git cherry")
    s.set_defaults(func=cmd_cleanup)
    return p


def main(argv=None):
    args = build_parser().parse_args(argv)
    try:
        return args.func(args)
    except GitFlowError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
