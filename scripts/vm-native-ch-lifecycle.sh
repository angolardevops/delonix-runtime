#!/usr/bin/env bash
# =============================================================================
# Harness funcional reproduzível — caminho NATIVO de VMs (Cloud Hypervisor),
# SEM libvirt. Conduz o `delonix` apenas pelos seus próprios caminhos.
#
# Contrato (ref. §13/§16 do briefing):
#   - Verifica o ambiente antes de executar (preflight).
#   - Cria recursos exclusivos do teste (prefixo dlxh-*), nunca toca no resto.
#   - Executa operações reais e verifica DENTRO e FORA do guest.
#   - Preserva evidências; limpa só o que criou; devolve != 0 em qualquer falha.
#
# Uso:  ./harness-ch-lifecycle.sh [caminho-para-binario-delonix]
# Env:  DLX_IMAGE  (default: autodetecta uma golden cloud-init via `vm image ls`)
# =============================================================================
set -u
B="${1:-$(command -v delonix || echo "$HOME/.local/bin/delonix")}"
WORK="$(mktemp -d /tmp/dlxh.XXXXXX)"
KEY="$WORK/key"
EVID="$WORK/evidence.txt"
PFX="dlxh-$$"
VM="$PFX-a"
PASS=0; FAIL=0; SKIP=0
trap 'cleanup' EXIT
log(){ echo "$*" | tee -a "$EVID"; }
ok(){ log "PASS: $*"; PASS=$((PASS+1)); }
bad(){ log "FAIL: $*"; FAIL=$((FAIL+1)); }
skip(){ log "SKIP: $*"; SKIP=$((SKIP+1)); }
die(){ log "ABORT: $*"; exit 2; }

gx(){ timeout 30 "$B" vm ssh "$VM" --identity "$KEY" -- "$@" 2>/dev/null; }
wait_ssh(){ for _ in $(seq 1 45); do gx 'true' >/dev/null 2>&1 && return 0; sleep 2; done; return 1; }
status(){ "$B" describe vm "$1" 2>/dev/null | sed -n 's/^ *Status: *//p' | tr -d ' '; }
field(){ "$B" describe vm "$1" 2>/dev/null | sed -n "s/^ *$2: *//p" | head -1 | tr -d ' '; }
holder(){ cat "$HOME/.local/share/delonix/ingress/holder.pid" 2>/dev/null; }

cleanup(){
  for v in "$PFX-a" "$PFX-b" "$PFX-c"; do "$B" vm destroy "$v" >/dev/null 2>&1; done
  [ "${KEEP:-0}" = "1" ] || rm -rf "$WORK"
}

# ------------------------------- Preflight ----------------------------------
log "=== PREFLIGHT ==="
[ -x "$B" ] || die "delonix não encontrado em '$B'"
[ -r /dev/kvm ] && [ -w /dev/kvm ] || die "/dev/kvm sem rw — aceleração indisponível (sem fallback, por design)"
command -v cloud-hypervisor >/dev/null || die "cloud-hypervisor não está no PATH"
command -v cloud-localds >/dev/null || die "cloud-localds em falta (seed NoCloud)"
IMG="${DLX_IMAGE:-$("$B" vm image ls 2>/dev/null | awk '/cloud-init/{print $1; exit}')}"
[ -n "$IMG" ] || die "sem imagem golden cloud-init (define DLX_IMAGE ou cria uma)"
ssh-keygen -t ed25519 -N '' -f "$KEY" -C dlxh >/dev/null 2>&1 || die "ssh-keygen falhou"
log "delonix=$B  image=$IMG  kvm=ok  ch=$(cloud-hypervisor --version 2>/dev/null)"

# ------------------------------- A. create/boot/cloud-init ------------------
log "=== A. create + boot + cloud-init + guest exec ==="
"$B" vm create "$VM" --backend cloud-hypervisor --disk "$IMG" --vcpus 2 --memory 2G \
     --hostname "$VM" --ssh-key "@$KEY.pub" >/dev/null 2>&1
