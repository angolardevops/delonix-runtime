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

# The handbook's languages. English is the source; a translation lives in
# docs/dev/<lang>/ with the same file names. Generated regions are rendered in
# each page's own language (only the prose — identifiers stay as in the code).
LANGS = ("en", "pt-AO", "fr-FR")

PROSE = {
    "en": {
        "toolchain": "- **Rust toolchain:** `{channel}` (pinned in `rust-toolchain.toml`; `rustup` installs it on the first `cargo` call)\n- **Components:** {components}\n",
        "layer_head": "| Layer | May depend on |",
        "exceptions": "Declared exceptions (each one names the ADR-0040 phase that removes it):",
        "removed_in": "removed in",
        "crates_head": "| Crate | Layer | Path | Binaries | Depends on (engine crates) | Used by |",
        "ci_head": "| CI job | What it checks |",
        "crate_count": "The workspace has **{crates} crates** and ships **{bins} binaries** ({names}).\n",
        "ratchets": "`scripts/arch_fitness.py` keeps **{n} debt ratchets** (baseline in `scripts/arch_baseline.json`):",
    },
    "pt-AO": {
        "toolchain": "- **Toolchain de Rust:** `{channel}` (fixada no `rust-toolchain.toml`; o `rustup` instala-a na primeira chamada ao `cargo`)\n- **Componentes:** {components}\n",
        "layer_head": "| Camada | Pode depender de |",
        "exceptions": "Excepções declaradas (cada uma nomeia a fase do ADR-0040 que a remove):",
        "removed_in": "removida na",
        "crates_head": "| Crate | Camada | Caminho | Binários | Depende de (crates do motor) | Usado por |",
        "ci_head": "| Job de CI | O que verifica |",
        "crate_count": "O workspace tem **{crates} crates** e produz **{bins} binários** ({names}).\n",
        "ratchets": "O `scripts/arch_fitness.py` mantém **{n} ratchets de dívida** (linha de base em `scripts/arch_baseline.json`):",
    },
    "fr-FR": {
        "toolchain": "- **Chaîne d'outils Rust :** `{channel}` (épinglée dans `rust-toolchain.toml` ; `rustup` l'installe au premier appel à `cargo`)\n- **Composants :** {components}\n",
        "layer_head": "| Couche | Peut dépendre de |",
        "exceptions": "Exceptions déclarées (chacune nomme la phase de l'ADR-0040 qui la supprime) :",
        "removed_in": "supprimée en",
        "crates_head": "| Crate | Couche | Chemin | Binaires | Dépend de (crates du moteur) | Utilisé par |",
        "ci_head": "| Job CI | Ce qu'il vérifie |",
        "crate_count": "Le workspace compte **{crates} crates** et livre **{bins} binaires** ({names}).\n",
        "ratchets": "`scripts/arch_fitness.py` maintient **{n} cliquets de dette** (référence dans `scripts/arch_baseline.json`) :",
    },
}

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
        package = manifest["package"]
        # A `[[bin]]` without `name` is named after the package, and so is the binary
        # Cargo discovers on its own from `src/main.rs`; missing either would make the
        # binary count read lower than the truth with the gate still green.
        bins = [b.get("name", name) for b in manifest.get("bin", [])]
        autobins = package.get("autobins", True)
        claimed = {b.get("path", "src/main.rs" if b.get("name", name) == name else "") for b in manifest.get("bin", [])}
        if autobins and (ROOT / member / "src" / "main.rs").is_file() and "src/main.rs" not in claimed and name not in bins:
            bins.append(name)
        bin_dir = ROOT / member / "src" / "bin"
        if autobins and bin_dir.is_dir():
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


def render_toolchain(lang: str) -> str:
    toolchain = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]
    components = ", ".join(f"`{c}`" for c in toolchain.get("components", []))
    return PROSE[lang]["toolchain"].format(channel=toolchain["channel"], components=components)


def render_layers(arch, lang: str) -> str:
    lines = [PROSE[lang]["layer_head"], "|---|---|"]
    for layer in LAYER_ORDER:
        allowed = ", ".join(LAYER_TITLE[x].lower() for x in LAYER_ORDER if x in arch.ALLOWED.get(layer, set()))
        lines.append(f"| {LAYER_TITLE[layer]} | {allowed or '—'} |")
    exceptions = sorted(
        (key, value) for key, value in arch.EXCEPTIONS.items() if key[0] == "dep"
    )
    if exceptions:
        lines += ["", PROSE[lang]["exceptions"], ""]
        for (_, source, target), (phase, _why) in exceptions:
            lines.append(f"- `{source}` → `{target}` — {PROSE[lang]['removed_in']} **{phase}**")
    return "\n".join(lines) + "\n"


