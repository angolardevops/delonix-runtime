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
#   stats   — o `container stats` também regista a morte e apaga o pid, e não
#   kind      soltava o slirp; a listagem de clusters (`cluster ls`) idem.
#
# Como se prova que foi o COMANDO a soltar o slirp (`exit`, `stats`, `kind`):
# «o slirp desapareceu» não discrimina — ele acaba por sair sozinho, em menos
# de 1 s ou em 17 s conforme o binário e o host (medido com a netns segura por
# um descritor: saiu na mesma). Por isso o slirp é PARADO (SIGSTOP) antes de o
# container sair: parado não sai sozinho, e um SIGTERM que lhe mandem fica
# pendente e lê-se em `/proc/<slirp>/status`. Pendente = alguém o soltou;
# depois o SIGCONT deixa-o morrer. (Outro `delonix` neste host a varrer órfãos
# no mesmo segundo também o deixaria pendente; só pode fazer passar, nunca
# chumbar.)
#   hang    — o `container start` não pode bloquear sem fim quando o slirp
#             que lançou não responde (um falso no PATH que só dorme): no
#             binário anterior o `add_hostfwd` falhava, o slirp ficava vivo com
#             a ponta de escrita do handshake, e o pai lia-a para sempre.
#   zombies — o slirp e o shim de logs são filhos do supervisor, que só colhia
#             o container: um `--restart always` que cai sempre juntava dois
#             zombies por reinício (6 ao fim de 3, medido).
#   stopgaveup — um `stop` que desiste (DX-8101) já sinalizou o processo, e
#             solta as portas nesse momento. A saída é segura como no
#             `e2e_rm_force_gave_up.sh`: um `container exec` com o pai parado
#             deixa o PID 1 em `zap_pid_ns_processes`.
#   infra   — o pin e o plano de controlo da rede vivem enquanto a infra viver,
#             e eram lançados com os descritores de quem calhasse arrancá-la
#             (aqui um fd 9 aberto pelo chamador). Corre em roots SEUS: subir e
#             descer a infra do chamador reiniciaria tudo o que lá corre.
set -u
scenario="${1:?cenário}"; BIN="${2:?binário}"; IMG="${3:?imagem}"; PFX="${4:-slp$$}"
name="$PFX-$scenario"

die() { echo "$*"; cleanup; exit 1; }
cannot() { echo "$*"; cleanup; exit 77; }
cleanup() {
  [[ -n "${held_parent:-}" ]] && kill -CONT "$held_parent" 2>/dev/null
  [[ -n "${execpid:-}" ]] && kill "$execpid" 2>/dev/null
  # Um slirp que o cenário parou não fica parado para trás.
  [[ -n "${slirp:-}" ]] && { kill -CONT "$slirp"; kill "$slirp"; } 2>/dev/null
  timeout 120 "$BIN" container rm -f "$name" >/dev/null 2>&1
}

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
# Saiu: já não existe, ou é um zombie à espera do pai (o slirp é filho do
# supervisor, que o colhe entre incarnações).
gone() { [[ ! -e /proc/$1 ]] || [[ "$(awk '{print $3}' "/proc/$1/stat" 2>/dev/null)" == Z ]]; }
# Filhos zombie do processo $1.
zombies_of() {
  local c n=0
  for c in $(pgrep -P "$1"); do [[ "$(awk '{print $3}' "/proc/$c/stat" 2>/dev/null)" == Z ]] && n=$((n+1)); done
  echo "$n"
}
# Verdadeiro se o processo $1 tem um SIGTERM (bit 15) por entregar.
term_pending() {
  local a b
  a=$(awk '/^SigPnd:/{print $2}' "/proc/$1/status" 2>/dev/null) || return 1
  b=$(awk '/^ShdPnd:/{print $2}' "/proc/$1/status" 2>/dev/null) || return 1
  [[ -n "$a" && -n "$b" ]] && (( (0x$a | 0x$b) & 0x4000 ))
}
# O container sai sozinho com o slirp parado: ninguém o soltou ainda.
exit_with_slirp_stopped() {
  kill -STOP "$slirp" || die "não consegui parar o slirp4netns $slirp"
  timeout 60 "$BIN" container exec "$name" touch /go >/dev/null 2>&1 || die "exec touch /go falhou"
  wait_for 120 gone "$pid" || cannot "o container ainda está a sair ao fim de 120s (disco saturado?)"
}
# O slirp parado recebeu o SIGTERM de quem o soltou; acordado, morre.
released() { # released <segundos> <quem>
  wait_for "$1" term_pending "$slirp" ||
    die "$2 e ninguém soltou o slirp4netns $slirp (nenhum SIGTERM pendente)"
  kill -CONT "$slirp" 2>/dev/null
  wait_for 5 gone "$slirp" || die "o slirp4netns $slirp recebeu o SIGTERM e não saiu"
  wait_for 5 not_listening "$port" || die "a porta $port continua à escuta"
}
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

