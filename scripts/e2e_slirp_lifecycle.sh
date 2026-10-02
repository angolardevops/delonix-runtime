#!/usr/bin/env bash
# O slirp4netns de um container (`-p` sem rede própria) acaba com o container,
# e não segura nada do arranque que o criou.
#
#   e2e_slirp_lifecycle.sh <cenário> <binário> <imagem> [prefixo]
#
# Chamado pelo `scripts/e2e.sh` (secção «ciclo de vida»), um cenário por check;
# corre sozinho contra qualquer binário, com `DELONIX_ROOT` e
# `DELONIX_NET_RUNTIME_DIR` já isolados pelo chamador. Sai 0 (certo), 1
# (defeito, com a medição na saída) ou 77 (o host não deixa medir).
#
# Medido a 2026-10-01/02: um `slirp4netns` não sai com o processo que serve.
# Sai quando o kernel desmonta a netns, e isso levou 13 s e 17 s num host
# calmo e mais de uma hora (dois deles, à escuta na porta publicada) num host
# com o disco saturado. Tudo o que aqui se mede decorre disso:
#
#   fds     — o slirp herdava do arranque as duas pontas do pipe de logs e a
#             ponta de escrita do handshake do supervisor (`readlink
#             /proc/<slirp>/fd/*`), e o comando do container a de leitura dos
#             logs e a do handshake. O shim de logs nunca via EOF.
#   exit    — um container supervisionado que sai sozinho deixava o slirp à
#             escuta e o shim vivo: o registo perde o pid com a saída, e o pid
#             era a única coisa que nomeava o slirp.
#   start   — com o supervisor morto e o slirp antigo na porta, o `container
#             start` pendurava para sempre: o `add_hostfwd` do slirp novo era
#             recusado, o supervisor dizia-o e saía, e o pai continuava a ler o
#             pipe do handshake à espera de um EOF que só o slirp novo — que o
#             herdara e ficara para trás — podia dar.
#   listed  — sem supervisor, o primeiro `container ps` regista a morte e apaga
#             o pid; o `rm` a seguir já não tinha por onde chegar ao slirp.
#   infra   — o pin e o plano de controlo da rede vivem enquanto a infra viver,
#             e eram lançados com os descritores de quem calhasse arrancá-la
#             (aqui um fd 9 aberto pelo chamador). Corre em roots SEUS: subir e
#             descer a infra do chamador reiniciaria tudo o que lá corre.
set -u
scenario="${1:?cenário}"; BIN="${2:?binário}"; IMG="${3:?imagem}"; PFX="${4:-slp$$}"
name="$PFX-$scenario"

die() { echo "$*"; cleanup; exit 1; }
cannot() { echo "$*"; cleanup; exit 77; }
cleanup() { timeout 120 "$BIN" container rm -f "$name" >/dev/null 2>&1; }

free_port() {
  python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])'
}
field() { # field <nome> <campo>
  "$BIN" container inspect "$1" 2>/dev/null |
    python3 -c 'import json,sys;v=json.load(sys.stdin)[0].get(sys.argv[1]);print("" if v is None else v)' "$2"
}
# O slirp que serve o pid $1: `… <pid> tap0` no fim do argv.
slirp_of() {
  local p a
  for p in /proc/[0-9]*; do
    a=$({ tr '\0' ' ' <"$p/cmdline"; } 2>/dev/null) || continue
    case "$a" in *slirp4netns*" $1 tap0 ") echo "${p#/proc/}"; return ;; esac
  done
}
# Processos com o ficheiro de log do container $1 aberto — o shim de logs.
shims_of() {
  local log="$DELONIX_ROOT/containers/$1/log" d
  for d in /proc/[0-9]*; do
    ls -l "$d/fd" 2>/dev/null | grep -qF -- "-> $log" && echo "${d#/proc/}"
  done
}
listening() { ss -Hltn "sport = :$1" 2>/dev/null | grep -q .; }
# wait_for <segundos> <comando…> — verdadeiro assim que o comando o for.
wait_for() {
  local n=$(( $1 * 5 )); shift
  while (( n-- > 0 )); do "$@" && return 0; sleep 0.2; done
  "$@"
}
gone() { [[ ! -e /proc/$1 ]]; }
not_listening() { ! listening "$1"; }
no_shims() { [[ -z "$(shims_of "$1")" ]]; }

if [[ "$scenario" == infra ]]; then
  # Curto: o socket de controlo vive aqui e o `sun_path` são 108 bytes.
  own="/tmp/dlxi-$$"
  export DELONIX_ROOT="$own/root" DELONIX_NET_RUNTIME_DIR="$own/run"
  mkdir -p "$DELONIX_ROOT" "$DELONIX_NET_RUNTIME_DIR"
  mark="$own/callers-fd"
  cleanup() { timeout 60 "$BIN" net netns down >/dev/null 2>&1; rm -rf "$own"; }
  ( exec 9>"$mark"; timeout 120 "$BIN" net netns up >/dev/null 2>&1 ) ||
    cannot "a infra de rede não sobe neste host (net netns up)"
  held=""
  for d in /proc/[0-9]*; do
    ls -l "$d/fd" 2>/dev/null | grep -qF -- "-> $mark" &&
      held+="${d#/proc/}($({ tr '\0' ' ' <"$d/cmdline"; } 2>/dev/null | cut -c1-60)) "
  done
  [[ -z "$held" ]] || die "a infra ficou com um descritor de quem a arrancou: $held"
  cleanup
  exit 0
