# ADR-0018: VM images stay amd64 until the release pipeline publishes arm64

- **Status:** Accepted
- **Date:** 2026-08-18
- **Deciders:** walter, Claude

## Context

Multi-architecture images are on every cloud's list of what a base image
should be, and `delonix build --platform linux/<arch>` already does the
equivalent for container images (arch-aware base resolution, and a preflight
against `/proc/sys/fs/binfmt_misc/qemu-<arch>` before starting a cross-arch
build).

The VM image path has none of it: the four `download_*_base` functions
hardcode `amd64`/`x86_64` in both the file name and the URL.

Making those four functions take an architecture is an afternoon. It is also
not the thing standing in the way. Two blockers, both measured rather than
assumed:

1. **The release pipeline publishes no arm64 binary.** `release.yml` builds
   exactly `delonix-x86_64-linux`, `delonix-x86_64-v3-linux` and the two
   `delonix-cri` equivalents. Every VM image recipe injects one of those into
   the guest (`install_cri_steps`, `rootless_customization_steps`), and
   `resolve_delonix_bin`/`resolve_cri_bin` fall back to downloading the
   `x86_64` asset by name. An arm64 base image would therefore be an arm64
   guest carrying an amd64 engine — an image that builds, publishes, boots,
   and fails at the first `delonix` command.
2. **No cross-arch emulation on the build host.** `/proc/sys/fs/binfmt_misc`
   on the development host registers `python3.12` and nothing else — no
   `qemu-aarch64`. `virt-customize` runs commands INSIDE the guest, so
   without binfmt every `RunCommand` of every recipe fails. Unlike the
   container path, there is no useful subset that works without it.

## Decision

VM images stay amd64-only, and the ordering is fixed: **arm64 release
artefacts first, then arm64 VM images.**

Until then, the architecture is not parameterised at all. A half-plumbed
`--arch` that resolves an arm64 cloud image and then injects an amd64 binary
would be the accept-and-silently-ignore failure this repository names as its
worst — and it would fail late, inside a published guest, rather than at the
command that asked for it.

## Alternatives considered

- **Parameterise the downloads now, refuse `--arch aarch64` with a clear
  error.** Tempting, and rejected on cost/benefit: the refusal is the entire
  user-visible behaviour, and it can be written in one line the day the
  blocker lifts. Carrying arch through four download functions, the metadata,
  the cache paths and the OCI tags to power a refusal is dead plumbing, which
  this repository has had to delete three times (`publish_port_allow`,
  `delonix_net::Net`, `join_netns`).
- **Build arm64 images without the engine inside.** Rejected: the engine in
  the guest is what `delonix-vm-base` IS. Without it the artefact is a stock
  cloud image with a `delonix` account.
- **Cross-compile the engine as part of the image build.** Rejected: it would
  make an image build depend on a full Rust toolchain and a cross linker,
  when the release pipeline is the thing that already builds binaries and is
  the right place for it.

## Consequences

- `delonix-vm-base`, `delonix-vm-k8s` and `delonix-vm-appliances` remain
  amd64. Anyone on Graviton/Ampere cannot use them, and that is a real
  limitation stated rather than hidden behind a flag that appears to offer a
  choice.
- The work is now ordered and small: add an arm64 target to `release.yml`
  (the runner exists), teach `resolve_delonix_bin`/`resolve_cri_bin` the
  asset name, then parameterise the four downloads and add the binfmt
  preflight `delonix build --platform` already has.
- The build host needs `qemu-user-static` + binfmt registered. Same class of
  host prerequisite as `isc-dhcp-client` and the readable `/boot/vmlinuz`
  that `install.sh --with-image-build` already handles.