# Os slirp4netns deste root (o ambiente deles herda o DELONIX_ROOT).
our_slirps() {
  local p
  for p in $(pgrep -x slirp4netns); do
    tr '\0' '\n' <"/proc/$p/environ" 2>/dev/null | grep -qxF "DELONIX_ROOT=$DELONIX_ROOT" && echo "$p"
  done
}
# Prende a saída do PID 1 $1: um processo do `exec` com o pai, fora da pidns,
# parado. Devolve o pai parado em $held_parent.
hold_exit_of() {
  local p
  "$BIN" container exec "$name" sleep 902 >/dev/null 2>&1 &
  execpid=$!
  held_parent=""
  for _ in $(seq 1 50); do
    for p in $(pgrep -x sleep); do
      [[ "$({ tr '\0' ' ' <"/proc/$p/cmdline"; } 2>/dev/null)" == "sleep 902 " ]] || continue
      [[ "$(readlink "/proc/$p/ns/pid")" == "$(readlink "/proc/$1/ns/pid")" ]] || continue
      held_parent=$(awk '{print $4}' "/proc/$p/stat")
    done
    [[ -n "$held_parent" ]] && break
    sleep 0.2
  done
  [[ -n "$held_parent" ]] || die "o exec não deixou processo dentro da pidns do container"
  [[ "$(readlink "/proc/$held_parent/ns/pid")" != "$(readlink "/proc/$1/ns/pid")" ]] ||
    cannot "o pai do processo do exec está dentro da pidns — a saída não fica presa"
  kill -STOP "$held_parent"
}

case "$scenario" in
hang)
  # Um slirp4netns que nunca diz «pronto» nem responde — um falso, no PATH,
  # que só dorme com o que herdou. Ele próprio regista o pid.
  fake="$DELONIX_ROOT/fakeslirp-$name"; mkdir -p "$fake"
  printf '#!/bin/sh\necho $$ >> "%s/pids"\nexec sleep 300\n' "$fake" >"$fake/slirp4netns"
  chmod +x "$fake/slirp4netns"
  timeout 180 "$BIN" container run -d --name "$name" --restart always -p "$port:8000" "$IMG" sleep 1000 >/dev/null 2>&1 ||
    cannot "o container não arrancou neste host (run -d -p)"
  pid=$(field "$name" pid)
  timeout 120 "$BIN" container stop -t 1 "$name" >/dev/null 2>&1 || cannot "o stop falhou"
  wait_for 30 gone "$pid" || cannot "o container ainda está a sair ao fim de 30s"
  t0=$(date +%s)
  PATH="$fake:$PATH" timeout 90 "$BIN" container start "$name" >/dev/null 2>"$fake/err"
  rc=$?; took=$(( $(date +%s) - t0 ))
  left=""
  for p in $(cat "$fake/pids" 2>/dev/null); do [[ -e /proc/$p ]] && ! gone "$p" && left+="$p "; kill "$p" 2>/dev/null; done
  err=$(head -c 300 "$fake/err"); rm -rf "$fake"
  [[ $rc -ne 124 ]] || die "container start pendurou (90s) com o slirp que lançou mudo (falsos vivos: $left)"
  [[ $rc -ne 0 ]] || die "o start disse que correu bem com um slirp que nunca respondeu"
  [[ -n "$err" ]] || die "o start falhou (rc=$rc, ${took}s) sem dizer porquê"
  [[ -z "$left" ]] || die "o start falhou (rc=$rc, ${took}s) e deixou o slirp que lançou vivo: $left"
  ;;
zombies)
  # Um container que cai sempre, com `--restart always`: cada incarnação deixa
  # um slirp e um shim de logs, filhos do supervisor.
  timeout 180 "$BIN" container run -d --name "$name" --restart always -p "$port:8000" "$IMG" sh -c 'sleep 1; exit 1' >/dev/null 2>&1 ||
    cannot "o container não arrancou neste host (run -d -p)"
  pid=$(field "$name" pid); sup=$(awk '{print $4}' "/proc/$pid/stat")
  # Três reinícios: backoffs de 2, 4 e 8 s, mais o segundo de cada vida.
  for _ in $(seq 1 60); do
    n=$(python3 - "$DELONIX_ROOT/events.jsonl" "$name" <<'PY' 2>/dev/null
import json, sys
n = 0
for line in open(sys.argv[1]):
    e = json.loads(line)
    n += e.get("name") == sys.argv[2] and e.get("action") == "start"
print(n)
PY
)
    [[ ${n:-0} -ge 4 ]] && break
    sleep 1
  done
  [[ ${n:-0} -ge 4 ]] || cannot "o container não chegou a 3 reinícios em 60s (saídas lentas: disco saturado?)"
  [[ -e /proc/$sup ]] || die "o supervisor $sup morreu a meio dos reinícios"
  z=$(zombies_of "$sup")
  # Os da vida mais recente podem ainda estar por colher (no backoff): 2 no máximo.
  [[ $z -le 2 ]] || die "o supervisor $sup tem $z filhos zombie ao fim de 3 reinícios: $(for c in $(pgrep -P "$sup"); do echo -n "$(cat /proc/$c/comm)/$(awk '{print $3}' /proc/$c/stat) "; done)"
  ;;
