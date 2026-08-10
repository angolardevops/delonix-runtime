#!/usr/bin/env python3
"""Install OPNsense onto a fresh disk. **INCOMPLETE — see STATUS below.**

STATUS (2026-08-10): this gets as far as a GPT disk with a populated ESP that
EDK2 boots, and stops one step short: the FreeBSD kernel starts and then cannot
find the ZFS pool ("Cannot find the pool label for 'zroot'"), in QEMU *and* in
Cloud Hypervisor. Since it fails on both, the fault is in how this script
partitions — not in the hypervisor.

WHAT IS ESTABLISHED (each measured, not assumed):

  * The vendor's ready-made `nano` image is MBR/BIOS-only — `Disklabel type:
    dos`, one `a5 FreeBSD` partition, no ESP. Cloud Hypervisor boots UEFI only,
    which is why it reports "No bootable option or device was found" there. An
    INSTALLED OPNsense is GPT with a 260M ESP, which is the whole reason to
    install rather than convert.
  * The DVD's loader talks to VGA only (32 bytes of serial output in eight
    minutes). The `serial` image is the same installer with the console
    redirected — use that as the medium.
  * Two virtio disks with no explicit `bootindex` leave FreeBSD at `mountroot>`;
    the medium alone boots fine, so it is the enumeration.
  * `bsdinstall bootconfig` is what puts loader.efi on the ESP — skip it and the
    ESP is valid and EMPTY. `zfsboot` says so itself in a comment.
  * `bsdinstall config` is what copies the generated loader.conf/rc.conf into
    the installed system.
  * Driving the installer's dialog(1) menus with pexpect is the wrong tool:
    `expect` matches text as it streams past and cannot tell "this screen shows
    X" from "X scrolled by two minutes ago" — a `last chance` pattern matched
    the BOOT LOADER's `Do you want to proceed? [y/N]`, and the reply typed into
    a menu was read as navigation keys, walking the installer backwards.

WHAT IS LEFT, and the trap in it: `opnsense-zfs` (the partitioner the vendor's
"Install (ZFS)" menu entry runs, with OPNsense's own defaults) does NOT honour
`nonInteractive` the way FreeBSD's `zfsboot` does — it opens its own menu and
this script times out on it. FreeBSD's `zfsboot` runs unattended but produced
the unbootable pool above. So the next step is to find which of OPNsense's
defaults (`ZFSBOOT_POOL_CREATE_OPTIONS`, `ZFSBOOT_FORCE_4K_SECTORS`, the
bootfs/canmount handling) the plain `zfsboot` path is missing — the two scripts
are diffable, which is where to start.

Do not treat this as working. It is checked in for the diagnosis, not the
result.
"""

import os
import re
import sys
import time

import pexpect

MEDIUM = sys.argv[1]          # OPNsense-*-serial-amd64.img (installer, raw)
DISK = sys.argv[2]            # target qcow2, created empty by the caller
ROOT_PW = os.environ.get("ROOT_PW", "opnsense")

qemu = (
    "qemu-system-x86_64 -enable-kvm -m 2048 -smp 2 -cpu host "
    # Explicit bootindex: with two virtio disks and none, FreeBSD cannot find
    # /dev/ufs/OPNsense_Install and drops to mountroot> (measured).
    f"-drive if=none,id=med,file={MEDIUM},format=raw "
    "-device virtio-blk-pci,drive=med,bootindex=0 "
    f"-drive if=none,id=tgt,file={DISK},format=qcow2 "
    "-device virtio-blk-pci,drive=tgt,bootindex=1 "
    "-netdev user,id=n0 -device virtio-net-pci,netdev=n0 "
    "-display none -serial mon:stdio -no-reboot"
)

ANSI = re.compile(r"\x1b\[[0-9;?]*[a-zA-Z]|\x1b[()][B0]|\x1b[=>]|[\x0e\x0f]")


