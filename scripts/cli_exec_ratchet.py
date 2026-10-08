#!/usr/bin/env python3
"""Two-way ratchet on how many CLI leaves the battery actually EXECUTES.

Why this exists. `scripts/cli-tree.sh --gate` keeps the inventory of invocable
leaves honest, and the battery checks the `--help` of every one of them. That is
not the same as running them: the header of `scripts/e2e.sh` has carried the
fraction by hand since 2026-09-09 ("executes 91 — 37%"), and a number kept by
hand in a comment is a number that goes stale without anybody noticing. Wave F0
of the maturity plan asks for the instrument before the coverage, so this is the
instrument.

What the numerator means, exactly. A leaf counts as executed when at least one
`check`/`xfail` of a real battery run INVOKED it and had its exit status
asserted. Not when the script mentions it, and not when a teardown line runs it
with `>/dev/null 2>&1 || true` — an invocation nobody judged proves nothing. The
battery already records every asserted command in `$OUT/results.jsonl` (the
`cmd` field of the `check` helper), so the count comes from a run, never from a
grep over the script's text.

Where the committed number comes from. CI cannot run the battery (the hosted
runner blocks unprivileged user namespaces), so the gate reads a trace COMMITTED
from a named run, the same shape `docs/proxmox/trace-9.2.2.routes` uses: a
`# key: value` provenance header and one leaf per line. The gate then compares
it to `scripts/cli_exec_baseline.json`. A number without provenance would be a
number nobody can date.

It errs toward under-reporting, never over. A command reached through a helper
shell function, or one whose argv this parser cannot resolve to a leaf, is left
OUT of the numerator and listed by `--list-unmatched`. An undercount can only
make coverage look worse than it is, which is the safe direction for a ratchet;
an overcount would be false confidence.

The measured example of that, and it is worth keeping in mind before reading a
zero as an accusation: `build` counts 0 while the battery plainly builds images.
Its section invokes the engine from a shell function (`check … ok
build_env_path`) and from setup lines that end in `|| true`, and asserts the
CONSEQUENCE — file ownership inside the resulting image — instead of the build's
own exit status. By the rule above that is 0, correctly: nothing judged the
`build` invocation. Turning one of those setup lines into a `check` is the
cheapest point of coverage in the whole battery.

    scripts/cli_exec_ratchet.py                       # the gate (reads the trace)
    scripts/cli_exec_ratchet.py --list                # per-group breakdown
    scripts/cli_exec_ratchet.py --list-missing        # leaves never executed
    scripts/cli_exec_ratchet.py --from-results <f>... # derive from a real run
    scripts/cli_exec_ratchet.py --from-results <f>... --update   # record it
"""

from __future__ import annotations

import argparse
import json
import pathlib
import subprocess
import sys

REPO = pathlib.Path(__file__).resolve().parent.parent
LEAVES = REPO / "scripts" / "cli_baseline.tsv"
TRACE = REPO / "scripts" / "cli_exec_trace.tsv"
BASELINE = REPO / "scripts" / "cli_exec_baseline.json"

# The only global option of the engine CLI, read from the `Cli` struct in
# `bins/delonix-runtime-bin/src/main.rs` rather than guessed. It can arrive as
# `--l18n=pt` (one word) or `--l18n pt` (two), and in the second shape its value
# has to be skipped as well or it would be mistaken for a subcommand.
GLOBAL_VALUE_FLAGS = {"--l18n"}

# The command aliases of the CLI, which the leaf inventory cannot carry: the
# inventory is read from `--help`, and `--help` prints only the canonical name.
# Measured in the source rather than guessed — there are exactly three
# `#[command(alias…)]`/`visible_alias` sites in `bins/delonix-runtime-bin/src`,
# and `test_cli_exec_ratchet.py` greps for them so a fourth cannot be added
# without this map noticing. Without it `container ls`, which the battery runs
# more than any other command, resolved to nothing.
ALIASES = {
    "container ls": "container ps",
    "vm rm": "vm destroy",
    "vm down": "vm stop",
}

