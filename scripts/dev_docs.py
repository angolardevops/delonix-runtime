#!/usr/bin/env python3
"""Developer handbook generator: the facts in `docs/dev/` that come from the code.

The contributor handbook has two kinds of content, and they go stale differently:

* **Facts** — which crates exist, which layer each one is in, who depends on whom,
  which binaries they ship, the pinned toolchain, and the gates CI runs. Hand-written,
  these rot silently: `docs/runtime/current-state.md` said «10 crates» while the
  workspace had 18, and `CONTRIBUTING.md` listed four gates while CI ran ten jobs. So
  they are GENERATED here, from `Cargo.toml`, `scripts/arch_fitness.py`,
  `rust-toolchain.toml` and `.github/workflows/ci.yml`.
* **Narrative** — why a crate exists, how a flow works, how to contribute. That is
  the `mentor` agent's job (`.claude/agents/mentor.md`), reviewed after each release.

Generated regions are delimited in the Markdown by

    <!-- dev-docs:begin <key> -->
    ...
    <!-- dev-docs:end <key> -->

and nothing outside them is touched, so the narrative around a table survives a
regeneration. Deliberately NOT generated: line counts, test counts, commit counts —
numbers that change on every commit would make the gate fail on every PR, and a gate
that is always red is a gate nobody reads.

    python3 scripts/dev_docs.py            # rewrite the generated regions
    python3 scripts/dev_docs.py --check    # exit 1 when docs/dev is out of date (CI)
"""

from __future__ import annotations

import argparse
import importlib.util
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEV_DOCS = ROOT / "docs" / "dev"
MARKER = re.compile(
    r"(<!-- dev-docs:begin (?P<key>[a-z0-9-]+) -->\n)(?P<body>.*?)(<!-- dev-docs:end (?P=key) -->)",
    re.S,
)

LAYER_ORDER = ["foundation", "context", "adapter", "provider", "interface", "bin"]
LAYER_TITLE = {
    "foundation": "Foundation",
    "context": "Contexts",
    "adapter": "Adapters",
    "provider": "Providers",
    "interface": "Interfaces",
    "bin": "Binaries",
}


def load_arch_fitness():
    """The layer table and the allowed direction live in ONE place — the gate that
    enforces them. Importing it instead of copying it means the handbook cannot
    describe a layering the gate does not check."""
    spec = importlib.util.spec_from_file_location("arch_fitness", ROOT / "scripts" / "arch_fitness.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def workspace_crates() -> list[dict]:
    root = tomllib.loads((ROOT / "Cargo.toml").read_text())
    crates = []
    for member in root["workspace"]["members"]:
        manifest = tomllib.loads((ROOT / member / "Cargo.toml").read_text())
        name = manifest["package"]["name"]
        bins = [b["name"] for b in manifest.get("bin", [])]
        bin_dir = ROOT / member / "src" / "bin"
        if bin_dir.is_dir():
            bins += sorted(p.stem for p in bin_dir.glob("*.rs") if p.stem not in bins)
        crates.append(
            {
                "name": name,
                "path": member,
                "description": manifest["package"].get("description", ""),
                "bins": bins,
                "deps": sorted(d for d in manifest.get("dependencies", {}) if d.startswith("delonix-")),
            }
        )
    return crates


def layer_of(arch, name: str) -> str:
    layer = arch.LAYERS.get(name)
    if layer is None:
        # arch_fitness.py fails on an unlisted crate too; say the same thing here
        # instead of inventing a layer the gate would disagree with.
        sys.exit(f"dev_docs: crate {name!r} is not in arch_fitness.LAYERS — add it there first")
    return layer


def render_toolchain() -> str:
    toolchain = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]
    components = ", ".join(f"`{c}`" for c in toolchain.get("components", []))
    return (
        f"- **Rust toolchain:** `{toolchain['channel']}` (pinned in `rust-toolchain.toml`; "
        f"`rustup` installs it on the first `cargo` call)\n"
        f"- **Components:** {components}\n"
    )


def render_layers(arch) -> str:
    lines = ["| Layer | May depend on |", "|---|---|"]
    for layer in LAYER_ORDER:
        allowed = ", ".join(LAYER_TITLE[x].lower() for x in LAYER_ORDER if x in arch.ALLOWED.get(layer, set()))
        lines.append(f"| {LAYER_TITLE[layer]} | {allowed or '—'} |")
    exceptions = sorted(
        (key, value) for key, value in arch.EXCEPTIONS.items() if key[0] == "dep"
    )
    if exceptions:
        lines += ["", "Declared exceptions (each one names the ADR-0040 phase that removes it):", ""]
        for (_, source, target), (phase, _why) in exceptions:
            lines.append(f"- `{source}` → `{target}` — removed in **{phase}**")
    return "\n".join(lines) + "\n"


