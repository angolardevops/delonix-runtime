#!/usr/bin/env bash
# Lab 6 · verificar — ASSERÇÃO funcional: a imagem que construímos existe no
# store (lido de `vm image ls -o json`), uma VM arranca Running com IPv4 A
# PARTIR DELA, e o teardown com `vm destroy` não deixa órfãos. Falha em voz
# alta, distinguindo «imagem não construiu» de «VM não arrancou» de «órfão».
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-6
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"
state_load
: "${NAME:?corre prepare.sh primeiro}" "${TAG:?}"

log "A · a imagem '${TAG}' está no store (vm image ls -o json)"
vm_image_exists "$TAG" \
  || die "a imagem '${TAG}' não aparece em 'vm image ls' — o 'vm image build' não produziu o disco (corre run.sh e lê a saída do build)."
ok "imagem '${TAG}' construída e registada no store de imagens de VM"

log "B · uma VM arranca Running com IPv4 a partir da TUA imagem"
STATUS="$(vm_field "$NAME" status)"
[ -n "$STATUS" ] || die "a VM '${NAME}' não existe no 'vm ls' — o 'create --disk ${TAG}' não a definiu."
if IP="$(vm_wait_running_ip "$NAME" 120)"; then
  ok "vm ls → ${NAME} Running ${IP} (arrancou da imagem que construíste)"
else
  STATUS="$(vm_field "$NAME" status)"; IP="$(vm_field "$NAME" ip)"
  [ "$STATUS" = "Running" ] \
    && die "a VM '${NAME}' está Running mas NUNCA recebeu IPv4 em 120s (último IP: '${IP:-vazio}') — rede/cloud-init, não a imagem." \
    || die "a VM '${NAME}' nunca ficou Running em 120s (último status: '${STATUS:-vazio}') — a imagem construída não arranca."
fi

log "C · teardown real: 'vm destroy' remove tudo e NÃO deixa órfãos"
OUT="$("${DELONIX}" vm destroy "$NAME" -f 2>&1)" || die "vm destroy '${NAME}' falhou: ${OUT}"
printf '%s\n' "$OUT" | grep -qiE 'destroyed|freed|artifact' && ok "vm destroy: $(printf '%s' "$OUT" | grep -iE 'destroyed|freed' | head -1)"
if "${DELONIX}" describe vm "$NAME" >/dev/null 2>&1 || [ -n "$(vm_field "$NAME" status)" ]; then
  die "ÓRFÃO: a VM '${NAME}' ainda é resolvível depois do 'vm destroy' — ficou record/disco por trás."
fi
ok "sem órfãos: 'describe vm ${NAME}' já não resolve e saiu do 'vm ls'"
# A imagem construída SOBREVIVE ao destroy da VM (o overlay é por-VM) — é o
# cleanup que a remove. Prova-o aqui: destruir a VM não apaga a tua imagem.
vm_image_exists "$TAG" \
  || die "a imagem '${TAG}' desapareceu com o 'vm destroy' — o overlay por-VM não devia levar a base."
ok "a imagem '${TAG}' continua no store após o destroy (o overlay era por-VM)"

ok "TODAS as asserções do Lab 6 passaram"
