#!/usr/bin/env bash
# Preflight of the nightly lab runner (decision D1 of
# docs/discovery/65_PLANO_MATURIDADE.md).
#
# The lab exists because a hosted runner blocks unprivileged user namespaces,
# so the chaos job there goes green by skipping everything. A lab that is
# missing one of these is the same lie one level up: the battery would SKIP
# the sections that need it and the night would read as covered. So this
# script names EVERY missing piece and exits non-zero if there is one; it
# never stops at the first.
#
# Usage: scripts/lab_preflight.sh          (exit 0 = the host can run it all)
set -u
missing=()
ok() { printf '  ok    %s\n' "$1"; }
no() { printf '  MISS  %s — %s\n' "$1" "$2"; missing+=("$1"); }

echo "lab preflight on $(hostname) ($(uname -r))"

# The engine itself.
if unshare -r -n true 2>/dev/null; then ok "unprivileged user namespaces"
else no "unprivileged user namespaces" "unshare -r -n true failed (sysctl kernel.unprivileged_userns_clone / AppArmor restriction)"; fi
for id in subuid subgid; do
  if grep -q "^$(id -un):" /etc/$id 2>/dev/null; then ok "/etc/$id range"
  else no "/etc/$id range" "no entry for $(id -un)"; fi
done

# Delegated cgroup controllers: the limits the battery asserts are REFUSED
# without them, and a refused check is a SKIP, not a pass.
uid=$(id -u)
ctl=/sys/fs/cgroup/user.slice/user-$uid.slice/user@$uid.service/cgroup.subtree_control
have=$(cat "$ctl" 2>/dev/null || true)
for c in cpu cpuset io memory pids; do
  if grep -qw "$c" <<<"$have"; then ok "cgroup controller $c delegated"
  else no "cgroup controller $c delegated" "missing from $ctl (Delegate=cpu cpuset io memory pids on user@.service, then restart user@$uid)"; fi
done

# Namespace isolation is inert without br_netfilter, and refused since D5.
if [ -d /sys/module/br_netfilter ]; then ok "br_netfilter loaded"
else no "br_netfilter loaded" "modprobe br_netfilter (persist in /etc/modules-load.d)"; fi
if [ "$(cat /proc/sys/net/bridge/bridge-nf-call-iptables 2>/dev/null)" = 1 ]; then ok "bridge-nf-call-iptables=1"
else no "bridge-nf-call-iptables=1" "sysctl -w net.bridge.bridge-nf-call-iptables=1"; fi

# VMs: both local backends, with the firmware that actually boots the images.
if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then ok "/dev/kvm read-write"
else no "/dev/kvm read-write" "add the runner user to the kvm group"; fi
for t in virsh qemu-img qemu-system-x86_64 cloud-hypervisor cloud-localds virt-customize; do
  if command -v "$t" >/dev/null 2>&1; then ok "$t"
  else no "$t" "not on PATH"; fi
done
# The EDK2 paths of `DEFAULT_CH_FIRMWARES` (delonix-provider-cloud-hypervisor):
# the rust-hypervisor-fw boots none of this project's images, so it does not
# count here.
fw=""
for f in /usr/local/share/delonix/CLOUDHV.fd /usr/share/delonix/CLOUDHV.fd; do
  [ -r "$f" ] && fw="$f" && break
done
if [ -n "$fw" ]; then ok "EDK2 CLOUDHV.fd ($fw)"
else no "EDK2 CLOUDHV.fd" "install.sh downloads it to /usr/local/share/delonix"; fi
if virsh -c qemu:///system list >/dev/null 2>&1; then ok "qemu:///system reachable"
else no "qemu:///system reachable" "add the runner user to the libvirt group"; fi

# Rootless network and the build.
for t in slirp4netns nft ip conntrack newuidmap protoc wg; do
  if command -v "$t" >/dev/null 2>&1; then ok "$t"
  else no "$t" "not on PATH"; fi
done

# Room for images, VMs and the target dir.
free_g=$(df -BG --output=avail "${HOME}" | tail -1 | tr -dc 0-9)
if [ "${free_g:-0}" -ge 80 ]; then ok "free disk ${free_g}G (>= 80G)"
else no "free disk" "${free_g:-?}G free under $HOME, need 80G"; fi

echo
if [ ${#missing[@]} -gt 0 ]; then
  echo "lab preflight: ${#missing[@]} missing — the nightly run would skip what needs them"
  exit 1
fi
echo "lab preflight: complete"