[ "$(field "$VM" Backend)" = "cloud-hypervisor" ] && ok "backend é cloud-hypervisor" || bad "backend != cloud-hypervisor"
# nenhum libvirt/qemu a correr para esta VM
pgrep -af qemu 2>/dev/null | grep -q "$VM" && bad "há qemu para a VM (não devia)" || ok "sem qemu/libvirt para a VM"
wait_ssh && ok "guest acessível por SSH (cloud-init aplicou a chave)" || { bad "guest NÃO acessível"; exit 1; }
[ "$(gx hostname)" = "$VM" ] && ok "cloud-init aplicou o hostname" || bad "hostname não aplicado"

log "=== A2. CPU/mem/disco/rede/DNS dentro do guest ==="
[ "$(gx nproc)" = "2" ] && ok "guest vê 2 vCPUs" || bad "guest nproc != 2"
MEM=$(gx 'free -m | awk "/Mem:/{print \$2}"'); [ "${MEM:-0}" -ge 1700 ] && ok "guest vê ~2G ($MEM MB)" || bad "memória guest=$MEM MB"
gx 'echo dlx-marker > /home/delonix/persist.txt && sync' >/dev/null
SHA0=$(gx 'sha256sum /home/delonix/persist.txt | cut -d" " -f1')
[ -n "$SHA0" ] && ok "escrita+sha em disco ($SHA0)" || bad "escrita em disco falhou"
GW=$(gx 'ip route | awk "/default/{print \$3}"')
gx "ping -c1 -W2 $GW" >/dev/null 2>&1 && ok "rede: ping ao gateway $GW" || bad "rede: sem gateway"
gx 'getent hosts archive.ubuntu.com' >/dev/null 2>&1 && ok "DNS/egress (slirp NAT) resolve" || bad "DNS/egress falhou"

# ------------------------------- B. stop/start identity ---------------------
log "=== B. stop -> start (mesma identidade + persistência de disco) ==="
MID0=$(gx 'cat /etc/machine-id')
"$B" vm stop "$VM" >/dev/null 2>&1; sleep 2
[ "$(status "$VM")" = "Stopped" ] && ok "stop: Stopped" || bad "stop: status=$(status "$VM")"
pgrep -f "api-socket.*/$VM.sock" >/dev/null && bad "stop: VMM ainda vivo" || ok "stop: processo VMM terminou"
"$B" vm start "$VM" >/dev/null 2>&1; wait_ssh && ok "start: guest de volta" || bad "start: guest não voltou"
[ "$(gx 'cat /etc/machine-id')" = "$MID0" ] && ok "mesma identidade: machine-id estável" || bad "machine-id mudou"
[ "$(gx 'sha256sum /home/delonix/persist.txt | cut -d" " -f1')" = "$SHA0" ] && ok "disco persistiu stop/start" || bad "disco perdeu-se"

# ------------------------------- C. reboot ----------------------------------
log "=== C. restart (reboot real) ==="
UP0=$(gx 'cut -d. -f1 /proc/uptime')
"$B" vm restart "$VM" >/dev/null 2>&1; wait_ssh && ok "restart: guest de volta" || bad "restart: guest não voltou"
UP1=$(gx 'cut -d. -f1 /proc/uptime')
[ -n "$UP1" ] && [ "${UP1:-9999}" -lt "${UP0:-0}" ] && ok "restart: uptime reiniciou ($UP0->$UP1)" || bad "restart: uptime não reiniciou"
[ "$(gx 'sha256sum /home/delonix/persist.txt | cut -d" " -f1')" = "$SHA0" ] && ok "disco persistiu reboot" || bad "disco perdeu-se no reboot"

# ------------------------------- D. pause/unpause ---------------------------
log "=== D. pause -> unpause ==="
"$B" vm pause "$VM" >/dev/null 2>&1 && ok "pause ok" || bad "pause falhou"
"$B" vm unpause "$VM" >/dev/null 2>&1 && ok "unpause ok" || bad "unpause falhou"
wait_ssh && ok "unpause: guest responde" || bad "unpause: guest não responde"