# An invocation carrying one of these printed help or the version and did
# nothing else — clap short-circuits before the command runs. Excluding them is
# what makes this metric different from the one `cli surface` already holds: the
# battery checks the `--help` of EVERY leaf through `check`, so counting those
# gave 100% in every group on the first real measurement. There is no
# `short = 'h'` or `short = 'V'` anywhere in `bins/delonix-runtime-bin/src`, so
# the short forms are unambiguous.
HELP_FLAGS = {"--help", "-h", "--version", "-V"}

# Shell punctuation that ends a command inside a `bash -c` body. The `cmd` field
# is a space-joined argv, so a body arrives with its separators glued to words
# (`container ps;`) and a word-exact match would miss the leaf.
SEPARATORS = ";|&"
# What ENDS a command inside a shell body, read on the RAW word.
#
# `SEPARATORS` alone could not do it: `unquote` strips them (and `)`) before the
# scan ever sees the word, so the `ended` test built on the unquoted word was
# dead code from the first commit — it can never be true. Measured 2026-10-08
# while writing the first check with TWO invocations in one body:
#
#   diff <('$BIN' version …) <('$BIN' --version …)
#
# The first invocation really ran `version` and had its exit status judged, and
# the leaf counted ZERO: the scan after the first engine word never stopped at
# the `)`, walked into the second command, hit `--version` in `HELP_FLAGS` and
# suppressed the invocation that DID happen. The count errs low by design, but
# this was a silent mis-measurement, not a conservative one — a later `--help`
# in the same body could erase an earlier real invocation anywhere.
#
# `&` is in the set and that is deliberate even though `2>&1` is a redirection
# and not a command end: stopping there can only cost a LONGER leaf path, and no
# leaf name contains `&`, so the worst case is the same leaf found one word
# earlier. Erring low here is the same choice the module makes everywhere else.
ENDERS = ";|&)`"

# Punctuation glued to the LEFT of a word by command substitution. Measured
# against a real run: the battery's commonest shape is
# `[ "$('<path>/delonix' container ps …)" -eq 0 ]`, where the engine path
# arrives as `"$('<path>/delonix'` — one word with three characters of prefix.
# Without stripping them, 190 asserted commands of one run resolved to no leaf.
LEADING = "\"'$(`{["

# A verdict that proves the command ran. SKIP has no `cmd` field at all (the
# `skip` helper records only name and reason), but naming the set is what keeps a
# future verdict from being counted by accident.
RAN = {"PASS", "FAIL", "XFAIL", "XPASS"}


def read_leaves(path: pathlib.Path | None = None) -> set[str]:
    """The denominator: every invocable leaf, from the gated inventory.

    `cli_baseline.tsv` is used instead of calling `cli-tree.sh --leaves` so the
    gate needs no built binary in CI. The two cannot drift apart silently:
    `cli-tree.sh --gate` fails when the tree and this file disagree.

    The path is resolved on CALL, not bound as a default: a default argument is
    evaluated once at definition time, which made every test here measure
    against the repository's real inventory instead of its own fixture.
    """
    path = path or LEAVES
    leaves = set()
    for line in path.read_text().splitlines():
        if not line.strip():
            continue
        parts = line.split("\t", 1)
        if len(parts) != 2:
            raise SystemExit(f"{path}: line is not '<class>\\t<leaf>': {line!r}")
        leaves.add(parts[1].strip())
    if not leaves:
        raise SystemExit(f"{path}: no leaves — refusing to measure against nothing")
    return leaves


def lookup_table(leaves: set[str]) -> dict[str, str]:
    """Every spelling that reaches a leaf, mapped to the leaf it reaches."""
    table = {leaf: leaf for leaf in leaves}
    for alias, canonical in ALIASES.items():
        if canonical not in leaves:
            raise SystemExit(
                f"the alias map says `{alias}` reaches `{canonical}`, which is not a leaf.\n"
                "The CLI was renamed under it: fix ALIASES in this script."
            )
        table[alias] = canonical
    return table


