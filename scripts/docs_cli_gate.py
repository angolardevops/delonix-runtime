#!/usr/bin/env python3
"""Gate de documentação: cada `delonix ...` citado na documentação CORRENTE
tem de resolver na árvore do binário.

Existe porque duas remoções de comandos seguidas (v2.0.0 e v3.0.0) passaram sem
ninguém dar por elas: oito ficheiros de documentação corrente continuaram a
ensinar `delonix pod ls`, `delonix vm status` e `delonix schema print`, todos a
responder `unrecognized subcommand` no binário publicado. Um leitor que copie a
linha da página bate no erro; o gate bate primeiro.

A lista autoritativa de caminhos vem de `scripts/cli-tree.sh`, que percorre o
`--help` do binário DA ÁRVORE. Não se mede contra o `delonix` instalado no host:
a primeira medição deste achado foi feita assim e veio de uma versão atrás.

    python3 scripts/docs_cli_gate.py            # verifica (exit 1 se houver órfãos)
    python3 scripts/docs_cli_gate.py --list     # mostra cada citação e onde está

O que NÃO se verifica, e porquê:

  * `docs/releases/*.md`, `docs/RELEASES.md`, `docs/AUDITORIA-E2E.md` e
    `docs/discovery/*` são registo histórico datado. É CORRECTO citarem a grafia
    da versão de que falam — reescrevê-las apagava o registo.
  * Os cinco relatórios de auditoria/medição em `docs/` (`COMPARACAO-DOCKER-
    PODMAN.md`, `paridade-docker-podman.md`, `comparacao-medida.md`,
    `RELATORIO-PRE-PRODUCAO.md`, `cri-conformance.md`) são da MESMA classe —
    cada um já tem data e versão no próprio cabeçalho, e nenhum é reescrito a
    cada release. Ficam publicados (`comparacao.html`/`cri.html`/`AGENTS.md`/
    `cli-stability.md` ligam-nos como o detalhe por trás de um resumo), só não
    são "documentação corrente" para efeitos deste gate.
  * Só se extraem citações de CONTEXTO DE CÓDIGO (`<code>`, `<pre>`, cercas de
    markdown, `::` de reST, comentários de YAML). Prosa como «o delonix é um
    motor» não é uma citação de comando e não se tenta resolver.
"""

from __future__ import annotations

import argparse
import html
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CLI_TREE = ROOT / "scripts" / "cli-tree.sh"
ALLOW = Path(__file__).resolve().parent / "docs_cli_allow.tsv"

# Dated historical record — see the module docstring.
EXCLUDE = (
    "docs/releases/",
    "docs/RELEASES.md",
    "docs/AUDITORIA-E2E.md",
    "docs/discovery/",
    # Relatórios de auditoria/medição datados — ver a nota no docstring do
    # módulo. Medido 2026-09-11: nenhum produzia uma citação órfã hoje; a
    # exclusão é preventiva de categorização, não a correcção de uma falha
    # activa — mas pertencem à mesma classe dos quatro de cima, e não à
    # "documentação corrente" que este gate mantém sincronizada com o binário.
    "docs/COMPARACAO-DOCKER-PODMAN.md",
    "docs/paridade-docker-podman.md",
    "docs/comparacao-medida.md",
    "docs/RELATORIO-PRE-PRODUCAO.md",
    "docs/cri-conformance.md",
)

TARGETS = (
    "docs/*.html",
    "docs/comandos/*.html",
    "docs/*.md",
    "docs/roadmap/*.md",
    "docs/runtime/*.md",
    "examples/*.md",
    "examples/*.yaml",
    "examples/**/*.yaml",
    "examples/**/*.md",
    "README.rst",
)

# A token starting with `-`, `$`, `<`, `{` or an upper-case letter is no longer a
# subcommand: it is a flag, a shell substitution or a placeholder.
ARG = re.compile(r"^[a-z][a-z0-9-]*$")

