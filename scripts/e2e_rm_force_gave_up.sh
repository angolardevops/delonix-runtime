#!/usr/bin/env bash
# Um `rm -f` que desiste (DX-8101) de um container `--restart always` não o
# deixa ressuscitar quando o processo acaba por sair.
#
#   e2e_rm_force_gave_up.sh <binário> <imagem> [prefixo]
#
# Chamado pelo `scripts/e2e.sh`; corre sozinho contra qualquer binário, com
# `DELONIX_ROOT` e `DELONIX_NET_RUNTIME_DIR` já isolados pelo chamador. Sai 0
# (certo), 1 (defeito, com a medição na saída) ou 77 (o host não deixa medir).
#
# Visto duas vezes a 2026-10-02 com o disco saturado: `rm -f` → DX-8101, e
# minutos depois o container `Up` com RESTARTS 1. O `rm -f` não marcava nada no
# registo que mantinha, e o supervisor leu «morto e ninguém o parou» (#647).
#
# Como se faz um processo sobreviver ao SIGKILL sem disco saturado: o PID 1 de
# uma pidns só acaba de sair quando TODOS os processos dela foram colhidos
# (`zap_pid_ns_processes`). Um `container exec` deixa lá dentro um processo
# cujo pai está FORA da pidns; com esse pai parado (SIGSTOP), o processo morto
# fica por colher e o PID 1 fica em `zap_pid_ns_processes` — estado S, não
# zombie — até o pai voltar. O `rm -f` espera os seus 30 s e desiste; o
# SIGCONT deixa a saída acontecer, e o supervisor decide.
set -u
BIN="${1:?binário}"; IMG="${2:?imagem}"; PFX="${3:-rmf$$}"
name="$PFX-rmf"
ep=""

cleanup() {
  [[ -n "$ep" ]] && kill -CONT "$ep" 2>/dev/null
  [[ -n "${execpid:-}" ]] && kill "$execpid" 2>/dev/null
  timeout 120 "$BIN" container rm -f "$name" >/dev/null 2>&1
}
die() { echo "$*"; cleanup; exit 1; }
cannot() { echo "$*"; cleanup; exit 77; }
field() {
  "$BIN" container inspect "$1" 2>/dev/null |
    python3 -c 'import json,sys;v=json.load(sys.stdin)[0].get(sys.argv[1]);print("" if v is None else v)' "$2" 2>/dev/null
}
wait_for() { # wait_for <segundos> <comando…>
  local n=$(( $1 * 5 )); shift
  while (( n-- > 0 )); do "$@" && return 0; sleep 0.2; done
  "$@"
}
gone() { [[ ! -e /proc/$1 ]]; }

cleanup
timeout 180 "$BIN" container run -d --name "$name" --restart always "$IMG" sleep 1000 >/dev/null 2>&1 ||
  cannot "o container não arrancou neste host"
init=$(field "$name" pid)
[[ -n "$init" ]] || die "arrancou sem pid no registo"
sup=$(awk '{print $4}' "/proc/$init/stat")

# Um processo dentro da pidns com o pai fora dela.
"$BIN" container exec "$name" sleep 901 >/dev/null 2>&1 &
execpid=$!
inside=""
for _ in $(seq 1 50); do
  for p in $(pgrep -x sleep); do
    [[ "$({ tr '\0' ' ' <"/proc/$p/cmdline"; } 2>/dev/null)" == "sleep 901 " ]] || continue
    grep -qE "^NSpid:.*[[:space:]]$init[[:space:]]" "/proc/$p/status" 2>/dev/null && continue
    [[ "$(readlink "/proc/$p/ns/pid")" == "$(readlink "/proc/$init/ns/pid")" ]] && inside=$p
  done
  [[ -n "$inside" ]] && break
  sleep 0.2
done
[[ -n "$inside" ]] || die "o exec não deixou processo dentro da pidns do container"
ep=$(awk '{print $4}' "/proc/$inside/stat")
[[ "$(readlink "/proc/$ep/ns/pid")" != "$(readlink "/proc/$init/ns/pid")" ]] ||
  cannot "o pai do processo do exec está dentro da pidns — este binário não deixa a saída presa"
kill -STOP "$ep"

out=$(timeout 120 "$BIN" container rm -f "$name" 2>&1); rc=$?
if [[ $rc -eq 0 ]]; then
  cannot "o rm -f não desistiu (rc=0): a saída não ficou presa neste host"
fi
grep -q "DX-8101" <<<"$out" || die "o rm -f falhou, mas não por DX-8101 (rc=$rc): $out"
[[ -e /proc/$init ]] || die "o rm -f desistiu e o processo já tinha saído"

# A saída acontece agora; o supervisor colhe-a e decide.
kill -CONT "$ep"; ep=""
wait_for 30 gone "$init" || die "o PID 1 $init não saiu depois do SIGCONT"
wait_for 30 gone "$sup" || {
  # Um supervisor que ficou é um que reiniciou: o novo PID 1 é filho dele.
  sleep 1
  st=$(field "$name" status)
  die "o supervisor $sup ficou vivo depois da saída (status=$st, pid=$(field "$name" pid)): reiniciou o container que o rm -f mandou tirar"
}
# A política reinicia depois de um backoff de 2 s; 6 s cobrem-no.
sleep 6
st=$(field "$name" status); pid=$(field "$name" pid)
[[ "$st" != Running ]] || die "ressuscitou: status=$st pid=$pid depois de um rm -f que desistiu"
[[ -z "$pid" ]] || die "o registo ficou com pid=$pid (status=$st)"
timeout 120 "$BIN" container rm "$name" >/dev/null 2>&1 || die "o rm a seguir, que o erro pedia, falhou"
"$BIN" container inspect "$name" >/dev/null 2>&1 && die "o rm não tirou o registo"
exit 0
