# ADR-0080: Expose the engine's own init as an OCI-runtime CLI (`delonix oci-runtime`)

- **Status:** Proposed (2026-10-24, design only — no code). Awaiting the owner's GO. Grounded in the
  measured gap **GAP-RT-4** of `reports/conformance/B1-oci-runtime-baseline.md`.
- **Date:** 2026-10-24
- **Deciders:** Walter Angolar (owner)
- **Relates to:** the `delonix image export` bundle work (ADR-adjacent, `cmd/image.rs`
  `build_runtime_spec`; GAP-RT-1/2/3 closed 2026-10-24), the daemonless identity in `AGENTS.md`,
  ADR process guardrails 1 (daemonless), 5 (GO/NO-GO before a new privilege boundary) and 6 (no
  silent failure). Enables the `opencontainers/runtime-tools` validation suite to run **against
  Delonix** (today it can only run against runc/crun — see B1).

## Context

Measured in B1: Delonix is a bundle **producer** (`image export` emits a conformant `config.json` +
rootfs, validated by `oci-runtime-tool`) and has its own container **init**
(`delonix-linux::container_init`/`create`, 104 `unsafe` blocks doing clone/mount/pivot_root/setns/
seccomp/uid-map). But it does **not** expose the runc command-line contract
(`create|start|state|kill|delete <id> --bundle <dir> --root <state>`). Consequences:

1. **Not measurable like runc/crun.** The `runtime-tools` *validation suite* drives an OCI runtime
   through that CLI. With no CLI, Delonix's runtime behaviour is only covered by the in-repo E2E/chaos
   battery — there is no upstream, comparable number against containerd's runtimes.
2. **Not a drop-in.** containerd/CRI-O, and anything speaking the OCI runtime contract, invoke
   `runc`-shaped binaries. Delonix cannot sit where they sit, even though its init does the same job.
3. **Asymmetry.** Delonix *produces* OCI bundles it cannot itself *run from disk* — `container run`
   starts from an **image** in the Store, never from an external bundle dir + `config.json`.

`delonix container create` is the closest existing primitive: it writes a `Created` record and
prepares the rootfs without starting, then `container start` launches it. But it is **image-keyed**
(a Store record), not **bundle-keyed** (an external dir the caller owns). The runc contract is the
inverse: the caller owns the bundle and the runtime owns only a small state file per container.

A building block already exists: `seccomp_profile::parse` consumes an OCI seccomp JSON, and the init
already honours namespaces/capabilities/seccomp/mounts/rlimits/masked-paths (B1 "OCI Runtime" table).

## Decision

Add `delonix oci-runtime` — a subcommand implementing the **runc command-line contract** over the
existing init, so Delonix *is* an OCI runtime, not only a producer of bundles for one.

```
delonix oci-runtime [--root <state-dir>] create <id> --bundle <dir>
delonix oci-runtime [--root <state-dir>] start  <id>
delonix oci-runtime [--root <state-dir>] state  <id>     # prints the OCI state JSON
delonix oci-runtime [--root <state-dir>] kill   <id> [signal]
delonix oci-runtime [--root <state-dir>] delete <id> [--force]
```

### Verb mapping (runc CLI → Delonix internals)

| runc verb | Does | Maps to |
|---|---|---|
| `create <id> --bundle <dir>` | parse `<dir>/config.json`, prepare, DO NOT run the process | a **bundle-keyed** record in `--root` (not the image Store): `ociVersion/id/status=created/bundle/pid=0`; prepare rootfs mounts from the bundle, honour `config.json` |
| `start <id>` | run the process | the existing launch path (`container_init`/`reexec_mapped`), fed from the bundle record instead of an image record |
| `state <id>` | print OCI state | serialize `{ociVersion, id, status, pid, bundle}` from the `--root` record |
| `kill <id> [sig]` | signal | `runtime::send_signal` (already behind `container kill`) |
| `delete <id>` | tear down + forget | the existing teardown (netns/cgroup/mounts), then drop the `--root` record — zero orphans (the §6 invariant) |

### `config.json` — honoured vs refused (NEVER silently ignored — guardrail 6)

