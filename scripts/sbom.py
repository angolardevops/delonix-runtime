#!/usr/bin/env python3
"""SBOM (SPDX 2.3) of the binary, generated from `Cargo.lock`.

# Why not a tool

This repo already answered that once, for the VM images: their inventory
(`packages.tsv`) is "the content an SPDX carries, in the form the guest produces
without a new tool". The same holds here — `Cargo.lock` IS the resolved
dependency tree, with the name, version, origin and checksum of every package,
which are exactly the fields an SBOM consumer reads.

A third-party action in the release workflow would be new supply-chain surface
in the very step that exists to guarantee it, and this engine's rule is not to
grow that surface without a measured need.

# What this SBOM promises, and what it does not

**Promises** the Rust dependency tree, with the exact version and the checksum
published in `Cargo.lock`. That is what answers "does this CVE affect me?".

**Does not promise** a reproducible build, nor coverage of what is linked from
the system (libc, whatever `protoc` generates). It says so in the document's own
`comment` field, so nobody reads it as more than it is.

Usage:  scripts/sbom.py [--lock Cargo.lock] [--version X.Y.Z] > SBOM.spdx.json
"""
import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# SPDX requires unique ids over a restricted alphabet: letters, digits, `.`, `-`.
# A crate name with `_` (there are many) would produce an invalid id, and a
# validator rejects the whole document over it.
SAFE = re.compile(r"[^A-Za-z0-9.\-]")


def spdx_id(name: str, version: str) -> str:
    return f"SPDXRef-Package-{SAFE.sub('-', name)}-{SAFE.sub('-', version)}"


def parse_lock(text: str):
    """The packages in `Cargo.lock`, in the order they appear.

    A ten-line parser instead of a TOML library: the file is cargo-generated and
    has a fixed shape, and adding a parsing dependency to the script that
    documents the dependencies would be a poor joke.
    """
    pkgs, cur = [], None
    for line in text.splitlines():
        line = line.strip()
        if line == "[[package]]":
            if cur:
                pkgs.append(cur)
            cur = {}
            continue
        if cur is None or "=" not in line:
            continue
        k, _, v = line.partition("=")
        k, v = k.strip(), v.strip().strip('"')
        if k in ("name", "version", "source", "checksum"):
            cur[k] = v
    if cur:
        pkgs.append(cur)
    return [p for p in pkgs if "name" in p and "version" in p]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--lock", default=str(ROOT / "Cargo.lock"))
    ap.add_argument("--version", default=None, help="product version (default: from the root Cargo.toml)")
    ap.add_argument("--name", default="delonix")
    args = ap.parse_args()

    lock_text = Path(args.lock).read_text(encoding="utf-8")
    pkgs = parse_lock(lock_text)
    if not pkgs:
        print(f"{args.lock}: no packages found — did the format change?", file=sys.stderr)
        return 2

    version = args.version
    if not version:
        m = re.search(r'^version\s*=\s*"([^"]+)"', (ROOT / "Cargo.toml").read_text(encoding="utf-8"), re.M)
        version = m.group(1) if m else "0.0.0"

    # The namespace has to be unique per document. Derived from the lock's
    # CONTENT and not from a timestamp: two generations of the same lock give the
    # same document, which is the least reproducibility an SBOM should have.
    digest = hashlib.sha256(lock_text.encode("utf-8")).hexdigest()

    packages = [{
        "SPDXID": "SPDXRef-Package-delonix",
        "name": args.name,
        "versionInfo": version,
        "downloadLocation": "https://github.com/angolardevops/delonix-runtime",
        "licenseConcluded": "Apache-2.0",
        "licenseDeclared": "Apache-2.0",
        "filesAnalyzed": False,
        "supplier": "Organization: angolardevops",
    }]
    relationships = [{
        "spdxElementId": "SPDXRef-DOCUMENT",
        "relationshipType": "DESCRIBES",
        "relatedSpdxElement": "SPDXRef-Package-delonix",
    }]

    for p in pkgs:
        if p["name"].startswith("delonix"):
            continue  # our own crates are not third-party dependencies
        pid = spdx_id(p["name"], p["version"])
        entry = {
            "SPDXID": pid,
            "name": p["name"],
            "versionInfo": p["version"],
            "downloadLocation": p.get("source", "NOASSERTION"),
            "filesAnalyzed": False,
            "licenseConcluded": "NOASSERTION",
            "licenseDeclared": "NOASSERTION",
            "externalRefs": [{
                "referenceCategory": "PACKAGE-MANAGER",
                "referenceType": "purl",
                "referenceLocator": f"pkg:cargo/{p['name']}@{p['version']}",
            }],
        }
        # A checksum only exists for registry packages; a `path`/`git` one has
        # none, and inventing an empty field would be worse than omitting it.
        if "checksum" in p:
            entry["checksums"] = [{"algorithm": "SHA256", "checksumValue": p["checksum"]}]
        packages.append(entry)
        relationships.append({
            "spdxElementId": "SPDXRef-Package-delonix",
            "relationshipType": "DEPENDS_ON",
            "relatedSpdxElement": pid,
        })

    doc = {
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": f"{args.name}-{version}",
        "documentNamespace": f"https://github.com/angolardevops/delonix-runtime/sbom/{version}/{digest[:16]}",
        "creationInfo": {
            "creators": ["Tool: delonix-scripts-sbom", "Organization: angolardevops"],
            # NO generation timestamp, deliberately: the same `Cargo.lock` has
            # to give the same file, byte for byte. A real `created` would make
            # every run produce a different SBOM for the same software, and there
            # is nothing to compare between two documents that are never equal.
            "created": "1970-01-01T00:00:00Z",
            "comment": (
                "Generated from Cargo.lock. Covers the Rust dependency tree "
                "(name, version, registry checksum). Does NOT cover what is "
                "linked from the system, and claims no reproducible build."
            ),
        },
        "packages": packages,
        "relationships": relationships,
    }
    json.dump(doc, sys.stdout, indent=2, ensure_ascii=False, sort_keys=True)
    print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
