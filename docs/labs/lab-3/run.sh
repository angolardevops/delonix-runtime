#!/usr/bin/env bash
# Lab 3 · executar — uma rede própria, DNS interno por nome, e alcançabilidade
# DIRIGIDA com `kind: Dependency`. "Mesma rede, dois desfechos."
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-3
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"
state_load
: "${NET:?corre prepare.sh primeiro}" "${NAME_DB:?}" "${NAME_APP:?}" \
  "${IMG_DB:?}" "${IMG_APP:?}" "${IMG_THIRD:?}" "${DB_PORT:?}" "${HOST_PORT:?}" "${DEP_FILE:?}"

log "1. uma rede própria (DNS interno vem de graça, não se configura)"
"${DELONIX}" network create "$NET"

log "2. base de dados e app, ambas na rede ${NET}"
"${DELONIX}" container run -d --name "$NAME_DB"  --net "$NET" -e POSTGRES_PASSWORD=x "$IMG_DB"
"${DELONIX}" container run -d --name "$NAME_APP" --net "$NET" -p "${HOST_PORT}:80" "$IMG_APP"

log "3. a app resolve a '${NAME_DB}' PELO NOME — sem /etc/hosts, sem variáveis"
IP="$(container_resolves "$NAME_APP" "$NAME_DB")"
[ -n "$IP" ] && printf '    getent hosts %s → %s\n' "$NAME_DB" "$IP" || warn "ainda sem resolução (verify repete)"

# Espera a db ABRIR o 5432 (o postgres demora a aquecer). Sem isto, o baseline do
# passo 4 diria BLOCK por o serviço ainda não estar pronto — e não por política.
log "3b. esperar a db abrir o ${DB_PORT} (o postgres aquece devagar)"
assert_reach "$NAME_APP" "$NAME_DB" "$DB_PORT" 30 "db pronta (via app)"

log "4. ANTES da política: um terceiro container na MESMA rede ALCANÇA a db"
BEFORE="$(ephemeral_reaches "$NET" "$IMG_THIRD" "$NAME_DB" "$DB_PORT" 4)"
printf '    terceiro → %s:%s = %s\n' "$NAME_DB" "$DB_PORT" "$BEFORE"

log "5. fechar a db a tudo menos à app — kind: Dependency (baixa para NetworkPolicy)"
cat > "$DEP_FILE" <<EOF
apiVersion: delonix.io/v1
kind: Dependency
metadata:
  name: app-conhece-db
spec:
  from: ${NAME_APP}
  to: ${NAME_DB}
  ports: ["${DB_PORT}"]
EOF
"${DELONIX}" stack apply -f "$DEP_FILE"

log "6. DEPOIS da política: o MESMO terceiro já não alcança — alcançabilidade dirigida"
AFTER="$(ephemeral_reaches "$NET" "$IMG_THIRD" "$NAME_DB" "$DB_PORT" 4)"
printf '    terceiro → %s:%s = %s  (a app continua a alcançar)\n' "$NAME_DB" "$DB_PORT" "$AFTER"

ok "jornada executada — agora corre verify.sh"
