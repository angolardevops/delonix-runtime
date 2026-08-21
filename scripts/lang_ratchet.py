#!/usr/bin/env python3
"""Gate de língua: o código deste repositório escreve-se em inglês.

Conta três dívidas — identificadores, comentários e mensagens ao utilizador
ainda em português — e compara com `scripts/lang_baseline.json`.

O gate é um RATCHET, não um tecto: falha se o número SUBIR (entrou português
novo) e falha se DESCER sem a linha de base ter sido baixada no mesmo commit.
Um `<=` deixaria a dívida a ler-se como verde para sempre.

    python3 scripts/lang_ratchet.py             # verifica (exit 1 se desalinhado)
    python3 scripts/lang_ratchet.py --list      # mostra o que falta traduzir
    python3 scripts/lang_ratchet.py --update    # rebaixa a linha de base
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import unicodedata
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LEXICON = Path(__file__).resolve().parent / "lang_pt_lexicon.txt"
BASELINE = Path(__file__).resolve().parent / "lang_baseline.json"

SKIP_DIRS = {".git", "target", "node_modules", ".claude", "vendor", "dist", "build"}

# Declarações que introduzem um nome, por linguagem.
DECL = {
    ".rs": re.compile(
        r"\b(?:fn|struct|enum|trait|const|static|type|mod|union)\s+([A-Za-z_][A-Za-z0-9_]*)"
        r"|\blet\s+(?:mut\s+)?([a-z_][a-z0-9_]*)"
    ),
    ".ts": re.compile(
        r"\b(?:function|class|interface|enum|type|const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)"
    ),
    ".go": re.compile(r"\b(?:func|type|var|const)\s+([A-Za-z_][A-Za-z0-9_]*)"),
    ".py": re.compile(r"\b(?:def|class)\s+([A-Za-z_][A-Za-z0-9_]*)"),
}
DECL[".tsx"] = DECL[".ts"]
DECL[".js"] = DECL[".ts"]

COMMENT = {
    ".rs": re.compile(r"^\s*(?://|///|//!)\s*(.+)$"),
    ".ts": re.compile(r"^\s*(?://|\*|/\*)\s*(.+)$"),
    ".go": re.compile(r"^\s*//\s*(.+)$"),
    ".py": re.compile(r"^\s*#\s*(.+)$"),
    ".yml": re.compile(r"^\s*#\s*(.+)$"),
}
COMMENT[".tsx"] = COMMENT[".ts"]
COMMENT[".js"] = COMMENT[".ts"]
COMMENT[".yaml"] = COMMENT[".yml"]

# Texto que o operador lê. Em Rust vem por macro; em YAML pelo `name:` do Ansible.
USER_TEXT = {
    ".rs": re.compile(
        r"(?:anyhow!|bail!|panic!|eprintln!|println!|format!|expect|unimplemented!)"
        r"\s*\(\s*\"([^\"]{8,})\""
    ),
    ".yml": re.compile(r"^\s*-?\s*name:\s*[\"']?([^\"'\n]{8,})"),
}
USER_TEXT[".yaml"] = USER_TEXT[".yml"]

WORD = re.compile(r"[A-Za-z]+")


def strip_accents(s: str) -> str:
    return "".join(c for c in unicodedata.normalize("NFD", s) if not unicodedata.combining(c))


def load_lexicon() -> set[str]:
    words = set()
    for line in LEXICON.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            words.add(strip_accents(line).lower())
    return words


def segments(identifier: str) -> list[str]:
    """snake_case e camelCase para a mesma lista de segmentos minúsculos."""
    spaced = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", identifier)
    return [s for s in spaced.lower().split("_") if s]


def is_pt_identifier(name: str, pt: set[str]) -> bool:
    return any(s in pt for s in segments(name))


def is_pt_text(text: str, pt: set[str]) -> bool:
    return any(strip_accents(w).lower() in pt for w in WORD.findall(text))


def walk(root: Path):
    # NB: filtrar por partes RELATIVAS — o caminho absoluto pode conter um
    # `.claude/worktrees/...` e engolia a árvore inteira em silêncio.
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        if any(part in SKIP_DIRS for part in path.relative_to(root).parts):
            continue
        if path.suffix in DECL or path.suffix in COMMENT:
            yield path


def scan(pt: set[str]):
    hits = {"identifiers": [], "comments": [], "user_text": []}
    for path in walk(ROOT):
        ext = path.suffix
        try:
            lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
        except OSError:
            continue
        rel = path.relative_to(ROOT).as_posix()
        for n, line in enumerate(lines, 1):
            if ext in DECL:
                for m in DECL[ext].finditer(line):
                    name = next((g for g in m.groups() if g), None)
                    if name and is_pt_identifier(name, pt):
                        hits["identifiers"].append((rel, n, name))
            if ext in COMMENT:
                m = COMMENT[ext].match(line)
                if m and is_pt_text(m.group(1), pt):
                    hits["comments"].append((rel, n, m.group(1)[:70]))
            if ext in USER_TEXT:
                for m in USER_TEXT[ext].finditer(line):
                    if is_pt_text(m.group(1), pt):
                        hits["user_text"].append((rel, n, m.group(1)[:70]))
    return hits


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--list", action="store_true", help="mostra cada ocorrência")
    ap.add_argument("--update", action="store_true", help="reescreve a linha de base")
    ap.add_argument("--only", choices=sorted(("identifiers", "comments", "user_text")))
    args = ap.parse_args()

    pt = load_lexicon()
    hits = scan(pt)
    counts = {k: len(v) for k, v in hits.items()}

    if args.list:
        for kind, rows in hits.items():
            if args.only and kind != args.only:
                continue
            print(f"\n=== {kind} ({len(rows)}) ===")
            for rel, n, what in rows:
                print(f"{rel}:{n}: {what}")
        return 0

    if args.update:
        BASELINE.write_text(json.dumps(counts, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(f"linha de base reescrita: {counts}")
        return 0

    if not BASELINE.exists():
        print("sem linha de base — corre `python3 scripts/lang_ratchet.py --update`", file=sys.stderr)
        return 1

    base = json.loads(BASELINE.read_text(encoding="utf-8"))
    failed = False
    for kind in sorted(counts):
        actual, expected = counts[kind], base.get(kind)
        if expected is None:
            print(f"FALHA {kind}: categoria nova sem linha de base ({actual})", file=sys.stderr)
            failed = True
        elif actual > expected:
            print(
                f"FALHA {kind}: {actual} > {expected} — entrou português novo.\n"
                f"       `python3 scripts/lang_ratchet.py --list --only {kind}` mostra onde.",
                file=sys.stderr,
            )
            failed = True
        elif actual < expected:
            print(
                f"FALHA {kind}: {actual} < {expected} — traduziste, mas não baixaste a\n"
                f"       linha de base. Corre `python3 scripts/lang_ratchet.py --update`\n"
                f"       e comita `scripts/lang_baseline.json` no MESMO commit.",
                file=sys.stderr,
            )
            failed = True
        else:
            print(f"ok    {kind}: {actual}")

    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
