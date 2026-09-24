# ADR-0050: The libvirt and Linux providers are measured against ONE versioned capability catalog — never against a count of `virsh` commands or kernel features

- **Status:** Accepted (2026-09-24) — the catalog, the four declarations, `delonix provider
  ls|describe|matrix`, the evidence gate and the `virsh` inventory exist and are measured (D1–D4);
  the request-time refusal (D6) is built and was run live against the three VM providers on
  2026-09-24 (see the addendum at the end). The node-contract readback (D5) stays where D5 puts
  it: it is one handler when `delonix-node-api` exists, and this ADR does not wait for it
- **Date:** 2026-09-23
- **Deciders:** Walter Angolar
- **Related:** ADR-0049 (the Proxmox matrix this is the local counterpart of: a named denominator,
  three states, nothing hidden); ADR-0044 (`VmSpec`/`Extensions`/`VmProvider` — the port whose
  `capabilities()` this ADR fills in; D3 there says pause/snapshot/resume "are CAPABILITIES,
  requested through `capabilities()`", and until today nothing produced them); ADR-0040 D3 (the
  `Provider { id, capabilities, health }` skeleton); ADR-0041/ADR-0042 (the node contract that
  carries `ProviderInfo` and serves nothing of it yet); ADR-0031 (live migration NO-GO — the row
  this catalog says `unsupported-by-provider` for, with that ADR as the reason); ADR-0008
  (the backend registry the report factory rides on); ADR-0043 (`DX-` codes).

## Context

### What the task asked, and the part of it that was already decided

The request was an operator experience for libvirt/KVM and the Linux host "functionally
equivalent, where technically possible, to the capability interface of the Proxmox provider", with
a matrix of gaps per resource and per provider, five classification states, and the rule that a
Linux host without an external API is measured against **a versioned Delonix capability catalog,
not an artificial count of "Linux routes"**.

Most of the vocabulary already existed, in three places that did not talk to each other:

- `proto/delonix/node/v1/common.proto` carries `Capability { name, supported, detail }`,
  `ProviderInfo { id, kind, available, capabilities, health }` and the rule that a request needing
  a capability the selected provider lacks is `FAILED_PRECONDITION`/`CapabilityNotSupported`.
  **Nothing in Rust produced a `Capability`** (`grep -rn "enum Capability\|struct Capability"
  crates/` — zero hits outside the pod securityContext type of the same name).
- `VmBackend` (`crates/adapters/delonix-vm/src/lib.rs:816`) has the de facto capability set as
  **default methods that refuse**: `pause`/`unpause`/`snapshot`/`restore`/`snapshots`/
  `delete_snapshot` default to `unsupported_pause`/`unsupported_snapshot`, `resume` to `Ok(None)`,
  `disk_health` to `Ok(())`. A caller learns what a backend can do by calling it and reading the
  error. There was no way to ASK.
- ADR-0044's spike (`provider_spike.rs`) has `trait Provider { fn id() }` and says in its own
  doc-comment that `capabilities`/`health` are "out of this spike's scope".

### Measured on `origin/main` (`ae5a26d9`, v4.4.0, 2026-09-23), on this host

Host: Zorin 18.1 (Ubuntu 24.04 base), kernel 7.0.0-31, **libvirt 10.0.0**, QEMU 8.2.2, Cloud
Hypervisor v53.0, systemd 255, nftables 1.0.9, `/dev/kvm` present, the user in `libvirt` and `kvm`,
`qemu:///system` answering (three VMs of another workspace running on it — none touched).

**What the libvirt backend reaches, by the transport it actually uses.** `virsh help` of libvirt
10.0.0 lists **276 subcommands** in 15 groups. The engine invokes **31** of them (11.2 %), read from
the two source files that spawn `virsh` by `scripts/libvirt_virsh_inventory.py` — never from a list
kept next to the code:

```
group        called excluded missing total
domain           11        9      90   110
host              1        4      17    22
network           6        0      15    21
pool              0        0      21    21     ← storage pools: MISSING, not excluded
volume           0        0      16    16     ← idem
monitor          4        0      11    15
interface        0       14       0    14     ← excluded: host NIC administration
nodedev          0        0      13    13
snapshot         3        0       7    10
filter           1        0       8     9
checkpoint       0        0       8     8     ← incremental backup API: missing
secret           0        0       7     7
virsh            0        7       0     7     ← excluded: the client's own shell
backup           0        0       2     2
events           0        0       1     1     ← everything is polled
```

