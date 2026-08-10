# Appliance VM images

Turns vendor installation media into bootable Delonix VM images: **OPNsense**,
**Proxmox** (VE / Backup Server / Mail Gateway / Datacenter Manager) and
**TrueNAS SCALE**.

Nothing here hand-builds a guest. Each product installs itself exactly as it
would on metal — the scripts only drive its own unattended path and capture the
result. That is why the output is a system the vendor would recognise, and why
a new upstream release usually needs no change here beyond a version argument.

## Why these are "appliances"

None of them run cloud-init. They install and configure themselves, through a
console or a web UI on first boot. `delonix image vm import --appliance` marks
that in the image's metadata, and `vm create` then:

- does **not** generate the NoCloud seed it builds for every cloud image, and
- **refuses** `--hostname` / `--ssh-key` / `--user-data` instead of accepting
  and silently dropping them.

## Building

```bash
# OPNsense — no install step: the vendor publishes a pre-installed disk image
./build-opnsense.sh 26.1.2

# Proxmox — the vendor's own automated installer, unattended
./build-proxmox.sh pve /path/to/proxmox-ve_9.1-1.iso
./build-proxmox.sh pbs /path/to/proxmox-backup-server_4.1-1.iso
./build-proxmox.sh pmg /path/to/proxmox-mail-gateway_9.0-1.iso
./build-proxmox.sh pdm /path/to/proxmox-datacenter-manager_1.0-2.iso

# TrueNAS SCALE — the installer's own JSON-RPC API
./build-truenas.sh /path/to/TrueNAS-SCALE-25.10.5.iso

# Prove they serve something, not merely that they boot
./verify-boot.sh
```

Needs `qemu-system-x86_64` with KVM, `qemu-img`, `xorriso`, `curl`, `python3`.
Each Proxmox build wants ~4 GiB of RAM and a few minutes; TrueNAS wants ~6 GiB.

Answer files live next to the scripts (`answer-<product>.toml`); see
`answer.toml.example`. The fields are the vendor's, validated by the installer
itself — an invalid one aborts the build rather than producing a surprising
guest.

## Registering and publishing

```bash
delonix image vm import opnsense-26.1.2.qcow2 -t opnsense:26.1 --appliance \
    --distro opnsense --release 26.1.2 --default-vcpus 2 --default-memory 2G

delonix image vm push opnsense:26.1 ghcr.io/angolardevops/delonix-vm-appliances:opnsense-26.1
```

`push` stamps the metadata onto the OCI manifest as annotations and `pull`
reads them back, so a pulled appliance stays an appliance. Without that, the
image would land on the other side looking like a cloud image and get a seed
it cannot read.

## Credentials

Every image ships with a **known, public** password — they are in this
repository. Change them on first boot; do not expose one of these to an
untrusted network as-is.

| Product | Account | Password | Where |
|---|---|---|---|
| OPNsense | `root` | `opnsense` (vendor default) | console, web UI on the LAN interface |
| Proxmox (all four) | `root` | `delonix-admin` | web UI, console |
| TrueNAS SCALE | `truenas_admin` | `delonix-admin` | web UI, console |

The Proxmox and TrueNAS passwords are set by the answer file / RPC call, so
changing them for your own builds is an edit to `answer-*.toml` or the
`PASSWORD` environment variable of `build-truenas.sh`.

## Notes worth keeping

- **The OPNsense `vga`/`serial`/`dvd` images are NOT installed systems.** They
  boot live off the installation media. Only `nano` is pre-installed. This cost
  a build to discover.
- **The Proxmox ISO cannot be edited in place.** `xorriso -boot_image any
  replay` dies on its hybrid GPT, and `keep` yields an image SeaBIOS will not
  boot past `Booting from DVD/CD...`. `mkiso.sh` extracts the tree and authors
  the ISO again from the source's own `-report_el_torito as_mkisofs` recipe —
  which is also why one script is correct for all four products: they differ in
  volume id AND in partition geometry (`-partition_hd_cyl` is 110 for VE, 91
  for PBS).
- **A TCP probe does not prove a server is up.** QEMU's `hostfwd` accepts the
  connection whether or not anything listens in the guest, so waiting for the
  port to open returns immediately. The TrueNAS client retries the real
  WebSocket handshake instead.
- **`modprobe: ERROR:` in a Proxmox install log is not a failure.** It is the
  kernel shrugging at absent hardware. Only `ERROR: Installation failed`,
  `Auto-installation failed` and `unable to continue` are the installer's own.
