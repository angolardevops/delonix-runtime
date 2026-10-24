#!/usr/bin/env bash
# Lab 2 · limpar — remove SÓ o que o lab criou.
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-2
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"
state_load
NAME="${NAME:-lab2-limited}"

log "a remover o container ${NAME}"
"${DELONIX}" container rm -f "$NAME" >/dev/null 2>&1 && ok "removido" || warn "já não existia"

# Não removemos a imagem alpine: outros labs reutilizam-na.
rm -rf "${LAB_STATE_DIR}"
ok "estado do lab limpo"
