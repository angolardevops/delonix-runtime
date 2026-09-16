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

ROOT = Path(__file__).resolve().parent.parent
TAG = re.compile(r"^v(\d+)\.(\d+)\.(\d+)$")


def git(*args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=ROOT, capture_output=True, text=True, check=True
    ).stdout


def parse(tag: str) -> tuple[int, int, int] | None:
    m = TAG.match(tag)
    return tuple(int(x) for x in m.groups()) if m else None  # type: ignore[return-value]


def main() -> int:
    version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))[
        "workspace"
    ]["package"]["version"]
    mine = parse(f"v{version}")
    if mine is None:
        print(f"FAIL  workspace version {version!r} is not X.Y.Z")
        return 1

    tags = [t for t in git("tag", "--list", "v*").split() if parse(t)]
    if not tags:
        print("FAIL  no v* tags visible — fetch them (git fetch --tags; CI: fetch-depth: 0)")
        return 1
    contained = [t for t in git("tag", "--list", "v*", "--merged", "HEAD").split() if parse(t)]
    newest = max(tags, key=parse)
    base = max(contained, key=parse) if contained else None

    rc = 0
    if base is None or parse(newest) > parse(base):  # type: ignore[operator]
        print(
            f"FAIL  {newest} is published but this commit does not contain it "
            f"(newest contained: {base or 'none'}) — merge origin/main first; "
            "merging a branch that predates a release undoes what it shipped"
        )
        rc = 1
    if base is not None:
        if mine < parse(base):  # type: ignore[operator]
            print(f"FAIL  Cargo.toml says {version}, lower than {base} — an older version would be published")
            rc = 1
        elif mine > parse(base):  # type: ignore[operator]
            notes = ROOT / "docs" / "releases" / f"v{version}.md"
            if not notes.is_file():
                print(
                    f"FAIL  Cargo.toml says {version}, above {base}, and docs/releases/v{version}.md "
                    "does not exist — a bump belongs only to the release commit"
                )
                rc = 1
            else:
                print(f"ok    release commit: {version} (notes present), previous tag {base}")
        else:
            since = git("rev-list", "--count", f"{base}..HEAD").strip()
            print(f"ok    version {version} = {base} (+{since} commits since the tag)")
    return rc


if __name__ == "__main__":
    sys.exit(main())
