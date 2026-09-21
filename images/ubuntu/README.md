# Ubuntu — build a qcow2 image

This folder builds an Ubuntu cloud image with `delonix vm build`. Everything the
build needs is here; nothing is fetched from outside except the Ubuntu cloud
image itself (checksum-verified) and, if you ask for it, packages.

```
images/ubuntu/
├── vm.yaml            the recipe — every parameter is documented inline
├── cloud-init/
│   └── user-data      default first-boot config baked into the image
├── artifacts/
│   └── motd           a file copied into the image (see `files:` in vm.yaml)
└── README.md          this file
```

## 1. What you need

| Requirement | Why | Check |
|---|---|---|
| `delonix` | the builder | `delonix version` |
| `virt-customize`, `qemu-img` | edit and convert the disk offline | `command -v virt-customize qemu-img` |
| ~3 GiB of free disk | the base download plus a working copy | `df -h .` |
| Internet, **only** because `vm.yaml` sets `network: true` | to install packages | — |

Install the tools on Debian/Ubuntu with `sudo apt install libguestfs-tools qemu-utils`.
`./scripts/install.sh --with-image-build` also handles two host quirks that make
`virt-customize` fail with errors that look like network problems; see
*Troubleshooting*.

## 2. Build it

From the repository root:

```bash
delonix vm build -f images/ubuntu/vm.yaml -t 26.04
```

`-t` is two things at once: the release (it fills `${TAG}` in `vm.yaml`) and the
name of the result. The same file therefore builds any release:

```bash
delonix vm build -f images/ubuntu/vm.yaml -t 24.04
```

Or, from inside the folder, exactly like `docker build .`:

```bash
cd images/ubuntu && delonix vm build .
```

`vm build .` looks for a `vm.yaml` in the folder (then a `VMfile`, then falls
back to the built-in golden recipe). With `-t` omitted, the image's own `tag:`
is used (`ubuntu-26.04` here).

It prints a line per stage, then one per step, and ends by printing the tag it
stored. Add `DELONIX_VERBOSE=1` to unfold the output of each step.

## 3. Check the result

```bash
delonix image vm ls
delonix image vm describe 26.04
```

`describe` shows the size, the distro, the recorded defaults (2 vCPU, 2G) and
that the image runs cloud-init.

## 4. Boot a VM from it

```bash
delonix vm create web1 --disk 26.04 --ssh-key @$HOME/.ssh/id_ed25519.pub --wait
```

The VM takes the image's recorded vCPU/memory unless you pass `--vcpus` or
`--memory`. `--ssh-key` is applied on first boot by cloud-init, to the
`delonix` account the recipe created.

## 5. Change what goes in, and what comes out

Open `vm.yaml`; each block is one decision.

| You want to… | Edit |
|---|---|
| a different release | `-t <release>` — no edit needed |
| a bigger disk | `size: 20G` (grown *before* any step runs) |
| more packages | `packages.install: [...]` (needs `network: true`) |
| **remove** packages | `remove.packages: [snapd]` |
| **remove** files or directories | `remove.paths: [/usr/share/doc]` |
| **remove** an account or a service | `remove.users: [...]`, `remove.services: [...]` (disabled *and* masked) |
| a user with sudo and keys | `users: [{name: ops, sudo: true, ssh_keys: [~/.ssh/id_ed25519.pub]}]` |
| put a file in the image | drop it in `artifacts/`, then `files: [{src, dst, mode}]` |
| run a command | `run: ['...']` |
| first-boot config | edit `cloud-init/user-data` |
| keep something the cleanup removes | `cleanup: {logs: false}` |
| the defaults a VM gets | `vcpus`, `memory`, `hypervisor` |

`ssh_keys` takes a path (`~/` is expanded; a relative path is relative to where
you run the command, not to this folder) or the public key itself.

Removal runs **after** installs and `run:` steps, so it can also prune what they
pulled in. `remove.paths` must be absolute, must not contain `..`, and cannot be
a top-level system directory (`/etc`, `/usr`, …) — remove something *inside* it.

`cleanup` defaults are the ones a published image wants: package cache, logs,
shell history and `/tmp` are emptied, and `/etc/machine-id` is truncated so each
VM cloned from the image gets its own identity. Set any of them to `false` to
keep it.

The file is **strict**: an unknown key is an error, and a field the chosen
route cannot honour is refused by name rather than ignored.

## 6. Ubuntu with Kubernetes (or rootless-only) instead

The same schema selects the built-in golden recipes:

```yaml
images:
  ubuntu-k8s:
    tag: ubuntu-k8s-1.36
    profile: k8s            # or: rootless
    distro: ubuntu
    release: "24.04"
    k8s: { version: "1.36", offline: true }
```

Those recipes install their own software and ignore custom fields like
`hostname:`, so `vm build` rejects the combination and tells you which field.

## Troubleshooting

- **`${TAG} is not set`** — the file uses `${TAG}` with no default; pass `-t`,
  or write `${TAG:-26.04}`.
- **`packages needs the network inside the guest`** — set `network: true`, or
  pass `--network`. It is opt-in because a build that reaches the internet is
  not reproducible.
- **`Temporary failure resolving …` inside the build** — `virt-customize` has no
  network in its appliance. It looks like your DNS and is not. Run
  `sudo ./scripts/install.sh --with-image-build` once, or build with a
  `vm.yaml` that needs no packages (`network: false`).
- **`Permission denied` on `/boot/vmlinuz-*`** — the host kernel is `0600`
  (Debian/Ubuntu hardening). The same installer flag offers the `chmod`.
- **`No space left on device`** — the base image and a working copy need ~3 GiB
  free in the temp directory.
