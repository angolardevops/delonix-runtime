#!/usr/bin/env python3
"""Version gate: the workspace version stays aligned with the published tags.

The version in the root `Cargo.toml` is not decoration. It chooses which release
`delonix-cri` is downloaded from (`vmimage.rs`), what the Docker API reports as
`ServerVersion`, what a resource backup records as `delonix_version`, and what
the release workflow checks the built binary against. So it can only ever be
one of two things:

1. **Equal to the newest tag this commit contains** — ordinary work between
   releases. The binary says how far it is from that tag in its `--version`
   (`commit: <hash> (+N since vX.Y.Z)`), so two builds with the same number are
   never confused again.
2. **Greater, in the release commit itself** — and then `docs/releases/v<ver>.md`
   must exist, because that is what the release workflow publishes.

Everything else fails, and each failure is a regression with a history:

- **lower than the tag it contains**: a branch carries a `Cargo.toml` from
  before a release and would publish an older number over a newer one;
- **greater with no release notes**: a bump that no tag will ever match, which
  sends the CRI download to a release that does not exist;
- **a newer tag exists that this commit does not contain**: the branch started
  before that release and has not taken it in. Merging it as-is is how work
  already shipped gets undone. Merge `origin/main` into the branch first.

**Version lines.** The product rebranded from `delonix-runtime` to **Delonix**
and re-based the version number on purpose: the frozen `delonix-runtime` line is
the majors `v1..v5` (through `v5.1.1`), and the Delonix line opens at `0.1.0`. A
lower number is not a regression here, it is a deliberate new line — so the
monotonic check above runs *within a line*: `0.1.0` is never read as "older than
`v5.1.1`". What still holds across lines is that a commit must sit on top of the
whole shipped `delonix-runtime` history (it must contain the newest `v5.*` tag),
or merging it would undo released work. When the Delonix line reaches `1.0.0`
this `line_of` split (major 0 vs majors 1..5) must be revisited — see the
version-line note in `docs/dev/`.

    python3 scripts/version_gate.py

Needs the tags (`git fetch --tags`; in CI `fetch-depth: 0`). With no tags at
all it refuses instead of passing: a gate that cannot see what it guards is not
a green light.
"""

from __future__ import annotations

import re
import subprocess
import sys
import tomllib
from pathlib import Path
from typing import Callable

ROOT = Path(__file__).resolve().parent.parent
TAG = re.compile(r"^v(\d+)\.(\d+)\.(\d+)$")

# The frozen `delonix-runtime` line. It shipped v1.0.0 through v5.1.1 and will
# not continue — the product is Delonix now. Everything outside this set is the
# Delonix line (0.x today). Revisit when Delonix reaches 1.0.0.
LEGACY_MAJORS = frozenset({1, 2, 3, 4, 5})


def git(*args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=ROOT, capture_output=True, text=True, check=True
    ).stdout


def parse(tag: str) -> tuple[int, int, int] | None:
    m = TAG.match(tag)
    return tuple(int(x) for x in m.groups()) if m else None  # type: ignore[return-value]


def line_of(v: tuple[int, int, int]) -> str:
    """Which version line a X.Y.Z belongs to."""
    return "delonix-runtime" if v[0] in LEGACY_MAJORS else "delonix"


def _legacy_guard(all_v: list[str], contained_v: list[str]) -> str | None:
    """Across every line: never drop shipped delonix-runtime work. The newest
    v1..v5 tag must be an ancestor of this commit. Returns a failure message, or
    None when the commit is on top of the whole legacy history."""
    legacy_all = [t for t in all_v if parse(t)[0] in LEGACY_MAJORS]  # type: ignore[index]
    if not legacy_all:
        return None
    newest_legacy = max(legacy_all, key=parse)  # type: ignore[arg-type]
    legacy_contained = [t for t in contained_v if parse(t)[0] in LEGACY_MAJORS]  # type: ignore[index]
    base_legacy = max(legacy_contained, key=parse) if legacy_contained else None  # type: ignore[arg-type]
    if base_legacy is None or parse(newest_legacy) > parse(base_legacy):  # type: ignore[operator]
        return (
            f"FAIL  {newest_legacy} is published but this commit does not contain it "
            f"(newest contained: {base_legacy or 'none'}) — merge origin/main first; "
            "merging a branch that predates a release undoes what it shipped"
        )
    return None


