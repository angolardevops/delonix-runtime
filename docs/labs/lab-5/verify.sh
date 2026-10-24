#!/usr/bin/env bash
# Lab 5 · verificar — ASSERÇÃO funcional do ciclo de vida da microVM. Prova,
# lendo `vm ls -o json`, que a VM arranca Running com IPv4, que o snapshot com a
# VM a correr existe e o restore a mantém de pé, e que `vm destroy` não deixa
# órfãos. Falha em voz alta, distinguindo cada modo.
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-5
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"
state_load
: "${NAME:?corre prepare.sh primeiro}" "${SNAP:?}"

log "A · a VM está Running com um IPv4 (lido de vm ls -o json, com retry)"
STATUS="$(vm_field "$NAME" status)"
[ -n "$STATUS" ] || die "a VM '${NAME}' não existe no 'vm ls' — o 'create' não a definiu (corre run.sh e lê a saída)."
if IP="$(vm_wait_running_ip "$NAME" 90)"; then
  ok "vm ls → ${NAME} Running ${IP}"
else
  STATUS="$(vm_field "$NAME" status)"; IP="$(vm_field "$NAME" ip)"
  [ "$STATUS" = "Running" ] \
    && die "a VM '${NAME}' está Running mas NUNCA recebeu IPv4 em 90s (último IP: '${IP:-vazio}') — rede/cloud-init, não o ciclo de vida." \
    || die "a VM '${NAME}' nunca ficou Running em 90s (último status: '${STATUS:-vazio}') — o arranque falhou."
fi

log "B · o checkpoint existe: 'snapshot ls' mostra '${SNAP}'"
SNAPS="$("${DELONIX}" vm snapshot ls "$NAME" 2>/dev/null)"
printf '%s\n' "$SNAPS" | grep -qw "$SNAP" \
  || die "o snapshot '${SNAP}' não aparece em 'vm snapshot ls ${NAME}' — o checkpoint com a VM a correr falhou. Saída: ${SNAPS:-vazia}"
ok "snapshot '${SNAP}' presente (tirado com a VM a correr → memória+disco)"

log "C · o restore manteve a VM de pé (Running com IP)"
if IP2="$(vm_wait_running_ip "$NAME" 60)"; then
  ok "após restore: ${NAME} Running ${IP2}"
else
  die "após o restore a VM '${NAME}' não voltou a Running+IP em 60s (status: '$(vm_field "$NAME" status)') — o restore não reconstituiu o estado."
fi

log "D · teardown real: 'vm destroy' remove tudo e NÃO deixa órfãos"
# É o ponto onde o texto antigo errava: o verbo é `vm destroy`, não `delete vms`.
OUT="$("${DELONIX}" vm destroy "$NAME" -f 2>&1)" || die "vm destroy '${NAME}' falhou: ${OUT}"
printf '%s\n' "$OUT" | grep -qiE 'destroyed|freed|artifact' && ok "vm destroy: $(printf '%s' "$OUT" | grep -iE 'destroyed|freed' | head -1)"
# Órfão = o record ainda ser resolvível depois do destroy.
if "${DELONIX}" describe vm "$NAME" >/dev/null 2>&1 || [ -n "$(vm_field "$NAME" status)" ]; then
  die "ÓRFÃO: a VM '${NAME}' ainda é resolvível depois do 'vm destroy' — ficou record/disco por trás."
fi
ok "sem órfãos: 'describe vm ${NAME}' já não resolve (DX-4501) e saiu do 'vm ls'"

ok "TODAS as asserções do Lab 5 passaram"
