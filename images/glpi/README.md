# GLPI (asset inventory and ITSM) — build a qcow2 image

This folder builds GLPI 11 with the GLPI Agent, MariaDB and nginx/php-fpm. It is a cloud image with the software installed.

It is **a cloud image with software installed**: it still runs cloud-init, so `vm create --ssh-key/--hostname/--user-data` work.

```
images/glpi/
├── vm.yaml     the recipe: which builder, its arguments, what to record in the image
└── README.md   this file
```

The builder scripts and any answer files stay in `scripts/appliances/` (they are
shared with the CI workflow that publishes these images). `vm.yaml` selects the
script by **name**; it never contains a path, so a `vm.yaml` cannot make your
host run a file of its choosing.

## 1. What you need

- `qemu-system-x86_64` with **KVM**, `qemu-img`, `cloud-localds` or `xorriso`, `curl`
- ~4 GiB of RAM and 2 vCPUs for the build
- 12 GiB disk ceiling

Build from a **checkout of this repository**: the builder is looked up as
`scripts/appliances/build-<name>.sh` in the folder of `vm.yaml` or any folder
above it. The build downloads vendor media and **verifies its published SHA-256**
before using it; it is cached under `scripts/appliances/.media/` and reused.

## 2. Build it

From the repository root:

```bash
delonix vm build -f images/glpi/vm.yaml
```

Expect under half an hour (the build allows 30 minutes).

**Note.** The GLPI and agent downloads are checked against pinned SHA-256 values.

This image's versions are read from environment variables (see `appliance.env` in `vm.yaml`), so `-t` is free to be the image name, e.g. `-t mystack`.

```bash
# another version (example)
GLPI_VERSION=X.Y.Z delonix vm build -f images/glpi/vm.yaml
```

`vm build` runs the builder with its own scratch directory next to your image
store, streams the builder's output, then registers the resulting disk with the
recipe's defaults and deletes the scratch directory whether it worked or not.
The builder prints an `image vm import …` command at the end: that is what
`vm build` has just done for you, so you do not run it.

`--network` means nothing here and is refused: the builder decides its own
network. Tune the build with `appliance.env` (for example `MEM`, `SMP`,
`DISK_GB`, `BUILD_TIMEOUT`, `MEDIA_CACHE`) — the names each script reads are at
the top of `scripts/appliances/build-glpi.sh`.

## 3. Check the result

```bash
delonix image vm ls
delonix image vm describe glpi-11.0.9-r1
```

## 4. Prove it serves something

Booting is not serving. The repository ships a check for this image:

```bash
./scripts/appliances/verify-glpi.sh
```

## 5. Boot a VM from it

```bash
delonix vm create demo --disk glpi-11.0.9-r1 --wait
```

The VM takes the image's recorded vCPU and memory unless you pass `--vcpus` or
`--memory`. First-boot steps, default credentials and product notes are in
[`scripts/appliances/README.md`](../../scripts/appliances/README.md) (see the
*Credentials* section and the one for this product): they are documented there
once, next to the scripts that set them.

## 6. Publish it

```bash
delonix image vm push glpi-11.0.9-r1 ghcr.io/angolardevops/delonix-vm-appliances:<tag>
```

## Troubleshooting

- **`no builder 'glpi'`** — you are not inside a checkout of the
  repository, or `scripts/appliances/build-glpi.sh` is missing.
- **`builder … failed`** — the builder's own output is printed above that line;
  the scratch directory is already removed.
- **`No space left on device`** — see the disk figure in section 1; the scratch
  directory is created next to your image store (`$DELONIX_ROOT`), not in `/tmp`.
- **Nothing to build with `vm build .`** — from inside this folder it finds
  `vm.yaml` as it would in any other.
