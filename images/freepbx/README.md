# FreePBX 17 (PBX / IP telephony) — build a qcow2 image

This folder builds **stock** FreePBX 17 with Asterisk 22 (LTS) on Debian 12, with MariaDB
and Apache, exactly as the vendor's installer sets them up — commercial modules and the
ionCube loader included, unlicensed (see *Why stock* below).

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

**Why stock, and not `--opensourceonly`.** The vendor's installer has an open-source-only
mode. Measured on 2026-09-30 at the pinned commit, it cannot be made reproducible: it
aborts when `oracle_connector` cannot uninstall; later steps put six ionCube-encoded
commercial modules back (`cos`, `endpoint`, `oracle_connector`, `pms`, `restapps`,
`sysadmin`); and even with those removed from disk, database and module cache, FreePBX
re-downloaded `sysadmin` and `firewall` as "missing dependencies" (`cos → sysadmin →
firewall`) at the next `refreshsignatures` — in one run of two. So the image is what every
FreePBX install is: the commercial modules are present and **unlicensed**, and the ionCube
loader that runs them is installed. The build requires `fwconsole ma refreshsignatures` to
succeed, so module signatures verify.

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

### Measured — 2026-10-03, end to end

One run of `build-freepbx.sh` followed by `verify-freepbx.sh` in both modes, on the
commit that pins `mariadb.socket` to loopback:

| | |
|---|---|
| Build | guest reported success; record read back from the finished disk |
| What it carries | FreePBX framework 17.0.33, Asterisk 22.11.0, Debian `20260923-2610` |
| Image | 3.0 GiB compressed (zstd), 20 GiB virtual |
| `verify-freepbx.sh` (password given at creation) | 27 ok, 0 failed |
| `VERIFY_ADMIN=random verify-freepbx.sh` | 27 ok, 0 failed |

The verifier earned its place on its first run against a finished image: both modes came
back **26 ok, 1 failed** — "the database (3306) listens on loopback only". It was right.
`my.cnf` says `bind-address=127.0.0.1`, but `mariadb.socket` is enabled with
`ListenStream=3306`, so systemd opens the port on every interface and `bind-address` never
applies (`ss`: `*:3306`, held by systemd and mariadbd). The build now pins that socket to
`127.0.0.1:3306`, and the check passes.

**Not measured:** a SIP registration or a call through this PBX; a login through the web
form in a browser; TLS on 5061; the image booted by `delonix vm create` (the verifier
boots it with QEMU directly); and the image published — nothing has been pushed to a
registry.

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

- **Do not count on FreePBX's own firewall.** The Responsive Firewall ships (it
  needs the commercial `sysadmin`, present but unlicensed) and was **not** proved to
  work here. `fail2ban` is installed. Put the VM behind the network's own policy: SIP
  (5060/udp, 5061/tcp) and the RTP range only from where calls come from, and the
  web UI (80) from nowhere public.
- **The RTP range is FreePBX's default.** Publishing thousands of UDP ports is its
  own problem on this engine; narrow it in *Settings → Asterisk SIP Settings* if
  the VM's calls come through a published range.
