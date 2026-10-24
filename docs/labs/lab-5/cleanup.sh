#!/usr/bin/env bash
# Lab 5 · limpar — `vm destroy` da VM (idempotente: o verify já a destrói no
# passo D, por isso aqui normalmente já não existe). Não toca nas imagens de VM
# locais (outros labs reutilizam-nas).
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-5
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"
state_load
NAME="${NAME:-lab5vm}"

if [ -n "$(vm_field "$NAME" status)" ] || "${DELONIX}" describe vm "$NAME" >/dev/null 2>&1; then
  log "a destruir a VM ${NAME} (disco+snapshots+record)"
  "${DELONIX}" vm destroy "$NAME" -f >/dev/null 2>&1 && ok "VM destruída" || warn "falhou a destruir"
else
  ok "VM ${NAME} já não existia (o verify destrói-a no passo D)"
fi

rm -rf "${LAB_STATE_DIR}"
ok "estado do lab limpo"