# `[^\S\n]` and not `\s`: an invocation lives on ONE line. With `\s` the regex
# crossed the newline and glued the `delonix` ending one line to the first token
# of the next — that is how citations of `delonix echo` and `delonix delonix`,
# which nobody wrote, showed up.
INVOCATION = re.compile(
    r"\bdelonix[^\S\n]+((?:[A-Za-z0-9$<{./_-][^\s|;&>)\"']*)"
    r"(?:[^\S\n]+[^\s|;&>)\"']+)*)"
)


def paths_from_binary() -> tuple[set[str], set[str]]:
    """Os caminhos da árvore, separados em nós e folhas.

    A distinção não é cosmética: um NÓ (`net tunnel`) só aceita subcomandos, e
    uma FOLHA (`get`) aceita argumentos livres. Sem ela, `delonix net tunnel ls`
    resolvia pelo prefixo `net tunnel` e o `ls` — removido — passava calado.
    """
    out = subprocess.run(
        ["bash", str(CLI_TREE)], capture_output=True, text=True, cwd=ROOT
    )
    if out.returncode != 0:
        sys.stderr.write(out.stderr)
        sys.exit("cli-tree.sh falhou — constrói o binário da árvore primeiro")
    nodes: set[str] = set()
    leaves: set[str] = set()
    for line in out.stdout.splitlines():
        if "|" not in line:
            continue
        kind, path = line.split("|", 1)
        (leaves if kind.strip() == "LEAF" else nodes).add(path.strip())
    return nodes, leaves


def code_regions(path: Path) -> list[str]:
    """Devolve só os pedaços do ficheiro que são contexto de código."""
    text = path.read_text(encoding="utf-8", errors="replace")
    suffix = path.suffix.lower()
    regions: list[str] = []

    if suffix in {".html", ".htm"}:
        for m in re.finditer(r"<(code|pre)\b[^>]*>(.*?)</\1>", text, re.S | re.I):
            regions.append(html.unescape(re.sub(r"<[^>]+>", "", m.group(2))))
    elif suffix in {".md", ".markdown"}:
        for m in re.finditer(r"^```.*?^```", text, re.S | re.M):
            regions.append(m.group(0))
        # Indented code blocks (4 spaces), not just fences: `examples/
        # nas-vm-sdn-ch.md` writes every command that way and slipped past the
        # first version of this gate entirely.
        for m in re.finditer(r"(?:^(?: {4}|\t).*\n)+", text, re.M):
            regions.append(m.group(0))
        for m in re.finditer(r"`([^`\n]+)`", text):
            regions.append(m.group(1))
    elif suffix == ".rst":
        for m in re.finditer(r"``([^`]+)``", text):
            regions.append(m.group(1))
        # reST literal blocks: `::` followed by indented lines
        for m in re.finditer(r"::\n\n((?:[ \t]+\S.*\n|\n)+)", text):
            regions.append(m.group(1))
    elif suffix in {".yaml", ".yml"}:
        for line in text.splitlines():
            if line.lstrip().startswith("#"):
                regions.append(line)
    else:
        regions.append(text)
    return regions


def citations(path: Path):
    """(comando_normalizado, linha_do_ficheiro) para cada citação."""
    raw = path.read_text(encoding="utf-8", errors="replace")
    for region in code_regions(path):
        for m in INVOCATION.finditer(region):
            tokens = []
            for tok in m.group(1).split():
                if not ARG.match(tok):
                    break
                tokens.append(tok)
            if not tokens:
                continue
            snippet = "delonix " + " ".join(tokens)
            line = 0
            idx = raw.find(snippet)
            if idx >= 0:
                line = raw.count("\n", 0, idx) + 1
            yield tokens, line


