# Appliance VM images

Turns vendor installation media into bootable Delonix VM images: **OPNsense**,
**Proxmox** (VE / Backup Server / Mail Gateway / Datacenter Manager) and
**TrueNAS SCALE**. Plus one image that is deliberately not an appliance:
**OpenStack**, whose story is at the end.

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

# OpenStack — NOT an appliance; see the section at the end
./build-openstack.sh                 # 2026.1 "Gazpacho" on Ubuntu 24.04

# Monitoring (Zabbix + Grafana, pre-wired) — also NOT an appliance
./build-monitoring.sh                # Zabbix 7.0.30-1 + Grafana 13.2.2 + Prometheus/Loki/NetFlow (r3)
./verify-monitoring.sh               # prove the logins, every datasource and the starter dashboard

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
| `build-openstack.sh` | OpenStack via kolla-ansible 22.1.0 | 2026.1 Gazpacho | `openstack-2026.1-ubuntu-24.04.qcow2` |
| `build-monitoring.sh` | Zabbix 7.0 LTS + Grafana + Prometheus/Loki + NetFlow stack, pre-wired | Zabbix 7.0.30-1, Grafana 13.2.2, Prometheus 3.13.1 (LTS), Loki 3.7.8, goflow2 2.2.6 | `monitoring-zabbix7.0-grafana13.2.2-r3.qcow2` |

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
| OpenStack | `cloud-images.ubuntu.com` for the host OS; `opendev.org` for kolla-ansible; quay.io for the service images |
| Monitoring | `cloud-images.ubuntu.com` for the host OS; `repo.zabbix.com` for Zabbix; `apt.grafana.com` for Grafana; `grafana.com/api/plugins` for the Zabbix app |

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

The monitoring image registers WITHOUT `--appliance` — see "Monitoring" below
for why it still wants the NoCloud seed:

```bash
delonix image vm import monitoring-zabbix7.0-grafana13.2.2-r3.qcow2 -t monitoring:7.0-r3 \
    --distro ubuntu --release 24.04 --default-vcpus 2 --default-memory 4G

delonix image vm push monitoring:7.0-r3 ghcr.io/angolardevops/delonix-vm-appliances:monitoring-7.0-r3
```

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
| Monitoring — Zabbix | `Admin` | `delonix-admin` | `http://<ip>/` |
| Monitoring — Grafana | `admin` | `delonix-admin` | `http://<ip>:3000/` |

The monitoring image does not ship the vendor's own default (Zabbix's is
`Admin`/`zabbix`, Grafana's is `admin`/`admin`) — both are reset at build time
to the same `delonix-admin` this directory already uses everywhere else, so a
password grepped out of this repository does not also unlock every
unpatched Zabbix/Grafana on the internet still on its factory default.

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

## OpenStack — the one that is not an appliance

`build-openstack.sh` sits in this directory because it is the same discipline —
pinned upstream version, vendor checksum, a guest that reports its own verdict,
a read-back before publishing — but it breaks the rule the other four keep, and
the break is the interesting part.

