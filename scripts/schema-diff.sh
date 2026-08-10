#!/usr/bin/env bash
# Diferença do schema do manifesto entre duas versões.
#
# O schema é gerado do código (ADR-0007), por isso a única forma honesta de
# saber o que mudou nos Kinds entre duas versões é gerá-lo nas duas e comparar
# — escrever um changelog à mão seria recriar exactamente a segunda fonte de
# verdade que o ADR-0007 aboliu.
#
#   scripts/schema-diff.sh v0.46.0            # dessa tag até à árvore actual
#   scripts/schema-diff.sh v0.45.0 v0.46.0    # entre duas tags
#
# Sai 0 sem diferenças, 1 com diferenças (para um gate de CI), 2 em erro.
set -euo pipefail

die() { echo "erro: $*" >&2; exit 2; }
[ $# -ge 1 ] || die "uso: $0 <ref-antiga> [ref-nova]"

OLD="$1"
NEW="${2:-}"
ROOT="$(git rev-parse --show-toplevel)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# O schema publicado é, por contrato, o gerado (há um teste a garanti-lo), por
# isso lê-se do ficheiro em vez de se construir o binário de cada versão — o
# que tornaria isto lento ao ponto de ninguém o correr.
SCHEMA_PATH="docs/schema/v1/delonix.json"

git -C "$ROOT" show "$OLD:$SCHEMA_PATH" > "$TMP/old.json" 2>/dev/null \
  || die "$OLD não tem $SCHEMA_PATH (a versão é anterior ao schema gerado?)"

if [ -n "$NEW" ]; then
  git -C "$ROOT" show "$NEW:$SCHEMA_PATH" > "$TMP/new.json" 2>/dev/null \
    || die "$NEW não tem $SCHEMA_PATH"
else
  cp "$ROOT/$SCHEMA_PATH" "$TMP/new.json" \
    || die "não encontrei $SCHEMA_PATH na árvore actual"
fi

# Comparação campo a campo, e não `diff` do JSON cru: o que interessa a quem
# versiona manifestos é «que campos apareceram, desapareceram ou mudaram de
# tipo», não a formatação nem a ordem das chaves.
python3 - "$TMP/old.json" "$TMP/new.json" <<'PY'
import json, sys

def fields(doc):
    out = {}
    for name, d in (doc.get("$defs") or {}).items():
        for f, spec in (d.get("properties") or {}).items():
            t = spec.get("type")
            if isinstance(t, list):
                t = "|".join(x for x in t if x != "null")
            out[f"{name}.{f}"] = t or "?"
        for f in (d.get("required") or []):
            out[f"{name}.{f}"] = out.get(f"{name}.{f}", "?") + " (obrigatório)"
    return out

old, new = (fields(json.load(open(p))) for p in sys.argv[1:3])
added   = sorted(set(new) - set(old))
removed = sorted(set(old) - set(new))
changed = sorted(k for k in set(old) & set(new) if old[k] != new[k])

for k in added:   print(f"+ {k}: {new[k]}")
for k in removed: print(f"- {k}  (REMOVIDO — quebra de contrato)")
for k in changed: print(f"~ {k}: {old[k]} → {new[k]}  (TIPO MUDADO — quebra de contrato)")

if removed or changed:
    print("\nQuebra de contrato: um campo não pode ser removido nem mudar de tipo "
          "dentro do 0.x (ver docs/cli-stability.md).", file=sys.stderr)
    sys.exit(1)
if added:
    print("\nSó adições — compatível.", file=sys.stderr)
    sys.exit(1)
print("sem alterações de schema", file=sys.stderr)
PY
