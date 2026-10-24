#!/usr/bin/env bash
# Lab 1 · executar — a jornada que o leitor aprende: run → ps → observar → logs → exec.
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-1
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"
state_load
: "${NAME:?corre prepare.sh primeiro}" "${PORT:?}" "${IMAGE:?}"

log "1. um serviço web, em segundo plano, publicado em localhost:${PORT}"
"${DELONIX}" container run -d --name "$NAME" -p "${PORT}:80" "$IMAGE"

log "2. confirmar — STATUS diz há quanto tempo está de pé"
"${DELONIX}" container ps

# 3. A verdade sobre o "daemonless": NÃO há um daemon central arrancado no boot,
#    mas CADA recurso tem holders leves (netns pin/control, o holder do run) que
#    são filhos do init do TEU utilizador — não um dockerd partilhado.
log "3. processos 'delonix' residentes enquanto o container corre (os holders deste recurso)"
ps -o pid,ppid,comm,args -C delonix 2>/dev/null | tee "${LAB_STATE_DIR}/holders.txt" || true

log "4. falar com o serviço"
curl -s "localhost:${PORT}" | head -3 | tee "${LAB_STATE_DIR}/body.txt"

log "5. entrar lá dentro"
"${DELONIX}" container exec "$NAME" sh -c 'hostname; id -u'

log "6. o que ele escreveu"
"${DELONIX}" container logs "$NAME" 2>&1 | tail -5 | tee "${LAB_STATE_DIR}/logs.txt"

ok "jornada executada — agora corre verify.sh"
