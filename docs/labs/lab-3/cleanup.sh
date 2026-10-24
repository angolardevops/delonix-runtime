#!/usr/bin/env bash
# Lab 3 · limpar — remove SÓ o que o lab criou. A NetworkPolicy do Dependency
# vive no firewall da 'db' e vai-se com ela (comprovado ao vivo), por isso não
# há política órfã a limpar à parte.
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-3
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"
state_load
NET="${NET:-loja}"
NAME_DB="${NAME_DB:-db}"
NAME_APP="${NAME_APP:-app}"

log "a remover os containers ${NAME_DB} e ${NAME_APP} (leva a policy associada)"
"${DELONIX}" container rm -f "$NAME_DB" "$NAME_APP" >/dev/null 2>&1 && ok "containers removidos" || warn "já não existiam"

log "a remover a rede ${NET}"
"${DELONIX}" network rm "$NET" >/dev/null 2>&1 && ok "rede removida" || warn "já não existia"

# Não removemos as imagens (alpine/nginx/postgres): outros labs reutilizam-nas.
rm -rf "${LAB_STATE_DIR}"
ok "estado do lab (incl. dep.yaml) limpo"
