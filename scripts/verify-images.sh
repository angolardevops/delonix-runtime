#!/bin/bash
# Verify the recipes under images/ by BUILDING them and looking at what came out.
#
#   scripts/verify-images.sh                       # the 4 cloud distros, offline builds
#   scripts/verify-images.sh --packages --profile  # + package install + golden recipe (needs network)
#   scripts/verify-images.sh --boot                # + boot each image and log in (needs KVM)
#   scripts/verify-images.sh --appliance opnsense  # one appliance (heavy: disk, RAM, download)
#   scripts/verify-images.sh --self-test           # prove the checks CAN fail
#   scripts/verify-images.sh --self-test --no-build --seed-bases   # only that, no builds
#   scripts/verify-images.sh --dry-run             # print the plan and the preflight, build nothing
#
# Why it exists: `vm build` returning 0 proves the builder ran, not that the
# image is right. Every check below reads the CONTENT of the qcow2 (or of the
# booted VM) and compares it with what images/<distro>/vm.yaml declared.
# A check that cannot fail is not a check, so `--self-test` runs the same
# inspection on an image nobody customised and requires it to fail.
#
# Nothing here touches your real image store: everything runs under an isolated
# DELONIX_ROOT that is removed at the end (`--keep` to inspect it).
#
# Exit: 0 all executed checks passed, 1 at least one FAILED, 2 preflight failed.
# SKIPPED is reported, never counted as passed.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/.." && pwd)
DELONIX=${DELONIX:-delonix}
VDIR=${VERIFY_DIR:-$HOME/.cache/delonix-verify}
ONLY="ubuntu,debian,rocky,fedora"
APPLIANCES=()
DO_PACKAGES=0 DO_PROFILE=0 DO_BOOT=0 DO_SELFTEST=0 DRY=0 KEEP=0 SEED=0 NO_BUILD=0
MIN_FREE_GB=${MIN_FREE_GB:-12}
SEED_FROM=${SEED_FROM:-${DELONIX_ROOT:-$HOME/.local/share/delonix}}
BOOT_TIMEOUT=${BOOT_TIMEOUT:-300}

usage() { sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }
while [ $# -gt 0 ]; do
  case "$1" in
    --only) ONLY=$2; shift ;;
    --packages) DO_PACKAGES=1 ;;
    --profile) DO_PROFILE=1 ;;
    --boot) DO_BOOT=1 ;;
    --appliance) APPLIANCES+=("$2"); shift ;;
    --self-test) DO_SELFTEST=1 ;;
    --no-build) NO_BUILD=1 ;;
    --seed-bases) SEED=1 ;;
    --dry-run) DRY=1 ;;
    --keep) KEEP=1 ;;
    --delonix) DELONIX=$2; shift ;;
    --dir) VDIR=$2; shift ;;
    --min-free-gb) MIN_FREE_GB=$2; shift ;;
    -h|--help) usage 0 ;;
    *) echo "unknown option: $1" >&2; usage 2 ;;
  esac
  shift
done

# ── recipe facts the checks compare against ────────────────────────────────
# distro : release : family : admin group : packages the recipe REMOVES : base image (local store name)
declare -A REL=( [ubuntu]=26.04 [debian]=bookworm [rocky]=9 [fedora]=42 )
declare -A FAM=( [ubuntu]=dpkg [debian]=dpkg [rocky]=rpm [fedora]=rpm )
declare -A GRP=( [ubuntu]=adm [debian]=adm [rocky]=wheel [fedora]=wheel )
declare -A GONE=( [ubuntu]="snapd" [debian]="unattended-upgrades" [rocky]="cockpit-ws cockpit-bridge cockpit-system cockpit-ws-selinux" [fedora]="" )
declare -A BASE=( [ubuntu]=delonix-vm-base_ubuntu-26.04 [debian]=delonix-vm-base_debian-bookworm [rocky]=delonix-vm-base_rocky-9 [fedora]=delonix-vm-base_fedora-42 )
# The golden recipe's own release format (Fedora wants release AND build).
declare -A GOLDEN_REL=( [ubuntu]=26.04 [debian]=bookworm [rocky]=9 [fedora]=42-1.1 )
# appliance : min RAM GiB : min free disk GiB : needs KVM (1/0) : --target : verifier
declare -A APP=(
  [opnsense]="0:4:0::"
  [proxmox]="4:20:1:pve:boot"
  [truenas]="6:20:1::boot"
  [openstack]="4:65:1::"
  [monitoring]="4:20:1::monitoring"
  [carbonio]="6:50:1::carbonio"
  [glpi]="4:12:1::glpi"
  [wazuh]="6:30:1::wazuh"
)