stopgaveup)
  run --restart always
  listening "$port" || die "a porta $port não ficou publicada"
  hold_exit_of "$pid"
  out=$(timeout 120 "$BIN" container stop -t 1 "$name" 2>&1); rc=$?
  [[ $rc -ne 0 ]] || { kill -CONT "$held_parent"; cannot "o stop não desistiu: a saída não ficou presa"; }
  grep -q "DX-8101" <<<"$out" || { kill -CONT "$held_parent"; die "o stop falhou, mas não por DX-8101 (rc=$rc): $out"; }
  [[ -e /proc/$pid ]] || { kill -CONT "$held_parent"; die "o stop desistiu e o processo já tinha saído"; }
  # O PID 1 ainda existe — a netns também: o slirp não sai sozinho.
  wait_for 5 gone "$slirp" || { kill -CONT "$held_parent"; die "o stop desistiu (DX-8101) e deixou o slirp4netns $slirp vivo"; }
  not_listening "$port" || { kill -CONT "$held_parent"; die "o stop desistiu e a porta $port continua à escuta"; }
  kill -CONT "$held_parent"
  wait_for 30 gone "$pid" || die "o PID 1 $pid não saiu depois do SIGCONT"
  wait_for 15 no_shims "$id" || die "o shim de logs ficou vivo: $(shims_of "$id" | tr '\n' ' ')"
  sleep 4
  [[ "$(field "$name" status)" != Running ]] || die "o stop foi desfeito: o container voltou a correr"
  ;;
fds)
  run --restart always
  held=$(ls -l "/proc/$slirp/fd" 2>/dev/null | grep -o 'pipe:\[[0-9]*\]' | sort -u | tr '\n' ' ')
  [[ -z "$held" ]] || die "o slirp4netns $slirp segura pipes do arranque: $held"
  extra=$(ls "/proc/$pid/fd" 2>/dev/null | awk '$1 > 2' | tr '\n' ' ')
  [[ -z "$extra" ]] || die "o comando do container herdou descritores do arranque: $extra($(ls -l "/proc/$pid/fd" | awk '$9 > 2 {print $9 $10 $11}' | tr '\n' ' '))"
  ;;
exit)
  # `on-failure` e uma saída com 0: há supervisor, e ele não reinicia. Quem
  # tem de soltar o slirp é ele, no instante em que colhe o processo.
  run --restart on-failure
  listening "$port" || die "a porta $port não ficou publicada"
  exit_with_slirp_stopped
  released 5 "o container saiu sozinho"
  wait_for 15 no_shims "$id" || die "o shim de logs ficou vivo depois de o container sair: $(shims_of "$id" | tr '\n' ' ')"
  ;;
stats|kind)
  # Um `run -d` tem SEMPRE supervisor, e é ele que regista a saída e solta o
  # slirp (cenário `exit`). O caminho destes comandos é o de quem ficou sem
  # ele: morto o supervisor, o registo continua a dizer «a correr» com um pid
  # que já não existe, e é o comando que dá pela morte.
  if [[ $scenario == kind ]]; then run --label "io.x-k8s.kind.cluster=$PFX"; else run; fi
  sup=$(awk '{print $4}' "/proc/$pid/stat")
  kill -9 "$sup" 2>/dev/null
  exit_with_slirp_stopped
  sleep 1
  term_pending "$slirp" && cannot "o slirp já tinha um SIGTERM antes do comando — outro delonix varreu-o"
  if [[ $scenario == kind ]]; then
    timeout 60 "$BIN" cluster ls >/dev/null 2>&1
  else
    timeout 60 "$BIN" container stats "$name" >/dev/null 2>&1
  fi
  # Antes de qualquer `inspect`: esse também reconcilia, e soltaria o slirp
  # por conta própria.
  released 2 "o comando correu"
  [[ -z "$(field "$name" pid)" ]] || die "o comando não registou a saída do container"
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
  echo "cenário desconhecido: $scenario (fds|exit|start|listed|stats|kind|hang|zombies|stopgaveup|infra)"; exit 2 ;;
esac
cleanup
exit 0