The 31: `blockcommit console define destroy domblklist domifaddr domiflist domstate domuuid
net-autostart net-define net-dhcp-leases net-info net-list net-start net-update nwfilter-define
nwfilter-dumpxml resume snapshot-create snapshot-create-as snapshot-delete snapshot-dumpxml
snapshot-list snapshot-revert start suspend ttyconsole undefine uri vncdisplay`. Every one exists in
the 10.0.0 help (the script exits 1 otherwise). Disks are `qemu-img` files the engine manages; libvirt
pools and volumes are never asked for. There is no event subscription, no migration call (ADR-0031),
no guest-agent channel (`<channel>` is not emitted; the IP comes from DHCP leases with a lease floor,
then `domifaddr`).

**What the node contract serves of this: nothing.** No crate compiles `proto/delonix/node/v1`
(`tonic-build` is declared in the root `Cargo.toml` and used only by the CRI); `delonix-mgmt`, the
only remote surface, serves 32 hand-written routes of which `POST /v1/vms/:name/action` accepts
`stop` and `rm` and shells out to the CLI. `ListProviders` (`GET /v1/providers`) has a shape and no
handler. ADR-0041 D4 froze `delonix-mgmt` ("bug and security fixes only, no new routes"), so the
readback of this ADR cannot land there.

**The two providers are not the same kind of thing, and the catalog must not pretend they are.**
libvirt is a management API in front of QEMU on this host; its natural denominator is the API
(the `virsh` table above is a faithful proxy for the transport in use). The Linux "provider" is the
kernel driven by three adapters — `delonix-linux` (containers, cgroups, namespaces), `delonix-sdn`
(the rootless SDN: holder netns, bridge, slirp, nftables, WireGuard) and `delonix-volume` — and has
no vendor list to count against. A percentage of "syscalls used" would be a number without a
meaning. What both share is the operator's question: *can this provider do X, and how do I know*.

### The trap this ADR closes, with the same shape ADR-0049 found for Proxmox

A capability nobody can list is a capability nobody can require. Today `kind: VirtualMachine`
with `backend: proxmox` and `namespace: teamA` is refused (good), with `network: lab` is silently
ignored (ADR-0044 D1's finding), and with `snapshot` on a running Cloud Hypervisor VM is refused
with an error that names the backend — three different behaviours for the same class of question,
discoverable only by trying. The contract's `required_capabilities` field exists so the engine can
answer BEFORE trying; nothing fills the list it would compare against.

## Decision

### D1. The denominator is a versioned catalog in code, and every provider answers every entry

`delonix_compute::capability::Capability` — **100 entries** in catalog **1.0.0**, one dotted
stable name each (`vm.snapshot.memory`, `net.namespace-isolation`, `volume.nfs`, …), grouped by
the operator's domains (inventory, vm-compute, containers, network, storage, protection, mobility,
firewall, console, metrics, access) and tagged with the port kind the contract uses (`compute` /
`network` / `storage` / `image`). It lives in the compute context, next to `Vm`/`VmBootSpec` and
ADR-0044's port, because it is what a `VmProvider::capabilities()` returns.

A provider's report is built by **walking `Capability::ALL`** and asking a `match` with **no
wildcard arm** (`ProviderReport::build`). A catalog entry added tomorrow fails to compile in every
provider until that provider answers it; a provider cannot skip a row, and a row cannot be hidden.
This is the same property the Kind table (`kinds.rs`) and the exit-code map (`exitcode.rs`) already
rely on: the compiler, not a reviewer, notices the missing answer.

**Versioning:** adding an entry bumps the catalog minor; renaming or removing one bumps the major.
The catalog version travels in the JSON (`catalog_version`) so a consumer knows which list it is
reading. Names, once published, do not change meaning.

### D2. Six states, and `supported` is not self-certifying

```
supported                    implemented AND exercised — carries `evidence`
partial                      implemented with a written limit, or without live proof
unsupported-by-provider      cannot, by nature or by a written decision (the reason)
requires-external-component  possible only with something the engine does not ship
not-implemented              nobody built it yet — distinct from "cannot"
unavailable-on-host          declared yes, but the probe of THIS host said no (the piece named)
```

The first five are the task's five, declared in code. The sixth is not declared: it is what a
declared `supported`/`partial` becomes when the host probe fails (`CapabilityState::on_host`). A
declared "no" never becomes a host "no" — a host cannot make a provider support what it does not.

