#!/usr/bin/env bash
# Lab 2 · preparar — garante pré-requisitos e deixa o terreno limpo.
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-2
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"

log "a verificar pré-requisitos"
need python3; need awk
delonix_ok

NAME="lab2-limited"
MEM="128M"
MEM_BYTES="134217728"   # 128 * 1024 * 1024
IMAGE="alpine"
state_set NAME      "$NAME"
state_set MEM       "$MEM"
state_set MEM_BYTES "$MEM_BYTES"
state_set IMAGE     "$IMAGE"

# Idempotência: se já corria de uma execução anterior, remove para recomeçar.
if container_exists "$NAME"; then
  warn "container '${NAME}' já existia — a remover para recomeçar limpo"
  "${DELONIX}" container rm -f "$NAME" >/dev/null 2>&1 || true
fi

log "a garantir a imagem ${IMAGE} localmente"
"${DELONIX}" pull "$IMAGE" >/dev/null 2>&1 || "${DELONIX}" image pull "$IMAGE" >/dev/null 2>&1 || \
  warn "pull falhou agora; o 'run' volta a tentar (puxa se faltar)"
ok "preparado — nome=${NAME} limite=${MEM} (=${MEM_BYTES} bytes) imagem=${IMAGE}"
