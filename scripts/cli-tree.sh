#!/usr/bin/env bash
# Percorre a árvore pública da CLI e imprime um caminho por linha.
#
# Existe porque a Fase CLI-0 precisa de um inventário REPRODUTÍVEL: a primeira
# medição desta reestruturação foi feita contra o `delonix` instalado no host e
# ficou seis comandos atrasada (faltavam `namespace`, `net l4guard`,
# `stack history`, `stack rollback`, `system doctor`, `system features`) — uma
# matriz de destinos construída sobre isso deixava-os de fora sem ninguém dar
# por isso. Mede-se SEMPRE o binário da árvore, e diz-se qual.
#
#   scripts/cli-tree.sh              # um caminho por linha
#   scripts/cli-tree.sh --leaves     # só as folhas invocáveis
#   scripts/cli-tree.sh --count      # os totais
#   scripts/cli-tree.sh --classify   # folha + classe de impacto
#   scripts/cli-tree.sh --gate       # falha se alguma folha ficou sem destino
#
# Classes: `=` sem quebra · `~` muda de grafia · `→` movida por decisão escrita
# (secção 10 de docs/discovery/51_CLI_INVENTARIO.md) · `!` quebra contrato
# publicado.
#
# O `--gate` NÃO se baseia no classificador: o `else` final dele é `~`, portanto
# nada cai nunca numa classe «por decidir» e um gate construído sobre isso seria
# decorativo — escrevi-o assim à primeira e ele passou verde sobre nove folhas
# deliberadamente sabotadas. Compara-se contra uma LINHA DE BASE gravada, como o
# `lang_ratchet.py` faz com a dívida de língua: uma folha que não esteja lá é uma
# folha que ninguém classificou.
#
# `DELONIX_BIN` sobrepõe o binário usado.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
BIN=${DELONIX_BIN:-}
if [ -z "$BIN" ]; then
  for c in "$repo/target/release/delonix" "$repo/target/debug/delonix"; do
    [ -x "$c" ] && BIN=$c && break
  done
fi
[ -n "$BIN" ] && [ -x "$BIN" ] || {
  echo "sem binário: constrói com 'cargo build --release -p delonix-runtime-bin' ou aponta DELONIX_BIN" >&2
  exit 1
}

BASELINE="$repo/scripts/cli_baseline.tsv"

subs_of() {  # imprime os subcomandos listados no --help de um caminho
  "$BIN" $1 --help 2>/dev/null | awk '
    /^Commands:/ { inc=1; next }
    /^Options:/  { inc=0 }
    inc && /^  [a-z]/ { print $1 }'
}

walk() {
  local path="$*" s subs
  subs=$(subs_of "$path")
  if [ -z "$subs" ]; then echo "LEAF|$path"; return; fi
  echo "NODE|$path"
  for s in $subs; do
    [ "$s" = "help" ] && continue
    walk "$path $s"
  done
}

tree() {
  local top
  for top in $(subs_of ""); do
    [ "$top" = "help" ] && continue
    walk "$top"
  done
}