**`Supported { evidence }` is a struct variant on purpose.** The evidence names a battery check
(`check:<title>` in `scripts/e2e.sh`), a section (`e2e:<title>`), a chaos scenario
(`chaos:<fn>` in `scripts/chaos.sh`), a unit test (`test:<path>::<fn>`) or a live test
(`live:<path>::<fn>`), and `bins/delonix-runtime-bin`'s
`every_supported_capability_cites_evidence_that_exists` greps for each one. An evidence string that
names nothing is a red test, not a published claim. This is the difference between "the code has a
`pause` method" and "pause was exercised": the first is `partial` here, and it is the honest state
of most of the VM matrix today.

### D3. Four declarations, three of them with a host probe

- **libvirt** (`delonix-vm::capabilities::libvirt_report`, probe `LibvirtHost { virsh, qemu, kvm,
  system_uri }`): 15 supported, 24 partial, 17 unsupported, 2 external, 11 not implemented at this
  ADR's first commit — **25 / 14 / 17 / 2 / 11 since 2026-09-24**, when the E2E battery gained the
  checks the `partial` rows lacked (extra disks and NICs, CPU model/topology/pinning, VNC, static
  IP, restart, live backup and restore, destroy — each read from the LIVE domain) (declared,
  assumed-complete host). A session-only host (no `libvirt` group) keeps the lifecycle and loses
  every row that needs an observed address (`vm.network.nat`, `vm.antispoof`, `vm.ip.observed`,
  `vm.network.static-ip`) — the probe names the group to join.
- **cloud-hypervisor** (`cloud_hypervisor_report`, probe `{ binary, kvm, firmware }`): 12 / 17 / 28 /
  2 / 10 at first commit — **19 / 10 / 28 / 2 / 10 since 2026-09-24** (pause/resume read on the
  VMM's api-socket, restart by PID, anti-spoof read inside the holder, backup/restore, destroy). A host with the binary and no known firmware is *available* (selectable) and cannot boot —
  the two are different facts and the report says both.
- **proxmox** (`delonix-proxmox::capability_report(configured)`): 9 / 12 / 28 / 4 / 16 at this
  ADR's first commit — 10 / 12 / 27 / 4 / 16 since `vm.snapshot.delete` became a live-tested
  `supported` and `vm.network.sdn` a refusal by name (2026-09-23, second pass), **declared
  and never probed** — building the report contacts nothing, its health is `Unknown`/`NotProbed`
  when configured and `Unavailable`/`NotConfigured` otherwise. The 9 `supported` cite the live tests
  of ADR-0039 (`tests/live.rs`). It is listed on every host, configured or not, so a reader
  compares hosts by state and not by which rows exist.
- **linux** — three reports under one id, one per kind, because the contract's `ProviderInfo.kind`
  is one word and the Linux provider is genuinely three ports: `delonix-linux::provider_report`
  (containers/pods, probe `{ rootless, cgroup_delegated, subid_tools, nvidia_ctk }`),
  `delonix-sdn::provider_report` (probe `{ nft, slirp4netns, wg, br_netfilter, rootless }` —
  `br_netfilter` asked to the HOLDER when it is up, because the sysctl is per-netns and the
  holder's is the one that filters; the host's value otherwise), `delonix-volume::provider_report`
  (probe `{ can_mount, mount.nfs, mount.cifs, mount.davfs }`).

Each declaration is the honest reading of the code and the batteries as of its commit. At this
ADR's first commit the matrix had more `partial` than `supported` in the VM rows, because the E2E
battery exercised the snapshot cycle, pause/unpause and stop/start and almost nothing else of the
VM surface. The 2026-09-24 pass wrote those checks (sections «vm: o que o relatório libvirt declara
supported, medido» and the CH additions) and promoted what they proved; what still needs a guest
OS inside (cloud-init login, observed IP, guest agent, quiesced backup, serial console) stays
`partial` with the reason written. Writing the checks found three defects the code reading had
missed — a `default`-namespace CH VM had NO anti-spoof rule and was absent from `@dlxall` (the
short `vmtap` line carried no address), a live libvirt backup failed on any VM with a second disk,
and `vm vnc` printed a display index as a port — which is the argument for `supported` requiring
evidence in the first place.

### D4. One verb, one document, one script — all generated from the same declarations

- `delonix provider ls [--kind] [-o json]` — measured on this host; the JSON mirrors
  `ProviderInfo` field by field (`id`, `kind`, `available`, `health{status,reason,message}`,
  `capabilities[]{name,supported,state,detail,domain}`, plus `catalog_version`).
- `delonix provider describe <id> [--kind]` — every row with its reason.
- `delonix provider matrix` — the DECLARED view (every host assumed complete), which
  `docs/providers/capability-matrix.md` is generated from; `the_published_matrix_is_the_generated_one`
  keeps the file byte-equal to the output, the same guard the JSON schema has.
- `scripts/libvirt_virsh_inventory.py <virsh-help.txt>` — the transport-level number above, with
  its own excluded table (the client's own shell; host NIC administration; migration by ADR-0031;
  CPU baselines, a multi-host decision), reproducible by anyone with the help output of a named
  libvirt version. Tests in `scripts/test_libvirt_virsh_inventory.py`.

### D5. Readback through the node contract — where it goes, and why not now

`ListProviders` maps this JSON one-to-one; the handler is one function when `delonix-node-api`
exists (ADR-0042 step C). It does **not** go into `delonix-mgmt`: ADR-0041 D4 froze that server, and
a route added there today would be a route to migrate tomorrow. The CLI and its JSON are the
readback until then; a local client that needs the list runs `delonix provider ls -o json`, which is
the same shape the RPC will return.

### D6. Request-time refusal by name

The contract says a request needing a capability the selected provider lacks is
`FAILED_PRECONDITION` with `CapabilityNotSupported`. With D1 in place this is a comparison, not a
design, and it is built (2026-09-24):

- **The names travel as `VmConfig::required_capabilities`** — filled by `vm create --require
  <capability>` (repeatable, with tab completion of the catalog) and by `spec.requiredCapabilities`
  of `kind: VirtualMachine`; the proto's `VirtualMachineSpec.required_capabilities` maps onto the same
  field when a handler exists.
- **Resolved BEFORE any backend is touched** (`resolve_required_capabilities`, the first thing
  `create_with` does after the name check): an unknown name is `DX-1527 vm.unknown_capability`, an
  *invalid argument* — read as "unsupported" it would send the caller shopping for a provider
  instead of fixing the typo.
- **Checked against the provider's report on THIS host** (`require_capabilities`, the same
  `ReportFactory` `provider ls` runs, so a declared yes the host cannot honour is a no here too),
  before the disk is prepared, the seed written or the admission check run. An unmet entry is
  `DX-6507 vm.capability_not_supported`, in the **`Unavailable` class** (exit 69) — the
  classification the previous text left open: the remedy is another provider or this host, never the
  argument, which is what `FAILED_PRECONDITION` means and what DX-1501's *invalid argument* did not.
  The message lists every unmet entry as `name: state — detail`, the words `provider describe`
  prints.
