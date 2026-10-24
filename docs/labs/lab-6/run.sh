#!/usr/bin/env bash
# Lab 6 · executar — constrói uma imagem qcow2 à tua medida a partir de um
# VMfile (como se constrói uma imagem de container) e arranca uma VM a partir
# dela: init → build → ls → create. O VMfile de scaffold constrói OFFLINE (os
# RUN não tocam na rede); só a base `FROM ubuntu:24.04` é descarregada.
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-6
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"
state_load
: "${NAME:?corre prepare.sh primeiro}" "${TAG:?}" "${BUILDDIR:?}"

log "1. o VMfile que vamos construir (a receita da imagem)"
sed -n '1,12p' "${BUILDDIR}/VMfile"

log "2. construir a imagem qcow2 (vm image build) — assa a base com libguestfs"
# Como a prosa do lab: entra no contexto e constrói. Sem -f, o build acha o
# VMfile no contexto; a base é descarregada, os RUN correm offline.
( cd "$BUILDDIR" && "${DELONIX}" vm image build -t "$TAG" . )

log "3. a imagem ficou no store local de imagens de VM"
"${DELONIX}" vm image ls

log "4. arrancar uma VM a partir da TUA imagem (vira um overlay por-VM)"
"${DELONIX}" vm create "$NAME" --disk "$TAG"

log "5. esperar Running com IP (o cloud-init/DHCP demora)"
if IP="$(vm_wait_running_ip "$NAME" 120)"; then
  printf '    vm ls → %s Running %s\n' "$NAME" "$IP"
else
  warn "ainda sem Running+IP (o verify repete e decide)"
fi

ok "jornada executada — agora corre verify.sh"