def binary_accepts(path: str, cache: dict[str, bool]) -> bool:
    """Pergunta ao próprio binário se este caminho existe.

    A árvore do `cli-tree.sh` lê a secção `Commands:` do `--help`, que lista os
    nomes CANÓNICOS e não os ALIASES — `container ls` é `container ps` e não
    aparece lá. Sem este recurso, cada alias documentado saía como órfão.
    """
    if path not in cache:
        r = subprocess.run(
            [str(binary()), *path.split(), "--help"], capture_output=True, text=True
        )
        cache[path] = r.returncode == 0
    return cache[path]


def binary() -> Path:
    for c in (ROOT / "target/release/delonix", ROOT / "target/debug/delonix"):
        if c.is_file():
            return c
    sys.exit("sem binário: corre `cargo build --release -p delonix-runtime-bin`")


def resolve(
    tokens: list[str],
    nodes: set[str],
    leaves: set[str],
    cache: dict[str, bool] | None = None,
) -> str | None:
    """Desce a árvore token a token. Devolve o caminho atingido, ou None.

    Três desfechos:
      * cai numa FOLHA — o resto são argumentos (`delonix get pods` → `get`);
      * fica num NÓ e a citação acaba aí (`delonix vm --help`) — vale;
      * fica num NÓ com mais um token que NÃO é subcomando dele — não resolve,
        e é este o caso que apanha `net tunnel ls` e `vm status`.
    """
    path: list[str] = []
    for tok in tokens:
        candidate = " ".join(path + [tok])
        if candidate in leaves:
            return candidate
        if candidate in nodes:
            path.append(tok)
            continue
        # The token is not a child of the node we are standing on. A node takes
        # subcommands and nothing else, so there is no reading of this that
        # resolves — including the removed-subcommand case this gate exists for.
        # One last authority before failing: the tree lists canonical names, so
        # a documented ALIAS (`container ls` for `container ps`) is missing from
        # it and only the binary can confirm it.
        if cache is not None and binary_accepts(candidate, cache):
            return candidate
        return None
    return " ".join(path) if path else None


def allowed() -> dict[tuple[str, str], str]:
    """As excepções deliberadas, com a razão de cada uma.

    É uma lista de EXCEPÇÕES e não um filtro heurístico de propósito: prosa como
    «delonix picks a free one» e uma nota de migração como «o `delonix restore`
    de raiz passou a…» são indistinguíveis de um comando removido sem alguém
    ler a frase. Quem acrescentar uma linha tem de escrever porquê, e uma
    citação nova que ensine a correr um comando morto continua a falhar.
    """
    if not ALLOW.exists():
        return {}
    out: dict[tuple[str, str], str] = {}
    for raw in ALLOW.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split("\t")
        if len(parts) < 3:
            sys.exit(f"docs_cli_allow.tsv: 3 colunas separadas por TAB — {raw!r}")
        out[(parts[0], parts[1])] = parts[2]
    return out


def collect() -> list[Path]:
    seen: list[Path] = []
    for pattern in TARGETS:
        for p in sorted(ROOT.glob(pattern)):
            rel = p.relative_to(ROOT).as_posix()
            if any(rel.startswith(x) for x in EXCLUDE):
                continue
            if p.is_file() and p not in seen:
                seen.append(p)
    return seen


def readme_group_table() -> list[str]:
    """The `Group` column of README.rst's «Command groups» list-table.

    Each cell is a code-span (``x``) or a couple joined by `·`; what comes back
    is the flat set of group names the table claims to document.
    """
    readme = ROOT / "README.rst"
    if not readme.exists():
        return []
    text = readme.read_text(encoding="utf-8", errors="replace")
    try:
        table = text.split("Command groups", 1)[1].split("\nLanguages", 1)[0]
    except IndexError:
        return []
    names: list[str] = []
    for line in table.splitlines():
        stripped = line.strip()
        # Only the FIRST column of a row (`   * - ``x``` ), never the prose in
        # the second: that one legitimately names subcommands and flags.
        if not stripped.startswith("* - "):
            continue
        names.extend(re.findall(r"``([a-z][a-z0-9-]*)``", stripped))
    return names