def unquote(word: str) -> str:
    """Strip the shell punctuation a `bash -c` body glues to a word.

    The `cmd` field is a space-joined argv, so the engine path inside a shell
    body arrives as `'/t/delonix'` or, inside a command substitution, as
    `"$('/t/delonix'`. Both shapes were measured against a real run: the first
    swallowed every command the battery issues through a shell, the second the
    190 that go through `[ "$(…)" ]`.
    """
    return word.lstrip(LEADING).rstrip("\"')}" + SEPARATORS)


def is_engine(word: str) -> bool:
    """Whether a recorded word is the engine binary.

    Either a path whose basename is `delonix`, or the literal `$BIN` — the
    battery `export`s that variable (`scripts/e2e.sh`, line with `export BIN`)
    and a single-quoted `bash -c` body therefore reaches this trace with the
    name unexpanded. Measured against a real run: dropping that spelling left
    every command the battery issues through a shell body out of the count.
    """
    w = unquote(word)
    # `BIN` and not `$BIN`: `unquote` strips the `$` along with the rest of the
    # command-substitution punctuation, so the sigil is already gone here.
    return w.rsplit("/", 1)[-1] == "delonix" or w == "BIN"


def leaves_of(cmd: str, leaves: set[str], table: dict[str, str] | None = None) -> set[str]:
    """Every leaf a recorded command invoked — empty when it invoked none.

    EVERY invocation counts, not just the first: a `check` asserts the outcome of
    its whole command, so a shell body that runs three of them had all three
    judged. Each engine word is followed by the longest prefix of non-flag words
    that reaches a leaf; longest wins because `vm snapshot create` and `vm start`
    are both leaves and a shortest-match would file the first under the second's
    group. A wrapper (`timeout 30 <bin> …`) and a quoted path inside a shell body
    both resolve, since the `cmd` field is a space-joined argv.
    """
    table = table if table is not None else lookup_table(leaves)
    words = cmd.split()
    found: set[str] = set()
    for i, w in enumerate(words):
        if not is_engine(w):
            continue
        # The RAW words, because only they still carry the punctuation that says
        # where this command ends — see `ENDERS`.
        rest = words[i + 1 :]
        # Leading global options sit between the binary and the subcommand.
        while rest and unquote(rest[0]).startswith("-"):
            flag = unquote(rest[0])
            rest = rest[1:]
            if flag in GLOBAL_VALUE_FLAGS and rest:
                rest = rest[1:]
        best = None
        path: list[str] = []
        printed_help = False
        for raw in rest:
            stripped = unquote(raw)
            ended = any(c in raw for c in ENDERS)
            if stripped in HELP_FLAGS:
                printed_help = True
                break
            if stripped.startswith("-") or not stripped:
                break
            path.append(stripped)
            candidate = " ".join(path)
            if candidate in table:
                best = table[candidate]
            if ended:
                break
        if best is not None and not printed_help:
            found.add(best)
    return found


def printed_help(cmd: str) -> bool:
    """Whether a recorded command asked the engine for help or the version.

    Kept apart from "resolved to no leaf" so `--list-unmatched` stays an audit
    list: the battery checks the `--help` of every leaf, and folding those into
    the unmatched list buried the handful of commands worth looking at under
    hundreds of contract checks.
    """
    words = cmd.split()
    for i, w in enumerate(words):
        if not is_engine(w):
            continue
        for x in words[i + 1 :]:
            stripped = unquote(x).rstrip(SEPARATORS)
            if stripped in HELP_FLAGS:
                return True
            if stripped.startswith("-") or not stripped or stripped != unquote(x):
                break
    return False