# ── result bookkeeping ─────────────────────────────────────────────────────
PASS=0 FAIL=0 SKIP=0
RESULTS=()
ok()   { PASS=$((PASS+1)); RESULTS+=("PASS|$1|"); printf '    ✓ %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); RESULTS+=("FAIL|$1|$2"); printf '    ✗ %s — %s\n' "$1" "$2"; }
skip() { SKIP=$((SKIP+1)); RESULTS+=("SKIP|$1|$2"); printf '    - %s (skipped: %s)\n' "$1" "$2"; }
eq()   { # eq <name> <got> <want>
  if [ "$2" = "$3" ]; then ok "$1"; else bad "$1" "got '${2:-<empty>}', want '$3'"; fi
}
say()  { printf '\n== %s\n' "$*"; }

# ── isolation ──────────────────────────────────────────────────────────────
ROOT="$VDIR/root"
export LIBGUESTFS_BACKEND=direct
CREATED_VMS=()
cleanup() {
  local v
  for v in "${CREATED_VMS[@]:-}"; do
    [ -n "$v" ] && DELONIX_ROOT="$ROOT" "$DELONIX" vm rm --force "$v" >/dev/null 2>&1
  done
  if [ "$KEEP" = 0 ] && [ -d "$ROOT" ] && [ "${DRY}" = 0 ]; then rm -rf "$ROOT"; fi
}
trap cleanup EXIT
dl() { DELONIX_ROOT="$ROOT" "$DELONIX" "$@"; }

# ── preflight ──────────────────────────────────────────────────────────────
PRE_FAIL=0
need_cmd() { command -v "$1" >/dev/null 2>&1 && printf '  ok   %s\n' "$1" || { printf '  MISSING %s — %s\n' "$1" "$2"; PRE_FAIL=1; }; }
free_gb()  { df -Pk "$1" 2>/dev/null | awk 'NR==2{printf "%d", $4/1024/1024}'; }
mem_gb()   { awk '/MemAvailable/{printf "%d", $2/1024/1024}' /proc/meminfo; }

preflight() {
  say "preflight"
  mkdir -p "$VDIR"
  printf '  delonix  %s (%s)\n' "$("$DELONIX" version 2>&1 | head -1)" "$(command -v "$DELONIX" || echo not-found)"
  need_cmd "$DELONIX" "build it: cargo build -p delonix-runtime-bin, then --delonix target/debug/delonix"
  need_cmd qemu-img "sudo apt install qemu-utils"
  need_cmd virt-customize "sudo apt install libguestfs-tools"
  need_cmd guestfish "sudo apt install libguestfs-tools"
  need_cmd sha256sum "coreutils"
  need_cmd python3 "needed to read 'vm ls -o json'"
  local free; free=$(free_gb "$VDIR")
  printf '  free disk in %s: %s GiB (need >= %s)\n' "$VDIR" "${free:-?}" "$MIN_FREE_GB"
  if [ -z "$free" ] || [ "$free" -lt "$MIN_FREE_GB" ]; then echo "  NOT ENOUGH DISK"; PRE_FAIL=1; fi
  printf '  free RAM: %s GiB\n' "$(mem_gb)"
  if [ "$DO_BOOT" = 1 ] || [ "${#APPLIANCES[@]}" -gt 0 ]; then
    if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then echo "  ok   /dev/kvm"; else echo "  MISSING /dev/kvm (rw) — boot and appliance stages need KVM"; PRE_FAIL=1; fi
  fi
  if [ "$DO_BOOT" = 1 ]; then need_cmd ssh "openssh-client"; need_cmd ssh-keygen "openssh-client"; fi
  if [ "${#APPLIANCES[@]}" -gt 0 ]; then need_cmd qemu-system-x86_64 "sudo apt install qemu-system-x86"; need_cmd xorriso "needed by the Proxmox/OpenStack builders"; fi
  if [ "$DO_PACKAGES" = 1 ] || [ "$DO_PROFILE" = 1 ]; then
    if curl -fsS -m 8 -o /dev/null https://cloud-images.ubuntu.com/ 2>/dev/null; then echo "  ok   network"; else echo "  NO NETWORK — --packages/--profile install software from the internet"; PRE_FAIL=1; fi
  fi
  if [ "$PRE_FAIL" = 1 ]; then echo; echo "preflight FAILED — nothing was built"; exit 2; fi
}

# ── reading an image ───────────────────────────────────────────────────────
# ONE guestfish session per image, running a script inside the guest that
# prints key=value lines. Booting the libguestfs appliance costs seconds; doing
# it per check made a verification take minutes.
PROBE=""
probe() { # probe <qcow2> <script>
  PROBE=$(guestfish --ro -a "$1" -i sh "$2" 2>/dev/null)
}
pget() { printf '%s\n' "$PROBE" | sed -n "s/^$1=//p" | head -1; }

# The script that runs INSIDE the guest. Everything printed is a fact; nothing
# here decides pass/fail.
guest_script() { # guest_script <family> <grp> "<gone pkgs>"
  local fam=$1 grp=$2 gone=$3
  cat <<EOF
echo motd_sha=\$(sha256sum /etc/motd 2>/dev/null | cut -d' ' -f1)
echo motd_mode=\$(stat -c %a /etc/motd 2>/dev/null)
echo hostname=\$(cat /etc/hostname 2>/dev/null)
echo env=\$(grep '^DELONIX_IMAGE=' /etc/environment 2>/dev/null)
echo sudoers=\$(cat /etc/sudoers.d/90-delonix 2>/dev/null)
echo group=\$(getent group $grp 2>/dev/null)
echo machine_id_bytes=\$(wc -c < /etc/machine-id 2>/dev/null)
echo staging=\$(ls -A /tmp 2>/dev/null | grep -c '^\\.delonix-files')
echo man=\$([ -e /usr/share/man ] && echo present || echo absent)
echo doc=\$([ -d /usr/share/doc ] && echo present || echo absent)
echo bash_history=\$(ls /root/.bash_history /home/*/.bash_history 2>/dev/null | wc -l)
echo motd_label=\$(ls -Z /etc/motd 2>/dev/null)
echo user_shell=\$(getent passwd delonix | cut -d: -f7)
for p in $gone; do
  if [ "$fam" = dpkg ]; then
    s=\$(dpkg-query -W -f='\${Status}' \$p 2>/dev/null)
    case "\$s" in *"install ok installed"*) echo "pkg_\$p=present";; *) echo "pkg_\$p=absent";; esac
  else
    if rpm -q \$p >/dev/null 2>&1; then echo "pkg_\$p=present"; else echo "pkg_\$p=absent"; fi
  fi
done
if [ "$fam" = dpkg ]; then echo pkg_cache=\$(ls /var/lib/apt/lists 2>/dev/null | grep -vc '^lock\$\|^partial\$'); fi
EOF
}

# The content checks for a CUSTOM image (images/<distro>/vm.yaml).
check_custom() { # check_custom <distro> <qcow2>
  local d=$1 q=$2 dir="$REPO/images/$1"
  local want_host want_sha
  want_host=$(awk '/^    hostname:/{print $2; exit}' "$dir/vm.yaml")
  want_sha=$(sha256sum "$dir/artifacts/motd" | cut -d' ' -f1)
  probe "$q" "$(guest_script "${FAM[$d]}" "${GRP[$d]}" "${GONE[$d]}")"
  if [ -z "$PROBE" ]; then bad "$d: read the image" "guestfish returned nothing (is the qcow2 valid?)"; return; fi
  eq "$d: /etc/motd is the artifact, byte for byte" "$(pget motd_sha)" "$want_sha"
  eq "$d: /etc/motd mode (files.mode)" "$(pget motd_mode)" "644"
  eq "$d: hostname (hostname:)" "$(pget hostname)" "$want_host"
  eq "$d: env DELONIX_IMAGE (env:)" "$(pget env)" "DELONIX_IMAGE=$d"
  eq "$d: sudoers for the delonix user (users.sudo)" "$(pget sudoers)" "delonix ALL=(ALL) NOPASSWD:ALL"
  case "$(pget group)" in *delonix*) ok "$d: delonix is in ${GRP[$d]} (users.groups)";; *) bad "$d: delonix is in ${GRP[$d]}" "group line was '$(pget group)'";; esac
  eq "$d: shell of delonix (users.shell)" "$(pget user_shell)" "/bin/bash"
  eq "$d: /etc/machine-id emptied (cleanup.machine_id)" "$(pget machine_id_bytes)" "0"
  eq "$d: staging directory removed (files:)" "$(pget staging)" "0"
  eq "$d: /usr/share/man removed (remove.paths)" "$(pget man)" "absent"
  eq "$d: /usr/share/doc kept (licence files)" "$(pget doc)" "present"
  eq "$d: shell history emptied (cleanup.history)" "$(pget bash_history)" "0"
  local p
  for p in ${GONE[$d]}; do eq "$d: package $p removed (remove.packages)" "$(pget "pkg_$p")" "absent"; done
  if [ "${FAM[$d]}" = dpkg ]; then eq "$d: apt lists emptied (cleanup.package_cache)" "$(pget pkg_cache)" "0"; fi
  case "$d" in
    rocky|fedora)
      case "$(pget motd_label)" in
        *unlabeled_t*|"") bad "$d: SELinux label on /etc/motd" "label was '$(pget motd_label)' — the disk was not relabelled" ;;
        *) ok "$d: SELinux label on /etc/motd is set" ;;
      esac ;;
  esac
}

# What the store says about an image, from the record `vm build` wrote.
check_record() { # check_record <name> <cloud-init yes|no> <distro-substr> <vcpus> <mem>
  local out ci; out=$(dl image vm describe "$1" 2>&1)
  ci=$(printf '%s\n' "$out" | sed -n 's/^Cloud-init: *//p' | head -1)
  case "$ci" in "$2"*) ok "$1: record says cloud-init $2";; *) bad "$1: record says cloud-init $2" "the Cloud-init line was '${ci:-<none>}'";; esac
  if [ -n "$3" ]; then
    local dist; dist=$(printf '%s\n' "$out" | sed -n 's/^Distro: *//p' | head -1)
    case "$dist" in *"$3"*) ok "$1: record distro contains '$3'";; *) bad "$1: record distro" "the Distro line was '${dist:-<none>}'";; esac
  fi
  if [ -n "$4" ]; then eq "$1: default vCPUs recorded" "$(printf '%s\n' "$out" | sed -n 's/^Default vCPUs: *//p' | head -1)" "$4"; fi
  if [ -n "$5" ]; then eq "$1: default memory recorded" "$(printf '%s\n' "$out" | sed -n 's/^Default memory: *//p' | head -1)" "$5"; fi
}