- **Auto-selection filters by the requirement** (`auto_detect(entries, required)`): a candidate
  whose report lacks an entry is skipped and the next is tried; when every available candidate is
  skipped, the refusal names what each one lacked — never "no backend available", which would send
  the caller to install a hypervisor it has. A standing choice (`DELONIX_VM_BACKEND`, the persisted
  default) is an explicit backend and is refused like one.
- **A restart is gated too**: a record's backend that cannot do what the caller now requires is the
  same refusal, not a VM that came back without it. The requirement is not persisted in the record —
  it was decided against the backend the record names.

### D7. What stays out, and the boundaries this ADR does not move

- **No new port and no `VmProvider` migration.** The report rides on `BackendRegistration`
  (`report: ReportFactory`, a factory like `new`, never called at registration — ADR-0008's
  "registering does no I/O" holds). When ADR-0044 P4b moves the port into the compute context,
  `capabilities()` on `Provider` returns exactly this report; nothing in D1–D4 changes.
- **No daemon, no helper, no libvirt bindings.** The `virsh` transport is unchanged; migrating to the
  C API is a separate decision with a measured gain (events, typed errors) to show first — the
  inventory above is the baseline that decision compares against.
- **The raw XML escape hatch keeps its trust tier and does not travel.** `libvirtXml`/
  `libvirtXmlOverlay` are `vm.raw-definition = partial` with the trust model written in the detail:
  UNVALIDATED, trusted manifests, local CLI only. They are `Extensions::libvirt` fields in ADR-0044's
  model and MUST NOT be reachable from `ProviderExtensions` over the node contract — a caller of the
  socket is not a trusted manifest. This ADR records the rule; the contract's handler enforces it
  when it exists.
- **Multi-node claims stay `requires-external-component`.** `vm.high-availability`,
  `vm.replication`, `vm.migration.live` on libvirt and Cloud Hypervisor cite ADR-0031 and the missing
  quorum/fencing/shared storage. A single host never reads as HA.
