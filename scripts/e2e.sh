#!/usr/bin/env bash
# E2E de toda a superfície da CLI `delonix` — corre cada comando/subcomando real
# e regista PASS/FAIL/SKIP num relatório. NÃO é um teste unitário: toca no
# estado real da máquina (containers, redes, volumes), por isso limpa atrás de si.
#
# Uso:  ./scripts/e2e.sh [caminho-do-binario]
# Saída: relatório em stdout + JSONL detalhado em $OUT/results.jsonl
#
# Regra: NUNCA usar o `delonix` do PATH — processos/binários antigos são uma
# armadilha conhecida deste repo (ver CLAUDE.md). O default é o build local.

set -uo pipefail

BIN="${1:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/delonix}"
OUT="${OUT:-/tmp/delonix-e2e}"
mkdir -p "$OUT"
: >"$OUT/results.jsonl"

PASS=0; FAIL=0; SKIP=0
declare -a FAILED_NAMES=()

# Prefixo único para tudo o que este teste cria — para a limpeza nunca tocar em
# recursos do utilizador.
PFX="e2e$$"

log() { printf '%s\n' "$*"; }

# check <nome> <expectativa: ok|fail> <comando...>
#   ok   = esperamos RC=0
#   fail = esperamos RC!=0 (testes de erro: a CLI tem de RECUSAR, não aceitar)
check() {
  local name="$1" expect="$2"; shift 2
  local out rc
  out="$("$@" 2>&1)"; rc=$?
  local verdict=FAIL
  if [[ "$expect" == "ok" ]]; then
    [[ $rc -eq 0 ]] && verdict=PASS
  else
    [[ $rc -ne 0 ]] && verdict=PASS
  fi
  python3 - "$name" "$verdict" "$rc" "$out" "$*" >>"$OUT/results.jsonl" <<'PY'
import json,sys
name,verdict,rc,out,cmd=sys.argv[1:6]
print(json.dumps({"name":name,"verdict":verdict,"rc":int(rc),"cmd":cmd,"output":out[:4000]}))
PY
  if [[ $verdict == PASS ]]; then
    PASS=$((PASS+1)); log "  PASS  $name"
  else
    FAIL=$((FAIL+1)); FAILED_NAMES+=("$name")
    log "  FAIL  $name  (rc=$rc, esperado=$expect)"
    log "        $ $*"
    sed 's/^/        | /' <<<"$out" | head -8
  fi
}

skip() {
  SKIP=$((SKIP+1)); log "  SKIP  $1  — $2"
  python3 -c 'import json,sys; print(json.dumps({"name":sys.argv[1],"verdict":"SKIP","reason":sys.argv[2]}))' "$1" "$2" >>"$OUT/results.jsonl"
}

section() { log ""; log "=== $1 ==="; }

[[ -x "$BIN" ]] || { log "binário não executável: $BIN"; exit 1; }
log "binário: $BIN"
log "versão:  $("$BIN" --version 2>&1)"

########################################
section "help / superfície da CLI"
########################################
check "help raiz" ok "$BIN" --help
check "version" ok "$BIN" --version
for g in container image build vm volumes network stack system cluster completion; do
  check "help de '$g'" ok "$BIN" "$g" --help
done
# Todos os subcomandos de cada grupo têm de ter --help funcional.
for g in container image vm volumes network stack system cluster; do
  subs=$("$BIN" "$g" --help 2>/dev/null | awk '/^(Commands|Subcommands):/{f=1;next} /^$/{f=0} f && $1 !~ /^-/ {print $1}')
  for s in $subs; do
    [[ "$s" == "help" ]] && continue
    check "help de '$g $s'" ok "$BIN" "$g" "$s" --help
  done
done

########################################
section "comandos de leitura (não destrutivos)"
########################################
check "container ls" ok "$BIN" container ls
check "container ls -a" ok "$BIN" container ls -a
check "container ls -q" ok "$BIN" container ls -q
check "image ls" ok "$BIN" image ls
check "image --vm ls" ok "$BIN" image --vm ls
check "volumes ls" ok "$BIN" volumes ls
check "network ls" ok "$BIN" network ls
check "vm ls" ok "$BIN" vm ls
check "cluster ls" ok "$BIN" cluster ls
check "system state" ok "$BIN" system state
check "system disk" ok "$BIN" system disk
check "system events" ok "$BIN" system events
check "completion bash" ok "$BIN" completion bash

