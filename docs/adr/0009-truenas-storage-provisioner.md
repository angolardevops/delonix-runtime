# ADR-0009: Provision TrueNAS datasets over its API, as a separate crate

- **Status:** Proposed
- **Date:** 2026-08-10
- **Deciders:** Walter Angolar

## Context

`kind: Storage` already turns a NAS share into a named volume: `storage.rs`'s
`build_mount` maps `nfs`/`cifs`/`webdav` onto a `MountSpec` and
`VolumeStore::ensure_mounted` mounts it (validated end-to-end against a real
NFS server — see `AGENTS.md`). What it does **not** do is create anything on
the NAS: the dataset, the share and its permissions must already exist,
hand-made, before a manifest can reference them.

With a TrueNAS SCALE appliance now buildable from this repo
(`truenas-scale:25.10`), the gap is worth closing: a `kind: Volume` should be
able to *provision* its storage, not only consume it.

Measured before proposing anything:

- **No TrueNAS API code exists** anywhere in the workspace (same grep as
  ADR-0008).
- **`delonix-volume` has three dependencies**: `delonix-runtime-core`, `serde`,
  `serde_json` (`crates/delonix-volume/Cargo.toml`). Adding an HTTP client
  there has the same cost as in ADR-0008, for the same reason.
- **A quota field already exists** — `Volume::quota_bytes`
  (`crates/delonix-volume/src/lib.rs:37`), with a documented overflow trap
  already fixed. It is enforced locally where privilege allows; a ZFS dataset
  quota is the natural remote counterpart, and the field does not need
  inventing.

Guardrails in play:

- **#2 (boundary with the private PaaS).** "RBAC" here means **permissions on
  the NAS** — dataset owner, group, ACL, quota — set through TrueNAS's own API.
  Delonix is a *client* configuring the appliance. A model of *which delonix
  user may create which volume* would be tenancy, and belongs to the PaaS.
  This ADR does not introduce one.
- **#4 (engine crates dependency-clean).** Same as ADR-0008.
- **#6 (no silent failure).** Anything the API refuses — a quota below what the
  dataset already holds, a share type the target does not export — must surface,
  never be dropped on the way to a mount that then behaves differently than
  asked.

## Decision

1. **A new crate `delonix-truenas`** speaks the TrueNAS API (create/read/
   update/delete a dataset, set quota, expose an NFS/SMB share, set ownership
   and ACL). `delonix-volume` gains no dependency.

2. **`kind: Volume` gains an optional provisioner block** naming the target and
   the parameters (`dataset`, `quota`, `owner`, share type). Without it,
   behaviour is exactly what it is today; a volume that has it is created on the
   NAS and then mounted through the **existing** `build_mount`/`ensure_mounted`
   path — no second mounting mechanism.

3. **Deletion is opt-in and explicit.** Removing a `kind: Volume` must not
   destroy a dataset by default. This repo has already shipped one bug where
   removal deleted accounting before data (`volumes rm` under subuid, v0.37.0);
   the destructive path here needs its own flag and its own confirmation.

4. **Credentials come from `kind: Secret`** (API key), never a manifest
   literal.

5. **Quotas are reported, not assumed.** After provisioning, the observed quota
   is read back and stored, so `volumes inspect` shows what the NAS actually
   enforces rather than what was requested — the `Usage { bytes, unreadable }`
   discipline this repo already applies to local measurement.

## Alternatives considered

- **Extend `kind: Storage` instead of `kind: Volume`.** `Storage` is the
  *consume an existing share* object and the two intents read differently in a
  manifest. Worth revisiting if the field sets converge, but starting there
  would overload a Kind that currently has one clear meaning.
- **Shell out to `midclt` over SSH** on the TrueNAS host. Rejected for the same
  reason as `pvesh` in ADR-0008: it needs shell access instead of an API key,
  and interpolating into a remote shell is a class of bug this repo has already
  paid for.
- **A generic "CSI-like" provisioner interface** covering TrueNAS, Ceph and
  others. Rejected as premature: there is one target and no second consumer.
  ADR-0002 set the rule — promote an abstraction when the second implementation
  exists, not before.
- **Do nothing** — keep provisioning manual, which is what every validated
  deployment has done so far. This is the honest baseline the ADR must beat.

## Consequences

**Easier.** A stack becomes self-contained: the manifest that declares a
workload can also declare the storage it needs, with its size limit, instead of
depending on a NAS someone prepared by hand.

**Harder.** Delonix starts holding credentials that can destroy data on another
machine. That raises the stakes on the secret handling and on the deletion path,
and argues for a `delonix-runtime-sec` pass before this merges (guardrail #5's
spirit, even though no privilege boundary of ours moves).

**Debt accepted.** TrueNAS's API is versioned and has changed shape across major
releases (the installer's own RPC surface differs between 24.x and 25.x — read
first-hand while building the appliance). Pinning to one major and failing
clearly on others is better than best-effort compatibility that silently does
the wrong thing.

**Testing.** Unlike Proxmox, this *is* testable here: the TrueNAS appliance
built in this series boots and serves its API on this host, so CRUD, quota
enforcement and permission changes can be exercised end-to-end against a real
target rather than mocked.
