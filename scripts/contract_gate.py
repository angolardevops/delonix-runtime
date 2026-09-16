#!/usr/bin/env python3
"""Contract gate: the node contract (`proto/delonix/node/v1`) is formatted, linted,
compatible with the last release, and its REST/OpenAPI encoding is the generated one.

ADR-0040 P1. The contract is the source of truth for two encodings — gRPC and HTTP/JSON
on the same unix socket — and the OpenAPI document is GENERATED from it, never written
by hand. Five checks, each failing on its own:

1. `buf format`  — one canonical layout, so a diff is always a change of meaning.
2. `buf lint`    — the rules and the written exceptions in `buf.yaml`.
3. `buf breaking` against the newest release tag that contains `proto/`. Until a release
   carries the contract there is no baseline, and the gate SAYS so instead of passing in
   silence.
4. Every RPC that is not bidirectional has an HTTP mapping (`google.api.http`); the two
   bidirectional streams (`Exec`, `Console`) must NOT have one — REST serves them over
   WebSocket (ADR-0040 D4).
5. `docs/api/openapi.yaml` is exactly what `protoc-gen-openapi` produces from the
   contract, and no two of its paths are the same URL under different variable names
   (OpenAPI forbids it, and a transcoder would route one of them to the wrong method).

    python3 scripts/contract_gate.py            # check (exit 1 on any failure)
    python3 scripts/contract_gate.py --update   # rewrite docs/api/openapi.yaml

Needs `protoc`, `buf` and `protoc-gen-openapi` on PATH (CI pins them in the `contract`
job) and the git tags (`git fetch --tags`).
"""

from __future__ import annotations

import argparse
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PROTO = ROOT / "proto"
VENDOR = ROOT / "third_party" / "googleapis"
OPENAPI = ROOT / "docs" / "api" / "openapi.yaml"
SOURCES = sorted((PROTO / "delonix" / "node" / "v1").glob("*.proto"))
OPENAPI_OPTS = [
    "naming=proto",
    "fq_schema_naming=true",
    "title=Delonix node API",
    "version=v1",
    # No commas: the plugin splits its options on them.
    "description=Local contract of the Delonix Runtime node (delonix.node.v1) generated"
    " from proto/ and served on a local unix socket. Not a remote API.",
]


def run(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, **kw)


def need(tool: str) -> str | None:
    path = shutil.which(tool)
    if path is None:
        print(f"FAIL  {tool} is not on PATH — the gate cannot check what it guards")
    return path


def protoc_include() -> list[str]:
    """`-I` flags: the contract, the vendored googleapis, and protoc's well-known types."""
    flags = ["-I", str(PROTO), "-I", str(VENDOR)]
    protoc = shutil.which("protoc")
    if protoc:
        guess = Path(protoc).resolve().parent.parent / "include"
        if (guess / "google" / "protobuf" / "duration.proto").is_file():
            flags += ["-I", str(guess)]
    return flags


def generate_openapi(out_dir: Path) -> subprocess.CompletedProcess:
    cmd = ["protoc", *protoc_include(), f"--openapi_out={out_dir}"]
    cmd += [f"--openapi_opt={o}" for o in OPENAPI_OPTS]
    cmd += [str(s) for s in SOURCES]
    return run(cmd)


def strip_comments(text: str) -> str:
    """Drop `//` comments outside string literals (a path like `"/v1/x"` is not a comment)."""
    out = []
    for line in text.splitlines():
        in_str = False
        for i, ch in enumerate(line):
            if ch == '"' and (i == 0 or line[i - 1] != "\\"):
                in_str = not in_str
            elif not in_str and line.startswith("//", i):
                line = line[:i]
                break
        out.append(line)
    return "\n".join(out)


def rpc_body(text: str, start: int) -> str:
    """The `{ … }` block (or `;`) after an rpc head, counting braces OUTSIDE strings —
    HTTP paths carry `{namespace}` inside their literals, and a regex that counted those
    matched 13 RPCs out of 58."""
    if text[start] == ";":
        return ";"
    depth, in_str = 0, False
    for i in range(start, len(text)):
        ch = text[i]
        if ch == '"' and text[i - 1] != "\\":
            in_str = not in_str
        elif not in_str and ch == "{":
            depth += 1
        elif not in_str and ch == "}":
            depth -= 1
            if depth == 0:
                return text[start : i + 1]
    raise SystemExit(f"unbalanced braces after offset {start}")