# A classe de impacto de cada folha na reestruturação. A ordem dos ramos importa:
# o primeiro que casa ganha, e o default é `?` (fail-closed) — uma folha nova que
# ninguém classificou aparece como decisão em aberto, nunca como "sem quebra".
classify() {
  awk -F'|' '/^LEAF/{print $2}' | awk '
    {
      c = $0; cls = "?"
      if (c == "build" || c == "container apply")                 cls = "!"
      # As 17 que a Fase CLI-0 abriu e a secção 10 do inventário fechou. `→` é
      # «movida por decisão escrita», e é uma classe à parte de `~` de propósito:
      # um destino DECIDIDO não se lê como uma renomeação de rotina, e quem
      # revir a CLI-5 tem de poder distingui-las.
      else if (c ~ /^net netns /)                                 cls = "→"
      else if (c ~ /^net l4guard/)                                cls = "→"
      else if (c == "stack history")                              cls = "→"
      else if (c == "stack rollback")                             cls = "→"
      else if (c == "container ssh")                              cls = "→"
      else if (c == "vm bridge" || c == "vm unbridge")            cls = "→"
      else if (c ~ /^container /)                                 cls = "="
      else if (c ~ /^vm (start|stop|restart|console|ssh)$/)       cls = "="
      else if (c ~ /^vm snapshot /)                               cls = "="
      else if (c ~ /^image (pull|push|build|scan|verify|convert|import|export|tag|login|logout)$/) cls = "="
      else if (c == "pod logs" || c == "cluster load")            cls = "="
      else if (c == "secret create" || c == "stack init")         cls = "="
      else if (c ~ /^system (info|events|df|prune|doctor|features|setup|resources)$/) cls = "="
      # `api-resources` é da árvore-alvo, não da antiga: nasce no destino, logo
      # `=` e não `~`. Sem este braço caía no `else` final e a linha de base
      # dizia «muda de grafia» sobre um comando que nunca teve outra.
      else if (c ~ /^(explain|init|man|completion|version|api-resources)$/) cls = "="
      else if (c ~ /^compose /)                                   cls = "="
      else if (c ~ /^serve (cri|docker-api)$/)                    cls = "="
      # `mcp` (ADR-0025) é superfície nova, sem relação nenhuma com a
      # reestruturação — mesma razão do `api-resources` acima.
      else if (c ~ /^mcp /)                                       cls = "="
      else                                                        cls = "~"
      print cls "\t" c
    }'
}

case "${1:-}" in
  --leaves)   tree | awk -F'|' '/^LEAF/{print $2}' ;;
  --classify) tree | classify ;;
  --gate)
    # A Fase CLI-0 fechou com as 233 folhas classificadas. Isto mantém-no
    # verdadeiro: um comando acrescentado a partir de agora não está na linha de
    # base e falha AQUI, em vez de ser apagado por omissão quando a CLI-5 correr
    # o corte limpo.
    [ -f "$BASELINE" ] || { echo "sem linha de base: corre '$0 --update'" >&2; exit 1; }
    novas=$(comm -23 <(tree | classify | sort) <(sort "$BASELINE"))
    idas=$(comm -13 <(tree | classify | sort) <(sort "$BASELINE"))
    rc=0
    if [ -n "$novas" ]; then
      echo "FALHA: folhas sem destino decidido (ou com a classe mudada):" >&2
      printf '%s\n' "$novas" | sed 's/^/  + /' >&2
      rc=1
    fi
    if [ -n "$idas" ]; then
      echo "FALHA: folhas que a linha de base tem e a árvore já não:" >&2
      printf '%s\n' "$idas" | sed 's/^/  - /' >&2
      echo "  (se a remoção é intencional, baixa a base no MESMO commit)" >&2
      rc=1
    fi
    [ $rc -eq 0 ] && echo "ok: as $(tree | grep -c '^LEAF') folhas batem com a linha de base"
    exit $rc
    ;;
  --update)
    tree | classify | sort > "$BASELINE"
    echo "linha de base actualizada: $(grep -c . "$BASELINE") folhas"
    ;;
  --count)
    t=$(tree)
    printf 'binário:  %s\n' "$BIN"
    printf 'comandos: %s\n' "$(printf '%s\n' "$t" | grep -c .)"
    printf 'folhas:   %s\n' "$(printf '%s\n' "$t" | grep -c '^LEAF')"
    printf 'grupos:   %s\n' "$(printf '%s\n' "$t" | awk -F'|' '{print $2}' | awk '{print $1}' | sort -u | wc -l)"
    echo '--- classes ---'
    printf '%s\n' "$t" | classify | cut -f1 | sort | uniq -c | sort -rn
    ;;
  *)          tree ;;
esac