def evaluate(
    version: str,
    all_tags: list[str],
    contained: list[str],
    notes_exist: bool,
    commits_since: Callable[[str], int] = lambda _tag: 0,
) -> tuple[int, str]:
    """Pure core of the gate, so it is testable without a git fixture.

    `all_tags` / `contained` are `vX.Y.Z` strings (unparseable ones ignored);
    `notes_exist` says whether `docs/releases/v<version>.md` is present;
    `commits_since(tag)` counts commits from `tag` to HEAD (only called in the
    ordinary between-releases case). Returns `(rc, message)`.
    """
    mine = parse(f"v{version}")
    if mine is None:
        return 1, f"FAIL  workspace version {version!r} is not X.Y.Z"

    all_v = [t for t in all_tags if parse(t)]
    if not all_v:
        return 1, "FAIL  no v* tags visible — fetch them (git fetch --tags; CI: fetch-depth: 0)"
    contained_v = [t for t in contained if parse(t)]

    # (A) Across every line: never drop shipped delonix-runtime work.
    legacy_failure = _legacy_guard(all_v, contained_v)
    if legacy_failure is not None:
        return 1, legacy_failure

    # (B) Within the version's own line, the Cargo version tracks the newest tag
    #     of that line.
    my_line = line_of(mine)
    line_tags = [t for t in all_v if line_of(parse(t)) == my_line]  # type: ignore[arg-type]
    line_contained = [t for t in contained_v if line_of(parse(t)) == my_line]  # type: ignore[arg-type]
    base = max(line_contained, key=parse) if line_contained else None  # type: ignore[arg-type]

    if base is None:
        # No tag of this line is an ancestor yet.
        if line_tags:
            newest_line = max(line_tags, key=parse)  # type: ignore[arg-type]
            return 1, (
                f"FAIL  {newest_line} (the {my_line} line) is published but this commit does "
                "not contain it — merge it first"
            )
        # Brand-new line: allowed only as the release commit that opens it.
        if notes_exist:
            return 0, f"ok    opens the {my_line} line at {version} (notes present)"
        return 1, (
            f"FAIL  Cargo.toml says {version}, opening the {my_line} line, but "
            f"docs/releases/v{version}.md does not exist — a line opens on a release commit"
        )

    if mine < parse(base):  # type: ignore[operator]
        return 1, (
            f"FAIL  Cargo.toml says {version}, lower than {base} on the {my_line} line "
            "— an older version would be published"
        )
    if mine > parse(base):  # type: ignore[operator]
        if notes_exist:
            return 0, f"ok    release commit: {version} (notes present), previous tag {base}"
        return 1, (
            f"FAIL  Cargo.toml says {version}, above {base}, and docs/releases/v{version}.md "
            "does not exist — a bump belongs only to the release commit"
        )
    return 0, f"ok    version {version} = {base} (+{commits_since(base)} commits since the tag)"


def main() -> int:
    version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))[
        "workspace"
    ]["package"]["version"]
    all_tags = git("tag", "--list", "v*").split()
    contained = git("tag", "--list", "v*", "--merged", "HEAD").split()
    notes_exist = (ROOT / "docs" / "releases" / f"v{version}.md").is_file()

    def commits_since(tag: str) -> int:
        return int(git("rev-list", "--count", f"{tag}..HEAD").strip() or "0")

    rc, message = evaluate(version, all_tags, contained, notes_exist, commits_since)
    print(message)
    return rc


if __name__ == "__main__":
    sys.exit(main())