def executed_from_results(paths: list[pathlib.Path], leaves: set[str]) -> tuple[set[str], list[str]]:
    """Leaves executed by a run, plus the commands that resolved to no leaf."""
    executed: set[str] = set()
    unmatched: list[str] = []
    table = lookup_table(leaves)
    seen = 0
    for p in paths:
        for line in p.read_text().splitlines():
            if not line.strip():
                continue
            rec = json.loads(line)
            if rec.get("verdict") not in RAN:
                continue
            cmd = rec.get("cmd")
            if not cmd:
                continue
            seen += 1
            found = leaves_of(cmd, leaves, table)
            if found:
                executed |= found
            elif not printed_help(cmd):
                unmatched.append(cmd)
    if seen == 0:
        raise SystemExit(
            "the results carry no asserted command — nothing was measured.\n"
            "Point --from-results at the results.jsonl of a real battery run."
        )
    return executed, unmatched


def git_head() -> str:
    try:
        out = subprocess.run(
            ["git", "-C", str(REPO), "rev-parse", "--short", "HEAD"],
            capture_output=True,
            text=True,
            check=True,
        )
        return out.stdout.strip()
    except Exception:
        return "unknown"


def write_trace(
    path: pathlib.Path,
    executed: set[str],
    leaves: set[str],
    note: str,
    commit: str | None = None,
) -> None:
    """Write the trace, stamped with the commit the RUN saw.

    `commit` defaults to HEAD, which is right while recording a fresh run and
    WRONG while re-recording an older one: a `--update` on a newer HEAD used to
    move the stamp to a commit the battery never saw. Measured on 2026-10-08 —
    re-recording the 160-leaf run moved `commit:` from 32efdbb5 to 759d1ea6,
    three commits later, one of them a 28-line change to `scripts/e2e.sh`. A
    provenance header that names the wrong commit is worse than none: it is a
    number nobody can date, wearing a date.
    """
    lines = [
        "# The CLI leaves a real battery run executed and asserted (F0.2 of the",
        "# maturity plan). Derived, never edited by hand:",
        "#   scripts/cli_exec_ratchet.py --from-results <results.jsonl> --update",
        f"# recorded: {note}",
        f"# commit: {commit or git_head()}",
        f"# executed: {len(executed)}",
        f"# leaves: {len(leaves)}",
    ]
    lines += sorted(executed)
    path.write_text("\n".join(lines) + "\n")


def read_trace(path: pathlib.Path) -> tuple[set[str], dict[str, str]]:
    if not path.exists():
        raise SystemExit(
            f"no trace at {path}.\n"
            "Record one from a real run:\n"
            "  scripts/cli_exec_ratchet.py --from-results <results.jsonl> --update"
        )
    executed, meta = set(), {}
    for line in path.read_text().splitlines():
        line = line.rstrip()
        if not line:
            continue
        if line.startswith("#"):
            body = line.lstrip("# ")
            if ":" in body:
                k, v = body.split(":", 1)
                meta[k.strip()] = v.strip()
            continue
        executed.add(line)
    return executed, meta


def load_baseline() -> dict[str, int]:
    if not BASELINE.exists():
        raise SystemExit(
            f"no baseline at {BASELINE}.\n"
            "Record one from a real run:\n"
            "  scripts/cli_exec_ratchet.py --from-results <results.jsonl> --update"
        )
    return json.loads(BASELINE.read_text())


