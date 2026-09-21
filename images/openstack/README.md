# OpenStack (Kolla Ansible host) — build a qcow2 image

This folder builds a host image with Kolla Ansible and every container image of one OpenStack release already inside it. It is not an appliance: the deployment itself is done later, on the real host, because the addresses it needs cannot be known at build time.

It is **a cloud image with software installed**: it still runs cloud-init, so `vm create --ssh-key/--hostname/--user-data` work.

```
images/openstack/
├── vm.yaml     the recipe: which builder, its arguments, what to record in the image
└── README.md   this file
```

The builder scripts and any answer files stay in `scripts/appliances/` (they are
shared with the CI workflow that publishes these images). `vm.yaml` selects the
script by **name**; it never contains a path, so a `vm.yaml` cannot make your
host run a file of its choosing.

## 1. What you need

- `qemu-system-x86_64` with **KVM**, `qemu-img`, `xorriso`/`cloud-localds`, `curl`
- ~4 GiB of RAM and 4 vCPUs for the build
- a 60 GiB **ceiling** (thin qcow2) — the pulled container images are ~20 GiB
- a good link: the pull is ~20 GiB

Build from a **checkout of this repository**: the builder is looked up as
`scripts/appliances/build-<name>.sh` in the folder of `vm.yaml` or any folder
above it. The build downloads vendor media and **verifies its published SHA-256**
before using it; it is cached under `scripts/appliances/.media/` and reused.

## 2. Build it

From the repository root:

```bash
delonix vm build -f images/openstack/vm.yaml -t 2026.1
```

Expect over an hour on a slow link (the build allows up to 4 hours).

**Note.** The release name (`2026.1`) is what `-t` means here, as in `-t 2025.2`.

With `-t`, the value is the **version** (`${TAG}` in `vm.yaml`) and also the name of the image in your store.

```bash
# another version
delonix vm build -f images/openstack/vm.yaml -t <version>
```

`vm build` runs the builder with its own scratch directory next to your image
store, streams the builder's output, then registers the resulting disk with the
recipe's defaults and deletes the scratch directory whether it worked or not.
The builder prints an `image vm import …` command at the end: that is what
`vm build` has just done for you, so you do not run it.

`--network` means nothing here and is refused: the builder decides its own
network. Tune the build with `appliance.env` (for example `MEM`, `SMP`,
`DISK_GB`, `BUILD_TIMEOUT`, `MEDIA_CACHE`) — the names each script reads are at
the top of `scripts/appliances/build-openstack.sh`.

## 3. Check the result

```bash
delonix image vm ls
delonix image vm describe 2026.1
```

## 4. Check the result

`delonix image vm ls` and `delonix image vm describe <name>` show the size, the recorded defaults and whether the image runs cloud-init. OpenStack itself is deployed later, on the real host, by `openstack_aio` in `delonix-deploy`.

## 5. Boot a VM from it

```bash
delonix vm create demo --disk 2026.1 --wait
```

The VM takes the image's recorded vCPU and memory unless you pass `--vcpus` or
`--memory`. First-boot steps, default credentials and product notes are in
[`scripts/appliances/README.md`](../../scripts/appliances/README.md) (see the
*Credentials* section and the one for this product): they are documented there
once, next to the scripts that set them.

## 6. Publish it

```bash
delonix image vm push 2026.1 ghcr.io/angolardevops/delonix-vm-appliances:<tag>
```

## Troubleshooting

- **`no builder 'openstack'`** — you are not inside a checkout of the
  repository, or `scripts/appliances/build-openstack.sh` is missing.
- **`builder … failed`** — the builder's own output is printed above that line;
  the scratch directory is already removed.
- **`No space left on device`** — see the disk figure in section 1; the scratch
  directory is created next to your image store (`$DELONIX_ROOT`), not in `/tmp`.
- **Nothing to build with `vm build .`** — from inside this folder it finds
  `vm.yaml` as it would in any other.
