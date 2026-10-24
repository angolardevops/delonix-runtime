#!/usr/bin/env bash
# Lab 4 · preparar — garante pré-requisitos, a imagem base e um terreno limpo.
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-4
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"

log "a verificar pré-requisitos"
need python3
delonix_ok

IMG_BASE="alpine:latest"       # base do Delonixfile
TAG="minha-app:1.0"            # imagem que construímos
NAME="a1"                      # container que corremos
# O lab cria Delonixfile/ola.txt num diretório PRÓPRIO (estado do lab), nunca no
# CWD do repo — senão sujaria a árvore de trabalho.
BUILD_DIR="${LAB_STATE_DIR}/lab-build"
HEALTH_INTERVAL="2"
WAIT_TIMEOUT="60"
state_set IMG_BASE        "$IMG_BASE"
state_set TAG             "$TAG"
state_set NAME            "$NAME"
state_set BUILD_DIR       "$BUILD_DIR"
state_set HEALTH_INTERVAL "$HEALTH_INTERVAL"
state_set WAIT_TIMEOUT    "$WAIT_TIMEOUT"

# Idempotência: apaga restos de uma execução anterior para recomeçar limpo.
if container_exists "$NAME"; then
  warn "container '${NAME}' já existia — a remover"
  "${DELONIX}" container rm -f "$NAME" >/dev/null 2>&1 || true
fi
if image_exists "$TAG"; then
  warn "imagem '${TAG}' já existia — a remover para construir de novo"
  "${DELONIX}" image remove "$TAG" >/dev/null 2>&1 || true
fi
rm -rf "$BUILD_DIR"

log "a garantir a imagem base (${IMG_BASE}) localmente"
"${DELONIX}" pull "$IMG_BASE" >/dev/null 2>&1 || "${DELONIX}" image pull "$IMG_BASE" >/dev/null 2>&1 || \
  warn "pull de ${IMG_BASE} falhou agora; o build volta a tentar (puxa se faltar)"

ok "preparado — base=${IMG_BASE} tag=${TAG} container=${NAME} build-dir=${BUILD_DIR}"
