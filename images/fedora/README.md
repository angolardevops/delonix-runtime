# Fedora — build a qcow2 image

This folder builds a Fedora cloud image with `delonix vm build`. Everything the build
needs is here. It works **offline inside the guest** (`network: false`), so the
first build succeeds on any host; the only download is the Fedora base image,
checksum-verified, and only when it is not already in your local store.

```
images/fedora/
├── vm.yaml            the recipe — every parameter is documented inline
├── cloud-init/
│   └── user-data      default first-boot config baked into the image
├── artifacts/
│   └── motd           a file copied into the image (see `files:` in vm.yaml)
└── README.md          this file
```

Family: RHEL family (`dnf`).

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
delonix vm build -f images/fedora/vm.yaml -t 42
```

`-t` is two things at once: the name of the result **and** `${TAG}` inside
`vm.yaml`, which this file uses as the Fedora release (`42`). So `-t` must
be a release here.


From inside the folder, exactly like `docker build .`:

```bash
cd images/fedora && delonix vm build .
```

`vm build .` looks for a `vm.yaml` in the folder (then a `VMfile`, then falls back
to the built-in golden recipe). With `-t` omitted, the image's own `tag:` is used
(`fedora-42` here). To name the image differently from the release, read the
release from another variable: change `release:` to `"${RELEASE:-42}"` and run
`RELEASE=42 delonix vm build … -t fedora-42`. Any `${NAME}` other than
`TAG` is read from the environment.

The base is resolved in this order: your local store (`delonix-vm-base:fedora-42`), the
official Delonix base on ghcr, then the distro's own cloud image. Add
`DELONIX_VERBOSE=1` to unfold the output of each step.

## 3. Check the result

```bash
delonix image vm ls
delonix image vm describe 42
```

`describe` shows the size, the distro (`fedora:42`), the recorded defaults
(2 vCPU, 2G) and that the image runs cloud-init.

## 4. Boot a VM from it

```bash
delonix vm create web1 --disk 42 --ssh-key @$HOME/.ssh/id_ed25519.pub --wait
```

The VM takes the image's recorded vCPU/memory unless you pass `--vcpus` or
`--memory`. `--ssh-key` is applied on first boot by cloud-init, to the
`delonix` account the recipe created (in `wheel`, with passwordless sudo).

## 5. What this recipe does, and how to change it

It adds a file (`/etc/motd`, mode 0644), an `env` variable, and a `delonix`
account. It **removes** man and info pages and the man and info pages, then
cleans up: logs, shell history, `/tmp` and `/etc/machine-id` (so each VM cloned
from the image gets its own identity).

| You want to… | Edit |
|---|---|
| a different release | `-t <release>` — no edit needed |
| a bigger disk | `size: 20G` (grown *before* any step runs) |
| install software | `network: true`, then `packages.install: [...]` (installed with `dnf`) |
| **remove** packages | `remove.packages: [...]` |
| **remove** files or directories | `remove.paths: [...]` |
| **remove** an account or a service | `remove.users: [...]`, `remove.services: [...]` (disabled *and* masked) |
| a user with sudo and keys | `users: [{name: ops, sudo: true, groups: [wheel], ssh_keys: [~/.ssh/id_ed25519.pub]}]` |
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

### Fedora-specific

Use the plain release (`42`), which matches the official base image. A release with a build number (`42-1.1`) is what the distro's own download page uses; it does not match the official base, so the builder ignores the local copy and downloads the whole cloud image instead. `qemu-guest-agent` is already in the base image. SELinux is enforcing: the build relabels the disk as its last step.

## 6. With Kubernetes (or rootless-only) instead

The same schema selects the built-in golden recipes:

```yaml
images:
  fedora-rootless:
    tag: fedora-rootless-42
    profile: rootless
    distro: fedora
    release: "42-1.1"      # the golden recipe wants release AND build
```

Those recipes install their own software and ignore custom fields like
`hostname:`, so `vm build` rejects the combination and tells you which field.
Kubernetes (`profile: k8s`) is not available for Fedora.

## Troubleshooting

- **`${TAG} is not set`** — the file uses `${TAG}` with no default; pass `-t`,
  or write `${TAG:-42}`.
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