class Screen(pexpect.spawn):
    """Strips terminal escapes before matching (dialog interleaves them)."""

    def read_nonblocking(self, size=1, timeout=None):
        return ANSI.sub("", super().read_nonblocking(size, timeout))


c = Screen("/bin/bash", ["-c", qemu], timeout=900, encoding="utf-8",
           codec_errors="replace")
c.logfile_read = sys.stdout


def sh(cmd, marker, timeout=1800):
    """Runs one shell command and waits for a marker IT prints.

    Waiting on a marker the command emits itself is what makes this reliable:
    the string cannot have come from earlier output, because it did not exist
    until now.
    """
    print(f"\n>>>>> {marker}", flush=True)
    c.sendline(f"{cmd}; echo {marker}-rc=$?")
    c.expect(rf"{marker}-rc=(\d+)", timeout=timeout)
    rc = c.match.group(1)
    if rc != "0":
        raise SystemExit(f"!! {marker} failed with rc={rc}")


c.expect(r"login:", timeout=900)
c.sendline("root")
c.expect("Password:", timeout=120)
c.sendline("opnsense")

# The OPNsense console menu is NUMBERED; "8" is the shell. A number is typed,
# not navigated — no dependence on which entry happens to be highlighted.
c.expect(r"Enter an option:", timeout=300)
c.sendline("8")
c.expect(r"[#$] $|root@|: ~ #", timeout=120)

# FreeBSD gives root csh, where `export` does not exist (it is `setenv`) — the
# first attempt died with rc=1 on that alone. Drop into a POSIX shell so every
# command below is ordinary sh.
c.sendline("/bin/sh")
c.expect(r"\$ $|# $", timeout=60)

# Partition + bootloaders. BIOS+UEFI is what makes the result boot on Cloud
# Hypervisor (UEFI) as well as on QEMU/SeaBIOS (BIOS); the installer's own menu
# calls this "ZFS GPT/UEFI Hybrid".
env = (
    "export ZFSBOOT_DISKS=vtbd1 "
    "ZFSBOOT_BOOT_TYPE=BIOS+UEFI "
    "ZFSBOOT_PARTITION_SCHEME=GPT "
    "ZFSBOOT_CONFIRM_LAYOUT=0 "
    "ZFSBOOT_SWAP_SIZE=0 "
    "nonInteractive=1 "
    "BSDINSTALL_CHROOT=/mnt "
    "BSDINSTALL_TMPETC=/tmp/bsdinstall_etc "
    "BSDINSTALL_TMPBOOT=/tmp/bsdinstall_boot"
)
sh(f"{env} && mkdir -p /tmp/bsdinstall_etc /tmp/bsdinstall_boot /mnt", "ENVOK", timeout=60)
# `opnsense-zfs`, not FreeBSD's `zfsboot`: this is the script the vendor's own
# menu runs ("Install (ZFS)"), and it is a full partitioner with OPNsense's
# defaults, not a thin wrapper. Calling zfsboot directly produced a disk whose
# pool the kernel could not find at boot — in QEMU as well as in CH, which is
# what showed the fault was mine and not the hypervisor's.
sh(f"{env} && bsdinstall opnsense-zfs < /dev/null", "ZFSBOOT", timeout=1200)
sh(f"{env} && bsdinstall mount", "MOUNT", timeout=300)
sh(f"{env} && bsdinstall opnsense-install", "COPY", timeout=3600)
# The step that puts loader.efi on the ESP. `zfsboot` creates and formats the
# partition and says so itself ("We'll configure the ESP in bootconfig") — skip
# this and the disk has a perfectly good, perfectly EMPTY 260M ESP, which is
# exactly what the first attempt produced: EDK2 still reported "No bootable
# option or device was found".
# `config` is what makes the installed system BOOTABLE, and skipping it is why
# the first UEFI-capable disk still stopped at `mountroot>`: this step appends
# `zfs_load="YES"` and copies the generated loader.conf (with
# vfs.root.mountfrom) and rc.conf into the target. Read from the script itself,
# not guessed.