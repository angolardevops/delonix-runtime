#!/usr/bin/env bash
# Fugas do ciclo de vida da rede: o que FICA no holder depois de um ciclo
# completo. Cada check conta o que resta (nft, ss, network ls, portas do host)
# e não se fia no código de saída do comando que devia ter limpo.
#
# Uso:  DELONIX_ROOT=<root> DELONIX_NET_RUNTIME_DIR=<run> \
#         scripts/net-lifecycle-leaks.sh <binário> [prefixo]
#
# Chamado pela secção «net — ciclo de vida» do `scripts/e2e.sh`, e corre sozinho
# para comparar dois binários: contra a v4.4.0+68 (`d3d6f394`) chumba nos checks
# de rm, DHCP, egress, publish e rede de VM; contra a correcção passa.
#
# Precisa dos DOIS roots isolados (nunca a infra real do host — o `network rm`
# daqui apagaria redes de verdade) e de uma imagem já no root (`E2E_IMAGE`,
# alpine por omissão). A rede de VM só corre com `E2E_VM_DISK=<qcow2>` e o
# `cloud-hypervisor` no PATH; sem eles é SKIP, nunca PASS.
#
# Saída: uma linha `PASS|FAIL|SKIP <nome>` por check; código 1 se algum chumbar.

set -uo pipefail

BIN="${1:?uso: $0 <binário> [prefixo]}"
PFX="${2:-lk$$}"
IMG="${E2E_IMAGE:-alpine:3.19}"
FAILS=0

if [[ -z "${DELONIX_ROOT:-}" || -z "${DELONIX_NET_RUNTIME_DIR:-}" ]]; then
  echo "SKIP net-lifecycle-leaks — exige DELONIX_ROOT E DELONIX_NET_RUNTIME_DIR"
  exit 0
fi

pass() { echo "PASS $1"; }
fail() { echo "FAIL $1 — $2"; FAILS=$((FAILS + 1)); }
skip() { echo "SKIP $1 — $2"; }

# O pin do holder; vazio se a infra estiver em baixo.
pin() { "$BIN" net netns status 2>/dev/null | grep -oE 'pin [0-9]+' | grep -oE '[0-9]+' | head -1; }
inholder() { nsenter -t "$(pin)" -U -m -n --preserve-credentials -- "$@"; }
bridge_of() { "$BIN" network ls 2>/dev/null | awk -v n="$1" '$1==n {print $3}'; }
in_ls() { "$BIN" network ls 2>/dev/null | awk -v n="$1" '$1==n' | wc -l; }

# Quantas vezes a bridge é nomeada em cada peça do dataplane.
count_dlxbr() { inholder nft list set ip dlxing dlxbr 2>/dev/null | grep -o "\"$1\"" | wc -l; }
count_netpair() { inholder nft list map ip dlxing netpair 2>/dev/null | grep -o "\"$1\"" | wc -l; }
count_fwdeny() { inholder nft list chain ip dlxing fwdeny 2>/dev/null | grep -c "\"$1\""; }
count_dhcp() { inholder ss -uanH 'sport = :67' 2>/dev/null | grep -c "%$1:"; }
# Sockets DHCP presos a um ifindex que já não existe (o kernel mostra `%if<N>`).
count_dhcp_orphan() { inholder ss -uanH 'sport = :67' 2>/dev/null | grep -cE '%if[0-9]+:'; }

NET="n$PFX"; KEEP="k$PFX"
cleanup() {
  for c in "c1$PFX" "c2$PFX" "c3$PFX" "keep$PFX"; do "$BIN" container rm -f "$c" >/dev/null 2>&1; done
  "$BIN" vm rm -f "v$PFX" >/dev/null 2>&1
  for n in "$NET" "$KEEP" "vn$PFX"; do "$BIN" network rm "$n" >/dev/null 2>&1; done
}
trap cleanup EXIT

# Um container noutra rede segura o holder de pé durante tudo: sem ele, o último
# `rm` derrubaria a infra inteira e cada contagem abaixo daria zero por acaso.
"$BIN" network create "$KEEP" >/dev/null 2>&1
if ! "$BIN" container run -d --name "keep$PFX" --net "$KEEP" "$IMG" sleep 3000 >/dev/null 2>&1; then
  echo "SKIP net-lifecycle-leaks — não arrancou o container que segura o holder ($IMG no root?)"
  exit 0
fi

# --- 1. network rm com um container ligado ----------------------------------
"$BIN" network create "$NET" >/dev/null 2>&1
"$BIN" container run -d --name "c1$PFX" --net "$NET" "$IMG" sleep 3000 >/dev/null 2>&1
BR=$(bridge_of "$NET")
# A política entra JÁ: se o `rm` abaixo (o defeito) apagar a rede, é com ela
# posta que apaga — e o check 3 mede se a rede seguinte a herda.
"$BIN" net egress net "$NET" deny >/dev/null 2>&1
before_fw=$(count_fwdeny "$BR")
"$BIN" network rm "$NET" >/dev/null 2>&1; rc=$?
if [[ $rc -eq 5 && $(in_ls "$NET") -eq 1 ]]; then
  pass "network rm com um container ligado recusa (5) e a rede fica"
else
  fail "network rm com um container ligado recusa (5) e a rede fica" "rc=$rc, em ls=$(in_ls "$NET")"
