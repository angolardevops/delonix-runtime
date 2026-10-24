#!/usr/bin/env bash
# Lab 3 · verificar — ASSERÇÃO funcional de DNS interno e isolamento dirigido.
# Distingue "bloqueado por política" de "serviço em baixo", e FALHA EM VOZ ALTA
# se a db tiver ficado aberta a todos.
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-3
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"
state_load
: "${NET:?corre prepare.sh primeiro}" "${NAME_DB:?}" "${NAME_APP:?}" \
  "${IMG_THIRD:?}" "${DB_PORT:?}" "${HOST_PORT:?}"

log "A · a db e a app estão Up"
for c in "$NAME_DB" "$NAME_APP"; do
  STATUS="$(container_field "$c" status)"
  case "$STATUS" in *Up*) ok "${c}: ${STATUS}" ;; *) die "container '${c}' não está Up (STATUS: ${STATUS:-vazio})" ;; esac
done

log "B · DNS interno: a app resolve a '${NAME_DB}' PELO NOME"
IP="$(container_resolves "$NAME_APP" "$NAME_DB")"
printf '%s' "$IP" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' \
  || die "'${NAME_APP}' não resolveu '${NAME_DB}' para um IP (getent devolveu: ${IP:-vazio})"
ok "getent hosts ${NAME_DB} → ${IP} (DNS interno, sem /etc/hosts nem variáveis)"

log "C · a porta responde ao par AUTORIZADO (prova que o serviço está EM PÉ)"
# Se isto falhar, a db está em baixo — e dizemo-lo com essa palavra, não como
# se fosse política. É também a âncora que torna o passo D inequívoco.
assert_reach "$NAME_APP" "$NAME_DB" "$DB_PORT" 30 "app autorizada → ${NAME_DB}:${DB_PORT}"

log "D · isolamento DIRIGIDO: o terceiro container na MESMA rede está BLOQUEADO"
TOK="$(ephemeral_reaches "$NET" "$IMG_THIRD" "$NAME_DB" "$DB_PORT" 4)"
case "$TOK" in
  REACH)
    die "a db ficou ABERTA A TODOS: um terceiro na rede '${NET}' alcançou ${NAME_DB}:${DB_PORT}. O kind: Dependency NÃO restringiu nada — é exactamente a falha que este lab existe para apanhar." ;;
  NOTOOL)
    die "sem 'nc' no provador efémero (${IMG_THIRD}) — não consigo distinguir bloqueio de falta de ferramenta; asserção inconclusiva." ;;
  BLOCK)
    # C provou que a db serve o par autorizado AGORA; logo o bloqueio do terceiro
    # só pode ser política, não serviço em baixo. É a distinção pedida.
    ok "terceiro → ${NAME_DB}:${DB_PORT} = BLOQUEADO, enquanto a app AUTORIZADA alcança (C). Bloqueio por POLÍTICA, não serviço em baixo." ;;
  *)
    die "desfecho inesperado da sonda do terceiro: '${TOK:-vazio}'" ;;
esac

log "E · a app é um serviço real: HTTP 200 na porta publicada do host"
assert_http "http://127.0.0.1:${HOST_PORT}/" 200 20

ok "TODAS as asserções do Lab 3 passaram"
