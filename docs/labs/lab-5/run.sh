#!/usr/bin/env bash
# Lab 5 · executar — uma microVM a sério, com o mesmo CLI dos containers:
# create → Running+IP → checkpoint de sistema (memória+disco, a VM A CORRER) →
# restore. O teardown é `vm destroy` (não `delete vms`).
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-5
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"
state_load
: "${NAME:?corre prepare.sh primeiro}" "${SNAP:?}" "${IMG:?}"

log "1. criar a VM a partir de uma imagem local (vira um overlay por-VM)"
"${DELONIX}" vm create "$NAME" --disk "$IMG"

log "2. esperar Running com IP (o cloud-init/DHCP demora)"
if IP="$(vm_wait_running_ip "$NAME" 90)"; then
  printf '    vm ls → %s Running %s\n' "$NAME" "$IP"
else
  warn "ainda sem Running+IP (o verify repete e decide)"
fi

log "3. checkpoint de SISTEMA com a VM a correr — captura memória E disco"
# Com a VM a correr o snapshot é memória+disco (um checkpoint vivo). Parada
# seria disk-only — ao contrário do que a nota antiga do lab dizia, NÃO falha.
"${DELONIX}" vm snapshot create "$NAME" "$SNAP"
"${DELONIX}" vm snapshot ls "$NAME"

log "4. voltar atrás ao checkpoint (restore)"
"${DELONIX}" vm snapshot restore "$NAME" "$SNAP"

ok "jornada executada — agora corre verify.sh"
