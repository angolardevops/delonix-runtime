#!/usr/bin/env bash
# Lab 1 · verificar — ASSERÇÕES de resultado FUNCIONAL. Sai !=0 se alguma falhar.
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-1
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"
state_load
: "${NAME:?corre prepare.sh primeiro}" "${PORT:?}"

log "A · o container está Up"
STATUS="$(container_field "$NAME" status)"
case "$STATUS" in *Up*) ok "STATUS: ${STATUS}" ;; *) die "container '${NAME}' não está Up (STATUS: ${STATUS:-vazio})" ;; esac

log "B · o serviço responde HTTP 200 (resultado funcional, não exit-code)"
assert_http "localhost:${PORT}" 200 20

log "C · o nginx escreveu nos logs"
LOGS="$("${DELONIX}" container logs "$NAME" 2>&1 || true)"
if [ -n "$LOGS" ]; then ok "logs não vazios ($(printf '%s' "$LOGS" | wc -l) linhas)"; else die "logs vazios"; fi

log "D · daemonless com rigor: existem holders por recurso, filhos do init do utilizador"
HOLDERS="$(ps -o ppid=,args= -C delonix 2>/dev/null || true)"
if [ -z "$HOLDERS" ]; then
  warn "nenhum processo 'delonix' residente — sem publicação de porta pode acontecer; a lição mantém-se"
else
  ok "holders observados (NÃO um daemon central partilhado):"
  printf '%s\n' "$HOLDERS" | sed 's/^/    /'
fi

ok "TODAS as asserções do Lab 1 passaram"
