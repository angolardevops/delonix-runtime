#!/usr/bin/env bash
# Lab 4 · executar — do Delonixfile à imagem, SEM daemon, e correr o que
# construíste. O motor sonda o HEALTHCHECK sozinho; `--wait` bloqueia até passar.
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-4
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"
state_load
: "${IMG_BASE:?corre prepare.sh primeiro}" "${TAG:?}" "${NAME:?}" "${BUILD_DIR:?}" \
  "${HEALTH_INTERVAL:?}" "${WAIT_TIMEOUT:?}"

log "1. um diretório de build com Delonixfile e o ficheiro que ele COPY-a"
mkdir -p "$BUILD_DIR"
cat > "${BUILD_DIR}/Delonixfile" <<EOF
FROM ${IMG_BASE}
RUN apk add --no-cache curl
COPY ola.txt /ola.txt
HEALTHCHECK CMD test -f /ola.txt
CMD ["sh", "-c", "cat /ola.txt; sleep 300"]
EOF
echo "construído com delonix" > "${BUILD_DIR}/ola.txt"

# Isco para PROVAR a precedência: um Dockerfile que falharia o build (base
# inexistente). Se o `build` sem `-f` o escolhesse, o build morria aqui. O
# build a passar com este ficheiro presente É a prova de que o Delonixfile vem
# primeiro. O verify confirma que o isco esteve mesmo lá.
cat > "${BUILD_DIR}/Dockerfile" <<'EOF'
FROM esta-imagem-nao-existe:nunca
RUN false  # se o build escolhesse o Dockerfile, morria aqui
EOF

log "2. build SEM -f — o Delonixfile é procurado antes de qualquer Dockerfile"
"${DELONIX}" build -t "$TAG" "$BUILD_DIR"

log "3. correr com --wait: o motor sonda o HEALTHCHECK e bloqueia até passar"
# Sem --wait escrever-se-ia `until ...; do sleep 1; done` à mão — e mal.
"${DELONIX}" container run -d --name "$NAME" \
  --wait --wait-timeout "$WAIT_TIMEOUT" --health-interval "$HEALTH_INTERVAL" "$TAG"

log "4. o estado, pelo CLI"
"${DELONIX}" container ps

ok "jornada executada — agora corre verify.sh"
