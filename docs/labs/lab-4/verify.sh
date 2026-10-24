#!/usr/bin/env bash
# Lab 4 · verificar — ASSERÇÃO funcional do HEALTHCHECK. Prova que a coluna
# STATUS de `container ps -o json` mostra `(healthy)` (não só `Up`), e DISTINGUE
# «build falhou» de «healthcheck nunca passou». Falha em voz alta em qualquer um.
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-4
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"
state_load
: "${TAG:?corre prepare.sh primeiro}" "${NAME:?}" "${BUILD_DIR:?}" "${WAIT_TIMEOUT:?}"

log "A · o BUILD produziu a imagem (${TAG})"
# Primeira bifurcação: sem imagem, o build falhou — e dizemo-lo com essa
# palavra. Tudo o que vem a seguir pressupõe que a imagem existe.
image_exists "$TAG" \
  || die "BUILD FALHOU: a imagem '${TAG}' não existe. O erro está no 'build', NÃO no healthcheck — corre run.sh e lê a saída do build."
ok "imagem '${TAG}' presente no store local"

log "B · precedência: o Delonixfile venceu um Dockerfile que falharia"
# O isco (Dockerfile com base inexistente) tem de ter estado lá para a prova
# fazer sentido: o build passou (A) COM ele presente → escolheu o Delonixfile.
[ -f "${BUILD_DIR}/Dockerfile" ] \
  || die "o Dockerfile-isco não está em ${BUILD_DIR} — não posso afirmar que o Delonixfile teve precedência; corre run.sh."
assert_contains "$(cat "${BUILD_DIR}/Dockerfile")" "esta-imagem-nao-existe" \
  "o Dockerfile-isco falharia o build; como o build PASSOU, 'delonix build' sem -f escolheu o Delonixfile primeiro"

log "C · o container está Up (não saiu)"
STATUS="$(container_field "$NAME" status)"
case "$STATUS" in
  *Up*) ok "${NAME}: ${STATUS}" ;;
  "")   die "container '${NAME}' não existe — a imagem construiu mas o 'run' não o criou." ;;
  *)    die "container '${NAME}' não está Up (STATUS: ${STATUS}) — o processo saiu; não é um healthcheck que nunca passa, é o container a morrer." ;;
esac

log "D · HEALTHCHECK: a coluna STATUS mostra (healthy), com retry"
# `(healthy)` só aparece depois de uma ou duas sondas, mesmo após o --wait ter
# retornado — por isso repetimos sobre `container ps -o json`, nunca um snapshot
# único. Como A já provou que o build correu, um «nunca fica healthy» aqui é
# INEQUIVOCAMENTE o healthcheck a não passar, não o build.
HEALTHY=""
for _ in $(seq 1 "${WAIT_TIMEOUT}"); do
  STATUS="$(container_field "$NAME" status)"
  case "$STATUS" in
    *"(healthy)"*)   HEALTHY=1; break ;;
    *"(unhealthy)"*) die "HEALTHCHECK FALHOU: STATUS='${STATUS}'. A imagem construiu (A), mas a sonda ficou 'unhealthy' — o problema é o HEALTHCHECK, não o build." ;;
  esac
  sleep 1
done
[ -n "$HEALTHY" ] \
  || die "HEALTHCHECK NUNCA PASSOU dentro de ${WAIT_TIMEOUT}s: último STATUS='${STATUS:-vazio}'. A imagem existe (A) e o container está Up (C) — logo NÃO é o build; é a sonda de saúde que nunca ficou verde."
ok "STATUS='${STATUS}' — o motor sondou o HEALTHCHECK sozinho (sem systemd) e o container ficou (healthy)"

ok "TODAS as asserções do Lab 4 passaram"