# ── building ───────────────────────────────────────────────────────────────
qcow2_of() { echo "$ROOT/vm-images/$1.qcow2"; }
seed_bases() {
  [ "$SEED" = 1 ] || return 0
  mkdir -p "$ROOT/vm-images"
  local d f
  for d in ${ONLY//,/ }; do
    f="${BASE[$d]:-}"; [ -n "$f" ] || continue
    if [ -f "$SEED_FROM/vm-images/$f.qcow2" ]; then
      cp "$SEED_FROM/vm-images/$f.json" "$SEED_FROM/vm-images/$f.qcow2" "$ROOT/vm-images/" && echo "  seeded base $f"
    else echo "  no local base $f in $SEED_FROM (it will be downloaded)"; fi
  done
}

build() { # build <label> <args...>
  local label=$1; shift
  printf '  building %s ...\n' "$label"
  local log="$VDIR/build-$label.log"
  if dl vm build "$@" >"$log" 2>&1; then ok "$label: vm build finished"; return 0; fi
  bad "$label: vm build finished" "see $log — $(tail -2 "$log" | tr '\n' ' ' | cut -c1-200)"
  return 1
}

# The shipped recipes cannot prove every field: the base image already has a
# `delonix` account with sudo, in the admin group, with /bin/bash, so checking
# those on images/<distro>/vm.yaml passes even if `users:` did nothing. The
# PROBE recipe exercises each field with a value the base cannot have.
probe_yaml() { # probe_yaml <distro> <dir>
  local d=$1 dir=$2
  mkdir -p "$dir/artifacts"
  echo "probe artifact for $d" >"$dir/artifacts/probe.txt"
  cat >"$dir/vm.yaml" <<YAML
images:
  probe:
    tag: verify-probe-$d
    distro: $d
    release: "${REL[$d]}"
    hostname: probe-host
    env: {PROBE_VAR: probe-value}
    files:
      - {src: artifacts/probe.txt, dst: /opt/verify/probe.txt, mode: "0600"}
    users:
      - {name: probeuser, sudo: true, groups: [${GRP[$d]}], shell: /bin/sh}
    run:
      - mkdir -p /opt/verify && echo keep > /opt/verify/keep.txt && echo gone > /opt/verify/gone.txt
    remove:
      paths: [/opt/verify/gone.txt]
YAML
}

check_probe() { # check_probe <distro> <qcow2> <artifact-dir>
  local d=$1 q=$2 art=$3
  probe "$q" "
echo p_sha=\$(sha256sum /opt/verify/probe.txt 2>/dev/null | cut -d' ' -f1)
echo p_mode=\$(stat -c %a /opt/verify/probe.txt 2>/dev/null)
echo p_host=\$(cat /etc/hostname 2>/dev/null)
echo p_env=\$(grep '^PROBE_VAR=' /etc/environment 2>/dev/null)
echo p_sudo=\$(cat /etc/sudoers.d/90-probeuser 2>/dev/null)
echo p_group=\$(getent group ${GRP[$d]} 2>/dev/null)
echo p_shell=\$(getent passwd probeuser 2>/dev/null | cut -d: -f7)
echo p_keep=\$([ -f /opt/verify/keep.txt ] && echo present || echo absent)
echo p_gone=\$([ -e /opt/verify/gone.txt ] && echo present || echo absent)
echo p_stage=\$(ls -A /tmp 2>/dev/null | grep -c '^\\.delonix-files')"
  if [ -z "$PROBE" ]; then bad "probe $d: read the image" "guestfish returned nothing"; return; fi
  eq "probe $d: files: copied to its full destination" "$(pget p_sha)" "$(sha256sum "$art/artifacts/probe.txt" | cut -d' ' -f1)"
  eq "probe $d: files.mode applied" "$(pget p_mode)" "600"
  eq "probe $d: hostname:" "$(pget p_host)" "probe-host"
  eq "probe $d: env:" "$(pget p_env)" "PROBE_VAR=probe-value"
  eq "probe $d: users.sudo" "$(pget p_sudo)" "probeuser ALL=(ALL) NOPASSWD:ALL"
  case "$(pget p_group)" in *probeuser*) ok "probe $d: users.groups (probeuser in ${GRP[$d]})";; *) bad "probe $d: users.groups (probeuser in ${GRP[$d]})" "group line was '$(pget p_group)'";; esac
  eq "probe $d: users.shell" "$(pget p_shell)" "/bin/sh"
  eq "probe $d: run: executed (keep.txt exists)" "$(pget p_keep)" "present"
  eq "probe $d: remove.paths ran after run: (gone.txt removed)" "$(pget p_gone)" "absent"
  eq "probe $d: staging directory removed" "$(pget p_stage)" "0"
}

