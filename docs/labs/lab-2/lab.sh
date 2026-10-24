#!/usr/bin/env bash
# Lab 2 · orquestrador — preparar → executar → verificar → limpar.
#
#   ./lab.sh            # jornada completa, com limpeza no fim
#   ./lab.sh --no-clean # deixa o container de pé para explorares
#   DELONIX=/caminho/delonix ./lab.sh   # binário específico (validação ao vivo)
#
# Se o teu host NÃO delegar o controlador `memory` (verify falha com
# memory.max=max), re-corre tudo dentro de um scope delegado, sem root:
#   systemd-run --user --scope -p Delegate=yes -- bash docs/labs/lab-2/lab.sh
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLEAN=1; [ "${1:-}" = "--no-clean" ] && CLEAN=0

bash "${DIR}/prepare.sh"
bash "${DIR}/run.sh"
bash "${DIR}/verify.sh"
if [ "$CLEAN" = 1 ]; then bash "${DIR}/cleanup.sh"; else
  printf '\n(--no-clean) o container continua de pé. Para limpar: %s/cleanup.sh\n' "$DIR"
fi
