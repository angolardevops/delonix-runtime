#!/usr/bin/env bash
# Lab 5 · orquestrador — preparar → executar → verificar → limpar.
#
#   ./lab.sh            # jornada completa, com limpeza no fim
#   ./lab.sh --no-clean # deixa a VM de pé para explorares (vm console / vm ssh)
#   DELONIX=/caminho/delonix ./lab.sh   # binário específico (validação ao vivo)
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLEAN=1; [ "${1:-}" = "--no-clean" ] && CLEAN=0

bash "${DIR}/prepare.sh"
bash "${DIR}/run.sh"
bash "${DIR}/verify.sh"
if [ "$CLEAN" = 1 ]; then bash "${DIR}/cleanup.sh"; else
  printf '\n(--no-clean) nota: o verify destrói a VM no passo D. Para explorar, corre\nprepare.sh + run.sh e pára antes do verify.\n'
fi
