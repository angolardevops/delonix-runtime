#!/usr/bin/env bash
# Lab 1 · preparar — garante os pré-requisitos e deixa a imagem local.
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-1
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"

log "a verificar pré-requisitos"
need curl; need ss; need python3
delonix_ok

NAME="lab1-web"
PORT="$(free_port 18080)"
IMAGE="nginx:alpine"
state_set NAME  "$NAME"
state_set PORT  "$PORT"
state_set IMAGE "$IMAGE"

# Se já corria de uma execução anterior, remove para o lab ser idempotente.
if container_exists "$NAME"; then
  warn "container '${NAME}' já existia — a remover para recomeçar limpo"
  "${DELONIX}" container rm -f "$NAME" >/dev/null 2>&1 || true
fi

log "a garantir a imagem ${IMAGE} localmente"
"${DELONIX}" pull "$IMAGE" >/dev/null 2>&1 || "${DELONIX}" image pull "$IMAGE" >/dev/null 2>&1 || \
  warn "pull falhou agora; o 'run' volta a tentar (puxa se faltar)"
ok "preparado — nome=${NAME} porta=${PORT} imagem=${IMAGE}"
