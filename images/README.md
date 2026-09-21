# images/ — one folder per VM image

Every folder holds a `vm.yaml` (the recipe), a `README.md` (end to end: what you
need, the command, what to check) and whatever the recipe ships. Build any of
them from a clone of this repository:

```bash
delonix vm build -f images/<folder>/vm.yaml -t <version>
```

| Folder | What it builds | Kind |
|---|---|---|
| [`ubuntu`](ubuntu/README.md) · [`debian`](debian/README.md) · [`rocky`](rocky/README.md) · [`fedora`](fedora/README.md) | a cloud image with a user, a file, an env var, removals and cleanup | custom recipe, builds **offline** |
| [`opnsense`](opnsense/README.md) · [`proxmox`](proxmox/README.md) · [`truenas`](truenas/README.md) | vendor appliances (no cloud-init) | runs a builder from `scripts/appliances/` |
| [`openstack`](openstack/README.md) · [`monitoring`](monitoring/README.md) · [`carbonio`](carbonio/README.md) · [`glpi`](glpi/README.md) · [`wazuh`](wazuh/README.md) | a cloud image with software installed | runs a builder from `scripts/appliances/` |

`vm.yaml` is to `vm build` what a `compose.yaml` is to `docker compose`; a
`VMfile` is the `Dockerfile`. The schema and its rules are in
[`docs/dev/delonixfile-and-vmfile.md`](../docs/dev/delonixfile-and-vmfile.md).

---

# Verifying the recipes

`vm build` returning 0 proves the builder ran, not that the image is right.
`scripts/verify-images.sh` builds the recipes and **reads what came out** — the
content of the qcow2, or of the booted VM — against what the recipe declared.

```bash
scripts/verify-images.sh --dry-run          # the plan and the preflight, nothing built
scripts/verify-images.sh                    # the 4 cloud distros, offline
scripts/verify-images.sh --self-test --no-build --seed-bases   # only: prove the checks can fail
```

Everything runs under an isolated `DELONIX_ROOT` that is removed at the end
(`--keep` to look at it). Your real image store is never touched. The exit code
is 0 only if every check that ran passed; a skipped check is reported and never
counted as passed. A markdown report is written next to the working files.

## The stages

| Stage | Flag | Needs | What it proves |
|---|---|---|---|
| shipped recipes | *(default)* | `virt-customize`, ~3 GiB per distro | `images/<distro>/vm.yaml` builds, is registered as a cloud-init image with the recorded defaults, and its file, env, hostname, removals and cleanup are in the qcow2 |
| **probe** | *(default)* | same | every schema field (`files`, `files.mode`, `hostname`, `env`, `users.sudo/groups/shell`, `run`, `remove.paths` after `run`) with values **the base image cannot already have** |
| self-test | `--self-test` | a base image | the checks above **fail** on an image nobody built — a check that cannot fail is not a check |
| packages | `--packages` | network | `packages.install` really installs (`jq`) and it runs in the guest |
| golden recipe | `--profile` | network | `profile: rootless` writes `/etc/delonix-image-release`, ships the `delonix` binary and leaves the account without a password |
| boot | `--boot` | KVM, `ssh` | the VM boots, you can log in with the injected key, `sudo` works, `/etc/motd` survived, a fresh `machine-id` exists, no systemd unit failed, SELinux is enforcing (Rocky, Fedora) |
| appliance | `--appliance <name>` | KVM, RAM, disk (see below) | the builder runs, a new image is registered with the right cloud-init flag, and the shipped `verify-*.sh` for that product passes |

**Why a probe recipe.** The base image already has a `delonix` account with sudo,
in the admin group, with `/bin/bash`. Checking those on the shipped recipe
passes even if `users:` did nothing. The probe recipe uses `probeuser`,
`PROBE_VAR` and `/opt/verify/probe.txt`, none of which exist in the base, so a
pass means the build did it. The self-test also lists, per distro, the shipped
checks that **also pass on the untouched base** — those detect a regression,
but cannot prove the recipe acted.

## Appliances: what each needs

| `--appliance` | RAM | Free disk | Notes |
|---|---|---|---|
| `opnsense` | — | 4 GiB | a verified download and a conversion; the lightest place to start |
| `proxmox` | 4 GiB | 20 GiB | the script builds Proxmox VE only (`--target pve`); build `pbs`, `pmg` or `pdm` by hand with `delonix vm build -f images/proxmox/vm.yaml --target <name>` |
| `truenas` | 6 GiB | 20 GiB | |
| `openstack` | 4 GiB | 65 GiB | pulls ~20 GiB of container images; the build allows 4 hours |
| `monitoring` | 4 GiB | 20 GiB | |
| `carbonio` | 6 GiB | 50 GiB | the build allows 90 minutes |
| `glpi` | 4 GiB | 12 GiB | |
| `wazuh` | 6 GiB | 30 GiB | |

A host that does not have the RAM or disk gets a **skip with the numbers**, not
a half-built image. Appliance builds download vendor media and verify its
SHA-256; they need `qemu-system-x86_64`, `xorriso` and `/dev/kvm`.

## A run on a capable host

```bash
cargo build -p delonix-runtime-bin
D="--delonix $PWD/target/debug/delonix"

scripts/verify-images.sh $D --seed-bases --self-test          # 1. offline distros + probe + self-test
scripts/verify-images.sh $D --only ubuntu --packages --profile # 2. network stages, one distro first
scripts/verify-images.sh $D --boot                             # 3. boot every image and log in
scripts/verify-images.sh $D --appliance opnsense               # 4. the lightest appliance, end to end
```

## What has and has not been run

Run on the development host (2026-09-21), against real images:

- **shipped recipes and probe, all four distros** — built offline; Ubuntu, Debian,
  Rocky and Fedora each pass every check (both the `dpkg` and the `rpm` branches);
- **self-test, all four distros** — every recipe-dependent check fails on the
  untouched base.

**Not run**, because this host has neither the disk nor the network for it — the
code for these stages is written and syntax-checked (`bash -n`), and its logic
was reviewed, but it has never executed:

- `--packages`, `--profile` (they install software from the internet);
- `--boot` (needs KVM and a hypervisor to create VMs);
- `--appliance` for every product.

Treat the first run of those stages on a capable host as the test of the script
itself: a failure there may be the script's, not the image's, and the log
(`build-*.log`, `boot-*.log`, `verify-*.log` under the working directory) says
which.
