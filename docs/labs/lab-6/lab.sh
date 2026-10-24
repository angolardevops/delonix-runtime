#!/usr/bin/env bash
# Lab 6 · orquestrador — preparar → executar → verificar → limpar.
#
#   ./lab.sh            # jornada completa, com limpeza no fim
#   ./lab.sh --no-clean # deixa a imagem construída de pé para a explorares
#   DELONIX=/caminho/delonix ./lab.sh   # binário específico (validação ao vivo)
#
# Nota: o build assa a base com libguestfs e descarrega `FROM ubuntu:24.04` —
# conta minutos na primeira vez.
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLEAN=1; [ "${1:-}" = "--no-clean" ] && CLEAN=0

bash "${DIR}/prepare.sh"
bash "${DIR}/run.sh"
bash "${DIR}/verify.sh"
if [ "$CLEAN" = 1 ]; then bash "${DIR}/cleanup.sh"; else
  printf '\n(--no-clean) nota: o verify destrói a VM no passo C, mas a imagem\nconstruída fica no store (`delonix vm image ls`). Para arrancar outra:\n  delonix vm create outra --disk minha-base:1.0\n'
fi
