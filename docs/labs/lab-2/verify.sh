#!/usr/bin/env bash
# Lab 2 · verificar — ASSERÇÃO funcional do cgroup REAL. Sai !=0 se o limite
# não pegou (incl. memory.max=max), em vez de passar em silêncio.
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-2
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"
state_load
: "${NAME:?corre prepare.sh primeiro}" "${MEM_BYTES:?}"

log "A · o container está Up"
STATUS="$(container_field "$NAME" status)"
case "$STATUS" in *Up*) ok "STATUS: ${STATUS}" ;; *) die "container '${NAME}' não está Up (STATUS: ${STATUS:-vazio})" ;; esac

log "B · o memory.max REAL do cgroup é ${MEM_BYTES} (128 MiB) — a verdade do kernel"
# Re-deriva o PID AGORA (não confia no estado): a prova tem de ler o cgroup vivo.
PID="$(container_pid "$NAME")"
[ -n "$PID" ] || die "sem PID para '${NAME}' — o container arrancou?"
assert_cgroup "$PID" memory.max "$MEM_BYTES" "tecto de memória"

ok "TODAS as asserções do Lab 2 passaram"