- **Honoured (init already does these):** process (args/env/cwd/user/caps/noNewPrivileges), namespaces
  (incl. `user` + uid/gid mappings — the rootless path from GAP-RT-2), mounts, `linux.seccomp`
  (via `seccomp_profile::parse`), masked/readonly paths, rlimits.
- **Refused with a clear error (not applied-and-ignored):** anything the init cannot enforce —
  e.g. OCI `hooks` (the engine has no OCI-hook stdin-state protocol), `linux.resources` fields with
  no delegated controller on the host (surfaced like `system resources` already does), AppArmor
  profile load in rootless. A refused field names itself and the reason; it is never dropped.

### State lives in `--root`, not the image Store

A bundle container is **not** an image workload. Its record lives in the `--root` state dir (runc
default shape), keyed by `<id>`, carrying only `{ociVersion,id,status,pid,bundle}`. This keeps the
image-keyed Store and the bundle-keyed state disjoint, and lets a caller (kubelet/containerd/the
validation suite) own the bundle lifecycle without the Store inventing an image for it.

## Guardrails check (ADR process)

- **Daemonless (1):** unchanged. The runc contract is inherently daemonless — the runtime binary is
  invoked per operation; between `create` and `start` the only persistence is the per-container
  supervisor the engine already forks (same model as `conmon`), plus the `--root` state file. **No
  central daemon is introduced.**
- **No new privilege boundary (5):** the init, userns mapping, seccomp and mount machinery are the
  SAME ones `container run` already uses — this CLI is a new *entry point* to existing privileged
  code, not a new boundary. **No GO/NO-GO spike required**, but the owner should confirm this
  reading before build, since "a caller-owned bundle" widens the *input* surface (arbitrary
  `config.json` from an untrusted caller) even if the mechanism is unchanged. See Open question 1.
- **No silent failure (6):** every `config.json` field is honoured or refused-by-name (above).

## Consequences

- **Enables the upstream number.** `runtime-tools` validation can run against
  `delonix oci-runtime`, giving a pass/fail per spec section comparable to runc/crun — closing the
  one conformance axis B1 could not measure.
- **Enables drop-in use.** containerd/CRI-O (and the CRI path itself, which today shells its own
  init) could call `delonix oci-runtime` as the low-level runtime — reinforcing "one engine:
  container + VM + k8s".
- **Cost.** Honouring an arbitrary third-party `config.json` fully is large; hence the phasing.
- **Input-surface risk.** A caller-owned bundle is attacker-controlled input to privileged code
  (the same class the whiteout/`--add-host` audits addressed). The refuse-by-name rule and the
  existing `safe_bind_target`/path-confinement must cover every bundle-sourced path. Open question 1.

## Phasing

- **Phase 1 (minimal, measurable):** `create/start/state/kill/delete` honouring the subset the init
  already enforces; everything else refused-by-name. Target: a published `runtime-tools` baseline
  (a number, like critest's), reproducible by a `scripts/runtime-tools.sh` (mirrors `critest.sh`).
- **Phase 2 (broaden):** close the highest-value refused fields the suite flags, one per slice, each
  with a failing-then-passing test — same method as the CRI track.

## Alternatives considered

1. **Status quo — keep producing bundles for runc.** Rejected as the *goal*: then runc is the
   runtime and Delonix is not; the "kernel-native own init" identity is unused where it matters most.
2. **Wrap/shell out to runc.** Rejected: defeats the daemonless kernel-native identity and adds a
   dependency the engine crates are meant to avoid.
3. **This ADR — expose the engine's own init as the OCI runtime CLI.** Chosen: it is the only option
   that makes Delonix *the* runtime and unlocks the upstream conformance measurement.

## Open questions

1. **Input surface.** Does a caller-owned `config.json`/bundle count as a new trust boundary needing
   a GO/NO-GO spike, or is it covered by the existing path-confinement + refuse-by-name? Owner call.
2. **`--root` default.** Match runc's `/run/user/<uid>/delonix-runtime` (rootless) / `/run/...`
   (root), or a Delonix-specific path. Interop with containerd expects a configurable `--root`.
3. **Shared state with the CRI path.** The CRI runtime (owner's active area) shells its own init;
   should it be refactored to call `delonix oci-runtime`, or do they stay separate until Phase 2?
