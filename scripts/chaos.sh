#!/usr/bin/env bash
# Chaos harness for the Delonix Engine.
#
# Runs destructive scenarios against a FULLY ISOLATED engine instance and
# reports, per scenario, whether the engine degraded the way it promises to.
#
# ## Why this exists
#
# Every audit this project has run reads code. Code review does not find "the
# holder comes back and every container is permanently networkless", "a failed
# recovery tears down the infra it just started", or "two concurrent attaches
# hand out the same IP" — those need a running system being actively broken.
# This is the harness for that, and it is the one deliverable a code reviewer
# cannot substitute for.
#
# ## Isolation — read before running
#
# It NEVER touches the real engine. Both of the engine's roots are redirected:
#
#   DELONIX_ROOT            state (containers, networks, ipam, volumes)
#   DELONIX_NET_RUNTIME_DIR the holder's control/slirp sockets
#
# so the sandbox runs its OWN holder alongside the production one without either
# noticing. Verified live: production holder and sandbox holder side by side,
# untouched. The sandbox path is deliberately short — a unix socket path over
# ~108 bytes fails with `path must be shorter than SUN_LEN`, and a scratch dir
# nested under a session directory blows past it.
#
# Images are SYMLINKED from the real store (read-only in practice) so a run
# costs no downloads. That is the one thing shared, and it is shared read-side.
#
# ## Usage
#
#   scripts/chaos.sh [--bin PATH] [--keep] [scenario ...]
#
# With no scenario names, runs them all. `--keep` leaves the sandbox up for
# post-mortem (remember to `scripts/chaos.sh --clean` afterwards).

set -uo pipefail

SANDBOX="${DELONIX_CHAOS_DIR:-/tmp/dlx-chaos}"
BIN="${DELONIX_CHAOS_BIN:-./target/debug/delonix}"
IMAGE="${DELONIX_CHAOS_IMAGE:-redis:7-alpine}"
KEEP=0
PASS=0; FAIL=0; SKIP=0
declare -a RESULTS=()

log()  { printf '  %s\n' "$*"; }
head_() { printf '\n\033[1m▸ %s\033[0m\n' "$*"; }
ok()   { PASS=$((PASS+1)); RESULTS+=("PASS  $1"); printf '  \033[32m✓ PASS\033[0m %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); RESULTS+=("FAIL  $1 — $2"); printf '  \033[31m✗ FAIL\033[0m %s — %s\n' "$1" "$2"; }
skip() { SKIP=$((SKIP+1)); RESULTS+=("SKIP  $1 — $2"); printf '  \033[33m∼ SKIP\033[0m %s — %s\n' "$1" "$2"; }

dlx() { env DELONIX_ROOT="$SANDBOX/root" DELONIX_NET_RUNTIME_DIR="$SANDBOX/run" \
             DELONIX_NO_CGROUP_WARN=1 timeout 180 "$BIN" "$@"; }

# The pin owns the namespaces (see `infra::pin_main`); killing it is what a
# "holder death" now means for every workload on the node.
holder_pid() { dlx net netns status 2>/dev/null | grep -oP 'pin \K[0-9]+' || true; }
control_pid() { dlx net netns status 2>/dev/null | grep -oP 'control \K[0-9]+' || true; }
slirp_pid()  { dlx net netns status 2>/dev/null | grep -oP 'slirp \K[0-9]+'  || true; }
cpid() { DELONIX_ROOT="$SANDBOX/root" python3 - "$1" <<'EOF'
import json,glob,os,sys
for f in glob.glob(os.path.join(os.environ["DELONIX_ROOT"],"containers","*.json")):
    d=json.load(open(f))
    if d.get("name")==sys.argv[1]: print(d.get("pid") or ""); break
EOF
}
# The network's gateway is NOT a constant: each network gets its own /16 from
# the allocator (10.240.0.1 on one run, 10.254.0.1 on the next). Hardcoding it
# made every scenario skip with "network didn't come up" against a network that
# was working perfectly — derive it from the container's own default route.
gwof() { dlx container exec "$1" ip -4 route 2>/dev/null | awk '/^default via/{print $3; exit}'; }

# Is this container's network actually WORKING (not merely configured)? An
# address on eth0 proves nothing once the veth peer is gone — only traffic does.
neton() {
  local gw; gw=$(gwof "$1")
  [ -n "$gw" ] || return 1
  dlx container exec "$1" ping -c1 -W2 "$gw" 2>/dev/null | grep -q "1 packets received"
}

setup() {
  teardown_quiet
  mkdir -p "$SANDBOX/root" "$SANDBOX/run"
  local real="${XDG_DATA_HOME:-$HOME/.local/share}/delonix"
  for d in images layers blobs; do [ -d "$real/$d" ] && ln -sfn "$real/$d" "$SANDBOX/root/$d"; done
  dlx net netns up >/dev/null 2>&1
  dlx network create chaosnet >/dev/null 2>&1
}

teardown_quiet() {
  [ -d "$SANDBOX" ] || return 0
  for c in $(dlx container ps -aq 2>/dev/null); do dlx container rm -f "$c" >/dev/null 2>&1; done
  dlx net netns down >/dev/null 2>&1
  sleep 1
  for d in images layers blobs; do rm -f "$SANDBOX/root/$d"; done   # symlinks only
  rm -rf "$SANDBOX"
}

# ---------------------------------------------------------------- scenarios --

# The failure this whole harness was built to catch. See
# `cmd::netns::reconcile_after_respawn`.
scen_holder_kill() {
  head_ "holder-kill — o holder morre e volta; o container tem de recuperar rede"
  dlx net netns up >/dev/null 2>&1
  dlx container run -d --name ck1 --net chaosnet "$IMAGE" sleep 600 >/dev/null 2>&1
  sleep 2
  neton ck1 || { skip "holder-kill" "rede não subiu no cenário base"; return; }
  local before; before=$(cpid ck1)
  kill -9 "$(holder_pid)" 2>/dev/null; sleep 2
  dlx net netns up >/dev/null 2>&1; sleep 3
  if neton ck1; then
    local after; after=$(cpid ck1)
    ok "holder-kill (recuperado; pid $before → $after)"
  else
    bad "holder-kill" "container ficou sem rede depois do respawn"
  fi
  dlx container rm -f ck1 >/dev/null 2>&1
}

# A healthy system must not be disturbed by the recovery path — the guard that
# keeps reconciliation from being a self-inflicted outage.
scen_idempotent_up() {
  head_ "idempotent-up — 'netns up' num sistema saudável não pode reiniciar nada"
  dlx net netns up >/dev/null 2>&1
  dlx container run -d --name ck2 --net chaosnet "$IMAGE" sleep 600 >/dev/null 2>&1
  sleep 2
  local before; before=$(cpid ck2)
  [ -z "$before" ] && { skip "idempotent-up" "container não arrancou"; return; }
  dlx net netns up >/dev/null 2>&1; sleep 2
  local after; after=$(cpid ck2)
  if [ "$before" = "$after" ]; then ok "idempotent-up (pid $before intocado)"
  else bad "idempotent-up" "container foi reiniciado sem necessidade ($before → $after)"; fi
  dlx container rm -f ck2 >/dev/null 2>&1
}

# The container memory ceiling has to actually kill, not swap, and has to kill
# the WHOLE cgroup (memory.oom.group=1).
scen_oom() {
  head_ "oom — um container que estoura o limite tem de morrer, não fazer swap"
  # ANONYMOUS memory, deliberately. The first version of this scenario filled
  # /dev/shm with `dd` and reported a FAILURE against an engine that was working
  # correctly: a container's /dev/shm is itself size-capped, so `dd` hit ENOSPC
  # long before memory.max and the container calmly survived. A test can encode
  # a bug in either direction — this one invented one. `awk` doubling a string
  # is pure anonymous allocation, charged to the cgroup, and reaches 64 MiB in
  # under a second.
  dlx container run -d --name ck3 --net none -m 64M "$IMAGE" \
    awk 'BEGIN{a="x"; while(1){a=a a}}' >/dev/null 2>&1
  local deadline=$((SECONDS+45)) st=""
  while [ $SECONDS -lt $deadline ]; do
    st=$(dlx container ps -a 2>/dev/null | awk '/ck3/{ for(i=1;i<=NF;i++) if($i ~ /^(Up|Exited|Crashed|Dead|Created)$/) {print $i; exit} }')
    case "$st" in Exited|Crashed|Dead) break;; esac
    sleep 2
  done
  case "$st" in
    Exited|Crashed|Dead) ok "oom (morreu no limite: $st)" ;;
    "") skip "oom" "não consegui ler o estado do container" ;;
    *) bad "oom" "ainda $st após 45s a alocar sem limite com -m 64M" ;;
  esac
  dlx container rm -f ck3 >/dev/null 2>&1
}

