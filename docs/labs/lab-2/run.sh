#!/usr/bin/env bash
# Lab 2 · executar — perguntar se o host aplica limites, pôr um tecto e
# localizar o container no cgroup v2 do teu utilizador.
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-2
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"
state_load
: "${NAME:?corre prepare.sh primeiro}" "${MEM:?}" "${IMAGE:?}"

# 1. Perguntar ANTES de assumir. Se isto disser INERT/PARTIAL para `memory`, a
#    1.ª correcção não precisa de root: re-executa o lab dentro de um scope
#    delegado — `systemd-run --user --scope -p Delegate=yes -- bash lab.sh`.
#    Informativo: não falha o lab; a prova real é no cgroup (verify.sh).
log "1. perguntar ANTES de assumir: este host aplica limites?"
"${DELONIX}" system setup 2>&1 | sed 's/^/    /' || true

log "2. um container com tecto de memória (-m ${MEM})"
"${DELONIX}" container run -d --name "$NAME" -m "$MEM" "$IMAGE" sleep 300

log "3. onde vive o container no cgroup v2 do TEU utilizador"
PID="$(container_pid "$NAME")"
[ -n "$PID" ] || die "sem PID — o container arrancou? vê 'container ps -a'"
REL="$(cgroup_path_of_pid "$PID")"
[ -n "$REL" ] || die "sem linha 0:: em /proc/${PID}/cgroup — este host tem cgroup v2?"
state_set PID "$PID"
printf '    PID=%s\n    cgroup=%s\n' "$PID" "$REL"

log "4. a prova: ler o memory.max REAL do kernel, NÃO a saída do comando"
cat "/sys/fs/cgroup${REL}/memory.max" | tee "${LAB_STATE_DIR}/memory.max.txt"

ok "jornada executada — agora corre verify.sh"
