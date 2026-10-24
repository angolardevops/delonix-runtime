#!/usr/bin/env bash
# Lab 4 · limpar — remove SÓ o que o lab criou: o container, a imagem construída
# e o diretório de build. NÃO remove a base (alpine): outros labs reutilizam-na.
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-4
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"
state_load
NAME="${NAME:-a1}"
TAG="${TAG:-minha-app:1.0}"

log "a remover o container ${NAME}"
"${DELONIX}" container rm -f "$NAME" >/dev/null 2>&1 && ok "container removido" || warn "já não existia"

log "a remover a imagem construída ${TAG}"
"${DELONIX}" image remove "$TAG" >/dev/null 2>&1 && ok "imagem removida" || warn "já não existia"

# O diretório de build (Delonixfile/ola.txt/Dockerfile-isco) vive no estado do
# lab, nunca no repo — vai-se com o resto do estado.
rm -rf "${LAB_STATE_DIR}"
ok "estado do lab (incl. diretório de build) limpo"