- **libvirt LXC is not a provider.** The `lxc:///` driver is not in the catalog; a container served
  by libvirt would be the first container of a non-kernel provider, the same boundary question
  ADR-0049 D4 asks for Proxmox LXC, and it gets the same answer: its own ADR after a spike, or nothing.
- **A coverage claim names the catalog version, the host and the date.** "libvirt: 15 of 100
  supported" means nothing without "catalog 1.0.0, declared" or "measured on <host> on <date>";
  `provider ls` prints both in its header for that reason.

## Alternatives considered

- **`virsh help` as the denominator for libvirt (mirroring `apidoc.js`).** Kept as the
  transport-level number (D4's script), rejected as the capability denominator: 110 of the 276 are
  `domain` commands of which the engine legitimately never needs most (`domjobabort`,
  `domfsthaw`, `iothreadadd`…), and a percentage over them says how much of `virsh` is used, not what
  an operator can rely on. Two numbers, two questions, both published.
- **A YAML catalog read at runtime.** Rejected: the whole value of D1 is that a provider CANNOT skip
  an entry, and only an exhaustive `match` on an enum gives that at compile time.
- **`capabilities()` as a `VmBackend` trait method with a default.** Rejected for the reason ADR-0044
  D3 already wrote: "never a default method that quietly does nothing". A required trait method was
  possible but would have touched twelve test fakes for no gain over a required registration field;
  the registration is also the only place a REMOTE backend can declare without being constructed.
- **Hiding the rows of the other kinds.** A VM provider answers the 46 container rows with
  `unsupported-by-provider: a VM provider`, and the Linux compute provider answers the VM rows the
  same way — 46 and 28 lines of "no" in the compute table. Considered filtering them out; kept, because
  a compute provider that does not run containers is a fact the contract's `ProviderInfo.kind =
  compute` alone does not say, and the state names why.

## Consequences

- An operator can ask the engine what a provider can do on this host and get the reason for every
  "no", without `virsh`, the hypervisor UI or a shell — the readback half of the task. The
  management half (create/start/stop/snapshot/console) was already there for the operator; what
  this ADR adds is that it is now DESCRIBED by the same code that does it.
- The matrix is measured and honest, which means it is mostly `partial` in the VM rows: that is the
  backlog, one row per line, with the missing proof named. A future slice that adds a battery check
  turns a row to `supported` by citing it — and the evidence gate refuses a citation that does not
  exist.
- Two numbers now have a reproducible denominator: 31/276 `virsh` subcommands of libvirt 10.0.0
  called, and N/100 catalog entries per provider per state.
- One more list has to agree with another: the six-state vocabulary in `CapabilityState` and the
  contract's one-bit `Capability.supported`. The mapping is one function (`is_usable`): `supported`
  and `partial` are usable; the other four are not. The contract keeps its bit; the CLI keeps the
  five-way answer next to it.

## Proven vs not validated

**Proven** (on this host, commit of this ADR): the catalog and its tests (names unique, `from_name`
round-trip, `on_host` never turns a declared no into a host no, a report walks exactly its kind);
the four declarations answer every row of their kind (compile-time, plus a test per crate); a
session-only libvirt host, a libvirt host without `virsh`, a Cloud Hypervisor host without firmware,
a Linux session without cgroup delegation, an SDN with `bridge-nf-call-iptables=0`, a rootless
storage session — each rendered and asserted; every `supported` evidence resolves to a real check,
scenario or test (the gate is green at 58 citations); `delonix provider ls` on this host lists six
rows and probes libvirt (`Ok`, `qemu:///system` answering), Cloud Hypervisor (`Ok`), Proxmox
(`NotConfigured`), Linux compute (`Ok`), network (`Ok`), storage (`NetworkSharesNeedRoot`);
`provider matrix` equals `docs/providers/capability-matrix.md`; the `virsh` inventory reproduces
31/276 with the multi-line argv sites included (the first version missed five, caught by comparing
with a hand list, and fixed by joining a `vec![` block onto its first line).

**Not validated:** the readback over the node contract (D5 — nothing serves it); the request-time
refusal (D6 — not built); the report of a CONFIGURED Proxmox target (declared only, by design);
Debian/Rocky/root-mode hosts for the Linux probes; whether `br_netfilter` read from the host's
`/proc/sys` when the holder is down matches what a fresh holder inherits (the code says which value
it read, it does not claim they agree); and the claim, implicit in every `partial`, that the row is
implemented — that is the code's own claim, and this ADR turns it into a list rather than a proof.