The other four install themselves from vendor media and configure themselves
through a web UI. **OpenStack has no such media.** It is not one program: it is
a dozen services that only become a cloud once something deploys them against
a specific host, with that host's addresses. The upstream way to do that is
[kolla-ansible](https://docs.openstack.org/kolla-ansible/), which runs the
services as containers.

So the image is split at the seam where machine-independence actually ends:

| Inside the image (this script) | Outside it (`delonix-deploy`) |
|---|---|
| Ubuntu 24.04, checksum-verified | the VM, its two NICs, its LVM volume group |
| kolla-ansible pinned to one release's stable branch | `kolla_internal_vip_address` |
| `bootstrap-servers` already run (docker, host prep) | `network_interface` / `neutron_external_interface` |
| ~20 GiB of service container images, pre-pulled | `kolla-genpwd`, on the target |
| `/etc/delonix/openstack-image.json` | `deploy`, `prechecks`, `post-deploy` |

Three consequences worth knowing before reading the script:

- **It is imported WITHOUT `--appliance`.** It is a cloud image and it wants the
  NoCloud seed `vm create` generates; that seed is how the target's hostname and
  SSH key get in. Marking it an appliance makes `vm create` refuse `--ssh-key`
  and hand over a VM nobody can log into.
- **`verify-boot.sh` deliberately has no `openstack` case.** That script's whole
  claim is that an image *serves* a port. A freshly built image here serves
  nothing — OpenStack is pulled, not deployed. Adding a case for it would mean
  either a probe that always fails or a probe weakened until it passes, and the
  second is worse. Keystone on `:5000` is proved by the deploy role, on a host
  that has a VIP. What this script proves instead, and does prove, is read back
  out of the finished disk with `virt-cat` before it will publish.
- **kolla-ansible comes from PyPI, not from `stable/<release>`.** Upstream's
  quickstart installs `git+https://opendev.org/openstack/kolla-ansible@stable/…`,
  and two measured things argue against it here. A `git clone` that STALLS never
  returns, so a retry wrapper never gets its turn — this build hung 31 silent
  minutes on exactly that, console quiet and disk not growing. And the branch
  head is a dev snapshot (`22.1.1.dev5` on the day), whose content changes
  daily; that is not a pin, in a script whose whole point is that two builds a
  month apart make the same image. The published wheel carries the data files
  the build needs — checked, not assumed: `etc_examples/kolla/globals.yml`,
  `ansible/inventory/all-in-one` and `ansible/site.yml` are all in it. Every
  network step is additionally wrapped in `timeout`, because the general lesson
  is that **a stalled connection is not a failure**, and nothing that only
  handles failure will save you from one.
- **The build's `globals.yml` is moved aside, not kept.** It ships as
  `/etc/kolla/globals.yml.build` with an unroutable VIP and the build VM's
  interface names. Leaving a plausible one in place is exactly how Proxmox VE
  once published an appliance that announced the QEMU slirp address as its own.
  A generated `passwords.yml` is destroyed for the same class of reason: baked
  secrets would be shared by every cloud ever deployed from the image.

Budget: the pull is ~20 GiB, and this workspace measures 3.3 MB/s to the
mirrors. Over an hour, on a link that never gets faster by being asked twice —
which is the entire argument for paying it once, here, instead of once per
deployment.

## Monitoring — Zabbix + Grafana, pre-wired, also not an appliance

`build-monitoring.sh` follows the OpenStack script's shape for the same
reason: there is no vendor installer here either, just Ubuntu 24.04 with two
pinned packages installed and wired to each other. It registers WITHOUT
`--appliance`, for the same reason OpenStack does — it wants the NoCloud seed
`vm create` builds, because that seed is how the target's hostname and SSH
key get in.

**"Pre-wired" is a small, precise claim, and it stops exactly where a golden
image's authority should stop:**

- The Zabbix frontend never shows its setup wizard — `zabbix.conf.php` is
  written at build time, so a clone answers ready-to-use on `:80`.
- Grafana already has Zabbix configured as a data source
  (`/etc/grafana/provisioning/datasources/zabbix.yaml`) the moment it boots,
  reachable on `:3000`.
- **What it does NOT decide is which remote network to monitor.** That is a
  per-deployment choice — SNMP/agent hosts added inside Zabbix, reachability
  through whatever the VM is attached to (a delonix `--net`, a
  `kind: NetworkRoute` between two networks, a VPN reached through
  `kind: Gateway`/a WireGuard overlay). None of that is new mechanism: it is
  the SDN and IaC primitives this repo already has, aimed at a VM that
  happens to run Zabbix. A "tenant picks a network and it just works"
  self-service flow is a real, separate thing to build — on top of this VM,
  in a platform that has a concept of tenant to hang it off. This engine does
  not (see `AGENTS.md`, "Identidade e fronteira do motor"), so it is not a
  `kind:` this repository can own; baking one guess of "the network" into
  every clone would also be wrong for every clone but one.

**The database password is generated once per build, not shipped as a
literal, and never printed** — `openssl rand` inside the guest, immediately
before `CREATE USER`. Unlike the OpenStack image's Keystone/database
passwords (a real externally-relevant secret the deploy role must generate
per-target, so the build image never bakes one in), Postgres here listens
only on `localhost` for a Zabbix server that is the only other thing on the
machine: one password per build, shared by every clone of that build, has no
external surface to leak from. What every clone gets instead is the
`Admin`/`delonix-admin` and `admin`/`delonix-admin` logins in the credentials
table above — the actual, human-facing secrets — reset from each vendor's
factory default for the reason already given there.

**Both Zabbix and Grafana packages are `apt-mark hold` at the end of the
build.** An unattended `apt upgrade` moving the server past the schema
already imported into Postgres — or Grafana past the plugin API the pinned
`alexanderzobnin-zabbix-app` build was compiled against — is exactly the
kind of drift a golden image exists to prevent; the same reasoning the golden
Kubernetes image already applies to `kubeadm`/`kubelet`/`kubectl`.

### What the monitoring image carries (revision 3)

Revision 1 was Zabbix + Grafana. Revision 2 added the layer an operator needs to
watch a whole estate — servers, network equipment and workstations — with the
data sources already provisioned and a starter dashboard already loaded.

| Component | Version | Listens on | Reachable from outside |
|---|---|---|---|
| Zabbix server + frontend (PostgreSQL) | 7.0.30-1 | `:80`, trapper `:10051` | yes |
| Grafana | 13.2.2 | `:3000` | yes |
| Alloy (syslog receiver + journal reader) | 1.19.2-1 | `:1514` tcp+udp | yes |
| goflow2 (NetFlow v5/v9, IPFIX, sFlow collector) | 2.2.6 | `:2055` udp, `:6343` udp | yes |
| Prometheus | 3.13.1 (LTS line) | `127.0.0.1:9090` | no — through Grafana |
| Alertmanager | 0.34.1 | `127.0.0.1:9093` | no |
| blackbox_exporter | 0.28.0 | `127.0.0.1:9115` | no |
| node_exporter | 1.12.1 | `127.0.0.1:9100` | no |
| Loki | 3.7.8 | `127.0.0.1:3100` | no |

Everything not in the "yes" rows is loopback-only on purpose: Grafana reaches it,
so the extra services add no external surface. It wants **4 GiB** (2 was enough
for revision 1); metrics are kept 30 days or 6 GB, logs 30 days. Loki's
anonymous usage reporting is switched off.

**Data sources, all provisioned, all proved by `verify-monitoring.sh`:**
Zabbix (API **and** a direct PostgreSQL connection), Zabbix PostgreSQL,
Prometheus (default), Loki, Alertmanager. The PostgreSQL role Grafana uses,
`grafana_ro`, is granted `SELECT` on the tables dashboards need — and **not** on
`users` or `config`, so anyone allowed to query that data source cannot read
Zabbix's password hashes. That is a check, not a claim: the verifier tries the
read and requires `permission denied`.

**Watching something is dropping a file, not editing a config.** Prometheus
re-reads `/etc/prometheus/targets/` without a restart:

```yaml
# /etc/prometheus/targets/icmp-office.yml  -- ping
- targets: ['10.10.0.1', '10.10.0.2']
  labels: {site: office}
# http-*.yml  -> http_2xx probes      tcp-*.yml -> tcp_connect probes
# node-*.yml  -> a node_exporter to scrape (host:9100)
```

The image ships with real self-probes (ICMP, HTTP, TCP against itself) so a
fresh boot shows data instead of an empty dashboard. **Logs** arrive by pointing
`rsyslog`, a switch or a firewall at `<this-vm>:1514` (RFC 5424, TCP or UDP);
they land in Loki labelled `job=syslog` plus `host`, `app` and `severity` taken
from the message header. The VM's own journal is read as `job=journal`. Which
hosts to watch, and how they reach this VM (a `--net`, a `kind: NetworkRoute`, a
tunnel), stays the operator's decision — see the section above.

**Alertmanager is deliberately inert.** It receives alerts (a target down for two
minutes, a failed probe, a disk predicted to fill within 24 h) and Grafana shows
them, but it ships no receiver: a guessed webhook or e-mail address would either
notify a stranger or drop pages silently. Add your own receiver in
`/etc/alertmanager/alertmanager.yml`.

**What `verify-monitoring.sh` measured that a green boot would not have:** the
Alloy journal source overwrote the `job` label the README promised (a query
written from this document matched nothing), and the first version of its own
Loki check grepped for the word `values`, which is in the schema of an empty
answer too — a check that could not fail. Both are fixed; the syslog check now
sends a real message over each transport and requires it back with its labels.

**NetFlow, IPFIX and sFlow (revision 3).** Point a router, switch or firewall's
flow export at `<this-vm>:2055` (NetFlow v5/v9 **and** IPFIX) or `:6343` (sFlow).
Records land in Loki as `job=netflow`; the dashboard ranks the top sources by
bytes over five minutes. Two things worth knowing before you trust the numbers:

- **IPFIX goes to 2055, not to its registered port 4739.** goflow2 panics at
  start-up with two `netflow://` listeners (measured — each registers the same
  HTTP handler), and one listener already decodes all three versions. Most
  exporters let you choose the port.
- **`goflow2_flow_traffic_bytes_total` is not traffic.** It counts the size of the
  *export packets* (216 bytes for three v5 datagrams). It is the collector's
  telemetry and feeds the "active exporters" panel; the volume of traffic is the
  `bytes` field of each record, which is what the top-talkers panel sums.

Only the exporter and the protocol become Loki labels — addresses stay inside the
JSON and are read with `| json`, because a label per address would create a
stream per host. That is also the honest limit of this design: **Loki is not a
flow database.** It comfortably serves dozens of exporters; at thousands of flows
per second the right tool is a columnar store such as Akvorado (AGPL, ClickHouse),
which belongs in an image of its own. What was proved with real packets is NetFlow
v5 only; v9, IPFIX and sFlow are decoded by the same collector upstream but were
not exercised here.

## Carbonio CE — the mail image, and why it is not Zimbra

`build-carbonio.sh` builds the open-source mail and collaboration server that
Zextras maintains as the successor of Zimbra OSE. It replaces the Zimbra image
that was asked for, for three facts found while researching it: Zimbra OSE has no
official binary for Ubuntu 24.04 (the only one is a third party's, behind a
registration form, and beta on that release); the OSE line is described by Zextras
as ended in 2023; and its binary EULA governs redistribution, which could not be
confirmed as allowing a public registry. Carbonio CE is AGPL, has an official
signed repository for Ubuntu 24.04, and its packages can be pinned.

**It is deliberately PRE-bootstrap.** `carbonio-bootstrap` asks for the machine's
FQDN, address, mail domain and admin password — none of which exist at build time.
What is baked in is everything machine-independent: the 28 packages the vendor's
manual-installation guide names, each **pinned to the exact version the repository
served and held** so an unattended upgrade cannot move one past the schema its
database bootstrap expects; PostgreSQL 16; and the repository, whose signing key
is checked against the fingerprint the vendor documents rather than trusted
because a keyserver returned it.

**Two deliberate departures from the vendor's guide, both about PostgreSQL:**

- The guide sets `listen_addresses='*'` and a `host all all 0.0.0.0/0 md5` line
  for a **superuser** role. That is needed only when Carbonio is split across
  servers; a single-server install talks to `127.0.0.1`, so here PostgreSQL stays
  on loopback and `verify-carbonio.sh` fails if the world-open line appears.
- The guide has you type a database password. `carbonio-prepare-db` generates one
  per installation, keeps it in `/root/.carbonio-db-password` (mode 0600), and is
  safe to run twice. It is never in the image.

**To finish the installation on the target** (the vendor's steps 3 and 6-10; the
address below is an example):

```bash
hostnamectl set-hostname mail.example.com
echo -e "127.0.0.1 localhost\n172.16.0.10 mail.example.com mail" > /etc/hosts
carbonio-prepare-db
systemctl enable --now carbonio-videoserver.service   # then set nat_1_1_mapping in /etc/janus/janus.jcfg
carbonio-bootstrap                                    # interactive: domain, admin password
service-discover setup-wizard && pending-setups -a
systemctl restart carbonio-ws-collaboration-sidecar.service
for s in files tasks ws-collaboration message-dispatcher; do
  PGPASSWORD=$(cat /root/.carbonio-db-password) carbonio-$s-db-bootstrap carbonio_adm 127.0.0.1
done
```

The vendor asks for **4 cores, 16 GiB of RAM and 50 GB of disk** as a minimum, and
DNS records (A and MX) for the mail domain. Ports to open: 25, 80, 110, 143, 443,
465, 587, 993, 995, 6071, 5222, and UDP 20000-40000 for the video server. About 22
Carbonio services start at boot on the unbootstrapped image, which is normal for
the vendor's own install and is why a 4 GiB test VM cannot carry them.

**What `verify-carbonio.sh` proves, and what it cannot.** It boots the image and
checks, from inside the guest: all 28 packages at their pinned versions and held;
PostgreSQL on loopback only; the helper creating a working role with a 0600
password file and changing nothing on a second run; and — read from the disk
*before* boot, because cloud-init renames the guest by then — that the image does
not carry the build VM's hostname. That last check exists because the first
version of it could not fail: it passed against an image that still said
`carbonio-build`. **It does not prove that `carbonio-bootstrap` completes, that
mail flows, or that the web client and admin panel work.** Those need a real
deployment with an FQDN and 16 GiB, and the verifier prints that instead of a
bare "passed".