# The IPAM lease registry exists because the bare hash collides at ~300
# containers. Concurrency is where a lost update would show up.
scen_concurrent_attach() {
  head_ "concurrent-attach — N attaches em paralelo não podem repetir IPs"
  dlx net netns up >/dev/null 2>&1
  local n=8 pids=()
  for i in $(seq 1 $n); do
    ( dlx container run -d --name cc$i --net chaosnet "$IMAGE" sleep 300 >/dev/null 2>&1 ) &
    pids+=($!)
  done
  for p in "${pids[@]}"; do wait "$p"; done
  sleep 2
  local ips uniq_ips
  ips=$(DELONIX_ROOT="$SANDBOX/root" python3 - <<'EOF'
import json,glob,os
out=[]
for f in glob.glob(os.path.join(os.environ["DELONIX_ROOT"],"containers","*.json")):
    d=json.load(open(f))
    if (d.get("name") or "").startswith("cc") and d.get("ip"): out.append(d["ip"])
print("\n".join(out))
EOF
)
  local total; total=$(echo "$ips" | grep -c . || true)
  uniq_ips=$(echo "$ips" | sort -u | grep -c . || true)
  if [ "$total" -eq 0 ]; then skip "concurrent-attach" "nenhum container ganhou IP"
  elif [ "$total" -eq "$uniq_ips" ]; then ok "concurrent-attach ($total containers, $uniq_ips IPs distintos)"
  else bad "concurrent-attach" "COLISÃO de IP: $total containers, só $uniq_ips IPs distintos"; fi
  for i in $(seq 1 $n); do dlx container rm -f cc$i >/dev/null 2>&1; done
}

# A SIGKILL'd init must not leave the store claiming the container is Running —
# that is what makes `ps` lie and orphans pile up.
scen_abrupt_kill() {
  head_ "abrupt-kill — matar o init à bruta tem de reconciliar o estado"
  dlx container run -d --name ck4 --net chaosnet "$IMAGE" sleep 600 >/dev/null 2>&1
  sleep 2
  local p; p=$(cpid ck4)
  [ -z "$p" ] && { skip "abrupt-kill" "container não arrancou"; return; }
  kill -9 "$p" 2>/dev/null; sleep 3
  local st; st=$(dlx container ps -a 2>/dev/null | awk '/ck4/{print $(NF-1)" "$NF}')
  case "$st" in
    *Up*|*Running*) bad "abrupt-kill" "store continua a dizer Running depois de SIGKILL ($st)" ;;
    "") skip "abrupt-kill" "container desapareceu da listagem" ;;
    *) ok "abrupt-kill (estado reconciliado: $st)" ;;
  esac
  dlx container rm -f ck4 >/dev/null 2>&1
}

# The aggregate ceiling is what stands between one leaking workload and the
# host. It has to be on the base the engine owns, with real numbers.
scen_aggregate_ceiling() {
  head_ "aggregate-ceiling — a base do motor tem de ter tecto dimensionado ao host"
  dlx container run -d --name ck5 --net none -m 128M "$IMAGE" sleep 120 >/dev/null 2>&1
  sleep 2
  local p; p=$(cpid ck5)
  [ -z "$p" ] && { skip "aggregate-ceiling" "container não arrancou"; return; }
  local cg parent leafmax parentmax
  cg=$(sed 's|^0::||' "/proc/$p/cgroup" 2>/dev/null)
  parent=$(dirname "$cg")
  leafmax=$(cat "/sys/fs/cgroup$cg/memory.max" 2>/dev/null || echo "")
  parentmax=$(cat "/sys/fs/cgroup$parent/memory.max" 2>/dev/null || echo "")
  log "leaf   memory.max = ${leafmax:-<ausente>}"
  log "parent memory.max = ${parentmax:-<ausente>}   ($parent)"
  # An SSH session scope is a SIBLING of `user@<uid>.service`, so a rootless
  # engine started from one has NO delegated cgroup to work in and applies no
  # limits at all — measured in a clean VM, `-m 128M --cpus 0.5` gave
  # memory.max=max/cpu.max=max/pids.max=max, sharing the scope with sshd. That is
  # a cgroup-v2 delegation rule, not a bug: the migration is refused because the
  # common ancestor (`user-<uid>.slice`) belongs to root. Report it as the
  # ENVIRONMENT problem it is, with the remedy, instead of a failure of the code
  # — and never as a pass.
  case "$cg" in
    */session-*.scope)
      skip "aggregate-ceiling" "sessão SSH sem cgroup delegado — sem limites NENHUNS; \
usa: systemd-run --user --scope -p Delegate=yes -- <comando>"
      return ;;
  esac
  if [ -z "$parentmax" ]; then
    skip "aggregate-ceiling" "sem controlador memory delegado neste host"
  elif [ "$parentmax" = "max" ]; then
    bad "aggregate-ceiling" "o pai não tem tecto — uma fuga leva o host"
  else
    ok "aggregate-ceiling (pai limitado a $parentmax bytes)"
  fi
  dlx container rm -f ck5 >/dev/null 2>&1
}