fi

# --- 2. network rm depois de desligado: o holder não guarda nada da bridge ---
"$BIN" container rm -f "c1$PFX" >/dev/null 2>&1
"$BIN" network rm "$NET" >/dev/null 2>&1
if [[ -z "$(pin)" ]]; then
  fail "network rm tira a bridge do dataplane" "o holder caiu — a contagem não mede nada"
else
  left="dlxbr=$(count_dlxbr "$BR") netpair=$(count_netpair "$BR") fwdeny=$(count_fwdeny "$BR")"
  if [[ "$left" == "dlxbr=0 netpair=0 fwdeny=0" && $before_fw -ge 1 ]]; then
    pass "network rm tira a bridge de @dlxbr, @netpair e fwdeny"
  else
    fail "network rm tira a bridge de @dlxbr, @netpair e fwdeny" "$left (fwdeny antes=$before_fw)"
  fi
fi

# --- 3. a mesma rede recriada: DHCP próprio, e nenhuma política herdada -------
"$BIN" network create "$NET" >/dev/null 2>&1
"$BIN" container run -d --name "c2$PFX" --net "$NET" "$IMG" sleep 3000 >/dev/null 2>&1
sleep 2 # o servidor DHCP antigo vê a flag no fim do recv (1 s)
d=$(count_dhcp "$BR"); o=$(count_dhcp_orphan)
if [[ $d -eq 1 && $o -eq 0 ]]; then
  pass "a rede recriada tem o seu servidor DHCP e nenhum ficou preso a uma bridge morta"
else
  fail "a rede recriada tem o seu servidor DHCP e nenhum ficou preso a uma bridge morta" "dhcp=$d órfãos=$o"
fi
f=$(count_fwdeny "$BR")
if [[ $f -eq 0 ]]; then
  pass "a rede recriada não herda o egress deny da anterior"
else
  fail "a rede recriada não herda o egress deny da anterior" "fwdeny=$f com a política em allow"
fi

# --- 4. publish que falha a meio não deixa a primeira porta presa ------------
# A segunda porta é ocupada por um processo do host DEPOIS do preflight e antes
# do publish. É uma corrida, por isso tenta vários atrasos, e só conta a tentativa
# em que foi MESMO o publish que falhou (o erro do `slirp hostfwd`): uma recusa do
# preflight não publicou nada, e passaria este check sem medir o rollback.
caught=""
for delay in 0.3 0.6 1.0 1.5 2.0; do
  P1=$((20000 + RANDOM % 5000)); P2=$((P1 + 5000))
  (sleep "$delay"; exec python3 -m http.server "$P2" --bind 127.0.0.1) >/dev/null 2>&1 &
  PY=$!
  out=$("$BIN" container run -d --name "c3$PFX" --net "$NET" -p "$P1:80" -p "$P2:80" "$IMG" sleep 3000 2>&1); rc=$?
  if [[ $rc -ne 0 && "$out" == *"slirp hostfwd"* ]]; then
    caught="$P1"
    l=$(ss -ltnH "sport = :$P1" | wc -l)
  fi
  kill "$PY" 2>/dev/null; wait "$PY" 2>/dev/null
  "$BIN" container rm -f "c3$PFX" >/dev/null 2>&1
  [[ -n "$caught" ]] && break
done
if [[ -z "$caught" ]]; then
  skip "publish falhado a meio liberta as portas já publicadas" "nenhum atraso apanhou o publish entre o preflight e o slirp"
elif [[ $l -eq 0 ]]; then
  pass "publish falhado a meio liberta as portas já publicadas"
else
  fail "publish falhado a meio liberta as portas já publicadas" ":$caught continua LISTEN no host sem container"
fi

# --- 5. rede criada por uma VM: é uma rede como as outras ---------------------
if [[ -n "${E2E_VM_DISK:-}" ]] && command -v cloud-hypervisor >/dev/null; then
  VN="vn$PFX"
  if "$BIN" vm create "v$PFX" --disk "$E2E_VM_DISK" --memory 512M --vcpus 1 \
      --network "$VN" --backend cloud-hypervisor >/dev/null 2>&1; then
    [[ $(in_ls "$VN") -eq 1 ]] && pass "a rede de uma VM aparece no network ls" \
      || fail "a rede de uma VM aparece no network ls" "não listada"
    "$BIN" network rm "$VN" >/dev/null 2>&1; rc=$?
    [[ $rc -eq 5 ]] && pass "network rm de uma rede com uma VM recusa (5)" \
      || fail "network rm de uma rede com uma VM recusa (5)" "rc=$rc"
    "$BIN" vm rm -f "v$PFX" >/dev/null 2>&1
    "$BIN" network rm "$VN" >/dev/null 2>&1; rc=$?
    [[ $rc -eq 0 && $(in_ls "$VN") -eq 0 ]] && pass "a rede de uma VM remove-se depois da VM" \
      || fail "a rede de uma VM remove-se depois da VM" "rc=$rc, em ls=$(in_ls "$VN")"
  else
    skip "rede de VM" "vm create falhou (hipervisor, imagem ou RAM)"
  fi
else
  skip "rede de VM" "sem E2E_VM_DISK ou sem cloud-hypervisor"
fi

[[ $FAILS -eq 0 ]]