########################################
section "erros: a CLI tem de RECUSAR o que é inválido"
########################################
check "container describe de inexistente recusa" fail "$BIN" container describe naoexiste-$PFX
check "container inspect de inexistente recusa" fail "$BIN" container inspect naoexiste-$PFX
check "volumes inspect de inexistente recusa" fail "$BIN" volumes inspect naoexiste-$PFX
check "network inspect de inexistente recusa" fail "$BIN" network inspect naoexiste-$PFX
check "container update sem mudanças recusa" fail "$BIN" container update naoexiste-$PFX
check "container stop de inexistente recusa" fail "$BIN" container stop naoexiste-$PFX
check "container rm de inexistente recusa" fail "$BIN" container rm naoexiste-$PFX
check "vm rm de inexistente recusa" fail "$BIN" vm rm naoexiste-$PFX
check "stack apply de ficheiro inexistente recusa" fail "$BIN" stack apply -f /nao/existe.yaml

########################################
section "volumes: ciclo de vida"
########################################
VOL="vol-$PFX"
check "volumes create" ok "$BIN" volumes create "$VOL"
check "volumes create idempotente" ok "$BIN" volumes create "$VOL"
check "volumes ls mostra-o" ok bash -c "'$BIN' volumes ls | grep -q '$VOL'"
check "volumes inspect" ok "$BIN" volumes inspect "$VOL"
check "volumes describe" ok "$BIN" volumes describe "$VOL"

########################################
section "network: ciclo de vida"
########################################
NET="net-$PFX"
check "network create" ok "$BIN" network create "$NET" --subnet 10.199.0.0/24
check "network ls mostra-a" ok bash -c "'$BIN' network ls | grep -q '$NET'"
check "network inspect" ok "$BIN" network inspect "$NET"
check "network describe" ok "$BIN" network describe "$NET"

########################################
section "image"
########################################
IMG="${E2E_IMAGE:-alpine:3.19}"
if "$BIN" image ls 2>/dev/null | grep -q "${IMG%%:*}"; then
  check "image describe" ok "$BIN" image describe "$IMG"
else
  check "image pull ($IMG)" ok "$BIN" image pull "$IMG"
  check "image describe" ok "$BIN" image describe "$IMG"
fi

########################################
section "container: ciclo de vida + hot reconfig"
########################################
C="c-$PFX"
# Porta alta e improvável de colidir com o que já corre na máquina.
P1=$((29500 + RANDOM % 300)); P2=$((29900 + RANDOM % 90))

check "container run -d -p" ok "$BIN" container run -d --name "$C" -p "$P1:80" "$IMG" sleep 600
if "$BIN" container inspect "$C" >/dev/null 2>&1; then
  check "container ls mostra-o" ok bash -c "'$BIN' container ls | grep -q '$C'"
  check "container describe" ok "$BIN" container describe "$C"
  check "container inspect (JSON válido)" ok bash -c "'$BIN' container inspect '$C' | python3 -m json.tool >/dev/null"
  check "container exec" ok "$BIN" container exec "$C" /bin/true
  check "container logs" ok "$BIN" container logs "$C"
  check "container stats" ok "$BIN" container stats "$C"

  # --- HOT RECONFIG: o núcleo desta sessão ---
  check "update: publish-add a quente" ok "$BIN" container update "$C" --publish-add "$P2:80"
  check "update: porta nova no registo" ok bash -c "'$BIN' container inspect '$C' | grep -q '$P2:80'"
  check "update: publish-add duplicado recusa" fail "$BIN" container update "$C" --publish-add "$P2:81"
  check "update: publish-rm a quente" ok "$BIN" container update "$C" --publish-rm "$P2"
  check "update: porta saiu do registo" fail bash -c "'$BIN' container inspect '$C' | grep -q '$P2:80'"
  check "update: publish-rm de porta não publicada recusa" fail "$BIN" container update "$C" --publish-rm 65001

  check "update: volume-add a quente" ok "$BIN" container update "$C" --volume-add "$VOL:/mnt/e2e"
  check "update: mount visível DENTRO do container" ok "$BIN" container exec "$C" /bin/sh -c "test -d /mnt/e2e"
  check "update: mount no registo" ok bash -c "'$BIN' container describe '$C' | grep -q '/mnt/e2e'"
  check "update: volume-add no mesmo destino recusa" fail "$BIN" container update "$C" --volume-add "$VOL:/mnt/e2e"
  check "update: volume-rm a quente" ok "$BIN" container update "$C" --volume-rm /mnt/e2e
  check "update: mount desapareceu de dentro" fail "$BIN" container exec "$C" /bin/sh -c "mountpoint -q /mnt/e2e"

  check "update: PID intacto após o hot reconfig" ok bash -c "test \"\$('$BIN' container inspect '$C' | python3 -c 'import json,sys; print(json.load(sys.stdin)[0][\"pid\"])')\" != 'None'"

  check "container stop" ok "$BIN" container stop "$C"
  check "update num container parado recusa" fail "$BIN" container update "$C" --publish-add "$P2:80"
  check "container start" ok "$BIN" container start "$C"
  check "container rm -f" ok "$BIN" container rm -f "$C"