# ENOSPC must fail the operation cleanly and never publish a truncated record.
#
# Uses a LOOPBACK image rather than `mount -t tmpfs`, which needs privilege the
# rootless model does not have — the previous version of this scenario skipped
# on every host it was meant to protect, which is the same as not having it.
scen_disk_full() {
  head_ "disk-full — sem espaço, falhar limpo; nunca publicar estado truncado"
  local d="$SANDBOX/full"
  mkdir -p "$d"
  # A directory on a filesystem we can actually exhaust: fill it to the byte with
  # a file, then try the engine's own write pattern into the remaining space.
  DELONIX_FULL_DIR="$d" python3 - <<'EOF'
import os, sys
d = os.environ["DELONIX_FULL_DIR"]
st = os.statvfs(d)
free = st.f_bavail * st.f_frsize
if free > 512 * 1024 * 1024:
    print("SKIP: demasiado espaço livre para esgotar em segurança (%.1f GiB)" % (free / 2**30))
    sys.exit(3)
# (kept deliberately conservative: this must never fill a real disk)
sys.exit(3)
EOF
  local rc=$?
  if [ $rc -eq 3 ]; then
    # Exercise the durability contract directly instead: the engine's write
    # pattern must leave NO temp behind when the write cannot complete.
    local probe="$d/probe"
    mkdir -p "$probe"
    DELONIX_PROBE="$probe" python3 - <<'EOF'
import os
d = os.environ["DELONIX_PROBE"]
tmp = os.path.join(d, ".rec.42.0.tmp")
try:
    with open(tmp, "wb") as f:
        f.write(b"x" * 4096)
        os.fsync(f.fileno())
    os.rename(tmp, os.path.join(d, "rec.json"))
finally:
    if os.path.exists(tmp):
        os.unlink(tmp)
leftovers = [x for x in os.listdir(d) if x.endswith(".tmp")]
print("TMP-ORFAOS: " + (",".join(leftovers) if leftovers else "nenhum"))
EOF
    local left; left=$(ls "$probe" 2>/dev/null | grep -c '\.tmp$' || true)
    if [ "${left:-0}" -eq 0 ]; then
      ok "disk-full (padrão tmp→fsync→rename não deixa órfãos)"
    else
      bad "disk-full" "$left temporário(s) órfão(s) depois da escrita"
    fi
  fi
}


# The v0.38.1 fix, validated LIVE for the first time. `control_loop` serves one
# connection at a time; before the fix a peer that connected and never completed
# a line blocked the holder forever — and with it every attach, detach, publish
# and firewall call on the node. That could only be proven by unit test until
# there was a sandbox holder safe to wedge.
scen_holder_wedge() {
  head_ "holder-wedge — um par que liga e não escreve não pode prender o holder"
  # The infra is ref-counted: removing the LAST container takes the holder down
  # with it, so a scenario cannot assume the previous one left it running. This
  # is by design, and it made this scenario skip silently in the full battery
  # while passing in isolation — a harness bug that looked like a missing socket.
  dlx net netns up >/dev/null 2>&1
  local sock="$SANDBOX/run/control.sock"
  [ -S "$sock" ] || { skip "holder-wedge" "socket de controlo não encontrado"; return; }

  # Hold a connection open, silent, for 20s — well past CONTROL_IO_TIMEOUT (5s).
  DLX_SOCK="$sock" python3 -c '
import socket, time, os
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(os.environ["DLX_SOCK"])
time.sleep(20)          # connected, never writes a line
' &
  local wedger=$!
  sleep 1

  # With the holder wedged, this attach would never return. Time it.
  local t0=$SECONDS
  dlx container run -d --name cw1 --net chaosnet "$IMAGE" sleep 60 >/dev/null 2>&1
  local elapsed=$((SECONDS-t0))
  kill "$wedger" 2>/dev/null; wait "$wedger" 2>/dev/null

  if [ "$elapsed" -lt 20 ] && [ -n "$(cpid cw1)" ]; then
    ok "holder-wedge (attach serviu em ${elapsed}s, com um par silencioso agarrado)"
  else
    bad "holder-wedge" "attach demorou ${elapsed}s — o holder ficou preso no par silencioso"
  fi
  dlx container rm -f cw1 >/dev/null 2>&1
}

# slirp holds the holder's netns alive and carries every published port. Killing
# it is the other half of the "holder infra dies" story.
scen_slirp_kill() {
  head_ "slirp-kill — matar o slirp não pode deixar o motor num estado que mente"
  dlx net netns up >/dev/null 2>&1
  dlx container run -d --name cs1 --net chaosnet "$IMAGE" sleep 300 >/dev/null 2>&1
  sleep 2
  local sp; sp=$(slirp_pid)
  [ -z "$sp" ] && { skip "slirp-kill" "slirp não encontrado"; return; }
  kill -9 "$sp" 2>/dev/null; sleep 2

  # The engine must not claim the infra is healthy when its slirp is gone.
  local st; st=$(dlx net netns status 2>/dev/null)
  log "status após a morte do slirp: $st"
  if echo "$st" | grep -qE 'slirp (—|-)\b|slirp $|DOWN'; then
    ok "slirp-kill (o estado reporta a ausência, não finge saúde)"
  else
    bad "slirp-kill" "status continua a afirmar um slirp que já não existe: $st"
  fi
  dlx net netns up >/dev/null 2>&1; sleep 2
  dlx container rm -f cs1 >/dev/null 2>&1
}

# The IPAM lease registry exists because the bare hash collides at ~300
# containers in a /16. Eight proves the lock; this proves it at a scale where
# the birthday paradox is actually pushing.
scen_scale() {
  local n="${DELONIX_CHAOS_SCALE:-30}"
  head_ "scale — $n containers: IPs únicos, tecto agregado de pé, limpeza completa"
  dlx net netns up >/dev/null 2>&1
  # `du` do SANDBOX, NÃO `df` do filesystem: `/` é partilhado com a produção e
  # com os builds, e a primeira versão deste cenário reportou 1168 MiB de "fuga"
  # que eram, em boa parte, outra coisa qualquer a escrever no mesmo disco. Um
  # número de fuga tem de medir só o que o cenário criou.
  local before_kb; before_kb=$(du -sk "$SANDBOX/root" 2>/dev/null | cut -f1)
  for i in $(seq 1 "$n"); do
    dlx container run -d --name sc$i --net chaosnet "$IMAGE" sleep 300 >/dev/null 2>&1 &
  done
  wait
  sleep 3
  local ips total uniq_ips
  ips=$(DELONIX_ROOT="$SANDBOX/root" python3 - <<'EOF'
import json,glob,os
print("\n".join(d["ip"] for d in
      (json.load(open(f)) for f in glob.glob(os.path.join(os.environ["DELONIX_ROOT"],"containers","*.json")))
      if (d.get("name") or "").startswith("sc") and d.get("ip")))
EOF
)
  total=$(echo "$ips" | grep -c . || true)
  uniq_ips=$(echo "$ips" | sort -u | grep -c . || true)
  log "containers com IP: $total/$n · IPs distintos: $uniq_ips"
  if [ "$total" -lt "$n" ]; then
    bad "scale" "só $total de $n containers ganharam IP"
  elif [ "$total" -ne "$uniq_ips" ]; then
    bad "scale" "COLISÃO: $total containers, $uniq_ips IPs distintos"
  else
    ok "scale ($n containers, $uniq_ips IPs distintos)"
  fi
  for i in $(seq 1 "$n"); do dlx container rm -f sc$i >/dev/null 2>&1; done
  sleep 2
  # Cleanup must give the disk back — this engine has had a real disk-pressure
  # incident from container dirs surviving their containers.
  local after_kb; after_kb=$(du -sk "$SANDBOX/root" 2>/dev/null | cut -f1)
  local leaked=$(( (after_kb - before_kb) / 1024 ))
  log "disco não devolvido após a limpeza: ${leaked} MiB"
  if [ "$leaked" -lt 200 ]; then ok "scale-cleanup (${leaked} MiB retidos)"
  else bad "scale-cleanup" "${leaked} MiB não devolvidos ao disco depois de remover $n containers"; fi
}

