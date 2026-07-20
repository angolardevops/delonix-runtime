#!/usr/bin/env bash
# Gera docs/RELEASES.md — o apêndice "features por release" — a partir de
# docs/releases/<tag>.md (uma nota por release, a MESMA publicada no GitHub
# Releases; a 1.ª linha é o título). Determinístico e idempotente: corre
# localmente ou no workflow de release (que o comita no main a cada tag).
set -euo pipefail
cd "$(dirname "$0")/.."
out=docs/RELEASES.md
{
  echo "# Delonix Runtime — features por release"
  echo
  echo "> Gerado por \`scripts/gen-releases.sh\` a partir de \`docs/releases/<tag>.md\`"
  echo "> (regenerado automaticamente pelo pipeline de release a cada tag publicada)."
  echo "> Não editar à mão — edita a nota da release respectiva."
  echo
  for f in $(ls docs/releases/v*.md | sort -rV); do
    cat "$f"
    echo
    echo "---"
    echo
  done
} > "$out"
echo "gerado: $out ($(grep -c '^## ' "$out") releases)"
