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

Every script fetches its own media and **verifies it against the vendor's
published SHA-256** before using it. Pass nothing and you get the pinned version
below; pass a version to get another one; pass a path to use an ISO you already
have.

```bash
# OPNsense — no install step: the vendor publishes a pre-installed disk image
./build-opnsense.sh                  # 26.1.2

# Proxmox — the vendor's own automated installer, unattended
./build-proxmox.sh pve               # 9.2-1
./build-proxmox.sh pbs               # 4.2-1
./build-proxmox.sh pmg               # 9.1-1
./build-proxmox.sh pdm               # 1.1-1

# TrueNAS SCALE — the installer's own JSON-RPC API
./build-truenas.sh                   # 25.10.5

# Another version, or media you already have
./build-proxmox.sh pve 9.1-1
./build-proxmox.sh pve /path/to/proxmox-ve_9.1-1.iso
./build-truenas.sh 25.04.2

# Prove they serve something, not merely that they boot
./verify-boot.sh
```

Needs `qemu-system-x86_64` with KVM, `qemu-img`, `xorriso`, `curl`, `python3`.
Each Proxmox build wants ~4 GiB of RAM and a few minutes; TrueNAS wants ~6 GiB.
Media lands in `.media/` (override with `MEDIA_CACHE`) and is re-used across
builds — the checksum is what makes that safe.

### Pinned versions

| Script | Product | Version | Output |
|---|---|---|---|
| `build-opnsense.sh` | OPNsense | 26.1.2 | `opnsense-26.1.2.qcow2` |
| `build-proxmox.sh pve` | Proxmox VE | 9.2-1 | `pve-9.2-1.qcow2` |
| `build-proxmox.sh pbs` | Proxmox Backup Server | 4.2-1 | `pbs-4.2-1.qcow2` |
| `build-proxmox.sh pmg` | Proxmox Mail Gateway | 9.1-1 | `pmg-9.1-1.qcow2` |
| `build-proxmox.sh pdm` | Proxmox Datacenter Manager | 1.1-1 | `pdm-1.1-1.qcow2` |
| `build-truenas.sh` | TrueNAS SCALE | 25.10.5 | `truenas-25.10.5.qcow2` |

The version is in the output name on purpose: without it, building 9.2 quietly
overwrites the 9.1 image sitting in the same directory, and both tags are meant
to coexist in the store.

**A version the vendor does not publish stops the build**, and the error lists
the versions that do exist. There is no flag to skip verification — the moment
there is one, it ends up in a script somewhere.

### Where the media comes from

All four Proxmox products download from `enterprise.proxmox.com/iso/`, which is
what the vendor's own download pages link to — verified page by page, not
assumed:

| Product | Vendor page |
|---|---|
| Proxmox VE | <https://proxmox.com/en/downloads/proxmox-virtual-environment/iso> |
| Proxmox Backup Server | <https://proxmox.com/en/downloads/proxmox-backup-server> |
| Proxmox Mail Gateway | <https://proxmox.com/en/downloads/proxmox-mail-gateway> |
| Proxmox Datacenter Manager | <https://proxmox.com/en/downloads/proxmox-datacenter-manager> |
| OPNsense | the `MIRROR` in `build-opnsense.sh` (dotsrc by default) |
| TrueNAS SCALE | `download.sys.truenas.net/TrueNAS-SCALE-<train>/<version>/` |

`download.proxmox.com` is **not** where these ISOs live — it serves the apt
repositories, and none of the four pages links to it. An earlier note in the CI
workflow named it as the reason the ISOs could not be fetched here; the host was
right and the conclusion was wrong.

To check what is current before bumping a pinned version, open the page for that
product — or just ask the build for a version that does not exist, which prints
the list the vendor publishes.

TrueNAS download paths carry the release train's codename (`Goldeye` for
25.10.x), which is not derivable from the version number. Known trains are in
`build-truenas.sh`; for a newer one, pass `TRUENAS_TRAIN=<Name>` rather than let
the script guess a URL that would 404 halfway through a 3 GiB download.

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

| Product | Account | Password | Web UI (`<ip>` is the VM's address) |
|---|---|---|---|
| OPNsense | `root` | `opnsense` (vendor default) | `https://192.168.1.1/` — **LAN only**, see below |
| Proxmox VE | `root` | `delonix-admin` | `https://<ip>:8006/` |
| Proxmox Backup Server | `root` | `delonix-admin` | `https://<ip>:8007/` |
| Proxmox Mail Gateway | `root` | `delonix-admin` | `https://<ip>:8006/` |
| Proxmox Datacenter Manager | `root` | `delonix-admin` | `https://<ip>:8443/` |
| TrueNAS SCALE | `truenas_admin` | `delonix-admin` | `http://<ip>/` — API at `https://<ip>/api/v2.0` |

Every account above also works on the console. The ports are not a guess: they
are the `CASES` table of `verify-boot.sh`, which is the port each image was
proved to answer on before it was published.

**OPNsense does not answer on the WAN, by design.** Its web UI listens on the
LAN interface (`vtnet0`, `192.168.1.1/24`) while the WAN takes DHCP, so a probe
from the WAN side is refused — that is a firewall behaving correctly, not a
broken image. It is also why `verify-boot.sh` leaves it out.

**TrueNAS was proved on :80, but `kind: Volume` provisioning talks to :443.**
The appliance's factory certificate is not verifiable from anywhere else, so
that path needs `insecureTLS: true` — or a certificate you install yourself.
See `examples/provision-truenas.yaml`.

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