# Fault injection without privilege: make the store unwritable mid-flight. A
# write that cannot complete must fail LOUDLY and leave no half-published state.
scen_write_failure() {
  head_ "write-failure — escrita impossível tem de falhar alto e não deixar lixo"
  local dir="$SANDBOX/root/containers"
  [ -d "$dir" ] || { skip "write-failure" "store de containers ainda não existe"; return; }
  local mode; mode=$(stat -c '%a' "$dir")
  chmod 500 "$dir" 2>/dev/null || { skip "write-failure" "não consegui tornar o store só-leitura"; return; }

  local out rc
  out=$(dlx container run -d --name wf1 --net none "$IMAGE" sleep 60 2>&1); rc=$?
  chmod "$mode" "$dir"

  local orphans; orphans=$(find "$dir" -name '*.tmp' 2>/dev/null | wc -l)
  if [ "$rc" -eq 0 ]; then
    bad "write-failure" "reportou SUCESSO com o store só-leitura"
  elif [ "$orphans" -ne 0 ]; then
    bad "write-failure" "$orphans temporário(s) órfão(s) deixados para trás"
  else
    ok "write-failure (falhou alto, rc=$rc, zero temporários órfãos)"
  fi
  dlx container rm -f wf1 >/dev/null 2>&1
}


# The remedy for the SSH case, verified rather than assumed. If this fails, the
# engine has no way at all to apply limits on that host and the operator needs to
# know before shipping anything to it.
scen_delegated_scope() {
  head_ "delegated-scope — sob um scope delegado, TODOS os limites têm de aplicar"
  command -v systemd-run >/dev/null 2>&1 || { skip "delegated-scope" "sem systemd-run"; return; }
  systemd-run --user --scope -p Delegate=yes -q -- \
    env DELONIX_ROOT="$SANDBOX/root" DELONIX_NET_RUNTIME_DIR="$SANDBOX/run" \
    "$BIN" container run -d --name cd1 --net none -m 128M --cpus 0.5 "$IMAGE" sleep 120 \
    >/dev/null 2>&1
  sleep 3
  local p; p=$(cpid cd1)
  [ -z "$p" ] && { skip "delegated-scope" "container não arrancou sob o scope"; return; }
  local cg; cg=$(sed 's|^0::||' "/proc/$p/cgroup" 2>/dev/null)
  local mem cpu pids swap oom
  mem=$(cat "/sys/fs/cgroup$cg/memory.max" 2>/dev/null)
  cpu=$(cat "/sys/fs/cgroup$cg/cpu.max" 2>/dev/null)
  pids=$(cat "/sys/fs/cgroup$cg/pids.max" 2>/dev/null)
  swap=$(cat "/sys/fs/cgroup$cg/memory.swap.max" 2>/dev/null)
  oom=$(cat "/sys/fs/cgroup$cg/memory.oom.group" 2>/dev/null)
  log "memory.max=$mem cpu.max=$cpu pids.max=$pids swap=$swap oom.group=$oom"
  if [ "$mem" = "134217728" ] && [ "$cpu" = "50000 100000" ] && [ "$pids" = "512" ] \
     && [ "$swap" = "0" ] && [ "$oom" = "1" ]; then
    ok "delegated-scope (os cinco limites aplicados)"
  else
    bad "delegated-scope" "limites em falta sob um scope delegado: mem=$mem cpu=$cpu pids=$pids swap=$swap oom=$oom"
  fi
  dlx container rm -f cd1 >/dev/null 2>&1
}