fi

command -v slirp4netns >/dev/null || cannot "sem slirp4netns neste host"
port=$(free_port)
cleanup
# O container fica à espera de /go: sai com 0 quando o cenário o pedir.
WAIT_GO='while [ ! -e /go ]; do sleep 0.2; done'

run() { # run <flags…> — arranca o container do cenário e devolve-o em $id/$pid/$slirp
  timeout 180 "$BIN" container run -d --name "$name" -p "$port:8000" "$@" "$IMG" sh -c "$WAIT_GO" >/dev/null 2>&1 ||
    cannot "o container não arrancou neste host (run -d -p)"
  id=$(field "$name" id); pid=$(field "$name" pid)
  [[ -n "$pid" ]] || die "arrancou sem pid no registo"
  slirp=$(slirp_of "$pid")
  [[ -n "$slirp" ]] || die "nenhum slirp4netns serve o pid $pid do container"
}

case "$scenario" in
fds)
  run --restart always
  held=$(ls -l "/proc/$slirp/fd" 2>/dev/null | grep -o 'pipe:\[[0-9]*\]' | sort -u | tr '\n' ' ')
  [[ -z "$held" ]] || die "o slirp4netns $slirp segura pipes do arranque: $held"
  extra=$(ls "/proc/$pid/fd" 2>/dev/null | awk '$1 > 2' | tr '\n' ' ')
  [[ -z "$extra" ]] || die "o comando do container herdou descritores do arranque: $extra($(ls -l "/proc/$pid/fd" | awk '$9 > 2 {print $9 $10 $11}' | tr '\n' ' '))"
  ;;
exit)
  # `on-failure` e uma saída com 0: há supervisor, e ele não reinicia.
  run --restart on-failure
  listening "$port" || die "a porta $port não ficou publicada"
  timeout 60 "$BIN" container exec "$name" touch /go >/dev/null 2>&1 || die "exec touch /go falhou"
  wait_for 120 gone "$pid" || cannot "o container ainda está a sair ao fim de 120s (disco saturado?)"
  # 5s, e não mais: o slirp acaba por dar pela netns desaparecida, mas quando
  # o kernel a desmontar — medido 13 s, 17 s e «nunca» (uma hora) no binário
  # anterior. Quem o tem de soltar é o supervisor, no instante em que colhe o
  # processo.
  wait_for 5 gone "$slirp" || die "o container saiu sozinho e o slirp4netns $slirp ficou vivo"
  wait_for 5 not_listening "$port" || die "a porta $port continua à escuta depois de o container sair"
  wait_for 15 no_shims "$id" || die "o shim de logs ficou vivo depois de o container sair: $(shims_of "$id" | tr '\n' ' ')"
  ;;
start)
  run --restart always
  sup=$(awk '{print $4}' "/proc/$pid/stat")
  kill -9 "$sup" "$pid" 2>/dev/null
  wait_for 120 gone "$pid" || cannot "o container ainda está a sair ao fim de 120s (disco saturado?)"
  # A pré-condição do defeito: ninguém ficou para soltar a porta.
  [[ -e /proc/$slirp ]] && listening "$port" ||
    cannot "o slirp4netns saiu sozinho com o alvo — este host não reproduz o órfão"
  timeout 120 "$BIN" container start "$name" >/dev/null 2>"$DELONIX_ROOT/$name.err"
  rc=$?
  [[ $rc -ne 124 ]] || die "container start pendurou (120s) com o slirp antigo na porta"
  [[ $rc -eq 0 ]] || die "container start falhou (rc=$rc): $(head -c 400 "$DELONIX_ROOT/$name.err")"
  rm -f "$DELONIX_ROOT/$name.err"
  gone "$slirp" || die "o slirp4netns $slirp da incarnação morta continua vivo depois do start"
  new=$(field "$name" pid)
  [[ -n "$(slirp_of "$new")" ]] || die "a nova incarnação (pid $new) não tem slirp"
  timeout 120 "$BIN" container rm -f "$name" >/dev/null 2>&1 || die "rm -f falhou"
  wait_for 15 not_listening "$port" || die "a porta $port continua à escuta depois do rm -f"
  wait_for 15 no_shims "$id" || die "ficaram shims de logs depois do rm -f: $(shims_of "$id" | tr '\n' ' ')"
  ;;
listed)
  run
  timeout 60 "$BIN" container exec "$name" touch /go >/dev/null 2>&1 || die "exec touch /go falhou"
  wait_for 120 gone "$pid" || cannot "o container ainda está a sair ao fim de 120s (disco saturado?)"
  # O `ps` reconcilia e apaga o pid do registo; só o `rm` vem depois.
  "$BIN" container ps -a >/dev/null 2>&1
  [[ -z "$(field "$name" pid)" ]] || die "o ps não registou a saída do container"
  timeout 120 "$BIN" container rm "$name" >/dev/null 2>&1 || die "rm falhou"
  wait_for 15 gone "$slirp" || die "o rm de um container já saído deixou o slirp4netns $slirp vivo"
  wait_for 5 not_listening "$port" || die "a porta $port continua à escuta depois do rm"
  ;;
*)
  echo "cenário desconhecido: $scenario (fds|exit|start|listed|infra)"; exit 2 ;;
esac
cleanup
exit 0
