#!/usr/bin/env bash
# Lab 6 · limpar — remove SÓ o que o lab criou: a VM (idempotente: o verify já a
# destrói no passo C), a imagem que construímos, o contexto de build e a chave
# do lab. Não toca nas imagens de VM de outros labs.
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-6
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"
state_load
NAME="${NAME:-lab6vm}"
TAG="${TAG:-minha-base:1.0}"

if [ -n "$(vm_field "$NAME" status)" ] || "${DELONIX}" describe vm "$NAME" >/dev/null 2>&1; then
  log "a destruir a VM ${NAME}"
  "${DELONIX}" vm destroy "$NAME" -f >/dev/null 2>&1 && ok "VM destruída" || warn "falhou a destruir a VM"
else
  ok "VM ${NAME} já não existia (o verify destrói-a no passo C)"
fi

if vm_image_exists "$TAG"; then
  log "a remover a imagem construída ${TAG}"
  "${DELONIX}" vm image rm "$TAG" >/dev/null 2>&1 && ok "imagem removida" || warn "falhou a remover a imagem"
else
  ok "imagem ${TAG} já não existia"
fi

rm -rf "${LAB_STATE_DIR}"
ok "estado do lab limpo (contexto de build e chave incluídos)"
