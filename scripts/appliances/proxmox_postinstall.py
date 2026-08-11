#!/usr/bin/env python3
"""Fix a freshly installed Proxmox image so it boots usable anywhere.

    proxmox_postinstall.py <ssh-port> <root-password>

The vendor installer leaves two things that only work in the machine it was
installed on, and both were found by booting a published image and looking at
its console:

1. **A STATIC IP from the build environment.** `source = "from-dhcp"` in the
   answer file means "get the configuration by DHCP *during installation* and
   write it down as static" — not "use DHCP at boot". Every image this repo
   published carries `10.0.2.15`, the QEMU slirp address, and is unreachable on
   any other network.

2. **No serial console.** Without `console=ttyS0` a guest that fails to reach
   the network cannot be observed at all without a graphics device — which is
   what made the first diagnosis take an hour.

The fix also pins the NIC name. Rewriting `interfaces` to DHCP is not enough on
its own: the bridge names a physical port, and a port called `ens18` under one
hypervisor is `enp0s3` under the next, leaving `vmbr0` with no member and the
guest off the network again. `net.ifnames=0` makes it `eth0` everywhere, which
is the only name that is true in every environment.

Driven over SSH with pexpect because that is what is available: the appliance
has sshd running and a known root password, and its serial console has no getty
to talk to (that is defect 2, and this is what fixes it).
"""

import sys
import pexpect

PORT = sys.argv[1]
PASSWORD = sys.argv[2]

# One heredoc, one connection. Each step is idempotent so a re-run of the build
# over an already-fixed image is a no-op rather than a second bridge.
SCRIPT = r"""
set -e
IFACE=$(ls /sys/class/net | grep -vE '^(lo|vmbr|tap|fwbr|fwln|fwpr|veth|docker|bonding_masters)$' | head -1)
echo "DLX: physical nic is $IFACE"

cat > /etc/network/interfaces <<EOF
# Rewritten by delonix (scripts/appliances/proxmox_postinstall.py).
#
# DHCP, and the port named eth0: the installer writes the address it had at
# install time as STATIC, and names the interface it saw then. Neither is true
# in the next machine this image boots on.
auto lo
iface lo inet loopback

iface eth0 inet manual

auto vmbr0
iface vmbr0 inet dhcp
    bridge-ports eth0
    bridge-stp off
    bridge-fd 0
EOF

# net.ifnames=0 makes the NIC eth0 in every hypervisor, which is what the
# bridge above names. console=ttyS0 makes the guest observable when the network
# does not come up — the failure this whole script exists because of.
sed -i 's|^GRUB_CMDLINE_LINUX_DEFAULT=.*|GRUB_CMDLINE_LINUX_DEFAULT="quiet net.ifnames=0 biosdevname=0 console=tty0 console=ttyS0,115200"|' /etc/default/grub
grep -q 'GRUB_TERMINAL' /etc/default/grub || echo 'GRUB_TERMINAL="console serial"' >> /etc/default/grub
update-grub 2>&1 | tail -2

# A getty on the serial port, so the console is a login and not just boot logs.
systemctl enable serial-getty@ttyS0.service >/dev/null 2>&1 || true

# The host keys were generated during the build: every image made from it would
# otherwise present the SAME identity, so any one of them can impersonate any
# other. Regenerated on first boot instead.
#
# `ssh-keygen -A` and NOT `dpkg-reconfigure openssh-server`: the latter hangs
# (measured — the unit sat in `activating` forever), and while it hangs sshd
# has no host keys and refuses to start at all. A hardening step that takes the
# service down with it is worse than the exposure it was closing.
rm -f /etc/ssh/ssh_host_*
cat > /etc/systemd/system/dlx-regen-hostkeys.service <<EOF
[Unit]
Description=Regenerate SSH host keys on first boot
ConditionPathExistsGlob=!/etc/ssh/ssh_host_*_key
Before=ssh.service
[Service]
Type=oneshot
ExecStart=/usr/bin/ssh-keygen -A
RemainAfterExit=yes
[Install]
WantedBy=multi-user.target
EOF
systemctl enable dlx-regen-hostkeys.service >/dev/null 2>&1 || true

echo "DLX: done"
"""


def main() -> int:
    ssh = pexpect.spawn(
        "ssh",
        [
            "-p", PORT,
            "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null",
            "-o", "ConnectTimeout=20",
            "root@127.0.0.1",
        ],
        encoding="utf-8",
        timeout=180,
    )
    ssh.logfile_read = sys.stdout
    if ssh.expect([r"[Pp]assword:", pexpect.EOF, pexpect.TIMEOUT]) != 0:
        print("\n!! ssh did not ask for a password")
        return 1
    ssh.sendline(PASSWORD)
    if ssh.expect([r"root@\S+:~#", pexpect.EOF, pexpect.TIMEOUT]) != 0:
        print("\n!! no shell prompt after the password")
        return 1

    ssh.sendline("bash -s <<'DLXEOF'\n" + SCRIPT + "\nDLXEOF")
    # "DLX: done" is the script's own last word. Waiting for the prompt instead
    # would accept a shell that came back because a step died halfway.
    if ssh.expect([r"DLX: done", pexpect.EOF, pexpect.TIMEOUT]) != 0:
        print("\n!! the fix-up script did not reach its end")
        return 1
    ssh.expect([r"root@\S+:~#", pexpect.TIMEOUT])

    print("\n==> post-install applied; powering off")
    ssh.sendline("systemctl poweroff")
    ssh.expect([pexpect.EOF, pexpect.TIMEOUT], timeout=120)
    return 0


if __name__ == "__main__":
    sys.exit(main())
