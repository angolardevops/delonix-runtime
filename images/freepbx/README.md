# FreePBX 17 (PBX / IP telephony) — build a qcow2 image

This folder builds FreePBX 17 with Asterisk 22 (LTS) and the open-source modules only,
on Debian 12, with MariaDB and Apache as the vendor's installer sets them up.

It is **a cloud image with software installed**: it still runs cloud-init, so
`vm create --ssh-key/--hostname/--user-data` work — and `--user-data` is also how the
first boot receives an admin password (see 5).

```
images/freepbx/
├── vm.yaml     the recipe: which builder, its arguments, what to record in the image
└── README.md   this file
```

The builder, its guest script and the first-boot unit stay in `scripts/appliances/`
(`build-freepbx.sh`, `freepbx-build.yaml`, `freepbx-first-boot`). `vm.yaml` selects
the builder by **name**; it never contains a path.

## Why Asterisk 22, and why not Issabel

The whole point of a PBX image is a telephony stack that still gets security fixes:
PBXs on the internet are a standing target for toll fraud. Asterisk 22 is the LTS
line with security fixes to October 2029 (docs.asterisk.org, *Asterisk Versions*).
Issabel 5 was measured first, on 2026-09-30: its repository's newest Asterisk is
`asterisk18-18.19.0` (September 2023), and Asterisk 18 lost security support in
October 2025 — so it was not built.

## 1. What you need

- `qemu-system-x86_64` with **KVM**, `qemu-img`, `cloud-localds`, `curl`, and the
  libguestfs tools (`virt-cat`, `virt-sparsify`, `guestfish`)
- ~4 GiB of RAM and 2 vCPUs for the build
- 20 GiB disk ceiling

## 2. Build it

From the repository root:

```bash
delonix vm build -f images/freepbx/vm.yaml
```

The build allows 60 minutes. What it pins, and checks before using:

| What | Pinned to | Checked by |
|---|---|---|
| Debian 12 genericcloud | the dated build `20260923-2610` | Debian's `SHA512SUMS` |
| FreePBX's installer (`sng_freepbx_debian_install.sh`) | a commit of `github.com/FreePBX/sng_freepbx_debian_install` | its sha256, before it runs |
| The `deb.freepbx.org` apt key | fingerprint `991C357C8A359D0382BC6E87C4DFE68FCE6DE186` | after the installer, which fetches it over plain http |
| Asterisk | major `22` | read from the running Asterisk; another major fails the build |

**"Open-source only" is made true by this build, not by the vendor's flag.** Measured on
2026-09-30: the installer's `--opensourceonly` leaves six ionCube-encoded commercial
modules on disk (`cos`, `endpoint`, `oracle_connector`, `pms`, `restapps`, `sysadmin`) and
encoded AGI scripts in `agi-bin`, and on the way it aborts twice — once when
`oracle_connector` cannot uninstall, once when `fwconsole ma refreshsignatures` meets the
encoded code. The build applies two one-line patches to the pinned installer (each checked
to touch exactly one line) so it reaches its end, then removes every module whose
`module.xml` says `Commercial`, plus `firewall` (which needs the commercial `sysadmin`), and
the encoded AGI scripts. It then **fails** if any commercial module or any ionCube-encoded
file is left, or if `fwconsole ma refreshsignatures` does not succeed.

**What is NOT pinned:** the FreePBX modules. The vendor's installer runs
`fwconsole ma upgradeall`, so two builds can carry different module versions. The
list each build got is in `/etc/delonix/freepbx-modules.txt` inside the image.

## 3. Check the result

```bash
delonix image vm ls
delonix image vm describe freepbx-17-asterisk22-r1
```

## 4. Prove it serves something

```bash
./scripts/appliances/verify-freepbx.sh                          # admin password given at creation
VERIFY_ADMIN=random ./scripts/appliances/verify-freepbx.sh      # admin password generated on first boot
```

## 5. Boot a VM from it — and the first boot

```bash
delonix vm create pbx --disk freepbx-17-asterisk22-r1 --wait
```

**No secret in this image is shared by its clones.** On each clone's first boot,
`delonix-freepbx-first-boot.service`:

- sets the web admin login `admin` to the password found in
  `/etc/delonix/freepbx-admin-password` — a cloud-init `write_files` from whoever
  creates the VM — and deletes that file; with no such file, it generates one;
- writes the result to `/root/freepbx-admin.txt` (0600), and nowhere else;
- regenerates the AMI secret, which FreePBX writes into `manager.conf`.

It runs once: a stamp in `/var/lib/delonix/` stops a second run. There is **no
known default password** for this image, unlike the others in this directory —
for a PBX that is not a courtesy.

## 6. What you still have to decide

- **There is no host firewall from FreePBX.** `--opensourceonly` leaves out the
  commercial modules, and the Responsive Firewall depends on one of them.
  `fail2ban` is installed. Put the VM behind the network's own policy: SIP
  (5060/udp, 5061/tcp) and the RTP range only from where calls come from, and the
  web UI (80) from nowhere public.
- **The RTP range is FreePBX's default.** Publishing thousands of UDP ports is its
  own problem on this engine; narrow it in *Settings → Asterisk SIP Settings* if
  the VM's calls come through a published range.