def percent(executed: int, total: int) -> str:
    if total == 0:
        return "0.0"
    tenths = (executed * 1000 + total // 2) // total
    return f"{tenths // 10}.{tenths % 10}"


def group_of(leaf: str) -> str:
    return leaf.split()[0]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--from-results", nargs="+", metavar="FILE", help="derive from a battery run")
    ap.add_argument("--update", action="store_true", help="write the trace and the baseline")
    ap.add_argument("--note", default="", help="provenance note for --update (host, date, command)")
    ap.add_argument(
        "--commit",
        default="",
        metavar="SHA",
        help="the commit the run saw (default: HEAD; name it when re-recording an older run)",
    )
    ap.add_argument("--list", action="store_true", help="per-group breakdown")
    ap.add_argument("--list-missing", action="store_true", help="leaves never executed")
    ap.add_argument("--list-unmatched", action="store_true", help="recorded commands that resolved to no leaf")
    args = ap.parse_args()

    leaves = read_leaves(LEAVES)
    lookup_table(leaves)  # fails loudly if an alias now points at nothing
    unmatched: list[str] = []

    if args.from_results:
        paths = [pathlib.Path(p) for p in args.from_results]
        for p in paths:
            if not p.exists():
                raise SystemExit(f"no such results file: {p}")
        executed, unmatched = executed_from_results(paths, leaves)
        stale = executed - leaves  # cannot happen: leaves_of only returns known leaves
        assert not stale, stale
    else:
        executed, meta = read_trace(TRACE)
        stale = executed - leaves
        if stale:
            print("FAIL: the committed trace names leaves the CLI no longer has:", file=sys.stderr)
            for s in sorted(stale):
                print(f"  - {s}", file=sys.stderr)
            print("  (re-record the trace from a run of this tree)", file=sys.stderr)
            return 1
        if meta.get("recorded"):
            print(f"trace recorded: {meta['recorded']} (commit {meta.get('commit', '?')})")

    total = len(leaves)
    count = len(executed)

    if args.update:
        if not args.from_results:
            raise SystemExit("--update needs --from-results: a baseline is recorded from a run, not from itself")
        note = args.note or "unnamed run — pass --note with host, date and command"
        write_trace(TRACE, executed, leaves, note, args.commit or None)
        BASELINE.write_text(json.dumps({"executed": count, "leaves": total}, indent=2) + "\n")
        print(f"recorded: {count} of {total} leaves invoked under an assertion — {percent(count, total)} %")
        if unmatched:
            print(f"note: {len(unmatched)} asserted command(s) resolved to no leaf (see --list-unmatched)")
        return 0

    if args.list:
        by_group: dict[str, list[int]] = {}
        for leaf in sorted(leaves):
            g = group_of(leaf)
            seen, tot = by_group.setdefault(g, [0, 0])
            by_group[g] = [seen + (1 if leaf in executed else 0), tot + 1]
        print(f"{'group':<18} {'exec':>5} {'leaves':>7} {'%':>7}")
        for g in sorted(by_group, key=lambda k: (-by_group[k][1], k)):
            e, t = by_group[g]
            print(f"{g:<18} {e:>5} {t:>7} {percent(e, t):>7}")

    if args.list_missing:
        for leaf in sorted(leaves - executed):
            print(leaf)

    if args.list_unmatched:
        for cmd in unmatched:
            print(cmd)

    base = load_baseline()
    want, want_total = base["executed"], base["leaves"]

    if total != want_total:
        direction = "grew" if total > want_total else "shrank"
        print(
            f"FAIL: the CLI {direction}: {want_total} leaves in the baseline, {total} today.",
            file=sys.stderr,
        )
        print(
            "  A leaf nobody exercises lowers the fraction, and that is the point of the\n"
            "  ratchet: either give it a check, or lower the baseline in the SAME commit\n"
            "  with the reason in the message.",
            file=sys.stderr,
        )
        return 1

    if count < want:
        print(f"FAIL: coverage dropped: {want} leaves executed in the baseline, {count} today.", file=sys.stderr)
        print("  A check that stopped exercising a leaf is a check that stopped testing.", file=sys.stderr)
        return 1

    if count > want:
        print(f"FAIL: coverage rose to {count} (baseline {want}) and the baseline did not.", file=sys.stderr)
        print("  Raise it in the SAME commit:", file=sys.stderr)
        print("    scripts/cli_exec_ratchet.py --from-results <results.jsonl> --update --note '<run>'", file=sys.stderr)
        return 1

    print(f"ok: {count} of {total} CLI leaves invoked under an assertion — {percent(count, total)} %")
    return 0


if __name__ == "__main__":
    sys.exit(main())