def check_readme_groups(nodes: set[str], leaves: set[str]) -> list[str]:
    """**Every top-level group is in the README table, and the table invents none.**

    The `delonix ...` citation check above never saw this: a table cell holding
    ```volumes``` is not a command line, so nothing tried to resolve it. Measured
    2026-09-10, with the gate green: the table still documented five groups that
    the v2/v3 restructuring had removed (`volumes`, `storage`, `sharevolume`,
    `schema`, `dash`) and promised six subcommands that were cut with them
    (`pod ls/describe/rm`, `vm status`, `image --vm`, `net boot`) — while
    fourteen real groups, the generic verbs among them, were missing entirely.

    A reader picking a group name off that table typed a command the binary
    answers `unrecognized subcommand` to, which is the exact failure the rest of
    this gate exists to prevent.
    """
    tops = {p.split()[0] for p in list(nodes) + list(leaves) if p}
    claimed = set(readme_group_table())
    problems = []
    for name in sorted(claimed - tops):
        problems.append(f"README.rst cita o grupo `{name}`, que o binário não tem")
    for name in sorted(tops - claimed):
        problems.append(f"README.rst não documenta o grupo `{name}`")
    return problems


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--list", action="store_true", help="mostra cada citação resolvida")
    args = ap.parse_args()

    nodes, leaves = paths_from_binary()
    allow = allowed()
    cache: dict[str, bool] = {}
    orphans: list[tuple[str, int, str]] = []
    used: set[tuple[str, str]] = set()
    total = 0

    for path in collect():
        rel = path.relative_to(ROOT).as_posix()
        for tokens, line in citations(path):
            total += 1
            cmd = "delonix " + " ".join(tokens)
            hit = resolve(tokens, nodes, leaves, cache)
            if hit is not None:
                if args.list:
                    print(f"ok    {rel}:{line}\t{cmd}\t→ {hit}")
                continue
            if (rel, cmd) in allow:
                used.add((rel, cmd))
                if args.list:
                    print(f"excp  {rel}:{line}\t{cmd}\t— {allow[(rel, cmd)]}")
                continue
            orphans.append((rel, line, cmd))

    rc = 0
    if orphans:
        print(
            f"FALHA: {len(orphans)} citação(ões) da documentação corrente não "
            f"resolvem na árvore do binário ({total} verificadas):",
            file=sys.stderr,
        )
        for rel, line, cmd in orphans:
            print(f"  {rel}:{line}\t{cmd}", file=sys.stderr)
        print(
            "  (se a citação é deliberada — prosa, ou uma frase cujo assunto É o "
            "comando não existir —\n   acrescenta-a a scripts/docs_cli_allow.tsv "
            "COM a razão)",
            file=sys.stderr,
        )
        rc = 1

    # An exception that no longer matches anything is debt posing as coverage:
    # either the sentence was fixed, or the file was renamed. Either way the line
    # has to go, or the list grows until nobody can read it.
    stale = sorted(set(allow) - used)
    if stale:
        print(
            f"FALHA: {len(stale)} excepção(ões) em docs_cli_allow.tsv que já não "
            "casam com nenhuma citação:",
            file=sys.stderr,
        )
        for rel, cmd in stale:
            print(f"  {rel}\t{cmd}", file=sys.stderr)
        rc = 1

    table = check_readme_groups(nodes, leaves)
    if table:
        print(
            f"FALHA: {len(table)} divergência(s) entre a tabela «Command groups» "
            "do README e os grupos do binário:",
            file=sys.stderr,
        )
        for line in table:
            print(f"  {line}", file=sys.stderr)
        rc = 1

    if rc == 0:
        print(
            f"ok: as {total} citações da documentação corrente resolvem na árvore "
            f"({len(allow)} excepções declaradas), e a tabela de grupos do README "
            "bate com o binário"
        )
    return rc


if __name__ == "__main__":
    raise SystemExit(main())
