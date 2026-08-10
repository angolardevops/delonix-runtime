#!/usr/bin/env python3
"""Boot an OPNsense image and log in on the serial console to prove what it is.

"The image booted" is not the claim worth making — plenty of half-installed
disks reach a login prompt. This logs in and reads the state that decides
whether the image is usable as an appliance: is it running off installation
media or off its own root, how big is the filesystem, and does the web UI
actually listen.
"""
import sys

import pexpect

IMG = sys.argv[1]
MEM = sys.argv[2] if len(sys.argv) > 2 else "1024"

cmd = (
    f"qemu-system-x86_64 -enable-kvm -m {MEM} -smp 2 -cpu host "
    f"-drive file={IMG},if=virtio,format=qcow2 "
    f"-netdev user,id=n0 -device virtio-net-pci,netdev=n0 "
    f"-netdev user,id=n1 -device virtio-net-pci,netdev=n1 "
    f"-display none -serial mon:stdio"
)
c = pexpect.spawn("/bin/bash", ["-c", cmd], timeout=300, encoding="utf-8", codec_errors="replace")
c.logfile_read = None

c.expect("login:")
c.sendline("root")
c.expect("Password:")
c.sendline("opnsense")

# The OPNsense console menu; option 8 drops to a shell.
c.expect(r"Enter an option:")
c.sendline("8")
c.expect(r"[#$] $|root@")

for label, command in [
    ("version", "opnsense-version -v"),
    ("root-fs", "mount | head -3"),
    ("disk", "df -h / /var /tmp | cat"),
    ("listeners", "sockstat -4 -l | grep -E ':(80|443) ' | head -5"),
    ("services", "service -e | tail -12"),
]:
    c.sendline(f"echo '### {label}'; {command}")
    c.expect(r"[#$] $|root@", timeout=60)
    print(c.before.strip(), flush=True)

c.sendline("halt -p")
c.expect(pexpect.EOF, timeout=120)
print("### powered off", flush=True)