def rpc_mappings() -> tuple[list[str], list[str]]:
    """(mapped RPCs that must have http, problems) read from the proto text."""
    problems: list[str] = []
    mapped: list[str] = []
    head = re.compile(
        r"rpc\s+(\w+)\s*\(\s*(stream\s+)?[\w.]+\s*\)\s*returns\s*\(\s*(?:stream\s+)?[\w.]+\s*\)\s*"
    )
    for src in SOURCES:
        text = strip_comments(src.read_text(encoding="utf-8"))
        for m in head.finditer(text):
            name, client_stream = m.group(1), m.group(2)
            body = rpc_body(text, m.end())
            has_http = "google.api.http" in body
            if client_stream:
                if has_http:
                    problems.append(f"{src.name}: {name} streams from the client and cannot have an HTTP mapping")
            elif not has_http:
                problems.append(f"{src.name}: {name} has no google.api.http mapping")
            else:
                mapped.append(name)
    return mapped, problems


def newest_tag_with_contract() -> str | None:
    tags = run(["git", "tag", "--list", "v*", "--sort=-v:refname"]).stdout.split()
    for tag in tags:
        if run(["git", "cat-file", "-e", f"{tag}:proto/delonix/node/v1"]).returncode == 0:
            return tag
    return None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--update", action="store_true", help="rewrite docs/api/openapi.yaml")
    args = ap.parse_args()

    if not all([need("protoc"), need("buf"), need("protoc-gen-openapi")]):
        return 1

    if args.update:
        OPENAPI.parent.mkdir(parents=True, exist_ok=True)
        r = generate_openapi(OPENAPI.parent)
        print(r.stdout + r.stderr if r.returncode else f"updated {OPENAPI.relative_to(ROOT)}")
        return r.returncode

    rc = 0

    r = run(["buf", "format", "--exit-code", "-d", "proto"])
    if r.returncode:
        print("FAIL  buf format — run `buf format -w proto`")
        print(r.stdout[-2000:])
        rc = 1
    else:
        print("ok    buf format")

    r = run(["buf", "lint"])
    if r.returncode:
        print("FAIL  buf lint")
        print(r.stdout + r.stderr)
        rc = 1
    else:
        print("ok    buf lint")

    base = newest_tag_with_contract()
    if base is None:
        print("ok    buf breaking — no release tag contains proto/ yet, so there is no "
              "baseline to break; this check starts comparing at the first release that ships it")
    else:
        r = run(["buf", "breaking", "--against", f".git#tag={base}"])
        if r.returncode:
            print(f"FAIL  buf breaking against {base}")
            print(r.stdout + r.stderr)
            rc = 1
        else:
            print(f"ok    buf breaking against {base}")

    mapped, problems = rpc_mappings()
    for p in problems:
        print(f"FAIL  {p}")
        rc = 1
    if not problems:
        print(f"ok    HTTP mapping: {len(mapped)} RPCs mapped, client-streaming RPCs unmapped")

    with tempfile.TemporaryDirectory() as tmp:
        r = generate_openapi(Path(tmp))
        if r.returncode:
            print("FAIL  protoc-gen-openapi")
            print(r.stdout + r.stderr)
            return 1
        fresh = (Path(tmp) / "openapi.yaml").read_text(encoding="utf-8")
    committed = OPENAPI.read_text(encoding="utf-8") if OPENAPI.is_file() else ""
    if fresh != committed:
        print("FAIL  docs/api/openapi.yaml is not the generated one — run "
              "`python3 scripts/contract_gate.py --update` and commit it")
        rc = 1
    else:
        ops = fresh.count("operationId:")
        print(f"ok    OpenAPI is the generated one ({ops} operations)")
        if ops != len(mapped):
            print(f"FAIL  OpenAPI has {ops} operations but {len(mapped)} RPCs are mapped")
            rc = 1

    seen: dict[str, str] = {}
    clashes = 0
    for path in re.findall(r"^    (/[^\n]*):$", fresh, re.M):
        shape = re.sub(r"\{[^}]+\}", "{}", path)
        if shape in seen and seen[shape] != path:
            print(f"FAIL  {path} and {seen[shape]} are the same URL under different variable names")
            clashes += 1
        seen.setdefault(shape, path)
    if clashes:
        rc = 1
    else:
        print(f"ok    REST paths: {len(seen)} distinct, none equivalent")
    return rc


if __name__ == "__main__":
    sys.exit(main())
