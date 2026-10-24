#!/usr/bin/env bash
# Lab 3 · preparar — garante pré-requisitos, imagens e um terreno limpo.
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-3
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"

log "a verificar pré-requisitos"
need python3; need awk; need ss
delonix_ok

NET="loja"
NAME_DB="db"
NAME_APP="app"
IMG_DB="postgres:16-alpine"   # abre o 5432 (demora a aquecer — ver assert_reach)
IMG_APP="nginx:alpine"        # serve HTTP e traz busybox nc
IMG_THIRD="alpine"            # o "terceiro" sem identidade, com busybox nc
DB_PORT="5432"
HOST_PORT="$(free_port 18080)"   # publicação da app (evita colisão com 8080)
DEP_FILE="${LAB_STATE_DIR}/dep.yaml"
state_set NET       "$NET"
state_set NAME_DB   "$NAME_DB"
state_set NAME_APP  "$NAME_APP"
state_set IMG_DB    "$IMG_DB"
state_set IMG_APP   "$IMG_APP"
state_set IMG_THIRD "$IMG_THIRD"
state_set DB_PORT   "$DB_PORT"
state_set HOST_PORT "$HOST_PORT"
state_set DEP_FILE  "$DEP_FILE"

# Idempotência: apaga restos de uma execução anterior para recomeçar limpo.
for c in "$NAME_DB" "$NAME_APP"; do
  if container_exists "$c"; then
    warn "container '${c}' já existia — a remover (leva a policy associada com ele)"
    "${DELONIX}" container rm -f "$c" >/dev/null 2>&1 || true
  fi
done
if "${DELONIX}" network ls 2>/dev/null | awk '{print $1}' | grep -qx "$NET"; then
  warn "rede '${NET}' já existia — a remover para recomeçar limpa"
  "${DELONIX}" network rm "$NET" >/dev/null 2>&1 || true
fi
rm -f "$DEP_FILE"

log "a garantir as imagens localmente"
for img in "$IMG_DB" "$IMG_APP" "$IMG_THIRD"; do
  "${DELONIX}" pull "$img" >/dev/null 2>&1 || "${DELONIX}" image pull "$img" >/dev/null 2>&1 || \
    warn "pull de ${img} falhou agora; o 'run' volta a tentar (puxa se faltar)"
done
ok "preparado — rede=${NET} db=${NAME_DB}(${IMG_DB}) app=${NAME_APP}(${IMG_APP}) publicação=${HOST_PORT}:80"