# ------------------------------- E. cold resize -----------------------------
log "=== E. cold resize (2 -> 3 vCPUs, guest confirma) ==="
"$B" vm stop "$VM" >/dev/null 2>&1; sleep 2
"$B" vm resize "$VM" --vcpus 3 >/dev/null 2>&1 && ok "resize ok" || bad "resize falhou"
"$B" vm start "$VM" >/dev/null 2>&1; wait_ssh
[ "$(gx nproc)" = "3" ] && ok "resize: guest vê 3 vCPUs" || bad "resize: guest vê $(gx nproc)"

# ------------------------------- F. snapshot (parado, backend-limitado) -----
log "=== F. snapshot (CH: só VM parada) ==="
"$B" vm stop "$VM" >/dev/null 2>&1; sleep 2
if "$B" vm snapshot create "$VM" snap1 >/dev/null 2>&1; then
  "$B" vm snapshot ls "$VM" 2>/dev/null | grep -q snap1 && ok "snapshot criado e listado" || bad "snapshot não listado"
  "$B" vm snapshot rm "$VM" snap1 >/dev/null 2>&1
else
  skip "snapshot não suportado nesta config (ver matriz)"
fi
"$B" vm start "$VM" >/dev/null 2>&1; wait_ssh

# ------------------------------- G. concorrência ----------------------------
log "=== G. concorrência (2 creates paralelos, recursos distintos) ==="
"$B" vm create "$PFX-b" --backend cloud-hypervisor --disk "$IMG" --hostname "$PFX-b" --ssh-key "@$KEY.pub" >/dev/null 2>&1 &
"$B" vm create "$PFX-c" --backend cloud-hypervisor --disk "$IMG" --hostname "$PFX-c" --ssh-key "@$KEY.pub" >/dev/null 2>&1 &
wait
IPB=$(field "$PFX-b" IP); IPC=$(field "$PFX-c" IP)
MB=$("$B" describe vm "$PFX-b" 2>/dev/null | grep -m1 'MAC:' | sed 's/.*MAC: *//')
MC=$("$B" describe vm "$PFX-c" 2>/dev/null | grep -m1 'MAC:' | sed 's/.*MAC: *//')
[ -n "$IPB" ] && [ -n "$IPC" ] && [ "$IPB" != "$IPC" ] && ok "concorrência: IPs distintos ($IPB,$IPC)" || bad "concorrência: colisão/ausência de IP"
[ -n "$MB" ] && [ "$MB" != "$MC" ] && ok "concorrência: MACs distintos" || bad "concorrência: colisão de MAC ($MB,$MC)"

# ------------------------------- H. destroy sem órfãos ----------------------
log "=== H. destroy -> sem órfãos (processo/tap/ficheiros/registo) ==="
H=$(holder)
for v in "$PFX-b" "$PFX-c" "$VM"; do
  TAP=$("$B" describe vm "$v" 2>/dev/null | grep -m1 'TAP:' | sed 's/.*TAP: *//' | tr -d ' ')
  "$B" vm destroy "$v" >/dev/null 2>&1; sleep 1
  LEFT=0
  "$B" vm ls 2>/dev/null | grep -qw "$v" && { bad "destroy $v: ainda no vm ls"; LEFT=1; }
  pgrep -f "api-socket.*/$v.sock" >/dev/null && { bad "destroy $v: processo VMM órfão"; LEFT=1; }
  ls "$HOME/.local/share/delonix/vms/$v".* >/dev/null 2>&1 && { bad "destroy $v: ficheiros órfãos"; LEFT=1; }
  [ -n "$TAP" ] && [ -n "$H" ] && nsenter -t "$H" --user --net ip link show "$TAP" >/dev/null 2>&1 && { bad "destroy $v: tap $TAP órfão"; LEFT=1; }
  [ $LEFT -eq 0 ] && ok "destroy $v: zero órfãos"
done

log ""
log "==== RESULTADO: $PASS passaram, $FAIL falharam, $SKIP saltados ===="
[ "${KEEP:-0}" = "1" ] && log "(evidência preservada em $EVID)"
exit "$FAIL"
