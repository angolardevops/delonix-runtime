# ADR-0044: P4 — the `VmSpec`/`Extensions` port, provider crates, and `delonix-launcher`

- **Status:** Proposed
- **Date:** 2026-09-18
- **Deciders:** Walter (owner)
- **Related:** ADR-0040 (the restructuring this closes phase P4 of — D2.2, D2.3, D2.4,
  D2.5, D3, and the P1b spike result it records); ADR-0008 (the `VmBackend` registry
  this generalizes); ADR-0020 (`RunOpts` — the precedent this ADR follows and departs
  from, and why); ADR-0038 (the CRI's resource model, unaffected here); ADR-0039
  (OpenStack backend, gated on its own spike, explicitly out of scope below); ADR-0043
  (the `DX-CDNN` codes every provider error maps into).

## Context

**Measured on `origin/main` (`d16b2f69`, 2026-09-18). P4 has zero code.** No
`delonix-provider-*` crate exists (`crates/providers/` holds only `delonix-proxmox` and
`delonix-truenas`, under their pre-rename names), no `delonix-launcher` or
`delonix-netns-holder` binary exists (`bins/` holds only `delonix-mcp-bin`,
`delonix-mgmt-bin`, `delonix-runtime-bin`), and no `VmSpec`/`Extensions`/`VmProvider`/
`ProcessLauncher` type exists anywhere in the workspace (`grep -rn "struct VmSpec\b\|struct
Extensions\b\|trait VmProvider\b\|trait ProcessLauncher\b" --include=*.rs .` — zero hits
outside `bins/delonix-runtime-bin/src/cmd/vm.rs`'s unrelated manifest struct of the same
name, addressed in D1). This ADR is the design ADR-0020 of `AGENTS.md`'s own house rule
requires before that code is written.

### P4 is not starting from an empty crate — three things already moved ahead of it

P2 (extract) and P3 (adapters) already did part of what looked, from ADR-0040's own P4
description, like P4's job:

- **`delonix-compute::ports`** already has five read ports the container `run` use case
  resolves through (`ImageStore`, `StorageProvider`, `DeviceResolver`, `RunHost`), plus
  **`VmNetwork`** and **`NetworkProvider`** — the two network ports P4's plan line assigns
  to this phase. Their doc-comments say so plainly: *"Its home is the networking context
  (ADR-0040); it lives here until that context exists."* They are not this ADR's problem
  to design, only to place correctly once `delonix-networking` exists (P2, not P4).
- **`delonix-compute::launch::WorkloadRuntime`** already exists — `create`/`supervise`/
  `discard_unstarted` over a `Launch` value, with the `start()` use case calling it. It is
  a real, working precedent for "a port lives in the context, an adapter implements it",
  but its shape is narrower than D3's `Provider` skeleton (no `id`/`capabilities`/`health`)
  because it has exactly one implementation (`delonix-linux`) and nothing to discover.
  **This ADR does not retrofit `WorkloadRuntime`** — see D3's scoping note.
- **`Vm` and `VmBootSpec` already live in `delonix-compute`**, not in the `delonix-vm`
  adapter (`crates/contexts/delonix-compute/src/record.rs:928,1033`). `delonix-vm` already
  imports them back (`use delonix_compute::{Vm, VmBootSpec}`). This matters more than it
  looks: `VmBootSpec`'s own doc-comment already draws most of the universal/backend-specific
  line this ADR needs — see D1.

### What `scripts/arch_fitness.py` has already committed P4 to

The fitness script is not a description of a plan; it is what CI enforces, and its
`EXCEPTIONS` table already names P4 against seven concrete debts, each with the reason a
previous session recorded when it landed the code the debt is IN:

```
("dep", "delonix-linux",  "delonix-state"): P4 — adapter opens its record store directly;
("dep", "delonix-vm",     "delonix-state"): P4 —   P4 hands it a StateRepository port
("dep", "delonix-sdn",    "delonix-state"): P4 —   from the composition root
("dep", "delonix-oci",    "delonix-state"): P4
("dep", "delonix-volume", "delonix-state"): P4
("dep", "delonix-proxmox","delonix-vm"):    P4 — the VmBackend port lives in the same
                                                   crate as the CH/libvirt adapters; P4
                                                   moves the port into the compute context
                                                   and each backend into its own provider
                                                   crate
("dep", "delonix-scanner","delonix-oci"):   P4 — the scanner reads layers through the OCI
                                                   adapter instead of an ImageStore port
```

This is the authoritative, CI-checked definition of "P4 is done": these seven exceptions
disappear from the table, or the fitness test starts failing on the day someone tries. It
is a bigger scope than "`VmSpec` + provider crates" alone — a **`StateRepository<T>`** port
five adapters are already waiting for is in it too. D6 covers that half; D1–D5 and D8–D9
cover the `VmProvider` half, which is where most of this ADR's weight is because it is
where the trap already caught a real bug (below).

### The trap that motivates `VmSpec`/`Extensions`, measured against the code that exists today

`delonix_vm::VmConfig` is one flat struct, ~30 fields, and `delonix-proxmox`'s
`refuse_unsupported` (`crates/providers/delonix-proxmox/src/lib.rs:965`) is the only thing
standing between an unsupported field and a VM that silently ignores it:

```rust
add(cfg.kernel.is_some(), "kernel");        add(cfg.initrd.is_some(), "initrd");
add(cfg.firmware.is_some(), "firmware");    add(cfg.cmdline.is_some(), "cmdline");
add(cfg.seed.is_some(), "seed");            add(cfg.hugepages, "hugepages");
add(cfg.cpu_affinity.is_some(), …);         add(!cfg.devices.is_empty(), "devices");
add(!cfg.volumes.is_empty(), "volumes");    add(cfg.vnc, "vnc");
add(cfg.machine.is_some(), …);              add(cfg.cpu_model.is_some(), …);
add(cfg.cpu_topology.is_some(), …);         add(cfg.tpm, "tpm");
add(cfg.video.is_some(), …);                add(!cfg.boot_order.is_empty(), …);
add(!cfg.extra_disks.is_empty(), …);        add(!cfg.extra_nics.is_empty(), …);
add(!cfg.libvirt_xml_overlay.is_empty(), …);add(cfg.libvirt_xml.is_some(), …);
add(cfg.net_mode.is_some(), "netMode");
```

**This list is a hand-maintained inverse of the universal set, and it already missed two
fields.** `cfg.network` (the SDN network name a Cloud Hypervisor VM's `tap` attaches to)
and `cfg.namespace` (the isolation namespace, the same notion `container run --namespace`
carries) are `String`/`Option<String>` fields on `VmConfig` that `delonix-proxmox` neither
reads nor refuses (`grep -n "cfg\.network\b\|cfg\.namespace\b"
crates/providers/delonix-proxmox/src/lib.rs` — zero hits, and neither is in
`refuse_unsupported`'s list above). A `kind: VirtualMachine` with `namespace: teamA` and
`backend: proxmox` today builds without a warning and the isolation guarantee that field
promises everywhere else in this engine — VM, pod, container — silently does not apply.
This is measured by static read, not by running the CLI (this ADR does not execute
anything), so it should be confirmed live before anyone treats it as more than a strong
signal; it is exactly the class of defect `AGENTS.md` already calls "aceite e ignorado" and
has fixed three times over for other flags. **A flat spec plus a hand-written refusal list
per provider is a design that already leaks, once, in the one crate that exists to prove
the design.** That is the case for a spec the compiler — not a reviewer reading a `match`
by eye — partitions into what is safe to send everywhere and what is not.

A second, smaller instance of the same shape: `restart_policy_unsupervised(backend.id(),
vm.restart_policy.as_deref())` (`delonix-vm/src/lib.rs:4196`) decides whether a backend can
honour a crash-restart policy natively by matching on the backend's **name**, which is
exactly what D3 rule 3 (below, carried over from ADR-0040 verbatim) forbids outside a
composition root. It has one caller today and costs nothing yet; it is included here
because a design that fixes the leaking-fields problem and leaves this one standing has
only half-learned the lesson.

### The P1b spike, and what it does and does not decide for this ADR

ADR-0040 records the spike's GO-with-condition verdict for `delonix-launcher` and
`delonix-netns-holder`: the launcher path (container namespaces, the `__ovlhold`/`__rmtree`
re-execs) passed unconditionally under an AppArmor profile naming only its own path; the
netns holder passed **only after being amended to create its namespaces with `unshare(2)` +
`newuidmap` in-process**, because `Command::new("unshare")` inherits the caller's profile at
`exec` and giving `userns` to `/usr/bin/unshare` itself would open user namespaces to every
user on the host. The spike's own "not validated" list says the two-binary split was never
actually measured — every run put the CLI and the pin on the *same* profiled path — and
that `--net <custom>` and pods, which are what actually exercise the holder, were not
touched at all (`docs/discovery/53_P1B_LAUNCHER_SPIKE.md`).

**Two things this ADR needs to state plainly rather than assume, because the task that
motivated it treats them as one topic and the evidence says they are not:**

1. **`delonix-launcher` and `delonix-netns-holder` are two different binaries, from two
   different adapter crates, with two different lifecycles**, per ADR-0040's own D2.4
   table: `delonix-linux → delonix-launcher` (the nine `__*` re-execs plus the container
   init — ephemeral, one per spawn, ADR-0040 §2.4) and `delonix-sdn → delonix-netns-holder`
   (`netns pin`/`netns control` — a long-lived pair the CLAUDE.md "Pin/controlo" section
   describes at length, surviving every workload it serves). The P1b spike measured both,
   separately, in the same table, and reached the same in-process-`unshare` verdict for
   each by the same reasoning — but they do not become one process by sharing that
   reasoning.
2. **Neither binary sits on today's VM boot path.** `crates/adapters/delonix-vm/src/lib.rs`
   has no `unshare`/`CLONE_NEWUSER` anywhere (`grep -n "unshare\|CLONE_NEWUSER"` — zero
   hits); every `Command::new` in that crate shells out to `virsh`, `cloud-hypervisor`,
   `cloud-localds` or `qemu-img` — legitimate adapter behaviour under the fitness script's
   own rule ("`Command::new` is legitimate in an adapter... what this counts is the engine
   re-running **its own** binary"), not the self-exec cycle ADR-0040 is closing. A Cloud
   Hypervisor VM's `tap` is attached by asking the netns holder over its control socket
   (`VmNetwork::attach_tap`) and the guest itself never gets a Linux namespace of its own —
   KVM is the isolation boundary, not `unshare`. **`ProcessLauncher` and `VmProvider` are
   therefore independent pieces of P4's scope, bundled into one phase by the plan table, not
   coupled by any mechanism.** Building `delonix-launcher` teaches nothing about `VmSpec`
   and vice versa, and neither blocks the other. This ADR treats them as two decisions (D3–
   D4 for `VmProvider`, D5 for `delonix-launcher`) precisely so a reviewer can accept one
   without the other.

## Decision

### D1. `VmSpec`: the universal fields, drawn from what `VmBootSpec` already separated

`VmBootSpec` (`delonix-compute::record`) already exists to answer "what does a VM need to
be reconstructed", and its own doc-comment already states the rule this ADR generalizes to
the provider boundary: *"state needed to RECONSTRUCT a resource has to be persisted, not
merely used at creation"*. It is the **persisted** shape, though, and persistence is not
the same question as "what can every provider honour" — `VmBootSpec` nests fields
`delonix-proxmox` already reads (`bridge`) next to fields it refuses (`volumes`, `tpm`,
`libvirt_xml`) inside the same struct, because it was built to answer "does this survive a
restart", not "is this universal". `VmSpec` answers the second question, is never persisted
on its own (`Vm`/`VmBootSpec` keep doing that job unchanged — see the note at the end of
this decision), and is built at the call site from whichever entry point is starting a VM
(the CLI's `cmd::vm::VmSpec` manifest struct, `cluster kubeadm`'s in-memory `VmConfig`
construction, or, later, the node API).

Classified from `refuse_unsupported`'s list (backend-specific, confirmed by the Proxmox
provider's own refusal), from `bridge`'s actual use in `delonix-proxmox::net0_arg` (read,
not refused — universal despite living in `VmBootSpec` today), and from the two fields
D1's Context section found neither read nor refused anywhere (flagged, not silently
resolved either way):

| Field | Today | P4 |
|---|---|---|
| `name`, `disk`, `vcpus` | universal | stays in `VmSpec`, unchanged shape |
| `memory` (free-form `"2G"`/`"1024M"`) | universal, but re-implemented wrong once (ADR-0008 finding #2: Proxmox's own `mem_mib` copy ignored the `Gi`/`Mi` suffix) | `VmSpec.memory_mib: u32`, **already parsed** with the shared, tested `delonix_vm::mem_mib` before it reaches any provider — the class of bug ADR-0008 found becomes structurally impossible instead of merely fixed once |
| `disk_size_gib` | universal in intent (Proxmox's `<storage>:<gib>` fresh-disk form takes a size too) | stays, `Option<u32>` |
| `network`, `namespace` | **used by CH, silently ignored by Proxmox today** (Context, above) | universal by design (every provider attaches to *some* network and *some* isolation domain); moves into `VmSpec` and a provider that cannot honour `namespace` must **refuse** it through the same mechanism as any other unsupported field — no provider may go back to silently accepting it |
| `bridge` | lives in `VmBootSpec` (which reads as "local-only") but is **actually read** by `delonix-proxmox::net0_arg` | universal — this ADR corrects the classification `VmBootSpec`'s current shape implies |
| `hostname`, `ci_user`, `ssh_keys`, `cloud_init` (the cloud-init INTENT fields) | universal by design already — `VmConfig`'s own doc-comment says local backends realize them as a NoCloud ISO and Proxmox maps them to its own cloud-init keys, "the whole point of the intent fields" | stays universal, unchanged |
| `restart_policy` | read by `delonix-vm::create_with` (orchestration, above `VmBackend::boot`) to decide `on_crash` vs. an unsupervised policy, currently by matching `backend.id()` as a string | stays universal as *intent*; the "can this provider honour it natively" question moves from a name match to `capabilities().has(Capability::CrashRestart)` (D3) |
| `kernel`, `initrd`, `firmware`, `cmdline`, `seed`, `hugepages`, `cpu_affinity`, `vnc`, `serial_capture`, `static_ip`, `machine`, `cpu_model`, `cpu_topology`, `tpm`, `video`, `boot_order`, `extra_disks`, `extra_nics`, `libvirt_xml_overlay`, `libvirt_xml`, `net_mode` | explicitly refused by `delonix-proxmox` today | `Extensions`, keyed `cloud-hypervisor`/`libvirt` (D2) |
| `devices` (VFIO passthrough) | refused by Proxmox; **not universal even between the two local backends** — libvirt-only in practice today | `Extensions::libvirt` |
| `volumes` (9p mounts) | refused by Proxmox; `VmVolume`'s own doc-comment says libvirt-only ("Cloud Hypervisor does not do 9p") | `Extensions::libvirt` — **not** `Extensions::cloud_hypervisor`, because it never was CH's to refuse either |

```rust
/// Everything a VmProvider needs regardless of which one it is. Built once, at
/// the call site, from whatever entry point is starting a VM — never persisted
/// on its own; `Vm`/`VmBootSpec` keep doing that job.
pub struct VmSpec {
    pub name: String,
    pub disk: String,              // opaque to VmSpec — see the note below
    pub vcpus: u32,
    pub memory_mib: u32,           // already resolved: `mem_mib` runs once, here
    pub disk_size_gib: Option<u32>,
    pub network: String,
    pub namespace: String,         // "default" = the open SDN, same as everywhere else
    pub bridge: Option<String>,
    pub hostname: Option<String>,
    pub ci_user: Option<String>,
    pub ssh_keys: Vec<String>,
    pub cloud_init: Option<bool>,
    pub restart_policy: Option<String>,
}
```

**`disk` stays an opaque string, on purpose, and that is a scope decision this ADR is
making, not an oversight.** A local backend reads it as a filesystem path; Proxmox already
reads the same field as `template:<vmid>` (clone) or `<storage>:<gib>` (fresh disk) and
refuses anything else by naming both forms. A fully typed `DiskSource` enum covering every
provider's addressing scheme is real design work — OpenStack's Glance image IDs and Cinder
volume IDs are a third shape again — and nothing in this ADR needs it to close the seven
`arch_fitness` exceptions. It is named here as a candidate follow-on ADR, not blocking P4.

**Nothing about `Vm`/`VmBootSpec`'s on-disk format changes.** They keep reconstructing a
stopped VM exactly as they do today; `VmSpec` is assembled from them (or from the CLI's
manifest struct) immediately before a `VmProvider` call and is never itself serialized. This
is deliberate: `VmBootSpec` already carries real back-compat guarantees
(`#[serde(default)]` on every field, records written before a field existed keep
deserializing) that a persisted `VmSpec` would inherit for free — but inventing a second
persisted VM shape next to the one that already exists is exactly the "one recipe, two
places to check" failure this repo's Kind-name and Kind-fact tables were built to stop
repeating. `RunOpts` sets the same precedent for containers: it is not `Container`, and
nobody has needed it to be.

### D2. `Extensions`: namespaced, typed, and closed against the drift D1 measured

Per-provider knobs travel keyed by provider id, and **the provider that owns a key is the
only thing that ever reads or validates it** — an unknown key is a **refusal**, not a
silently ignored map entry, closing exactly the gap D1 found in `network`/`namespace`:

```rust
#[derive(Default)]
pub struct Extensions {
    pub cloud_hypervisor: Option<CloudHypervisorExt>,
    pub libvirt: Option<LibvirtExt>,
    pub proxmox: Option<ProxmoxExt>,
    // a new provider adds a field here and nowhere else in `delonix-compute`
}

pub struct LibvirtExt {
    pub kernel: Option<String>,
    pub initrd: Option<String>,
    // … the rest of D1's "libvirt-only" column
    pub volumes: Vec<VmVolume>,
    pub devices: Vec<String>,
    pub libvirt_xml_overlay: Vec<String>,   // UNVALIDATED — see the trust-model note
    pub libvirt_xml: Option<String>,        // UNVALIDATED — see the trust-model note
}
```

**Rejected: `HashMap<String, serde_json::Value>`.** Every provider that ships *in this
repository* is compiled with `delonix-compute` already — this is not a third-party plugin
ABI, it is an internal boundary between crates in the same workspace, and an untyped map
would trade compile-time field checking (which is precisely what caught D1's two
silently-ignored fields, once someone bothered to grep for them) for a flexibility nothing
here needs yet. A provider outside this repository is not a case this engine is designed
for: ADR-0040's own naming (`delonix-provider-<technology>`) and D2.5's fixed crate list
say the providers are known and finite, the same way `VmBackend`'s registry lets a third
backend register itself without a typed slot existing for it in advance — the registry
mechanism (D4) is what stays open; the spec shape does not need to.

**One name, one trust tier — deliberately narrower than `libvirtXml`/`libvirtXmlOverlay`
are today.** `VmConfig`'s doc-comment already marks those two fields **UNVALIDATED**, "only
for trusted manifests, same trust model as running an arbitrary disk image" — a caller who
can set them can already point at arbitrary host paths and devices. `Extensions::libvirt`
carries them forward with the exact same warning, not a stricter one: this ADR is not
proposing to sandbox raw XML injection, which would be new work with its own threat model,
only to give it a typed, namespaced home instead of two flat fields on a struct every
provider sees. Every other field on every `*Ext` struct **is** validated by its provider —
`ProxmoxExt` (empty today; nothing Proxmox-only exists yet beyond what `VmSpec` already
covers) would reject an unrecognised key the same way `delonix-proxmox::refuse_unsupported`
already reports unsupported fields today, grouped by why, before anything is created.

### D3. `VmProvider`: the port, its skeleton, and where it lives

Per ADR-0040 D3's `Provider` skeleton (identity, capabilities, health — quoted, not
reinvented) plus the lifecycle `VmBackend` already has, minus the trait-method-per-quirk
growth D1's evidence shows:

```rust
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn capabilities(&self) -> &CapabilitySet;
    fn health(&self) -> Condition;
}

pub trait VmProvider: Provider {
    fn create(&self, spec: &VmSpec, ext: &Extensions) -> Result<VmHandle, VmError>;
    fn start(&self, h: &VmHandle) -> Result<Boot, VmError>;
    fn stop(&self, h: &VmHandle) -> Result<(), VmError>;      // keeps the disk
    fn destroy(&self, h: &VmHandle) -> Result<(), VmError>;   // releases it
    fn observe(&self, h: &VmHandle) -> Result<VmObservation, VmError>;
    // pause/unpause, snapshot/restore/list, resume, disk_health are CAPABILITIES,
    // requested through `capabilities()` and refused with `CapabilityNotSupported`
    // when absent — never a default method that quietly does nothing.
}

pub struct VmObservation {
    pub running: bool,
    pub ip: Option<String>,
    pub ip_confidence: IpConfidence,   // was `VmBackend::ip_is_predicted() -> bool`
}
```

`stop`/`destroy`/`resume`/`ip_is_predicted`→`ip_confidence` carry `VmBackend`'s own already-
hard-won distinctions forward unchanged in *meaning* — ADR-0008 paid for both with real data
loss (`vm stop` destroying a Proxmox disk, `vm start` on a stopped remote VM building a
second one) and this ADR is not re-opening either question, only renaming the vocabulary
they are expressed in.

**Where it lives.** `crates/contexts/delonix-compute`, per ADR-0040 D2.2's table
(`compute.delonix.io` → "ports `WorkloadRuntime`, `SandboxProvider`, `VmProvider`") and
confirmed against `scripts/arch_fitness.py`'s own `ALLOWED` table: `PROVIDER: {FOUNDATION,
CONTEXT}` — a provider crate is explicitly permitted to depend on a context crate, which is
exactly what `delonix-provider-proxmox` implementing a trait defined in `delonix-compute`
requires, and exactly what the fitness script's own `("dep", "delonix-proxmox",
"delonix-vm")` exception already names as the destination: *"P4 moves the port into the
compute context and each backend into its own provider crate"*.

**Scoping note on `Provider`'s skeleton and `WorkloadRuntime`.** `WorkloadRuntime` has one
implementation and nothing to select between — `id()`/`capabilities()`/`health()` would be
three methods with exactly one caller and one always-true answer. This ADR does **not**
retrofit it to the `Provider` skeleton; that skeleton is for ports with plural, selectable
implementations (`VmProvider`, `NetworkProvider`, `StorageProvider`, `ImageRegistry`), which
is the actual reason D3 rule 3 ("no string matching on provider names outside the
composition root") exists — there is no name to match on when there is only one answer.

**Crate renames** (ADR-0040 D2.3, unchanged by this ADR, restated for the record): CH and
libvirt split out of `delonix-vm` into `delonix-provider-cloud-hypervisor` and
`delonix-provider-libvirt`; `delonix-proxmox` → `delonix-provider-proxmox`; `delonix-truenas`
→ `delonix-provider-truenas`; `delonix-provider-openstack` stays a name reserved for
ADR-0039, which is its own gated spike and not started by this decision. `delonix-vm` as a
crate name **disappears** — nothing in D2.3's provider table or D2.5's directory layout
lists it, and its remaining job (the record types) already moved to `delonix-compute`
ahead of this phase (Context, above).

### D4. The backend registry moves with the port, its four rules unchanged

ADR-0008's registry (`BackendFactory`, `BackendRegistration`, `register_backend`,
`auto_detect`) is not being redesigned — its four properties (registering does no I/O,
`auto_selectable` is a fact of the registration and not the built backend, a third-party
registration may opt out of auto-detection, names are owned) are exactly right for
`VmProvider` too, and the bug each one prevents is documented against real code in ADR-0008.
What changes is only *where* the table lives, following the port: today it is a `static` in
`delonix-vm`; once `VmProvider` moves into `delonix-compute`, the registry moves with it, as
a small module in that context's `app/` layer (`delonix_compute::app::vm_providers`, or
equivalent) — it is called from the composition root exactly as `cmd/vmbackends.rs` calls
`delonix_vm::register_backend` today (env-configured, resolved once, per ADR-0008 decision
2), and it is consulted by the `CreateVirtualMachine`/`ObserveVirtualMachine` use cases the
same context owns, so the mechanism and the port it selects between are never split across
a context boundary.

`delonix-provider-proxmox`'s registration in `cmd/vmbackends.rs` moves unchanged in spirit —
env vars, a `kind: Secret` credential, "reported and skipped, not fatal" on a misconfigured
target — only the function it calls changes name and crate.

### D5. `delonix-launcher`: scope, and the two questions answered without hedging

Restated from Context, because it is the answer this ADR commits to, not a summary of
someone else's finding:

1. **`delonix-launcher` is a new binary alongside, not instead of, the `delonix-sdn` →
   `delonix-netns-holder` pair.** It backs a different port (`ProcessLauncher`, containers
   and pods), owned by a different adapter (`delonix-linux`), with a different lifecycle
   (one process per spawn, exits when the spawn is done — the netns holder's whole point is
   to outlive every spawn it serves).
2. **It does not sit on `VmProvider`'s path.** Nothing in `delonix-vm` calls `unshare`
   today; a `VmProvider::create` for any of the three current backends stays a plain
   `Command::new` to `virsh`/`cloud-hypervisor`/the Proxmox HTTP client, which is legitimate
   adapter behaviour and not part of the cycle `delonix-launcher` exists to remove.

```rust
/// Received over a fd the caller passes at spawn time — never an argv another
/// program built, and never a name to look up on the process's own PATH.
pub struct LaunchSpec {
    pub rootfs: String,
    pub mounts: Vec<Mount>,
    pub userns: UsernsMode,        // Own | Inherit(holder_pid) — the two `Launch` cases today
    pub cgroup: CgroupTarget,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
}

pub trait ProcessLauncher {
    fn spawn(&self, spec: &LaunchSpec) -> Result<LaunchedProcess, LaunchError>;
}
```

It absorbs the nine `__*` re-execs (`__ovlhold`, `__ovlmigrate`, `__rmtree`, `__duusage`,
`__volsnap`, `__buildtar`, `__netnsconnect`, plus `__apirun` and the container init) per
ADR-0040 D2.4's table, and creates its user/mount namespaces **in-process**, `unshare(2)` +
`newuidmap`, the same mechanism `reexec_mapped` already uses (`crates/adapters/
delonix-linux/src/lib.rs:2102`) and the same one P1b's amendment requires of
`delonix-netns-holder` for the identical reason — a profile authorising `userns` only on
`/opt/.../delonix-launcher`'s own path must never be handed to a generic `unshare(1)` it
then `exec`s, or the restriction the profile exists to enforce is gone.

**One thing the sketch above states as a decision and P1b never measured: the transport.**
`reexec_mapped` and `__apirun` today pass their spec two different ways — the former as
plain `argv` strings, the latter as a **path to a `0600`, `create_new` file** the child
reads and both sides delete. ADR-0040 D2.4 says `delonix-launcher` "receives a typed spec
over an inherited fd" — a third mechanism, never built or measured anywhere in this
codebase. Whether an inherited fd (a `memfd`/pipe handed across `fork`) closes a real gap
the file-based approach already closes with `0600`+`create_new`+unlink-on-exit, or whether
it is complexity with no measured benefit over the existing precedent, **is an open
question this ADR does not resolve** — see the spike list below. The code sketch keeps
ADR-0040's stated intent; the spike decides whether it survives contact with a working
implementation.

**Addendum, 2026-09-18 — spike run, and it answers in the fd's favour, contrary to this
section's own stated uncertainty** (`docs/discovery/57_P4_D5_LAUNCHSPEC_TRANSPORT_SPIKE.md`,
full detail). A standalone harness (no delonix crates — `std` + `libc`, already a
workspace dependency via `delonix-linux`, zero new supply-chain surface) replicated
`write_run_spec`'s exact file precedent (`0700` dir, `0600` `create_new`, unique name,
unlink both sides) against a `memfd_create` (no `MFD_CLOEXEC`) fd inherited across a
**real `execve`** into a different binary — the thing P1b never measured for this
mechanism. Both transports: 50/50 correct under 50 concurrent writer/reader pairs, no
cross-talk. Where they diverge is the crash window this section calls "an open
question": when the writer process dies right after writing, before spawning the reader
and before its own defensive `unlink` runs (an OOM-kill, an external `SIGKILL`, a crash —
not a hypothetical), **the file transport leaves the secret on disk, readable by the
owning uid, with nothing in this engine today that sweeps it** (no reaper for
`cri/run/*.json`, unlike the IPAM-lease/ref-marker reapers `AGENTS.md`'s "O IPAM vaza"
describes); the fd transport leaves zero trace, confirmed both by a directory scan and by
`strace -e trace=openat,open,unlink,unlinkat` showing no syscall on any real path at all
(only `/proc/self/maps`/`/dev/null`, the Rust runtime's own bookkeeping). This is not a
flaw in `write_run_spec`'s implementation — `0600`+`create_new` already closes the
cross-user-readability risk this ADR originally named — it is a property no amount of
"write the file correctly" can close, because the guarantee depends on a *third* step
(`unlink`) that a dead process simply never reaches. A `memfd` has no such step: nothing
is ever `open()`ed, so there is nothing to forget to `unlink`. **D5's code sketch is kept
as written, now measured rather than merely stated**; SCM_RIGHTS was deliberately not
tested (`delonix-launcher` is always `fork`+`exec`ed directly by its caller, never handed
a fd by an unrelated process over an existing socket — plain fork inheritance suffices).

### D6. `StateRepository<T>`: the port five adapters already owe

The fitness script's five identical `P4` exceptions (`delonix-linux`, `delonix-vm`,
`delonix-sdn`, `delonix-oci`, `delonix-volume` → `delonix-state`) share one reason, worded
identically each time: *"the adapter opens/writes its own record store directly; P4 hands
it a StateRepository port from the composition root."* `delonix-state` already has the
mechanism worth generalizing — `JsonStore<T>`/`Store<T>` (`flock`, atomic writes,
`update`'s read-modify-write-under-lock, the same discipline `container update`'s five
mutation paths were fixed to use) — the debt is that five crates call it as a **concrete
dependency**, not through a trait a composition root could substitute or a test could fake
without touching a real state root.

```rust
pub trait StateRepository<T> {
    fn get(&self, id: &str) -> Result<T>;
    fn list(&self) -> Result<Vec<T>>;
    fn update(&self, id: &str, f: impl FnOnce(&mut T) -> Result<()>) -> Result<()>;
    fn remove(&self, id: &str) -> Result<()>;
}
```

**Addendum, 2026-09-18 — spike nº4 run, and the sketch above corrected against a real
caller** (`docs/discovery/56_P4_D6_STATE_REPOSITORY_SPIKE.md`, full detail). Two things
this addendum states plainly because the spike measured them, not because this section's
first draft was wrong to leave them for the spike:

1. **The sketch's `update` signature does not survive contact with
   `delonix-linux::wait_and_record`/`persist_stop`.** Real callers use `f`'s return to
   mean "abort, and it is not an error" (not expressible as `Result<()>`), need the final
   `T` back (not `Result<()>`, or a second unlocked read reopens the race the port exists
   to close), and need a `set`/`save` for first-time creation (`update` alone gives
   `NotFound` on a record that does not exist yet — the single biggest thing
   `delonix-linux` does with the store). The corrected shape (`update<F>(&self, id: &str,
   f: F) -> Result<T> where F: FnOnce(&mut T) -> bool`, plus `set`) is what the spike
   proved against a real 24-thread race, with and without the underlying `flock` (removed
   and restored to prove the guarantee comes from `Store::update`, not from the trait
   shape) — 34/34 `delonix-state` tests with the lock in place, the port's own test
   FAILING (5 of 24) with it removed.
2. **The five exceptions are not one debt.** Only `delonix-linux` (`Store<Container>`)
   and `delonix-vm` (`JsonStore<Vm>`) match this section's assumption — a concrete
   `Store`/`JsonStore` call swapped for the port, mechanically. `delonix-sdn` has its own
   parallel `flock` in `ipam.rs` (never routed through `delonix-state`'s `FileLock`) plus
   unlocked `write_atomic` calls for routes/services in `infra.rs`; `delonix-oci`'s only
   `delonix-state` use is a one-time signing-key write with no collection of records to
   list/update/remove at all; `delonix-volume`'s `VolumeStore` does read-modify-write on
   raw `write_atomic` with no lock visible anywhere in it. None of the three is "call the
   port instead of the store" — each needs its own sentence in a future revision of this
   decision (or its own follow-on ADR) before P4a treats them as done by the same PR that
   migrates the other two. This does not block P4b–P4e, which do not depend on D6.

**No on-disk format changes.** This is a wrapper around `JsonStore<T>`'s existing
behaviour, not a new one — the `flock`, the atomic-rename write, the record layout on disk,
all stay exactly what a running node already has written there. What changes is that
`delonix-linux`/`delonix-vm`/`delonix-sdn`/`delonix-oci`/`delonix-volume` receive an `impl
StateRepository<Container>` (etc.) from the composition root instead of calling
`delonix_state::JsonStore::open` themselves, closing the `("dep", …, "delonix-state")`
direction the fitness script currently tolerates only by exception. `SecretVault` (the
`SecretStore`/`CredVault` half) is the same shape, listed separately in ADR-0040 D3's port
list because its contract (encrypted at rest, a `reveal` boundary) is different from a
plain record.

### D7. `ImageStore` for the scanner: the remaining exception, briefly

`delonix-scanner` reads layers by depending on `delonix-oci` directly (`use
delonix_oci::{Image, ImageStore}` — a **concrete struct**, `crates/adapters/delonix-oci/
src/image.rs:110`, not a trait). `delonix_compute::ports::ImageStore` already exists as a
**trait** with the shape the scanner needs (`resolve`/`config`/`prepare_rootfs`/
`resolve_user`), built for the container `run` use case and explicitly marked as living in
`delonix-compute` only until the artifact context (`delonix-artifact`, ADR-0040 D2.2) exists
to own it properly. Closing this exception is: the scanner takes an `impl ImageStore` (the
port, not the struct) from its composition root instead of naming `delonix-oci` in its own
`Cargo.toml`. It is a small, mechanical change once the port exists, and is listed here for
completeness against the fitness script's own committed scope, not because it needs new
design — D1–D5's work is what actually decides P4's shape.

### D8. Migrating `VmBackend` → `VmProvider`: what `delonix-provider-proxmox` has to change

`VmBackend`'s trait **signature changes** — `boot(&self, vmdir: &Path, cfg: &VmConfig,
overlay: &str, on: &dyn Fn(CreateStage))` becomes `create(&self, spec: &VmSpec, ext:
&Extensions)` plus a separate `start`. This is a breaking change to the trait, on the trait
`delonix-provider-proxmox` (merged, working, watched running against a real node this
session) already implements. What has to change in that crate, concretely:

- `refuse_unsupported`'s ~20-field hand-written list is **deleted**, not ported — every
  field it names no longer exists on `VmSpec` at all; a provider that does not understand a
  field cannot receive it, by construction, instead of by remembering to check for it.
  `ProxmoxExt` starts empty (nothing in D1's classification needs a Proxmox-only extension
  yet) and grows a field only when a real Proxmox-specific knob needs one.
- `mem_mib`'s re-implementation is deleted; `VmSpec.memory_mib` arrives already resolved.
- The async-task handling (`UPID` polling, `exitstatus` vs `status`), the vmid-in-
  `api_socket` handle scheme, the guest-agent `ip()` path, the snapshot/restore calls, the
  `send_authed` re-authentication — none of this changes. It is the HTTP client and the
  Proxmox-specific mechanics ADR-0008's spike proved; D1–D3 only change the shape of what
  arrives at the crate's boundary, not what the crate does with it.
- `network`/`namespace` (D1's finding) go from silently unread to **required decisions —
  and, measured live 2026-09-18** (spike nº5, `docs/discovery/
  58_P4_D1_D8_PROXMOX_NETWORK_NAMESPACE_LIVE.md`), **`namespace`'s half is already done**:
  `delonix-vm::vm_namespace_supported` already refuses it for Proxmox (and libvirt) today,
  from a month before this ADR — D8's job for `namespace` is to carry that same refusal
  into `Extensions`/`refuse_unsupported`'s new home, not invent it. `network`'s half is
  still open and is a genuine decision, not a formality: the live form sent to a real
  `pve92` node shows `net0=virtio,bridge=vmbr0` regardless of what `--network` named, and
  the topology (`pve92`'s only bridge rides this host's own `virbr0` NAT, structurally
  disjoint from the rootless SDN inside the netns holder) means "real support" is not a
  small addition — it needs the same class of privileged bridging `vm bridge`
  (EXPERIMENTAL, libvirt-only today) already does, extended to reach a remote node, which
  is its own ADR. **Refusing `network` explicitly, the same way `tpm` is refused today, is
  the D8-sized answer**; building the bridge is not.

`delonix-provider-cloud-hypervisor` and `delonix-provider-libvirt` change the least: they
are the two backends `VmConfig` was originally shaped around, so most of D1's "universal"
column is literally every field they already read, and D2's `Extensions::cloud_hypervisor`/
`Extensions::libvirt` are close to a straight split of `VmBootSpec`'s existing fields.

### D9. Sequencing: P4 lands as several PRs, each with its own gate

Following ADR-0040's own "strangler order" reasoning (Alternatives, "big-bang rewrite...
rejected") and the fact that the fitness script's seven exceptions are independently
removable:

| Slice | Work | Gate |
|---|---|---|
| P4a | `StateRepository<T>` + `SecretVault` ports; the five `delonix-state` exceptions close | fitness test green on those five; `cargo test -p delonix-state` and every migrated adapter's own suite unchanged (no format change to prove, since none happens) |
| P4b | `VmSpec`/`Extensions`/`VmProvider` land in `delonix-compute`; `delonix-provider-cloud-hypervisor`/`-libvirt` split out of `delonix-vm`, `delonix-vm` retired | the substitution spike (below) green; every existing `delonix-vm` test migrates and passes under the new crates |
| P4c | `delonix-proxmox` → `delonix-provider-proxmox` on the new port (D8); the `network`/`namespace` gap closed one way or the other | the `("dep", "delonix-proxmox", "delonix-vm")` exception removed from `arch_fitness.py`; a live run against the `proxmox-ve` appliance repeats ADR-0008's watched lifecycle on the new trait |
| P4d | `delonix-launcher` binary; `ProcessLauncher` port; the nine `__*` re-execs move | a repeat of the P1b matrix with `delonix` and `delonix-launcher` as genuinely separate installed binaries (the gap the spike's own "not validated" section names), `--net <custom>` and a pod included |
| P4e | `delonix-truenas` → `delonix-provider-truenas` on a `Provisioner` port; scanner takes `ImageStore` (D7) | `("dep", "delonix-scanner", "delonix-oci")` exception removed; `provision.rs`/`network.rs` string-matching call sites (ADR-0040 §2) replaced by the port |

P4b before P4c: writing the Proxmox migration against a port that has not itself been
proven against the two backends it was designed from would risk baking Proxmox's shape
into `VmSpec` a second time, the same mistake `VmConfig` already made once.

## Alternatives considered

- **`Extensions` as `HashMap<String, serde_json::Value>`.** Rejected in D2: every provider
  ships in this workspace, so an untyped map trades away compile-time checking for a
  flexibility this repo does not need, and D1's own finding (two fields silently unread by
  a real provider) is exactly the failure mode a typed, per-provider struct set catches at
  review time instead of by grepping for it afterwards.
- **Keep `VmBackend` as it is; only move the crate.** Rejected: `arch_fitness.py`'s own
  exception for this dependency already names the destination as "the port moves into the
  compute context **and each backend into its own provider crate**" — a move without a
  signature change leaves `VmConfig`'s flat-struct leak (D1) exactly where it is.
- **A single `delonix-provider-vm` crate holding all VM providers.** Rejected: it is the
  "one `delonix-app` crate" mistake ADR-0040's own Alternatives section already rejected,
  applied one layer down — a shared provider crate would make CH, libvirt and Proxmox share
  a dependency footprint none of the three needs (Proxmox needs `reqwest`; the two local
  backends do not, which is the whole reason ADR-0008 put Proxmox in its own crate in the
  first place).
- **Have `delonix-launcher` also own VM boot, "for consistency".** Considered and rejected
  in D5: nothing in the three current VM backends creates a Linux namespace, so routing
  their `Command::new` calls through a launcher process would add a process hop with no
  privilege boundary to justify it. Revisit only if a future backend genuinely needs one
  (a namespaced microVM path, say) — not as a symmetry argument.
- **The file-based transport, generalized instead of building the fd.** Considered as the
  fallback if D5's spike came back negative; it did not (see the D5 addendum and
  `docs/discovery/57_P4_D5_LAUNCHSPEC_TRANSPORT_SPIKE.md`) — the file precedent
  (`0600`+`create_new`+unlink, `__apirun`) closes cross-user readability but not the
  crash-before-`unlink` window, which the fd closes structurally. Rejected once measured,
  not assumed away.
- **One big P4 PR.** Rejected in D9, on ADR-0040's own precedent and because the fitness
  script's seven exceptions are independently gate-able — forcing them into one PR would
  recreate the "several concurrent sessions in the same crates" risk ADR-0040's plan table
  already works around with coordination windows.

## Consequences

**Easier:** a fourth VM provider (OpenStack, ADR-0039; a future Firecracker backend) is a
new crate plus a composition-root registration, with a compiler-checked boundary against
sending it a field it cannot honour — the exact bar ADR-0008 set and D1 shows the current
design already misses. `delonix-linux`/`delonix-vm`/`delonix-sdn`/`delonix-oci`/
`delonix-volume` stop opening their own record files, which is what lets a future
`delonix-node-api` (P5) or a test fake the state layer without a real state root. The
scanner stops depending on the OCI adapter's internals to read a layer.

**Harder / cost, in order of risk:**

1. ~~**The `ProcessLauncher` transport is unproven.**~~ **CLOSED, 2026-09-18** — measured
   in the fd's favour (`docs/discovery/57_P4_D5_LAUNCHSPEC_TRANSPORT_SPIKE.md`, D5
   addendum): the fd closes a crash-before-`unlink` window the file precedent cannot
   close by construction. D2.4's wording stands; no amendment needed.
2. **The two-binary launcher/holder split has never been measured**, only the same-path
   variant. P1b's own "not validated" list already says so; P4d's gate exists because of
   it, not despite it.
3. **Smaller than this section first estimated.** `namespace` was already closed a month
   before this ADR (spike nº5, D8 addendum) — P4c's job there is relocation, not new
   behaviour. `network` is still real cost: an explicit refusal (the honest, D8-sized
   answer) is a small addition; anything more ("real support") is the scope of a
   follow-on ADR for a privileged remote bridge, not this migration.
4. **Distribution**, again: P4d adds an eighth-ish executable to what ADR-0040's own
   Consequences section already counted going from two to eight; `install.sh` and
   `delonix-deploy` change with each provider crate rename.

**Guardrail audit:** #1 no resident process added (the launcher is per-spawn, the same
shape `reexec_mapped` already has) ✅ · #2 `VmSpec`/`Extensions` carry no tenant, account or
plan field — the isolation concept they DO carry (`namespace`) is the engine's own, present
since before this ADR ✅ · #3 no private dependency: `delonix-provider-*` names follow
ADR-0040's convention, none named after the private monorepo ✅ · #4 the provider crates
stay dependency-clean per `arch_fitness.py`'s `ALLOWED` table (`PROVIDER: {FOUNDATION,
CONTEXT}` — no adapter-to-adapter or provider-to-provider edge) ✅ · #5 the one new privilege
boundary this ADR touches (`delonix-launcher` creating namespaces) reuses the mechanism
P1b already measured and gets its own two-binary spike (P4d) before merging, not after ✅ ·
#6 an unknown `Extensions` key and an unsupported capability both fail closed by name,
never by silent drop — closing exactly the gap D1 measured, not just avoiding a new one ✅.

## What this ADR does not decide

- **P5 (node API), P6 (in-process CRI), P7 (observability).** They depend on P4's ports
  existing but are separate phases with their own gates in ADR-0040's plan table; nothing
  here designs `delonix-node-api`'s contract or the CRI's in-process dispatch.
- **The OpenStack backend itself.** ADR-0039 is Proposed and gated on its own spike against
  a live cloud; this ADR only confirms the port shape it would implement, not whether or
  when it lands.
- **A typed `DiskSource` covering every provider's addressing scheme** (D1) — named as a
  candidate follow-on, not designed here.
- **Renaming `bins/delonix-runtime-bin/src/cmd/vm.rs`'s own `VmSpec` struct** (the
  `kind: VirtualMachine` manifest shape) to avoid reading like the same type as
  `delonix_compute::VmSpec` this ADR proposes. They are not the same type today and will not
  be after P4 — the manifest struct stays `Option`-heavy and schema-derived, the engine
  struct stays resolved and provider-facing, exactly the relationship `RunOpts` already has
  with a `kind: Container`/`Pod` document. A rename to reduce the naming collision (e.g. the
  manifest struct becoming `VirtualMachineSpec`, following the Kind's own canonical name) is
  a docs/CLI-layer decision this ADR flags but does not make.
- **Whether `Extensions`' per-provider structs get their own generated JSON Schema entry**
  the way `kind: VirtualMachine`'s manifest fields already do (ADR-0007) — a P4d/P4e detail,
  not a P4 architectural question.

## Spikes required before this ADR can be marked Accepted

Per this repository's own standing rule (ADR-0008's Proxmox spike, ADR-0039's gated status,
the P1b spike this ADR builds on): nothing above is accepted on the strength of the design
alone.

1. **The substitution spike (P4b's gate).** Hand the *same* `VmSpec` to
   `delonix-provider-cloud-hypervisor` and `delonix-provider-libvirt` and converge both —
   `delonix vm create`, `stop`, `start` on each, from one manifest, with **zero**
   `#[cfg]`/name-branching outside the composition root. Measure by `grep -rn
   'backend.contains\|backend\.id() ==' --include=*.rs crates/` returning zero outside
   `bins/`, not by inspection.
2. **The two-binary launcher/holder spike (P4d's gate).** Repeat P1b's exact matrix
   (`53_P1B_LAUNCHER_SPIKE/matrix.sh`) with `delonix`, `delonix-launcher` and
   `delonix-netns-holder` as three genuinely separate installed binaries — the gap the
   original spike's own "not validated" section names — and add `--net <custom>` and a pod
   to the matrix, neither of which P1b touched.
3. **The IPC-transport spike (D5's open question).** DONE — 2026-09-18,
   `docs/discovery/57_P4_D5_LAUNCHSPEC_TRANSPORT_SPIKE.md`. A standalone harness (no
   delonix crates) measured `memfd_create` (no `SCM_RIGHTS` — unneeded, see the D5
   addendum) inherited across a real `execve`, against the file precedent generalized
   exactly as it exists in `write_run_spec` today, under 50 concurrent writer/reader
   pairs (both: 50/50 correct, no cross-talk) and a crash-window simulation (writer dies
   right after writing, before either side's cleanup runs). **The fd approach closes a
   real window the file approach cannot close by construction**: the file transport
   leaves the secret on disk, unswept, when the writer dies before its `unlink`; the fd
   transport leaves zero trace (confirmed by `strace`, zero `openat`/`unlink` on any real
   path). D5's code sketch is kept as written — this spike confirms it rather than
   rolling it back to the file-based form.
4. **`StateRepository<T>` against a live `flock`.** DONE for `Store<Container>`
   (`delonix-linux`'s shape) — 2026-09-18, `docs/discovery/56_P4_D6_STATE_REPOSITORY_SPIKE.md`.
   24-thread concurrency test through the port, both with the underlying `flock` (24/24,
   no lost writes) and with it removed by reversion (5/24 — proves the guarantee is the
   lock's, not the trait's). The sketch above did not survive contact with a real caller
   unchanged — see the D6 addendum. **Still open**: the same test through the generic
   `JsonStore<T>` impl (`delonix-vm`'s shape — checked by reading only, the impl forwards
   verbatim to `JsonStore::update`, which already has its own proof, but never run
   through the port itself), and `delonix-sdn`/`delonix-oci`/`delonix-volume`, which the
   spike found do not reach the port through the same mechanism at all (D6 addendum) and
   need their own spike once someone decides how each one adopts it.
5. **The `network`/`namespace` gap on Proxmox, confirmed live, not by grep.** DONE —
   2026-09-18, `docs/discovery/58_P4_D1_D8_PROXMOX_NETWORK_NAMESPACE_LIVE.md`, against the
   `pve92` appliance already on this host (Proxmox VE 9.2.2). **`namespace` was already
   half of D1's own finding, wrong**: `--namespace teamA` is REFUSED before any API call
   reaches the node — `delonix-vm::vm_namespace_supported` (landed `c1ed34ec8`,
   2026-08-05, a month before this ADR's Context section) returns `true` only for
   `"cloud-hypervisor"`, so libvirt and Proxmox both refuse today. D1's grep was scoped to
   `crates/providers/delonix-proxmox/src/lib.rs` and never reached this guard, which lives
   a layer up in `delonix-vm::create_with` — right file to grep, wrong crate. `network`
   is confirmed exactly as D1 read it: accepted, silently ignored — the real HTTP form
   sent to the node carries `net0=virtio,bridge=vmbr0` (the node's own default), never the
   SDN network name, and the local `Vm` record misleadingly shows `Network: p4spike-net`
   for a VM that is not on it. No guest reachability test was needed to settle this: `pve92`'s
   only bridge (`vmbr0`) is wired to its own `eth0`, itself on this host's `virbr0`
   (192.168.122.0/24) — structurally disjoint from the rootless SDN inside the netns
   holder, with or without a `namespace`. See the D8 addendum below.

## Proven vs not validated

**Proven** (read or measured on `origin/main`, `d16b2f69`, with the references above): the
crate/binary layout P4 starts from; `VmConfig`'s field list and `delonix-proxmox`'s
`refuse_unsupported` list; that `network`/`namespace` are neither read nor refused by that
crate; that `bridge` **is** read there despite living in `VmBootSpec`; that `Vm`/
`VmBootSpec` already live in `delonix-compute`; that `delonix_compute::ports` already has
five working ports plus `WorkloadRuntime`; that no `unshare`/`CLONE_NEWUSER` exists in
`delonix-vm`; the `arch_fitness.py` `ALLOWED`/`EXCEPTIONS` tables verbatim; the P1b spike's
recorded verdict and its own "not validated" list; **added 2026-09-18** —
`StateRepository<T>`'s corrected shape (D6 addendum) against a real 24-thread race,
proven both with and without the underlying `flock` (spike nº4, `docs/discovery/
56_P4_D6_STATE_REPOSITORY_SPIKE.md`); that `delonix-sdn`/`delonix-oci`/`delonix-volume`
do not reach `delonix-state` through `Store`/`JsonStore` today, by reading each one, not
by the fitness script's shared exception text; that the inherited-fd `LaunchSpec`
transport (D5) closes a crash-before-`unlink` window the file-based precedent cannot,
against a real `execve` and 50 concurrent pairs, confirmed by `strace` (spike nº3,
`docs/discovery/57_P4_D5_LAUNCHSPEC_TRANSPORT_SPIKE.md`); that `namespace` on Proxmox is
already refused (`delonix-vm::vm_namespace_supported`, landed a month before this ADR)
and that `network` is silently ignored exactly as Context first read, confirmed against a
real `pve92` node's HTTP form and its network topology, not by grep (spike nº5,
`docs/discovery/58_P4_D1_D8_PROXMOX_NETWORK_NAMESPACE_LIVE.md`).

**Not validated:** everything else under Decision — no other code was written for this ADR, no build
or test was run, and every remaining spike (nº1, nº2) in the section above is exactly
that, not yet run. Whether `Extensions`'
per-provider structs stay small (as this ADR's classification suggests) or grow the way
`VmConfig` did is unmeasured by construction — this ADR can state the design pressure that
caused `VmConfig`'s growth (each provider quirk became a trait method) and claim `Extensions`
removes that specific pressure, but cannot claim no comparable pressure exists elsewhere
until a fourth provider (OpenStack, or a Firecracker backend) is actually built against it.
