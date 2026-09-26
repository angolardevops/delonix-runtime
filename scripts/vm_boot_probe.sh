#!/usr/bin/env bash
# Boots a VM image under BOTH firmwares and fails unless each one reaches the
# guest kernel. Meant to run between `image --vm build` and `image --vm push`.
#
# Why this exists: `ghcr.io/angolardevops/delonix-vm-k8s:1.36` was published on
# 2026-08-24 from a build that exited 0, and it does not boot under SeaBIOS —
# GRUB stops at `grub rescue>` with `error: no such partition`, because
# `virt-resize` renumbered /boot from gpt16 to gpt3 and the BIOS core image
# still carried `prefix=(hd0,gpt16)/grub`. The reinstall that fixes it landed
# three days later (57bb5fab) and the tag was never rebuilt. Nothing between
# "build" and "push" ever tried to start the artefact, so a disk that cannot
# boot was indistinguishable from one that can.
#
# What it checks, and what it does not: the kernel printing `Linux version` on
# the serial console. That proves firmware -> bootloader -> kernel, which is
# the layer that broke. It does not prove cloud-init, networking or the CRI —
# those need a seed and belong to the provider tests.
#
# The BIOS run uses virtio-scsi on purpose: it is what a Proxmox template gets
# (`--scsihw virtio-scsi-single --scsi0 ...`) and what the failure was
# reported on.
#
# Usage: scripts/vm_boot_probe.sh <image.qcow2> [bios|uefi|both]
# Env:   PROBE_TIMEOUT  seconds per firmware (default 240; TCG is slow)
#        PROBE_WORKDIR  where overlays and logs go (default: a mktemp dir)
set -euo pipefail

img=${1:?usage: $0 <image.qcow2> [bios|uefi|both]}
which=${2:-both}
timeout_s=${PROBE_TIMEOUT:-240}
[ -r "$img" ] || { echo "vm_boot_probe: cannot read $img" >&2; exit 2; }
command -v qemu-system-x86_64 >/dev/null || { echo "vm_boot_probe: qemu-system-x86_64 not installed (qemu-system-x86)" >&2; exit 2; }
command -v qemu-img >/dev/null || { echo "vm_boot_probe: qemu-img not installed (qemu-utils)" >&2; exit 2; }

work=${PROBE_WORKDIR:-$(mktemp -d)}
mkdir -p "$work"
img=$(readlink -f "$img")

accel=(-accel tcg)
if [ -w /dev/kvm ]; then accel=(-accel kvm); fi

ovmf_code="" ovmf_vars=""
for c in /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd /usr/share/edk2/ovmf/OVMF_CODE.fd; do
    [ -r "$c" ] && { ovmf_code=$c; break; }
done
for v in /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd /usr/share/edk2/ovmf/OVMF_VARS.fd; do
    [ -r "$v" ] && { ovmf_vars=$v; break; }
done

# One boot. Returns 0 as soon as the kernel speaks, 1 on timeout or on qemu
# exiting first. The qemu process is always killed and reaped before return.
probe() {
    local fw=$1 log="$work/serial-$1.log" ov="$work/overlay-$1.qcow2"
    local -a fwargs=()
    rm -f "$log" "$ov"
    qemu-img create -q -f qcow2 -F qcow2 -b "$img" "$ov"
    if [ "$fw" = uefi ]; then
        if [ -z "$ovmf_code" ] || [ -z "$ovmf_vars" ]; then
            echo "vm_boot_probe: uefi requested but no OVMF found (install ovmf)" >&2
            return 1
        fi
        cp "$ovmf_vars" "$work/vars.fd"
        fwargs=(-machine q35
            -drive "if=pflash,format=raw,readonly=on,file=$ovmf_code"
            -drive "if=pflash,format=raw,file=$work/vars.fd")
    fi
    qemu-system-x86_64 "${accel[@]}" "${fwargs[@]}" -m 2048 -smp 2 \
        -device virtio-scsi-pci,id=scsi0 \
        -drive "file=$ov,if=none,id=disk0,format=qcow2" \
        -device scsi-hd,drive=disk0,bus=scsi0.0,bootindex=1 \
        -nic none -display none -vga std -serial "file:$log" \
        >/dev/null 2>&1 &
    local pid=$! t=0 ok=1
    while [ "$t" -lt "$timeout_s" ]; do
        if grep -aq 'Linux version' "$log" 2>/dev/null; then ok=0; break; fi
        kill -0 "$pid" 2>/dev/null || break
        sleep 2; t=$((t + 2))
    done
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    if [ "$ok" -eq 0 ]; then
        echo "vm_boot_probe: $fw OK — kernel reached after ~${t}s ($(grep -aom1 'Linux version [^ ]*' "$log"))"
    else
        echo "vm_boot_probe: $fw FAIL — no kernel on the serial console within ${timeout_s}s" >&2
        echo "  serial log: $log ($(wc -c < "$log" 2>/dev/null || echo 0) bytes); last lines:" >&2
        tr -d '\r' < "$log" 2>/dev/null | tail -5 | sed 's/^/    /' >&2 || true
        echo "  a GRUB that stops before the kernel prints to VGA, not serial — an empty log is the usual shape of a bootloader failure" >&2
    fi
    rm -f "$ov"
    return "$ok"
}

rc=0
case "$which" in
    bios) probe bios || rc=1 ;;
    uefi) probe uefi || rc=1 ;;
    both) probe bios || rc=1; probe uefi || rc=1 ;;
    *) echo "vm_boot_probe: unknown firmware '$which' (bios|uefi|both)" >&2; exit 2 ;;
esac
exit "$rc"