def render_crates_table(arch, crates: list[dict]) -> str:
    dependents: dict[str, list[str]] = {c["name"]: [] for c in crates}
    for crate in crates:
        for dep in crate["deps"]:
            dependents.setdefault(dep, []).append(crate["name"])
    ordered = sorted(crates, key=lambda c: (LAYER_ORDER.index(layer_of(arch, c["name"])), c["name"]))
    lines = [
        "| Crate | Layer | Path | Binaries | Depends on (engine crates) | Used by |",
        "|---|---|---|---|---|---|",
    ]
    for crate in ordered:
        deps = ", ".join(f"`{d}`" for d in crate["deps"]) or "—"
        used = ", ".join(f"`{d}`" for d in sorted(dependents[crate["name"]])) or "—"
        bins = ", ".join(f"`{b}`" for b in crate["bins"]) or "—"
        lines.append(
            f"| `{crate['name']}` | {LAYER_TITLE[layer_of(arch, crate['name'])]} "
            f"| `{crate['path']}` | {bins} | {deps} | {used} |"
        )
    return "\n".join(lines) + "\n"


def render_crates_graph(arch, crates: list[dict]) -> str:
    def node(name: str) -> str:
        return name.replace("-", "_")

    lines = ["```mermaid", "graph TB"]
    for layer in LAYER_ORDER:
        members = sorted(c["name"] for c in crates if layer_of(arch, c["name"]) == layer)
        if not members:
            continue
        lines.append(f'  subgraph {layer}["{LAYER_TITLE[layer]}"]')
        lines += [f'    {node(m)}["{m}"]' for m in members]
        lines.append("  end")
    for crate in sorted(crates, key=lambda c: c["name"]):
        for dep in crate["deps"]:
            lines.append(f"  {node(crate['name'])} --> {node(dep)}")
    lines.append("```")
    return "\n".join(lines) + "\n"


def ci_jobs() -> list[tuple[str, str]]:
    """Job id and display name, read without a YAML library: the runner that checks
    this has no PyYAML, and the two fields needed have a fixed shape in ci.yml."""
    text = (ROOT / ".github" / "workflows" / "ci.yml").read_text()
    jobs_block = text.split("\njobs:\n", 1)[1]
    jobs = []
    for match in re.finditer(r"^  ([A-Za-z0-9_-]+):\n(?:    .*\n|\s*\n|    #.*\n)*?    name: (.+)$", jobs_block, re.M):
        jobs.append((match.group(1), match.group(2).strip().strip("'\"")))
    return jobs


def render_ci_gates() -> str:
    lines = ["| CI job | What it checks |", "|---|---|"]
    lines += [f"| `{job}` | {name} |" for job, name in ci_jobs()]
    return "\n".join(lines) + "\n"


def render_crate_count(crates: list[dict]) -> str:
    bins = sorted(b for c in crates for b in c["bins"])
    return (
        f"The workspace has **{len(crates)} crates** and ships **{len(bins)} binaries** "
        f"({', '.join(f'`{b}`' for b in bins)}).\n"
    )


def regions() -> dict[str, str]:
    arch = load_arch_fitness()
    crates = workspace_crates()
    return {
        "toolchain": render_toolchain(),
        "layers": render_layers(arch),
        "crate-count": render_crate_count(crates),
        "crates-table": render_crates_table(arch, crates),
        "crates-graph": render_crates_graph(arch, crates),
        "ci-gates": render_ci_gates(),
    }


def rewrite(text: str, generated: dict[str, str], path: Path) -> str:
    def replace(match: re.Match) -> str:
        key = match.group("key")
        if key not in generated:
            sys.exit(f"dev_docs: {path.relative_to(ROOT)} has an unknown region {key!r}")
        return match.group(1) + generated[key] + match.group(4)

    return MARKER.sub(replace, text)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--check", action="store_true", help="fail instead of writing when docs/dev is stale")
    args = parser.parse_args()

    generated = regions()
    stale = []
    used = set()
    for path in sorted(DEV_DOCS.glob("*.md")):
        text = path.read_text()
        used.update(m.group("key") for m in MARKER.finditer(text))
        new = rewrite(text, generated, path)
        if new != text:
            stale.append(path.relative_to(ROOT))
            if not args.check:
                path.write_text(new)

    # A region nobody renders is a fact the handbook silently stopped stating.
    unused = sorted(set(generated) - used)
    if unused:
        print(f"dev_docs: generated regions not placed in any docs/dev page: {', '.join(unused)}")
        return 1

    if args.check and stale:
        print("dev_docs: docs/dev is out of date — run `python3 scripts/dev_docs.py` and commit:")
        for path in stale:
            print(f"  {path}")
        return 1
    if not args.check:
        print(f"dev_docs: {len(stale)} page(s) updated" if stale else "dev_docs: already up to date")
    return 0


if __name__ == "__main__":
    sys.exit(main())