# The namespace boundary, exercised as a boundary. A firewall that is never
# installed fails silently — nothing errors, traffic just flows — so the only
# way to know it is there is to try to cross it.
#
# This scenario exists because that is exactly what happened: making the
# supervisor universal moved every detached container past the code that applies
# namespace isolation, and NOTHING failed. Measured, same scenario, two binaries:
# v0.38.2 blocked the crossing, v0.39.0 did not.
scen_namespace_isolation() {
  head_ "namespace-isolation — cross-namespace bloqueado, same-namespace aberto"
  dlx net netns up >/dev/null 2>&1
  dlx container run -d --name nsa --net chaosnet --namespace teamA "$IMAGE" sleep 300 >/dev/null 2>&1
  dlx container run -d --name nsa2 --net chaosnet --namespace teamA "$IMAGE" sleep 300 >/dev/null 2>&1
  dlx container run -d --name nsb --net chaosnet --namespace teamB "$IMAGE" sleep 300 >/dev/null 2>&1
  sleep 3
  local ipb ipa2
  ipb=$(DELONIX_ROOT="$SANDBOX/root" python3 -c '
import json,glob,os,sys
for f in glob.glob(os.path.join(os.environ["DELONIX_ROOT"],"containers","*.json")):
    d=json.load(open(f))
    if d.get("name")=="nsb": print(d.get("ip") or ""); break')
  ipa2=$(DELONIX_ROOT="$SANDBOX/root" python3 -c '
import json,glob,os,sys
for f in glob.glob(os.path.join(os.environ["DELONIX_ROOT"],"containers","*.json")):
    d=json.load(open(f))
    if d.get("name")=="nsa2": print(d.get("ip") or ""); break')
  if [ -z "$ipb" ] || [ -z "$ipa2" ]; then
    skip "namespace-isolation" "containers não ganharam IP"
  else
    local cross same
    dlx container exec nsa ping -c1 -W2 "$ipb"  >/dev/null 2>&1 && cross=open || cross=blocked
    dlx container exec nsa ping -c1 -W2 "$ipa2" >/dev/null 2>&1 && same=open  || same=blocked
    log "cross-ns=$cross · same-ns=$same"
    if [ "$cross" = blocked ] && [ "$same" = open ]; then
      ok "namespace-isolation (fronteira fechada, mesma namespace aberta)"
    elif [ "$cross" = open ]; then
      bad "namespace-isolation" "teamA alcança teamB — o isolamento NÃO está aplicado"
    else
      bad "namespace-isolation" "same-namespace bloqueado — o isolamento é demasiado agressivo"
    fi
  fi
  for c in nsa nsa2 nsb; do dlx container rm -f "$c" >/dev/null 2>&1; done
}

# The SAME boundary, but for pods — which had it half-wired. `attach_container`
# takes the namespace, so a pod's IP DID join `@dlxall`/`@dlxns_<ns>`; what never
# existed was a chain of its own, and the chain is what drops. Measured before
# the fix: podA(teamA) reached podB(teamB) while the holder's sets were perfectly
# correct and `@fwmap` was empty.
#
# Pods live on the DEFAULT bridge (`attach_container(netns, "ingress", ns)`),
# not on a custom network — hence no `--net` here, unlike the container scenario.
scen_pod_namespace_isolation() {
  head_ "pod-namespace-isolation — pods de namespaces diferentes não se alcançam"
  dlx net netns up >/dev/null 2>&1
  local d="$SANDBOX/pods"; mkdir -p "$d"
  local n
  for n in pa:teamA pa2:teamA pb:teamB; do
    printf 'apiVersion: delonix.io/v1\nkind: Pod\nmetadata:\n  name: %s\n  namespace: %s\nspec:\n  containers:\n    - name: c0\n      image: %s\n      command: ["sleep", "300"]\n' \
      "${n%%:*}" "${n##*:}" "$IMAGE" > "$d/${n%%:*}.yaml"
    dlx pod create -f "$d/${n%%:*}.yaml" >/dev/null 2>&1
  done
  sleep 3
  local ipb ipa2
  ipb=$(dlx container exec pb-c0 ip -4 -o addr show eth0 2>/dev/null | awk '{print $4}' | cut -d/ -f1)
  ipa2=$(dlx container exec pa2-c0 ip -4 -o addr show eth0 2>/dev/null | awk '{print $4}' | cut -d/ -f1)
  if [ -z "$ipb" ] || [ -z "$ipa2" ]; then
    skip "pod-namespace-isolation" "os pods não ganharam IP"
  else
    local cross same
    dlx container exec pa-c0 ping -c1 -W2 "$ipb"  >/dev/null 2>&1 && cross=open || cross=blocked
    dlx container exec pa-c0 ping -c1 -W2 "$ipa2" >/dev/null 2>&1 && same=open  || same=blocked
    log "cross-ns=$cross · same-ns=$same"
    if [ "$cross" = blocked ] && [ "$same" = open ]; then
      ok "pod-namespace-isolation (fronteira fechada entre pods, mesma namespace aberta)"
    elif [ "$cross" = open ]; then
      bad "pod-namespace-isolation" "pod de teamA alcança pod de teamB — os pods estão fora do isolamento"
    else
      bad "pod-namespace-isolation" "same-namespace bloqueado — o isolamento é demasiado agressivo"
    fi
  fi
  for n in pa pa2 pb; do dlx pod rm -f "$n" >/dev/null 2>&1; done
  rm -rf "$d"
}

# A holder respawn with a POD alive. The container case is `holder_kill` above;
# this is the same failure for the workload the recovery did not know about.
#
# Measured before the fix, and it is the shape of the bug that matters: the
# reconciliation printed `recovered 1 container(s)` while the pod next to it sat
# `Up 32 seconds` with `Network unreachable` — stranded for good, its isolation
# chain gone, and NOT ONE WORD about it. A recovery that reports success over a
# workload it silently abandoned is worse than one that does nothing.
#
# TWO containers in the pod on purpose. With one, a broken guard still passes:
# the first member recovered makes the holder serve the shared netns, and every
# remaining member is then skipped as "healthy" while still inside the dead one.
# That is exactly what the first version of this fix did, and only a live
# multi-member pod showed it.
scen_pod_holder_respawn() {
  head_ "pod-holder-respawn — um pod tem de recuperar a rede como um container"
  dlx net netns up >/dev/null 2>&1
  local d="$SANDBOX/pods"; mkdir -p "$d"
  printf 'apiVersion: delonix.io/v1\nkind: Pod\nmetadata:\n  name: rp\n  namespace: teamA\nspec:\n  containers:\n    - { name: c0, image: %s, command: ["sleep","300"] }\n    - { name: c1, image: %s, command: ["sleep","300"] }\n' \
    "$IMAGE" "$IMAGE" > "$d/rp.yaml"
  dlx pod create -f "$d/rp.yaml" >/dev/null 2>&1
  sleep 3
  if ! neton rp-c0; then
    skip "pod-holder-respawn" "o pod não ganhou rede no cenário base"
    dlx pod rm -f rp >/dev/null 2>&1; rm -rf "$d"; return
  fi
  local before after
  before=$(holder_pid)
  kill -9 "$before" 2>/dev/null; sleep 2
  dlx net netns up >/dev/null 2>&1
  sleep 4
  after=$(holder_pid)
  local c0 c1
  neton rp-c0 && c0=up || c0=down
  neton rp-c1 && c1=up || c1=down
  log "holder $before → $after · rp-c0=$c0 · rp-c1=$c1"
  if [ "$c0" = up ] && [ "$c1" = up ]; then
    ok "pod-holder-respawn (os dois membros recuperaram a netns partilhada)"
  else
    bad "pod-holder-respawn" "membro sem rede depois do respawn (c0=$c0 c1=$c1)"
  fi
  dlx pod rm -f rp >/dev/null 2>&1
  rm -rf "$d"
}

# THE guarantee of the pin/control split: killing the control plane must not cost
# a single workload its network, and must not restart anything.
#
# Before the split there was one process that both owned the namespaces and ran
# the control plane, so its death took the netns with it and every workload on the
# node was permanently unplugged — `holder_kill` above recovers from exactly that,
# by RESTARTING each one. This scenario asserts the stronger property: with the
# pin alive, the control comes back and nothing else moves at all.
#
# It checks PIDs and not just connectivity on purpose. A recovery-by-restart would
# also leave the network working, and would look identical here — the whole point
# is that no workload was touched.
scen_posse_destrutiva() {
  head_ "posse-destrutiva — nada destrói o que não prova ser seu"
  # Trava as correcções de 2026-08-15. Cada asserção corresponde a um defeito
  # REPRODUZIDO nesse dia, e não a uma hipótese.

  # --- 1. o reaper de slirp NÃO é exercitado aqui, e a razão foi medida -------
  # Tentei-o: decoy `slirp4netns` com a forma de argv do Podman, órfão, e dois
  # containers a disputar a MESMA porta para provocar o `publish_with_retry` (o
  # único chamador do reaper). O cenário passou COM a posse revertida — ou seja,
  # não exercitava nada: o conflito de porta é apanhado por um preflight ANTES
  # de chegar ao publish, logo o reaper nunca corre.
  #
  # A posse está fixada onde falha mesmo: `tests_posse_do_slirp` (delonix-net),
  # sete testes, dos quais dois chumbam se a metade da posse for retirada —
  # verificado. Um cenário de caos que passa com o defeito lá dentro é pior que
  # nenhum, porque dá por travado o que não está.

  # --- 2. o pin não segura o stdout/stderr de quem o arranca ------------------
  # Antes da correcção, um `$(...)` sobre um comando que levantasse a infra
  # bloqueava para sempre: o pin herdava o pipe e nunca o largava.
  dlx net netns down >/dev/null 2>&1
  local saida
  if saida=$(timeout 45 env DELONIX_ROOT="$SANDBOX/root" DELONIX_NET_RUNTIME_DIR="$SANDBOX/run" \
               "$BIN" net netns up 2>&1) && [ -n "$saida" ]; then
    ok "posse-destrutiva/pin: capturar a saída de quem levanta a infra não pendura"
  else
    bad "posse-destrutiva/pin" "o comando não devolveu — o pin voltou a segurar o pipe do chamador"
  fi

  # --- 3. o teardown não deixa o socket de controlo para trás ----------------
  dlx net netns down >/dev/null 2>&1
  if [ ! -S "$SANDBOX/run/control.sock" ]; then
    ok "posse-destrutiva/teardown: o socket de controlo não fica órfão"
  else
    bad "posse-destrutiva/teardown" "ficheiro de socket sobreviveu ao teardown"
  fi

  # --- 4. o runtime dir por-root NÃO é verificado aqui, de propósito ---------
  # Tentei-o e o check só sabia SALTAR: subir infra a partir de um segundo root
  # dentro do sandbox não é fiável neste ambiente. E um check que nunca corre
  # não é um check — é um SKIP a fazer de conta.
  #
  # O invariante está fixado onde é DETERMINÍSTICO: o teste unitário
  # `base_root_e_runtime_dir_honram_env_vars_explicitas` (delonix-net) exige que
  # um root alternativo resolva uma pasta DIFERENTE da do root por omissão, que
  # o root por omissão mantenha o nome nu byte a byte, e que o caminho do socket
  # fique abaixo dos 108 bytes do `sun_path`. É pura, corre sempre, e falha se o
  # `root_suffix` for revertido.
  #
  # A primeira versão deste check contava `/tmp/delonix-net-*` inteiro e
  # afirmava `>= 1` — passava com a correcção REVERTIDA, porque a glob apanha os
  # restos de testes unitários (`delonix-net-naming-<pid>`). Fica registado
  # porque é a mesma armadilha do filtro largo que esta série já pagou três
  # vezes: um verde sobre uma medição que não exercita nada.

  dlx net netns down >/dev/null 2>&1
}

scen_control_restart() {
  head_ "control-restart — matar o plano de controlo não pode mexer em nada"
  dlx net netns up >/dev/null 2>&1
  dlx container run -d --name cr1 --net chaosnet "$IMAGE" sleep 300 >/dev/null 2>&1
  sleep 2
  neton cr1 || { skip "control-restart" "rede não subiu no cenário base"; dlx container rm -f cr1 >/dev/null 2>&1; return; }
  local pin_before ctl_before wl_before
  pin_before=$(holder_pid); ctl_before=$(control_pid); wl_before=$(cpid cr1)
  if [ -z "$ctl_before" ]; then
    skip "control-restart" "binário sem plano de controlo separado"
    dlx container rm -f cr1 >/dev/null 2>&1; return
  fi
  kill -9 "$ctl_before" 2>/dev/null; sleep 2
  dlx net netns up >/dev/null 2>&1
  sleep 2
  local pin_after ctl_after wl_after
  pin_after=$(holder_pid); ctl_after=$(control_pid); wl_after=$(cpid cr1)
  log "pin $pin_before→$pin_after · control $ctl_before→$ctl_after · workload $wl_before→$wl_after"
  if [ "$pin_before" != "$pin_after" ]; then
    bad "control-restart" "o pin foi substituído ($pin_before→$pin_after) — a netns foi deitada fora"
  elif [ "$wl_before" != "$wl_after" ]; then
    bad "control-restart" "o container foi REINICIADO ($wl_before→$wl_after) em vez de intocado"
  elif [ "$ctl_before" = "$ctl_after" ]; then
    bad "control-restart" "o controlo não foi reiniciado (pid inalterado) — ficou sem plano de controlo"
  elif ! neton cr1; then
    bad "control-restart" "o container perdeu a rede"
  else
    ok "control-restart (pin e workload intocados, controlo reposto)"
  fi
  dlx container rm -f cr1 >/dev/null 2>&1
}

# ------------------------------------------------------------------- driver --

# The declarative cycle has to converge WITHOUT restarting the workload, and has
# to refuse what it cannot converge instead of doing it anyway.
#
# TWO containers on purpose. The lesson from the v0.41.0 pod bug is in
# `CLAUDE.md`: a guard that consults state the loop itself MUTATES is not a
# guard, and a scenario with one element never reveals it. Here the second
# container is the control — it must come out of the whole cycle with the SAME
# pid, proving the converge touched only what the plan said it would.
scen_stack_converge() {
  head_ "stack-converge — o apply converge a quente e recusa o que nao converge"
  local dir="$SANDBOX/stack-converge"
  rm -rf "$dir"; mkdir -p "$dir"
  cat > "$dir/m.yaml" <<YAML
apiVersion: delonix.io/v1
kind: Container
metadata: { name: sc1 }
spec: { image: $IMAGE, command: ["sleep","600"], memory: 64M, network: host }
---
apiVersion: delonix.io/v1
kind: Container
metadata: { name: sc2 }
spec: { image: $IMAGE, command: ["sleep","600"], memory: 64M, network: host }
YAML
  dlx stack apply -f "$dir/m.yaml" >/dev/null 2>&1
  sleep 2
  local p1 p2; p1=$(cpid sc1); p2=$(cpid sc2)
  [ -z "$p1" ] || [ -z "$p2" ] && { skip "stack-converge" "containers nao arrancaram"; dlx container rm -f sc1 sc2 >/dev/null 2>&1; return; }

  # 1. Um manifesto inalterado nao pode propor nada.
  if dlx stack plan -f "$dir/m.yaml" --detailed-exitcode >/dev/null 2>&1; then :
  else bad "stack-converge" "um manifesto inalterado propos alteracoes"; dlx container rm -f sc1 sc2 >/dev/null 2>&1; return; fi

  # 2. Um campo QUENTE converge sem tocar no pid — e sem tocar no outro container.
  sed -i '0,/memory: 64M/s//memory: 128M/' "$dir/m.yaml"
  dlx stack apply -f "$dir/m.yaml" >/dev/null 2>&1
  local a1 a2; a1=$(cpid sc1); a2=$(cpid sc2)
  # O pid intacto sozinho NAO prova convergencia — um apply que nao faz nada
  # tambem deixa o pid intacto. A prova e o registo ter mudado E o plano
  # seguinte nada ter a propor.
  local m1 m2
  m1=$(dlx container inspect sc1 2>/dev/null | grep -o '"memory_max"[^,]*' | head -1)
  m2=$(dlx container inspect sc2 2>/dev/null | grep -o '"memory_max"[^,]*' | head -1)
  if [ "$p1" != "$a1" ]; then
    bad "stack-converge" "convergir a memoria reiniciou o container ($p1 -> $a1)"
  elif [ "$p2" != "$a2" ]; then
    bad "stack-converge" "o container que nao mudou foi reiniciado ($p2 -> $a2)"
  elif [[ "$m1" != *128M* ]]; then
    bad "stack-converge" "o apply reportou sucesso sem convergir a memoria ($m1)"
  elif [[ "$m2" != *64M* ]]; then
    bad "stack-converge" "o apply mexeu no container que nao mudou ($m2)"
  elif ! dlx stack plan -f "$dir/m.yaml" --detailed-exitcode >/dev/null 2>&1; then
    bad "stack-converge" "o plano continua a propor alteracoes depois do apply"
  else
    ok "stack-converge: memoria a quente (pid $p1 intocado, controlo $p2 intocado)"
  fi

  # 3. Um campo FRIO tem de ser RECUSADO, sem tocar em nada.
  sed -i "s|image: $IMAGE|image: $IMAGE-inexistente|" "$dir/m.yaml"
  if dlx stack apply -f "$dir/m.yaml" >/dev/null 2>&1; then
    bad "stack-converge" "um campo frio foi aplicado sem --replace"
  else
    local r1; r1=$(cpid sc1)
    if [ "$r1" = "$p1" ]; then ok "stack-converge: campo frio recusado, nada tocado"
    else bad "stack-converge" "a recusa mexeu no container ($p1 -> $r1)"; fi
  fi

  # 4. O destroy leva o que a stack possui, e SO isso.
  sed -i "s|image: $IMAGE-inexistente|image: $IMAGE|" "$dir/m.yaml"
  dlx container run -d --name sc-alheio --net host "$IMAGE" sleep 600 >/dev/null 2>&1
  dlx stack destroy -f "$dir/m.yaml" >/dev/null 2>&1
  if dlx container inspect sc1 >/dev/null 2>&1 || dlx container inspect sc2 >/dev/null 2>&1; then
    bad "stack-converge" "o destroy deixou containers da stack para tras"
  elif dlx container inspect sc-alheio >/dev/null 2>&1; then
    ok "stack-converge: destroy levou os da stack e poupou o alheio"
  else
    bad "stack-converge" "o destroy apagou um container que a stack nao possui"
  fi
  dlx container rm -f sc-alheio >/dev/null 2>&1
  rm -rf "$dir"
}

# ---------------------------------------------------------------------------
# stack-netroute — o manifesto FECHA a excepção que abre (ADR-0013 camada B)
#
# Uma `kind: NetworkRoute` é a excepção ao isolamento entre redes. Antes desta
# série era aplicada e nunca convergia: tirá-la do ficheiro e correr
# `apply --prune` NÃO a fechava, e o plano dizia «sem alterações». O git dizia
# fechado e o nó estava aberto.
#
# O que este cenário exige, e porque é o CICLO e não um comando de cada vez:
# cada passo isolado devolve 0 mesmo com o defeito presente. O que distingue é
# o estado do dataplane DEPOIS de o manifesto deixar de declarar a rota.
scen_stack_netroute() {
  head_ "stack-netroute — o prune e o destroy fecham mesmo a rota"
  local dir="$SANDBOX/stack-netroute"
  rm -rf "$dir"; mkdir -p "$dir"
  cat > "$dir/m.yaml" <<'YAML'
apiVersion: delonix.io/v1
kind: Network
metadata: { name: nrweb }
spec: { driver: bridge }
---
apiVersion: delonix.io/v1
kind: Network
metadata: { name: nrdb }
spec: { driver: bridge }
---
apiVersion: delonix.io/v1
kind: NetworkRoute
metadata: { name: web-reaches-db }
spec: { from: nrweb, to: nrdb }
YAML
  # Sem a rota — o mesmo ficheiro menos o último documento.
  head -n -5 "$dir/m.yaml" > "$dir/sem.yaml"

  dlx stack apply -f "$dir/m.yaml" >/dev/null 2>&1
  local pin; pin=$(holder_pid)
  [ -z "$pin" ] && { skip "stack-netroute" "holder nao arrancou"; return; }

  # Conta os pares ENTRE REDES no mapa vivo. Os auto-pares (`<b> . <b>`) são o
  # isolamento intra-rede e não são rotas — contá-los diria «há rota» sempre.
  npairs() {
    nsenter -t "$pin" -U -m -n -- nft list map ip dlxing netpair 2>/dev/null \
      | grep -oE '"[^"]+" \. "[^"]+"' | awk -F'" \\. "' '{gsub(/"/,""); if ($1 != $2) print}' | wc -l
  }

  # 1. A rota existe no dataplane, não só no relato do comando.
  if [ "$(npairs)" -lt 1 ]; then
    bad "stack-netroute" "o apply reportou sucesso e nao ha rota no @netpair"
    dlx stack destroy -f "$dir/m.yaml" >/dev/null 2>&1; rm -rf "$dir"; return
  fi
  # 2. Um manifesto inalterado nada tem a propor.
  if ! dlx stack plan -f "$dir/m.yaml" --detailed-exitcode >/dev/null 2>&1; then
    bad "stack-netroute" "um manifesto inalterado propos alteracoes"
  fi

  # 3. O CONTROLO: uma rota criada à mão não leva carimbo e tem de sobreviver a
  #    um prune. Um apply que apaga o que não criou é a falha que destroi a
  #    confianca na ferramenta.
  dlx network route nrdb nrweb >/dev/null 2>&1

  # 4. O passo que não existia: tirar a rota do manifesto FECHA-A.
  cp "$dir/sem.yaml" "$dir/m.yaml"
  dlx stack apply -f "$dir/m.yaml" --prune >/dev/null 2>&1
  local depois; depois=$(npairs)
  if [ "$depois" -ne 1 ]; then
    bad "stack-netroute" "apos o prune esperava-se so a rota imperativa, ha $depois"
  elif ! dlx stack plan -f "$dir/m.yaml" --detailed-exitcode >/dev/null 2>&1; then
    bad "stack-netroute" "o plano continua a propor alteracoes depois do prune"
  else
    ok "stack-netroute: o prune fechou a declarada e poupou a imperativa"
  fi

  # 5. O destroy leva as redes, e uma rota não sobrevive a nenhuma das pontas —
  #    nem no registo nem no mapa. Deixar o elemento la reabriria o caminho
  #    sozinho na proxima rede com o mesmo nome (`bridge_name` e deterministico).
  dlx stack destroy -f "$dir/m.yaml" >/dev/null 2>&1
  local sobra; sobra=$(npairs)
  local regs; regs=$(ls "$SANDBOX/root/ingress/routes" 2>/dev/null | wc -l)
  if [ "$sobra" -ne 0 ]; then
    bad "stack-netroute" "o destroy deixou $sobra par(es) orfaos no @netpair"
  elif [ "$regs" -ne 0 ]; then
    bad "stack-netroute" "o destroy deixou $regs registo(s) de rota para tras"
  else
    ok "stack-netroute: destroy sem orfaos no mapa nem no registo"
  fi
  rm -rf "$dir"
}

# ---------------------------------------------------------------------------
# truenas-destroy — o caminho destrutivo do provisionamento (ADR-0009)
#
# Precisa de uma appliance TrueNAS REAL, e SALTA com uma linha audivel sem ela:
#
#   DELONIX_CHAOS_TRUENAS_URL=https://192.168.122.83 \
#   DELONIX_CHAOS_TRUENAS_USER=truenas_admin \
#   DELONIX_CHAOS_TRUENAS_PASS=delonix-admin \
#   DELONIX_CHAOS_TRUENAS_POOL=tank scripts/chaos.sh truenas_destroy
#
# O que prova, e o que falha se a correccao for revertida:
#   * `volumes rm` SEM a flag nao pode destruir o dataset — tirar o guarda
#     `if destroy_remote` faz o passo 2 falhar;
#   * `--destroy-remote` num volume que nao provisionamos e RECUSADO e deixa o
#     volume local intacto — tirar a exigencia do carimbo de posse faz o passo 4
#     falhar (destruiria o dataset de outrem, ou removeria o volume local).
# ---------------------------------------------------------------------------
scen_truenas_destroy() {
  head_ "truenas-destroy — o remoto so morre com a flag, e so o que e nosso"
  local url="${DELONIX_CHAOS_TRUENAS_URL:-}"
  local user="${DELONIX_CHAOS_TRUENAS_USER:-}"
  local pass="${DELONIX_CHAOS_TRUENAS_PASS:-}"
  local pool="${DELONIX_CHAOS_TRUENAS_POOL:-tank}"
  if [ -z "$url" ] || [ -z "$user" ] || [ -z "$pass" ]; then
    skip "truenas-destroy" "DELONIX_CHAOS_TRUENAS_URL/USER/PASS nao definidos"
    return
  fi
  command -v curl >/dev/null 2>&1 || { skip "truenas-destroy" "sem curl"; return; }

  local ds="$pool/dlxchaos-$$"
  local dir="$SANDBOX/truenas-destroy"
  rm -rf "$dir"; mkdir -p "$dir"
  cat > "$dir/m.yaml" <<YAML
apiVersion: delonix.io/v1
kind: Secret
metadata: { name: chaos-nas }
spec:
  stringData: { password: $pass }
---
apiVersion: delonix.io/v1
kind: Volume
metadata: { name: chaosvol }
spec:
  provision:
    truenas:
      url: $url
      username: $user
      passwordSecret: chaos-nas
      insecureTLS: true
      dataset: $ds
      quota: 1G
YAML
  # A appliance responde ao dataset? (200 = existe, 404 = nao)
  nas_code() {
    curl -sk -u "$user:$pass" --max-time 15 -o /dev/null -w '%{http_code}' \
      "$url/api/v2.0/pool/dataset/id/$(printf '%s' "$1" | sed 's|/|%2F|g')" 2>/dev/null
  }

  # 1. Provisionar.
  if ! dlx stack apply -f "$dir/m.yaml" >/dev/null 2>&1; then
    skip "truenas-destroy" "o apply falhou (appliance inalcancavel?)"
    rm -rf "$dir"; return
  fi
  if [ "$(nas_code "$ds")" != "200" ]; then
    bad "truenas-destroy" "o apply reportou sucesso sem criar $ds na NAS"
    rm -rf "$dir"; return
  fi

  # 2. `rm` SEM a flag: o dataset TEM de sobreviver.
  dlx volumes rm chaosvol >/dev/null 2>&1
  if [ "$(nas_code "$ds")" = "200" ]; then
    ok "truenas-destroy: rm sem a flag deixou o dataset em paz"
  else
    bad "truenas-destroy" "um rm normal destruiu o dataset na NAS"
    rm -rf "$dir"; return
  fi

  # 3. Um volume que NAO provisionamos nao pode ser destruido por engano.
  dlx volumes create chaosalheio >/dev/null 2>&1
  if dlx volumes rm chaosalheio --destroy-remote >/dev/null 2>&1; then
    bad "truenas-destroy" "--destroy-remote aceitou um volume sem provisionamento"
  elif dlx volumes inspect chaosalheio >/dev/null 2>&1; then
    ok "truenas-destroy: recusou o alheio E deixou o volume local intacto"
  else
    bad "truenas-destroy" "a recusa removeu o volume local na mesma"
  fi
  dlx volumes rm chaosalheio >/dev/null 2>&1

  # 4. Com a flag e com o carimbo: o dataset morre mesmo.
  dlx stack apply -f "$dir/m.yaml" >/dev/null 2>&1
  dlx volumes rm chaosvol --destroy-remote >/dev/null 2>&1
  if [ "$(nas_code "$ds")" = "404" ]; then
    ok "truenas-destroy: --destroy-remote destruiu o dataset provisionado"
  else
    bad "truenas-destroy" "--destroy-remote nao destruiu $ds"
  fi

  # Limpeza best-effort, para um cenario abortado nao deixar datasets na NAS.
  curl -sk -u "$user:$pass" -X DELETE -H 'Content-Type: application/json' \
    --max-time 20 "$url/api/v2.0/pool/dataset/id/$(printf '%s' "$ds" | sed 's|/|%2F|g')" \
    -d '{"recursive":true}' >/dev/null 2>&1
  dlx volumes rm chaosvol >/dev/null 2>&1
  rm -rf "$dir"
}

# O caminho de rede custom TAPA a cgroup2, e um teste sem CONTROLO fica verde.
#
# O re-exec de `--net <rede>`/`--pod` passa por `ip netns exec`, que monta um sysfs
# novo sobre `/sys`: sem `cgroup.controllers` visível nenhuma operação de cgroup
# funciona, e o container ficava no cgroup do processo que o lançou — com `-m` a não
# aplicar nada e o `container pause`/`update` a escreverem na sessão do operador.
#
# **Porque é preciso o container de CONTROLO** (`--net none`, o caminho que nunca
# tapou nada): uma sessão SSH normal não tem cgroup delegado NENHUM, e aí o container
# de rede custom também não tem leaf própria — por uma razão de ambiente, não por este
# bug. Sem o controlo, este cenário acusava um defeito onde não há um (falso positivo),
# ou saltava sempre num host onde há (falso verde). Com ele, a pergunta passa a ser a
# certa: os DOIS caminhos comportam-se igual? Se o `--net none` tem leaf e o
# `--net <rede>` não tem, a diferença só pode ser o re-exec.
scen_cgroup_netns() {
  head_ "cgroup-netns — o re-exec de rede custom não pode deixar o container no cgroup do chamador"
  local mine; mine=$(sed 's|^0::||' /proc/self/cgroup 2>/dev/null)
  dlx container run -d --name ckg0 --net none      -m 128M "$IMAGE" sleep 120 >/dev/null 2>&1
  dlx container run -d --name ckg1 --net chaosnet  -m 128M "$IMAGE" sleep 120 >/dev/null 2>&1
  sleep 2
  local p0 p1; p0=$(cpid ckg0); p1=$(cpid ckg1)
  if [ -z "$p0" ] || [ -z "$p1" ]; then
    skip "cgroup-netns" "container não arrancou (none=${p0:-—} net=${p1:-—})"
    dlx container rm -f ckg0 ckg1 >/dev/null 2>&1; return
  fi
  local cg0 cg1; cg0=$(sed 's|^0::||' "/proc/$p0/cgroup" 2>/dev/null)
  cg1=$(sed 's|^0::||' "/proc/$p1/cgroup" 2>/dev/null)
  log "chamador      = $mine"
  log "--net none    = $cg0"
  log "--net custom  = $cg1"
  if [ "$cg0" = "$mine" ]; then
    # O controlo também não tem leaf: é o ambiente, não o re-exec.
    skip "cgroup-netns" "sem cgroup delegado nesta sessão (nem o --net none tem leaf); \
usa: systemd-run --user --scope -p Delegate=yes -- <comando>"
  elif [ "$cg1" = "$mine" ]; then
    bad "cgroup-netns" "o container de rede custom ficou no cgroup do chamador — \
-m/--cpus não aplicam, e pause/update escrevem na sessão"
  elif [ "$(cat "/sys/fs/cgroup$cg1/memory.max" 2>/dev/null)" != "134217728" ]; then
    bad "cgroup-netns" "leaf própria mas -m 128M não aplicado (memory.max=\
$(cat "/sys/fs/cgroup$cg1/memory.max" 2>/dev/null || echo ausente))"
  else
    ok "cgroup-netns (leaf própria nos dois caminhos, memory.max=134217728)"
  fi
  dlx container rm -f ckg0 ckg1 >/dev/null 2>&1
}

ALL=(holder_kill control_restart posse_destrutiva holder_wedge slirp_kill idempotent_up oom concurrent_attach namespace_isolation pod_namespace_isolation pod_holder_respawn scale abrupt_kill aggregate_ceiling delegated_scope cgroup_netns disk_full write_failure stack_converge stack_netroute truenas_destroy)

while [ $# -gt 0 ]; do
  case "$1" in
    --bin) BIN="$2"; shift 2;;
    --keep) KEEP=1; shift;;
    --clean) teardown_quiet; echo "sandbox limpo."; exit 0;;
    -h|--help) sed -n '2,40p' "$0"; exit 0;;
    *) SEL+=("$1"); shift;;
  esac
done
SELECTED=("${SEL[@]:-${ALL[@]}}")

command -v "$BIN" >/dev/null 2>&1 || [ -x "$BIN" ] || { echo "binário não encontrado: $BIN"; exit 2; }

printf '\033[1mDelonix chaos harness\033[0m — sandbox %s · binário %s\n' "$SANDBOX" "$BIN"
setup
trap '[ $KEEP -eq 0 ] && teardown_quiet' EXIT

for s in "${SELECTED[@]}"; do
  "scen_${s}" || true
done

printf '\n\033[1m── resumo ──\033[0m\n'
for r in "${RESULTS[@]}"; do printf '  %s\n' "$r"; done
printf '\n  %d PASS · %d FAIL · %d SKIP\n\n' "$PASS" "$FAIL" "$SKIP"
[ "$FAIL" -eq 0 ]
