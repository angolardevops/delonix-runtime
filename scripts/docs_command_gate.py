#!/usr/bin/env python3
"""Gate barato do site `docs/gen.py`: o binário e a documentação concordam no
que existe, SEM gerar uma única página.

Corre a asserção `contract_gate()` do próprio `docs/gen.py` (importado como
módulo — só define dicts/funções e lê `sys.argv[1]`, não gera nada até alguém
chamar `main()`), portanto lê o `GROUPS`/`GROUP_PATH`/`KINDS_DOC` REAIS, nunca uma
segunda cópia que pudesse ela mesma derivar do ficheiro que devia verificar.

Apanha três derivas silenciosas (ver o docstring de `contract_gate()`):

  * um comando NOVO no COMMAND MAP do binário que `GROUPS` ainda não documenta
    (descartado sem aviso do índice/cheatsheet pelo `if g in GROUPS`);
  * uma página de topo em `GROUPS` que já não é um comando do binário (binário
    stale, como o que não tinha `bucket`, ou comando removido);
  * um `examples/<grupo>/<kind>.yaml` embebido em falta (hoje publica um bloco
    `# (exemplo em falta: …)` em silêncio).

Relação com `scripts/docs_cli_gate.py`: aquele resolve CADA `delonix …` citado na
documentação contra a árvore inteira do binário (`cli-tree.sh`) e precisa do
binário em `target/`. Este é a fatia barata — um só `delonix --help` — e aceita
um caminho de binário explícito, útil quando se gera contra uma árvore cujo
`target/` vive noutro sítio (ex.: um worktree sem build próprio).

    python3 scripts/docs_command_gate.py                 # usa ./target/release|debug/delonix
    python3 scripts/docs_command_gate.py /caminho/delonix  # binário explícito

Sai 0 se baterem certo; 1 (com a divergência) caso contrário.
"""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def binary() -> str:
    if len(sys.argv) > 1:
        return sys.argv[1]
    for c in (ROOT / "target/release/delonix", ROOT / "target/debug/delonix"):
        if c.is_file():
            return str(c)
    sys.exit(
        "sem binário: passa o caminho como argumento ou corre "
        "`cargo build --release -p delonix-runtime-bin`"
    )


def load_gen(bin_path: str):
    # `docs/gen.py` lê `sys.argv[1]` para o caminho do binário no import.
    sys.argv = ["gen.py", bin_path]
    spec = importlib.util.spec_from_file_location(
        "delonix_docs_gen", ROOT / "docs" / "gen.py"
    )
    assert spec and spec.loader
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def main() -> int:
    bin_path = binary()
    gen = load_gen(bin_path)
    try:
        gen.contract_gate()  # SystemExit com a divergência, se houver
    except SystemExit as e:
        print(e, file=sys.stderr)
        return 1
    members = gen.command_map_members()
    examples = sum(1 for _ in gen.KINDS_DOC)
    print(
        f"ok: COMMAND MAP ({len(members)} membros) e as páginas de `GROUPS` batem "
        f"certo, e os {examples} exemplos de Kind embebidos existem ({bin_path})"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