stage_probe() {
  local d
  for d in ${ONLY//,/ }; do
    [ -n "${REL[$d]:-}" ] || continue
    say "$d (probe recipe: every field, values the base cannot have)"
    local pd="$VDIR/probe-$d"; probe_yaml "$d" "$pd"
    build "probe-$d" -f "$pd/vm.yaml" || continue
    check_probe "$d" "$(qcow2_of "verify-probe-$d")" "$pd"
  done
}

stage_custom() {
  local d
  for d in ${ONLY//,/ }; do
    [ -d "$REPO/images/$d" ] && [ -n "${REL[$d]:-}" ] || { skip "$d" "no such recipe"; continue; }
    say "$d (custom recipe, offline)"
    local rel=${REL[$d]}
    build "$d" -f "$REPO/images/$d/vm.yaml" -t "$rel" || continue
    [ -f "$(qcow2_of "$rel")" ] && ok "$d: image is in the store" || { bad "$d: image is in the store" "no $(qcow2_of "$rel")"; continue; }
    check_record "$rel" yes "$d" 2 2G
    check_custom "$d" "$(qcow2_of "$rel")"
  done
}

stage_packages() {
  local d
  for d in ${ONLY//,/ }; do
    [ -n "${REL[$d]:-}" ] || continue
    say "$d (packages.install, needs network)"
    local y="$VDIR/pkg-$d.yaml" tag="verify-pkg-$d"
    cat >"$y" <<EOF
images:
  pkg:
    tag: $tag
    distro: $d
    release: "${REL[$d]}"
    network: true
    packages: {install: [jq]}
EOF
    build "pkg-$d" -f "$y" || continue
    probe "$(qcow2_of "$tag")" "if command -v jq >/dev/null; then echo jq=present; else echo jq=absent; fi; echo jqver=\$(jq --version 2>/dev/null)"
    eq "$d: jq installed by packages.install" "$(pget jq)" "present"
    case "$(pget jqver)" in jq-*) ok "$d: jq runs inside the guest";; *) bad "$d: jq runs inside the guest" "got '$(pget jqver)'";; esac
  done
}

stage_profile() {
  local d
  for d in ${ONLY//,/ }; do
    [ -n "${REL[$d]:-}" ] || continue
    say "$d (profile: rootless, the golden recipe, needs network)"
    local y="$VDIR/gold-$d.yaml" tag="verify-gold-$d"
    cat >"$y" <<EOF
images:
  gold:
    tag: $tag
    profile: rootless
    distro: $d
    release: "${GOLDEN_REL[$d]}"
EOF
    build "gold-$d" -f "$y" || continue
    probe "$(qcow2_of "$tag")" "echo rel=\$(test -s /etc/delonix-image-release && echo present || echo absent); echo bin=\$(find /usr -name delonix -type f 2>/dev/null | head -1); echo locked=\$(grep -c '^delonix:[!*]' /etc/shadow)"
    eq "$d: /etc/delonix-image-release written" "$(pget rel)" "present"
    [ -n "$(pget bin)" ] && ok "$d: the delonix binary is in the image ($(pget bin))" || bad "$d: the delonix binary is in the image" "not found under /usr"
    eq "$d: the delonix account has no password" "$(pget locked)" "1"
  done
}

# ── booting ────────────────────────────────────────────────────────────────
vm_ip() { # vm_ip <name>
  dl vm ls -o json 2>/dev/null | python3 -c '
import json,sys
name=sys.argv[1]
try: rows=json.load(sys.stdin)
except Exception: sys.exit(0)
rows = rows if isinstance(rows,list) else rows.get("items",rows.get("vms",[]))
for r in rows:
    if r.get("name")==name:
        for k,v in r.items():
            if "ip" in k.lower() and isinstance(v,str) and v[:1].isdigit(): print(v); sys.exit(0)
' "$1"
}
stage_boot() {
  local d key="$VDIR/id_verify"
  [ -f "$key" ] || ssh-keygen -q -t ed25519 -N '' -f "$key" -C delonix-verify
  for d in ${ONLY//,/ }; do
    [ -f "$(qcow2_of "${REL[$d]:-x}")" ] || { skip "$d: boot" "image not built"; continue; }
    say "$d (boot and log in)"
    local name="verify-$d-$$"; CREATED_VMS+=("$name")
    if ! dl vm create "$name" --disk "${REL[$d]}" --ssh-key "@$key.pub" --wait --boot-timeout "$BOOT_TIMEOUT" >"$VDIR/boot-$d.log" 2>&1; then
      bad "$d: vm create --wait" "see $VDIR/boot-$d.log — $(tail -1 "$VDIR/boot-$d.log" | cut -c1-160)"; continue
    fi
    ok "$d: vm create --wait returned"
    local ip; ip=$(vm_ip "$name")
    [ -n "$ip" ] && ok "$d: the VM has an address ($ip)" || { bad "$d: the VM has an address" "not in 'vm ls -o json'"; continue; }
    local out i=0
    while [ $i -lt 30 ]; do
      out=$(ssh -i "$key" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o ConnectTimeout=5 "delonix@$ip" '
        echo user=$(id -un)
        echo sudo=$(sudo -n true 2>/dev/null && echo ok || echo no)
        echo motd_sha=$(sha256sum /etc/motd | cut -d" " -f1)
        echo machine_id_bytes=$(wc -c </etc/machine-id)
        echo failed_units=$(systemctl --failed --no-legend 2>/dev/null | wc -l)
        echo selinux=$(getenforce 2>/dev/null || echo none)' 2>/dev/null) && break
      i=$((i+1)); sleep 5
    done
    if [ -z "$out" ]; then bad "$d: ssh as delonix" "no login after 150 s"; continue; fi
    ok "$d: ssh as delonix with the injected key"
    g() { printf '%s\n' "$out" | sed -n "s/^$1=//p" | head -1; }
    eq "$d: sudo works without a password" "$(g sudo)" "ok"
    eq "$d: /etc/motd survived the boot" "$(g motd_sha)" "$(sha256sum "$REPO/images/$d/artifacts/motd" | cut -d' ' -f1)"
    [ "$(g machine_id_bytes)" -gt 0 ] 2>/dev/null && ok "$d: a fresh machine-id was generated at boot" || bad "$d: a fresh machine-id was generated at boot" "size '$(g machine_id_bytes)'"
    eq "$d: no failed systemd units" "$(g failed_units)" "0"
    case "$d" in rocky|fedora) eq "$d: SELinux is enforcing" "$(g selinux)" "Enforcing";; esac
    dl vm rm --force "$name" >/dev/null 2>&1
  done
}

# ── appliances ─────────────────────────────────────────────────────────────
stage_appliance() { # stage_appliance <name>
  local n=$1 spec=${APP[$1]:-}
  [ -n "$spec" ] || { skip "appliance $n" "unknown (known: ${!APP[*]})"; return; }
  IFS=: read -r min_ram min_disk kvm target verifier <<<"$spec"
  say "appliance $n"
  [ -f "$REPO/images/$n/vm.yaml" ] || { bad "appliance $n" "no images/$n/vm.yaml"; return; }
  local ram disk; ram=$(mem_gb); disk=$(free_gb "$VDIR")
  if [ "$ram" -lt "$min_ram" ] || [ "$disk" -lt "$min_disk" ]; then
    skip "appliance $n" "needs ${min_ram} GiB RAM and ${min_disk} GiB free; this host has ${ram}/${disk}"; return
  fi
  local before after
  before=$(dl image vm ls -o json 2>/dev/null | python3 -c 'import json,sys
try: print("\n".join(sorted(str(r.get("name")) for r in json.load(sys.stdin))))
except Exception: pass')
  build "appliance-$n" -f "$REPO/images/$n/vm.yaml" ${target:+--target "$target"} || return
  after=$(dl image vm ls -o json 2>/dev/null | python3 -c 'import json,sys
try: print("\n".join(sorted(str(r.get("name")) for r in json.load(sys.stdin))))
except Exception: pass')
  local new; new=$(comm -13 <(printf '%s\n' "$before") <(printf '%s\n' "$after") | head -1)
  [ -n "$new" ] && ok "appliance $n: a new image was registered ($new)" || { bad "appliance $n: a new image was registered" "the store did not change"; return; }
  local ci=no; case "$n" in openstack|monitoring|carbonio|glpi|wazuh) ci=yes;; esac
  check_record "$new" "$ci" "" "" ""
  [ -f "$(qcow2_of "$new")" ] && ok "appliance $n: the qcow2 is in the store" || bad "appliance $n: the qcow2 is in the store" "missing"
  case "$verifier" in
    monitoring|carbonio|glpi|wazuh)
      if "$REPO/scripts/appliances/verify-$verifier.sh" "$(qcow2_of "$new")" >"$VDIR/verify-$n.log" 2>&1; then ok "appliance $n: scripts/appliances/verify-$verifier.sh passed"
      else bad "appliance $n: verify-$verifier.sh" "see $VDIR/verify-$n.log — $(tail -1 "$VDIR/verify-$n.log" | cut -c1-160)"; fi ;;
    boot)
      # verify-boot.sh finds images by name stem in OUT_DIR, so link ours under it.
      local od="$VDIR/appl-$n" stem=$n
      [ "$n" = proxmox ] && stem=$target
      mkdir -p "$od"; ln -sf "$(qcow2_of "$new")" "$od/$stem-verify.qcow2"
      if OUT_DIR="$od" "$REPO/scripts/appliances/verify-boot.sh" >"$VDIR/verify-$n.log" 2>&1; then ok "appliance $n: verify-boot.sh — it SERVES on its port"
      else bad "appliance $n: verify-boot.sh" "see $VDIR/verify-$n.log — $(tail -1 "$VDIR/verify-$n.log" | cut -c1-160)"; fi ;;
    *) skip "appliance $n: serves something" "no shipped verifier; boot it and check by hand (see images/$n/README.md)" ;;
  esac
}

# ── the verifier verifies itself ───────────────────────────────────────────
# Run the content checks on the UNTOUCHED base image. The ones that depend on
# what the recipe does (its file, env, sudoers, group, hostname) MUST fail
# there, or they prove nothing. A count threshold is the wrong test: some
# checks legitimately pass on a base (a minimal image has no man pages and an
# empty machine-id already), so those are listed instead, as checks that cannot
# prove the recipe acted.
stage_selftest() {
  say "self-test: the recipe-dependent checks must FAIL on an image nobody built"
  local d f
  for d in ${ONLY//,/ }; do
    f="${BASE[$d]:-}"; [ -n "$f" ] || continue
    [ -f "$ROOT/vm-images/$f.qcow2" ] || { skip "self-test $d" "no base image (use --seed-bases or build once)"; continue; }
    local pd="$VDIR/probe-$d"; probe_yaml "$d" "$pd"
    selftest_one "$d" custom "motd is the artifact|env DELONIX_IMAGE" check_custom "$d" "$ROOT/vm-images/$f.qcow2"
    selftest_one "$d" probe "files: copied|files.mode|hostname:|env:|users.sudo|users.groups|users.shell|run: executed" check_probe "$d" "$ROOT/vm-images/$f.qcow2" "$pd"
  done
}

selftest_one() { # selftest_one <distro> <name> <required, |-separated> <fn> <args...>
  local d=$1 name=$2 required=$3; shift 3
  local p0=$PASS f0=$FAIL s0=$SKIP r0=${#RESULTS[@]}
  "$@" >/dev/null
  local fresh=("${RESULTS[@]:$r0}")
  # These were experiments, not results.
  PASS=$p0; FAIL=$f0; SKIP=$s0; RESULTS=("${RESULTS[@]:0:$r0}")
  local weak=() r line hit reqs
  IFS='|' read -ra reqs <<<"$required"
  for r in "${reqs[@]}"; do
    hit=0
    for line in "${fresh[@]}"; do case "$line" in FAIL*"$r"*) hit=1; break;; esac; done
    [ "$hit" = 1 ] || weak+=("$r")
  done
  if [ "${#weak[@]}" -eq 0 ]; then ok "self-test $d/$name: every recipe-dependent check fails on the untouched base"
  else bad "self-test $d/$name" "PASSED on an untouched base, so they prove nothing: ${weak[*]}"; fi
  local same=() l
  for l in "${fresh[@]}"; do case "$l" in PASS*) same+=("$(printf '%s' "${l#PASS|}" | sed 's/|$//')");; esac; done
  [ "${#same[@]}" -gt 0 ] && printf '    · %s/%s: also pass on the base, so they cannot prove the recipe acted: %s\n' "$d" "$name" "$(IFS=';'; echo "${same[*]}")"
}

# ── plan / run / report ────────────────────────────────────────────────────
print_plan() {
  say "plan"
  echo "  root (isolated store)  $ROOT"
  echo "  distros                $ONLY"
  echo "  packages / profile     $DO_PACKAGES / $DO_PROFILE"
  echo "  boot                   $DO_BOOT"
  echo "  appliances             ${APPLIANCES[*]:-none}"
  echo "  self-test              $DO_SELFTEST"
}

report() {
  local f="$VDIR/report-$(date +%Y%m%dT%H%M%S).md"
  {
    echo "# images verification — $(date -Is)"
    echo
    echo "- delonix: $("$DELONIX" version 2>&1 | head -1)"
    echo "- host: $(uname -srm), $(mem_gb) GiB RAM available, $(free_gb "$VDIR") GiB free"
    echo "- stages: distros=$ONLY packages=$DO_PACKAGES profile=$DO_PROFILE boot=$DO_BOOT appliances=${APPLIANCES[*]:-none} self-test=$DO_SELFTEST"
    echo
    echo "**$PASS passed, $FAIL failed, $SKIP skipped**"
    echo
    echo "## Proved"; printf '%s\n' "${RESULTS[@]}" | awk -F'|' '$1=="PASS"{print "- "$2}'
    echo; echo "## FAILED"; printf '%s\n' "${RESULTS[@]}" | awk -F'|' '$1=="FAIL"{print "- "$2" — "$3}'
    echo; echo "## Not validated by this run"
    printf '%s\n' "${RESULTS[@]}" | awk -F'|' '$1=="SKIP"{print "- "$2" — "$3}'
    [ "$DO_PACKAGES" = 0 ] && echo "- packages.install (rerun with --packages)"
    [ "$DO_PROFILE" = 0 ] && echo "- profile: rootless (rerun with --profile)"
    [ "$DO_BOOT" = 0 ] && echo "- booting the images (rerun with --boot)"
    [ "${#APPLIANCES[@]}" = 0 ] && echo "- every appliance builder (rerun with --appliance <name>)"
  } >"$f"
  echo; echo "report: $f"
}

main() {
  print_plan
  preflight
  if [ "$DRY" = 1 ]; then echo; echo "dry run: preflight passed, nothing built"; exit 0; fi
  mkdir -p "$ROOT/vm-images"
  seed_bases
  [ "$NO_BUILD" = 1 ] || { stage_custom; stage_probe; }
  [ "$DO_PACKAGES" = 1 ] && stage_packages
  [ "$DO_PROFILE" = 1 ] && stage_profile
  [ "$DO_BOOT" = 1 ] && stage_boot
  local a; for a in "${APPLIANCES[@]:-}"; do [ -n "$a" ] && stage_appliance "$a"; done
  [ "$DO_SELFTEST" = 1 ] && stage_selftest
  say "verdict"
  printf '  %d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIP"
  report
  [ "$FAIL" -eq 0 ]
}
main