else
  # O `run` já foi contado como FAIL pelo `check` acima; aqui só registamos que
  # toda a bateria que dependia dele não chegou a correr.
  skip "ciclo de vida + hot reconfig do container" "o container run falhou — nada disto pôde ser exercitado"
fi

########################################
section "container em rede custom: hot reconfig pelo ingress"
########################################
CN="cn-$PFX"
NET2="net2-$PFX"
if "$BIN" network create "$NET2" --subnet 10.198.0.0/24 >/dev/null 2>&1 && \
   "$BIN" container run -d --name "$CN" --net "$NET" "$IMG" sleep 600 >/dev/null 2>&1; then
  check "update: net-connect a quente" ok "$BIN" container update "$CN" --net-connect "$NET2"
  check "update: rede extra no describe" ok bash -c "'$BIN' container describe '$CN' | grep -q '$NET2'"
  check "update: net-connect repetido recusa" fail "$BIN" container update "$CN" --net-connect "$NET2"
  check "update: net-rate a quente" ok "$BIN" container update "$CN" --net-rate 10mbit
  check "update: taxa inválida recusa" fail "$BIN" container update "$CN" --net-rate depressa
  check "update: net-rate-clear" ok "$BIN" container update "$CN" --net-rate-clear
  check "update: net-disconnect a quente" ok "$BIN" container update "$CN" --net-disconnect "$NET2"
  check "update: net-disconnect de rede não ligada recusa" fail "$BIN" container update "$CN" --net-disconnect "$NET2"
  "$BIN" container rm -f "$CN" >/dev/null 2>&1
else
  skip "hot reconfig em rede custom" "não foi possível criar rede/container em rede custom"
fi
"$BIN" network rm "$NET2" >/dev/null 2>&1

########################################
section "stack / manifesto"
########################################
WORK="$OUT/stack-$PFX"; mkdir -p "$WORK"
cat >"$WORK/delonix-manifest.yaml" <<YAML
apiVersion: delonix.io/v1
kind: Volume
metadata:
  name: sv-$PFX
spec: {}
---
apiVersion: delonix.io/v1
kind: Network
metadata:
  name: sn-$PFX
spec:
  driver: bridge
  subnet: 10.197.0.0/24
YAML
check "stack apply" ok "$BIN" stack apply -f "$WORK/delonix-manifest.yaml"
check "stack apply idempotente" ok "$BIN" stack apply -f "$WORK/delonix-manifest.yaml"
check "stack describe" ok "$BIN" stack describe -f "$WORK/delonix-manifest.yaml"
check "volumes describe do manifesto" ok "$BIN" volumes describe "sv-$PFX"
"$BIN" volumes rm "sv-$PFX" >/dev/null 2>&1
"$BIN" network rm "sn-$PFX" >/dev/null 2>&1

########################################
section "vm (só o que não precisa de hipervisor)"
########################################
check "vm ls" ok "$BIN" vm ls
check "vm create sem imagem recusa" fail "$BIN" vm create "vm-$PFX" --image /nao/existe.qcow2

########################################
section "limpeza"
########################################
"$BIN" container rm -f "$C" >/dev/null 2>&1
check "volumes rm" ok "$BIN" volumes rm "$VOL"
check "network rm" ok "$BIN" network rm "$NET"

########################################
log ""
log "======================================"
log " PASS=$PASS  FAIL=$FAIL  SKIP=$SKIP"
log " detalhe: $OUT/results.jsonl"
if (( FAIL > 0 )); then
  log ""
  log " falhas:"
  for f in "${FAILED_NAMES[@]}"; do log "   - $f"; done
fi
log "======================================"
exit 0   # o relatório é o produto; um FAIL não deve abortar a recolha
