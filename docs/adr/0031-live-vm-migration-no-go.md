# ADR-0031: Live VM migration is a NO-GO for both backends, as this engine is built today

## Status

Accepted.

## Context

`vm migrate` (B3 of the CLI restructuring plan) shipped as a stop-copy-start MVP: real
downtime, no shared storage, entirely over primitives that already existed (`vm stop`,
`qemu-img convert` to flatten the overlay, `scp`, `image vm import --appliance`, `vm create`).
The user asked, separately, for a genuine GO/NO-GO spike into whether TRUE live migration
(near-zero downtime) is achievable for either backend, given this engine's non-negotiable
constraints: **rootless-first**, **daemonless**, and a **local disk model** (a VM's overlay
lives at `<state-root>/vms/<name>.qcow2`, backed by a golden image in
`<state-root>/vm-images/`, nothing shared between hosts today).

This ADR records that spike's findings and the resulting decision.

## What was measured

**Cloud Hypervisor — NO-GO for disk state.** Its documented migration protocol
(`ch-remote send-migration`/`receive-migration`, `docs/live_migration.md` upstream) transfers
memory and device state only. The proof is in the upstream examples themselves: in the local
walkthrough, the destination VM is launched with **no `--disk` at all** — the disk path
crosses as a reference during migration, never as bytes; in the remote walkthrough, the
kernel/initramfs must sit at "the same directory on both machines... this is important for
the migration to succeed" — CH resolves backing files by path at receive time, with zero
data-plane transfer for them. There is no NBD-equivalent, no dirty-bitmap API, nothing in the
migration parameters that ships disk content. This matches this engine's own earlier
measurement of `vm.snapshot` (pause → snapshot → resume produces `config.json`+`state.json`+a
memory dump, no disk). **CH's own documentation implicitly assumes identical or shared
storage across hosts for any VM with real disk state** — never stated outright, because the
upstream examples are careful never to test a VM that has one.

**libvirt/QEMU NBD-based migration — technically real, architecturally incompatible.** The
mechanism itself is mature: during `PREPARE`, libvirtd auto-starts an NBD server on the
destination and mirrors guest writes to it, and `virsh migrate --copy-storage-all` uses it
transparently. But it requires **libvirtd-to-libvirtd network connectivity** — a
persistently-listening management port on both hosts (16509 plain / 16514 TLS, via
`virtproxyd-tcp`) plus an NBD data port from the migration range (49152–49215/tcp) opened
automatically on the destination. This engine's `LibvirtBackend` (`crates/delonix-vm/src/
lib.rs`) only ever talks to **local** libvirt today — `qemu:///session` (rootless, lazily
local) or `qemu:///system` (root, still local-only, used only for NAT/bridge networking).
Standing up `virtproxyd-tcp` on both ends would be a new, always-listening, privileged
network daemon per host — the exact class of exception `vm bridge` already is (EXPERIMENTAL,
privileged, opt-in), not something to fold into a rootless default path.

**A hybrid — incremental live block-sync of the disk, then a short CH memory-only
migration for cutover — is not technically sound as proposed.** `qemu-img`'s `-U`/
force-share flag is read-only only, and its own man page warns of inconsistent results from
concurrent metadata changes; Cloud Hypervisor exposes no dirty-bitmap or backup-job API an
external syncer could key off, and this engine already measured CH holding the qcow2 under an
exclusive lock while running (`qemu-img convert` fails with "Failed to lock byte 100" against
a live VM). A userspace diff/rsync loop against a live, mutating qcow2 has no
crash-consistency guarantee without guest quiescing — which reintroduces the downtime the
hybrid exists to avoid. Even a fully-synced copy still needs a final delta transferred with
zero guest writes between "last sync" and CH's own cutover, and CH offers no "pause I/O,
confirm quiesced, migrate" primitive to close that race. The variant that tolerates a short
freeze during the final delta is closer to "warm stop-copy-start" than true near-zero
downtime, and the block-diff engine it would need is comparable in effort to wiring
libvirt+NBD — not a cheaper middle path.

## Decision

**No live migration now, for either backend.** The stop-copy-start MVP (`vm migrate`, PR
#220) stands as the only migration path this engine offers, and is documented as such —
real downtime, explicitly. This is not deferred pending more engineering time; it is blocked
on two things this engine deliberately does not have today:

1. **Shared or replicated VM-image storage** (NFS, Ceph, or an equivalent) — which would let
   Cloud Hypervisor's existing memory-only migration actually work, since the disk would
   already be identical on both ends without this engine transferring it at all.
2. **An accepted, opt-in privileged daemon exception** for libvirt's NBD path (`virtproxyd
   -tcp`), following the same pattern `vm bridge` already established — a network-listening
   management port on two hosts, deliberately outside the rootless default.

Either one removes the actual blocker. Building around the gap (the hybrid) does not,
without new upstream Cloud Hypervisor plumbing this engine does not control.

## Consequences

- `vm migrate --live`/any near-zero-downtime flag is explicitly NOT planned — a future
  request for it should point here first, not restart the investigation.
- `AGENTS.md`'s VM migration section states plainly that live migration needs one of the two
  preconditions above, so the next person who asks "why not?" has the answer without
  re-running this spike.
- Nothing in `crates/delonix-vm` changes as a result of this ADR.

## Not done here, and why

- **Building the shared-storage precondition** — a platform-level storage decision (this
  engine already has one precedent, `ADR-0009`'s TrueNAS-provisioned network volumes; VM
  images specifically are not yet provisionable that way), out of scope for a CLI-migration
  spike.
- **Opting into the libvirt NBD daemon exception** — would need its own ADR naming a
  concrete consumer who needs live migration badly enough to accept a persistently-listening
  management port on two hosts, mirroring how `vm bridge`'s own privileged exception was
  justified. No such consumer exists yet.
- **Verifying non-`main` Cloud Hypervisor forks/branches** for any newer block-migration
  work — only the current upstream `main` documentation was checked.
