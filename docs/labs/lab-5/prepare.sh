#!/usr/bin/env bash
# Lab 5 · preparar — garante o binário, uma imagem de VM local e terreno limpo.
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-5
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"

log "a verificar pré-requisitos"
need python3
delonix_ok

NAME="lab5vm"
SNAP="limpa"
# Escolhe uma imagem de VM LOCAL (o `vm create` sem --disk só serve quando há
# EXATAMENTE uma; aqui fixamos uma para ser reproduzível e não descarregar GiB).
# Preferimos a 'k8s-golden' (cloud-init, pequena); senão a primeira que exista.
IMG="$(
  "${DELONIX}" vm image ls 2>/dev/null | awk 'NR>1 && $1!="" {print $1}' | {
    grep -m1 -E 'k8s-golden' || true
  }
)"
if [ -z "$IMG" ]; then
  IMG="$("${DELONIX}" vm image ls 2>/dev/null | awk 'NR>1 && $1!="" {print $1; exit}')"
fi
[ -n "$IMG" ] || die "não há nenhuma imagem de VM local (vm image ls vazio). Puxa uma primeiro (ex.: delonix vm image pull …) ou deixa o 'vm create' descarregar a oficial."
state_set NAME "$NAME"
state_set SNAP "$SNAP"
state_set IMG  "$IMG"

# Idempotência: destrói restos de uma execução anterior (disco+snapshots+record).
if [ -n "$(vm_field "$NAME" status)" ] || "${DELONIX}" describe vm "$NAME" >/dev/null 2>&1; then
  warn "VM '${NAME}' já existia — a destruir (vm destroy leva disco+snapshots+record)"
  "${DELONIX}" vm destroy "$NAME" -f >/dev/null 2>&1 || true
fi

ok "preparado — vm=${NAME} imagem=${IMG} snapshot=${SNAP}"