def render_crates_table(arch, crates: list[dict], lang: str) -> str:
    dependents: dict[str, list[str]] = {c["name"]: [] for c in crates}
    for crate in crates:
        for dep in crate["deps"]:
            dependents.setdefault(dep, []).append(crate["name"])
    ordered = sorted(crates, key=lambda c: (LAYER_ORDER.index(layer_of(arch, c["name"])), c["name"]))
    lines = [
        PROSE[lang]["crates_head"],
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
    this has no PyYAML. Every job is listed — one without `name:` shows its id — and a
    file whose shape this parser does not recognise fails loudly instead of yielding a
    shorter table."""
    lines = (ROOT / ".github" / "workflows" / "ci.yml").read_text().splitlines()
    try:
        start = next(i for i, line in enumerate(lines) if re.match(r"^jobs:\s*(#.*)?$", line))
    except StopIteration:
        sys.exit("dev_docs: no top-level `jobs:` in .github/workflows/ci.yml")
    jobs: list[tuple[str, str]] = []
    for line in lines[start + 1 :]:
        if line and not line.startswith((" ", "#")):
            break  # the next top-level key ends the jobs block
        job = re.match(r"^  ([A-Za-z0-9_-]+):\s*(#.*)?$", line)
        if job:
            jobs.append((job.group(1), job.group(1)))
            continue
        name = re.match(r"^    name:\s*(.+?)\s*$", line)
        if name and jobs and jobs[-1][0] == jobs[-1][1]:
            jobs[-1] = (jobs[-1][0], name.group(1).strip("'\""))
    if not jobs:
        sys.exit("dev_docs: found `jobs:` in ci.yml but no job under it")
    return jobs


def render_ci_gates(lang: str) -> str:
    lines = [PROSE[lang]["ci_head"], "|---|---|"]
    lines += [f"| `{job}` | {name} |" for job, name in ci_jobs()]
    return "\n".join(lines) + "\n"


def render_ratchets(lang: str) -> str:
    """The NAMES of the debt ratchets, from the baseline the gate compares against — not
    their values, which change every time the debt goes down. A ratchet added to
    arch_fitness.py lands in the baseline in the same commit, and so here."""
    import json

    names = json.loads((ROOT / "scripts" / "arch_baseline.json").read_text())
    lines = [f"- `{name}`" for name in names]
    return PROSE[lang]["ratchets"].format(n=len(names)) + "\n\n" + "\n".join(lines) + "\n"


def render_crate_count(crates: list[dict], lang: str) -> str:
    bins = sorted(b for c in crates for b in c["bins"])
    return PROSE[lang]["crate_count"].format(
        crates=len(crates), bins=len(bins), names=", ".join(f"`{b}`" for b in bins)
    )


def regions(lang: str = "en") -> dict[str, str]:
    arch = load_arch_fitness()
    crates = workspace_crates()
    return {
        "toolchain": render_toolchain(lang),
        "layers": render_layers(arch, lang),
        "crate-count": render_crate_count(crates, lang),
        "crates-table": render_crates_table(arch, crates, lang),
        "crates-graph": render_crates_graph(arch, crates),
        "ci-gates": render_ci_gates(lang),
        "ratchets": render_ratchets(lang),
    }


def lang_dir(lang: str) -> Path:
    return DEV_DOCS if lang == "en" else DEV_DOCS / lang


def strip_regions(text: str) -> str:
    """The page without the bodies of its generated regions. A translation records the
    hash of THIS form of its English source, so regenerating facts (a crate added,
    a CI job renamed) does not mark every translation as behind — only a change to
    the narrative does."""
    return MARKER.sub(lambda m: m.group(1) + m.group(4), text)


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

    stale = []
    used = set()
    for lang in LANGS:
        generated = regions(lang)
        for path in sorted(lang_dir(lang).glob("*.md")):
            text = path.read_text()
            if lang == "en":
                used.update(m.group("key") for m in MARKER.finditer(text))
            new = rewrite(text, generated, path)
            if new != text:
                stale.append(path.relative_to(ROOT))
                if not args.check:
                    path.write_text(new)
    generated = regions("en")

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
