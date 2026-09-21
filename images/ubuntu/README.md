# Ubuntu — build a qcow2 image

This folder builds an Ubuntu cloud image with `delonix vm build`. Everything the build
needs is here. It works **offline inside the guest** (`network: false`), so the
first build succeeds on any host; the only download is the Ubuntu base image,
checksum-verified, and only when it is not already in your local store.

```
images/ubuntu/
├── vm.yaml            the recipe — every parameter is documented inline
├── cloud-init/
│   └── user-data      default first-boot config baked into the image
├── artifacts/
│   └── motd           a file copied into the image (see `files:` in vm.yaml)
└── README.md          this file
```

Family: Debian family (`apt`).

## 1. What you need

| Requirement | Why | Check |
|---|---|---|
| `delonix` | the builder | `delonix version` |
| `virt-customize`, `qemu-img` | edit and convert the disk offline | `command -v virt-customize qemu-img` |
| ~3 GiB free in `/tmp` and in the image store | base image plus a working copy | `df -h /tmp ~` |

Install the tools on Debian/Ubuntu with `sudo apt install libguestfs-tools qemu-utils`
(on Fedora/Rocky: `sudo dnf install guestfs-tools qemu-img`).
`./scripts/install.sh --with-image-build` also handles two host quirks that make
`virt-customize` fail with errors that look like network problems; see
*Troubleshooting*.

## 2. Build it

From the repository root:

```bash
delonix vm build -f images/ubuntu/vm.yaml -t 26.04
```

`-t` is two things at once: the name of the result **and** `${TAG}` inside
`vm.yaml`, which this file uses as the Ubuntu release (`26.04`, `24.04`). So `-t` must
be a release here.

```bash
delonix vm build -f images/ubuntu/vm.yaml -t 24.04
```

From inside the folder, exactly like `docker build .`:

```bash
cd images/ubuntu && delonix vm build .
```

`vm build .` looks for a `vm.yaml` in the folder (then a `VMfile`, then falls back
to the built-in golden recipe). With `-t` omitted, the image's own `tag:` is used
(`ubuntu-26.04` here). To name the image differently from the release, read the
release from another variable: change `release:` to `"${RELEASE:-26.04}"` and run
`RELEASE=24.04 delonix vm build … -t ubuntu-24.04`. Any `${NAME}` other than
`TAG` is read from the environment.

The base is resolved in this order: your local store (`delonix-vm-base:ubuntu-<release>`), the
official Delonix base on ghcr, then the distro's own cloud image. Add
`DELONIX_VERBOSE=1` to unfold the output of each step.

## 3. Check the result

```bash
delonix image vm ls
delonix image vm describe 26.04
```

`describe` shows the size, the distro (`ubuntu:26.04`), the recorded defaults
(2 vCPU, 2G) and that the image runs cloud-init.

## 4. Boot a VM from it

```bash
delonix vm create web1 --disk 26.04 --ssh-key @$HOME/.ssh/id_ed25519.pub --wait
```

The VM takes the image's recorded vCPU/memory unless you pass `--vcpus` or
`--memory`. `--ssh-key` is applied on first boot by cloud-init, to the
`delonix` account the recipe created (in `adm`, with passwordless sudo).

## 5. What this recipe does, and how to change it

It adds a file (`/etc/motd`, mode 0644), an `env` variable, and a `delonix`
account. It **removes** `snapd` and the `ModemManager` service and the man and info pages, then
cleans up: logs, shell history, `/tmp` and `/etc/machine-id` (so each VM cloned
from the image gets its own identity).

| You want to… | Edit |
|---|---|
| a different release | `-t <release>` — no edit needed |
| a bigger disk | `size: 20G` (grown *before* any step runs) |
| install software | `network: true`, then `packages.install: [...]` (installed with `apt`) |
| **remove** packages | `remove.packages: [...]` |
| **remove** files or directories | `remove.paths: [...]` |
| **remove** an account or a service | `remove.users: [...]`, `remove.services: [...]` (disabled *and* masked) |
| a user with sudo and keys | `users: [{name: ops, sudo: true, groups: [adm], ssh_keys: [~/.ssh/id_ed25519.pub]}]` |
| put a file in the image | drop it in `artifacts/`, then `files: [{src, dst, mode}]` — `dst` is the full path of the file inside the image |
| run a command | `run: ['...']` |
| first-boot config | edit `cloud-init/user-data` |
| keep something the cleanup removes | `cleanup: {logs: false}` |
| the defaults a VM gets | `vcpus`, `memory`, `hypervisor` |

Relative paths in `vm.yaml` (`files.src`, `cloud_init`) are relative to **the
folder of `vm.yaml`**, wherever you run the command from. `ssh_keys` takes a path
(`~/` is expanded; a relative path is relative to where you run the command) or
the public key itself.

Removal runs **after** installs and `run:` steps, so it can also prune what they
pulled in. `remove.paths` must be absolute, must not contain `..`, and cannot be
a top-level system directory (`/etc`, `/usr`, …) — remove something *inside* it.
The recipe keeps `/usr/share/doc` on purpose: it holds the licence files a
published image has to redistribute.

The file is **strict**: an unknown key is an error, and a field the chosen route
cannot honour is refused by name rather than ignored.

### Ubuntu-specific

The base image does **not** ship `qemu-guest-agent`. Without it the hypervisor cannot read the VM's IP or freeze its filesystem for a consistent snapshot. Add it with `network: true` and `packages.install: [qemu-guest-agent]`.

## 6. With Kubernetes (or rootless-only) instead

The same schema selects the built-in golden recipes:

```yaml
images:
  ubuntu-rootless:
    tag: ubuntu-rootless-26.04
    profile: rootless       # or: k8s (Ubuntu and Debian only)
    distro: ubuntu
    release: "26.04"
```

Those recipes install their own software and ignore custom fields like
`hostname:`, so `vm build` rejects the combination and tells you which field.
`profile: k8s` is only available for Ubuntu/Debian (apt).

## Troubleshooting

- **`${TAG} is not set`** — the file uses `${TAG}` with no default; pass `-t`,
  or write `${TAG:-26.04}`.
- **`packages needs the network inside the guest`** — set `network: true`, or
  pass `--network`. It is opt-in because a build that reaches the internet is
  not reproducible.
- **`Temporary failure resolving …` inside the build** — `virt-customize` has no
  network in its appliance when `network: true`. It looks like your DNS and is
  not. Run `./scripts/install.sh --with-image-build` once, or keep
  `network: false`.
- **`Permission denied` on `/boot/vmlinuz-*`** — the host kernel is `0600`
  (Debian/Ubuntu hardening). The same installer flag offers the `chmod`.
- **`No space left on device`** — the base image and a working copy need ~3 GiB
  free in the temp directory.
