# ADR-0017: Sign VM images with cosign, not with the release keypair

- **Status:** Accepted
- **Date:** 2026-08-18
- **Deciders:** walter, Claude

## Context

Reviewing `delonix-vm-base` against what a cloud requires of a base image
turned up seven gaps. Six are closed in code; this is the one that is a
decision rather than a patch.

A VM image is the artefact with the widest blast radius this project
publishes: it is a whole operating system, it is booted with privilege, and
`delonix cluster kubeadm` pulls it automatically. Today a `vm pull` proves
only that the bytes match the digest the registry itself served — which is
integrity against a corrupted transfer, not authenticity against a
compromised registry. The 2026-08-10 audit already found the neighbouring
version of this bug (digest-pinning that verified blobs against the
manifest but never the manifest against the requested digest) and rated it
HIGH.

Two signing mechanisms already exist in this repository, and that is the
fact that shapes the decision:

- **minisign** (`.github/workflows/release.yml`) signs `SHA256SUMS` for the
  release binaries. The keypair exists, the CI signs and then verifies with
  the public key embedded in `install.sh`, failing the release if they ever
  diverge.
- **cosign/sigstore** (`delonix-image::sign`, ECDSA P-256 over `ring`)
  verifies container image signatures, wired into `image pull --pubkey`.
  `verify_signature` fetches the manifest, computes `sha256-<hex>.sig`, and
  checks the simple-signing payload.

## Decision

VM images are signed and verified with **cosign**, reusing
`delonix_image::sign::verify_signature` unchanged, and `image vm pull`
grows the same `--pubkey` flag `image pull` already has.

minisign is NOT extended to cover them. Its job is to protect a download
that happens **before** any Delonix binary exists on the machine, which is
why its verifier has to be a shell script and its public key has to be
embedded in that script. A VM image is pulled BY a `delonix` that is
already installed and already carries an ECDSA verifier — using the shell
mechanism there would mean shipping a second trust root to solve a problem
the first one does not have.

**Not implemented in this change, deliberately.** The wiring is small (the
artefact is an ordinary OCI manifest, so `verify_signature` applies to it
as it stands; what it needs is an `ImageStore` for registry credentials and
the flag). What it is not is verifiable here: there is no signed VM
artefact in any registry to test against, and a signature path that has
never rejected a real bad signature is the same decorative verification
this project has already had to fix once. It ships when it can be exercised
end to end, and it takes a `delonix-runtime-sec` pass before it merges —
the standing rule for a new trust boundary.

## Alternatives considered

- **Extend minisign to VM images.** Rejected above: it solves bootstrapping,
  which is not this problem, and a second trust root doubles what an
  operator has to rotate.
- **Sign the qcow2 blob rather than the manifest.** Rejected: the manifest
  is what carries the annotations this series just added (distro, kernel,
  cloud-init, package inventory). Signing only the blob leaves every one of
  those forgeable while the signature still verifies.
- **Do nothing, rely on the digest.** Rejected: a digest verifies that the
  registry gave us what the registry said it would. That is precisely the
  assumption an attacker who owns the registry does not violate.

## Consequences

- Verification stays **opt-in** (`--pubkey`), matching `image pull`. Making
  it mandatory would break every image not yet signed, including images
  users build and push themselves — that is a separate decision, and it
  belongs after publishing is signing.
- One trust model for two artefact classes: an operator who already pins a
  cosign key for container images uses the same key here.
- Until this lands, `vm pull` from a compromised registry is undetected.
  That is stated here rather than left implicit, because the metadata this
  series added (`built_by`, `base_sha256`, the package inventory) reads like
  provenance and is not: it is self-reported by the image, and self-reported
  provenance is exactly what a signature exists to make trustworthy.
