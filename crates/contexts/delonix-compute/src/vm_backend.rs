//! The legacy `VmBackend` port (pre-ADR-0044's `VmProvider`): `VmConfig`
//! (the flat spec every current backend consumes), `CloudInitIntent`, `Boot`,
//! `CreateStage`/`DestroyStage`, the trait itself, and the shape of a registry
//! entry (`BackendRegistration`). Moved here from `delonix-vm` by P4b.2
//! (`docs/discovery/61`): a `PROVIDER` crate may depend
//! only on `FOUNDATION`/`CONTEXT`, and every backend implements this trait.
//!
//! The REGISTRY itself (the `static` table, `register_backend`,
//! `select_backend*`) stays in `delonix-vm` until P4b.4: it is seeded with the
//! two local backends, which live there until then. A remote backend hands the
//! composition root a `BackendRegistration`, and the composition registers it.
//!
//! `delonix-vm` re-exports every name here, so no caller changes a line.

use crate::vm_error::{Error, Result};
use crate::vm_firewall as firewall;
use crate::{CpuTopology, ExtraDisk, ExtraNic, Vm, VmVolume};
use std::path::Path;

/// The user the golden image creates at build time (`sudo` NOPASSWD), and the
/// account everything else assumes is the login target — the serial autologin
/// and `cluster kubeadm`'s SSH user. Here because a remote backend reads it too.
pub const DEFAULT_CI_USER: &str = "delonix";

/// The whole cloud-init intent a VM boots with — hostname, the account the
/// keys land on, and the keys — as `vm cloud-init` hands it to a backend:
/// already merged with what the record had, so the backend writes every value
/// and never has to guess which ones the caller left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CloudInitIntent {
    /// Guest hostname (`None` = the VM name).
    pub hostname: Option<String>,
    /// Account the keys land on (`None` = the default user).
    pub ci_user: Option<String>,
    /// Authorized public keys, one per entry.
    pub ssh_keys: Vec<String>,
}

/// Configuration to boot a microVM (flat fields, independent of the
/// `orchestrator` — the CLI translates the `VmSpec` into this).
#[derive(Debug, Clone, Default)]
pub struct VmConfig {
    /// Name (persistence key and of the deterministic `tap`/MAC).
    pub name: String,
    /// Base disk (qcow2/raw) — becomes a per-VM overlay.
    pub disk: String,
    /// vCPUs.
    pub vcpus: u32,
    /// Memory (e.g. `"2G"`, `"1024M"`).
    pub memory: String,
    /// Ingress network for the `tap`.
    pub network: String,
    /// Logical isolation namespace (`None`/`"default"` = the open SDN), the same
    /// notion `container run --namespace` uses. Only meaningful for a VM that
    /// actually lives on the holder's SDN — see `vm_namespace_supported`.
    pub namespace: Option<String>,
    /// Kernel for *direct boot* (vmlinux/bzImage).
    pub kernel: Option<String>,
    /// Initrd/initramfs (with `kernel`).
    pub initrd: Option<String>,
    /// Firmware (alternative to the kernel: rust-hypervisor-fw/EDK2 — for cloud images).
    pub firmware: Option<String>,
    /// Kernel command line (with `kernel`).
    pub cmdline: Option<String>,
    /// cloud-init *seed* ISO (NoCloud) — secondary disk.
    ///
    /// This is the MECHANISM, and it is a file on THIS host: a backend whose
    /// guest runs elsewhere cannot open it. Prefer the intent fields below and
    /// let each backend realize them; keep this for a seed you built yourself.
    pub seed: Option<String>,
    /// Tamanho do disco do nó, em GiB. `None` = herda o da imagem base.
    ///
    /// Existe porque sem ele **todo o nó herdava o tamanho da golden**, e não
    /// havia como dimensionar um nó pelo armazenamento que o inquilino paga —
    /// uma quota de armazenamento, de quem a tiver, conta-se sobre o
    /// PROVISIONADO, logo é aqui que ela se aplica.
    ///
    /// O overlay é fino: pedir 40 GiB não escreve 40 GiB: cresce à medida do
    /// uso. Mas o número PROMETIDO é o que a quota do inquilino paga, e é este.
    ///
    /// **Não pode ser menor que a imagem base** — um overlay qcow2 não encolhe
    /// o seu backing file, e tentar fazê-lo produz uma VM que arranca e corrompe
    /// o filesystem. Validado antes de criar (ver `prepare_local_overlay`).
    pub disk_size_gib: Option<u32>,
    // --- cloud-init INTENT ------------------------------------------------
    // What the operator MEANT, as opposed to `seed` above, which is one way of
    // delivering it. The local backends turn these into a NoCloud ISO
    // ([`cloudinit::generate_seed_iso`]); Proxmox maps them to the node's own
    // cloud-init (`--ciuser`/`--sshkeys`). Before this existed, a remote backend
    // was structurally excluded from cloud-init — the only vocabulary available
    // was a local path.
    /// Guest hostname. `None` means the VM name.
    pub hostname: Option<String>,
    /// Account the SSH keys are installed on, and that the serial console
    /// auto-logs in as. `None` means [`cloudinit::DEFAULT_CI_USER`] — the
    /// account the golden image already creates.
    pub ci_user: Option<String>,
    /// Authorized SSH public keys, ALREADY RESOLVED (never `@file` forms — see
    /// [`cloudinit::generate_seed_iso`]).
    pub ssh_keys: Vec<String>,
    /// Whether this guest runs cloud-init at all. `Some(false)` for an appliance
    /// (OPNsense, Proxmox, TrueNAS), which configures itself and for which a
    /// seed is an ISO nobody reads on a drive that changes the guest's device
    /// list for no reason.
    ///
    /// `Option` and not `bool` **because this struct derives `Default`**, and
    /// callers build it with `..Default::default()` all over this workspace: a
    /// bare `bool` would default to `false` and silently stop seeding every one
    /// of them — a VM with no datasource skips cloud-init's network phase and
    /// comes up with no address, which is the exact bug already on record for
    /// `kind: Vm`. `None` means "yes", so nothing changes for who never sets it.
    pub cloud_init: Option<bool>,
    /// Normalized restart policy (`"no"`|`"on-failure"`|`"always"`).
    pub restart_policy: Option<String>,
    // --- HPC (S4) ---------------------------------------------------------
    /// Backs the VM memory with *hugepages* (`--memory …,hugepages=on`). Reduces
    /// TLB misses and jitter in HPC workloads. Requires hugepages reserved on the host.
    pub hugepages: bool,
    /// CPU affinity (NUMA/pinning): list of host CPUs (e.g. `"8-15"`) to which
    /// ALL vCPUs are pinned (`--cpus …,affinity=<vcpu>@[<list>]`). Avoids
    /// vCPU migration between cores/NUMA nodes — latency determinism.
    pub cpu_affinity: Option<String>,
    /// PCI device passthrough (SR-IOV VF, GPU, …) via VFIO: sysfs paths
    /// (e.g. `/sys/bus/pci/devices/0000:65:00.1`). The VF must be pre-bound to
    /// `vfio-pci` on the host. Each one becomes a `--device path=…`.
    pub devices: Vec<String>,
    /// Virtualization backend: `Some("cloud-hypervisor")`, `Some("libvirt")` or
    /// `None` (auto-detection). Historical default = cloud-hypervisor.
    pub backend: Option<String>,
    /// Network mode of the **libvirt** backend (Cloud Hypervisor always uses the
    /// ingress `tap`). Abstracts the domain's `<interface>` — the user NEVER writes XML:
    ///   * `None`/`"user"` — user-mode network (SLIRP/passt): egress, no inbound IP.
    ///   * `"nat"`         — NAT network managed by libvirt (`<source network=…>`, DHCP +
    ///     IP via `virsh domifaddr`). Requires `qemu:///system` (root).
    ///   * `"bridge"`      — attaches to a host bridge (`bridge` below).
    pub net_mode: Option<String>,
    /// Name of the host bridge (mode `net_mode = "bridge"`) or of the libvirt network (mode
    /// `"nat"`; default `"default"`).
    pub bridge: Option<String>,
    /// Volumes/Storage shared into the VM (via **virtio-9p**). Each one
    /// comes already RESOLVED by the bin (the `Volume`/`Storage` name → host
    /// directory). Only the **libvirt** backend materializes them (Cloud Hypervisor does not do
    /// 9p) — see `create`. Closes the gap "mount a NAS into a VM without cloud-init/XML".
    pub volumes: Vec<VmVolume>,
    /// VNC graphical console (`--vnc`) — **libvirt backend only** (Cloud Hypervisor
    /// has no display). Binds to `127.0.0.1` on an auto port; see `vm vnc`.
    pub vnc: bool,
    /// Capture the serial console to `<vmdir>/<name>.serial` instead of exposing
    /// it as an interactive socket (Cloud Hypervisor) or pty (libvirt).
    ///
    /// **The two are mutually exclusive, in both backends**, and that is why this
    /// is a per-VM choice and not a second sink: the guest writes to a single
    /// `/dev/console` (`ttyS0`), and CH's `--serial` takes ONE destination
    /// (`off|null|pty|tty|file=|socket=`). With capture on, `delonix vm console`
    /// has nothing to attach to for this VM, and says so.
    ///
    /// Exists for an UNATTENDED reader that needs the boot log as a file — the DKS
    /// reads the `kubeadm join` marker its control-plane prints on the console.
    /// That reader was written when the serial WAS a file; the interactive console
    /// (`487c9d3f`, 2026-07-20) moved it to a socket and left the file unwritten,
    /// and nothing noticed because the reader's `unwrap_or_default()` reads a
    /// missing file as "the node has not printed yet".
    pub serial_capture: bool,
    /// Static IP (`--ip`) — libvirt `nat` mode only: materialized as a DHCP
    /// reservation (`<host mac=… ip=…/>`) on the libvirt network, so the guest
    /// needs NO cloud-init network config. Must belong to the network's subnet.
    pub static_ip: Option<String>,
    /// Catalog capabilities the backend MUST mark usable on this host, by name
    /// (`vm.snapshot.memory`, `vm.namespace-isolation`, … — `delonix provider
    /// ls` lists them). Resolved with [`Capability::from_name`] before any
    /// backend is touched (a typo is an invalid argument, never "unsupported"),
    /// and checked against the backend's report on THIS host before anything
    /// is created; auto-detection only picks a backend that has them all.
    /// The contract's `required_capabilities` (ADR-0050 D6).
    pub required_capabilities: Vec<String>,

    // --- Advanced libvirt knobs (libvirt backend only) ------------------------
    // Declarative `kind: Vm` parity with hand-written libvirt XML: typed fields
    // for the common cases + two raw-XML escape hatches for the long tail.
    /// Machine type (`<os><type machine=…>`), default `q35`.
    pub machine: Option<String>,
    /// CPU mode/model: `"host-passthrough"` (default), `"host-model"`, or a named
    /// model (e.g. `"Skylake-Server"`) → `<cpu mode='custom'>`.
    pub cpu_model: Option<String>,
    /// CPU topology (`<topology sockets cores threads/>`).
    pub cpu_topology: Option<CpuTopology>,
    /// Emulated TPM 2.0 (`<tpm>`) — needed by some guests (Windows/Secure Boot).
    pub tpm: bool,
    /// Video model (`<video><model type=…>`): `"virtio"`, `"qxl"`, `"vga"`,
    /// `"none"`. Overrides the default (virtio when `vnc`).
    pub video: Option<String>,
    /// OS boot device order (`<os><boot dev=…/>`): e.g. `["hd","cdrom","network"]`
    /// (ignored on direct-kernel boot).
    pub boot_order: Vec<String>,
    /// Extra disks beyond the main overlay + cloud-init seed.
    pub extra_disks: Vec<ExtraDisk>,
    /// Extra network interfaces beyond the primary one.
    pub extra_nics: Vec<ExtraNic>,
    /// Raw libvirt XML FRAGMENTS injected verbatim just before `</devices>` — the
    /// escape hatch for device knobs with no typed field. **UNVALIDATED**: a
    /// fragment can reference arbitrary host paths/devices, so only for TRUSTED
    /// manifests (same trust model as running an arbitrary disk image).
    pub libvirt_xml_overlay: Vec<String>,
    /// FULL `<domain>` override used VERBATIM (ignores everything generated from
    /// the fields above except the rootless seclabel injected at boot). The
    /// ultimate escape hatch — the author owns the entire XML. **UNVALIDATED**.
    pub libvirt_xml: Option<String>,
}

/// The memory size `s` names, in MiB, or `None` when it does not parse.
///
/// The one parser of the memory syntax (`512M`, `4G`, the k8s-style `4Gi`, a
/// bare number of MiB). [`mem_mib`] builds its lenient fallback on top of it;
/// a verb that must not act on a misread value (`vm resize`) calls this
/// directly and refuses on `None`. Zero is `None` too: no guest boots in it.
pub fn parse_mem_mib(s: &str) -> Option<u64> {
    let t = s.trim();
    // Tolerates the k8s-style `i` suffix (Gi/Mi): "2Gi" == "2G", "512Mi" == "512M".
    let t = t.strip_suffix(['i', 'I']).unwrap_or(t);
    let (num, mult) = if let Some(n) = t.strip_suffix(['G', 'g']) {
        (n, 1024)
    } else if let Some(n) = t.strip_suffix(['M', 'm']) {
        (n, 1)
    } else {
        (t, 1)
    };
    num.trim()
        .parse::<u64>()
        .ok()
        .and_then(|v| v.checked_mul(mult))
        .filter(|v| *v > 0)
}

/// Converts memory (`"2G"`/`"1024M"`/`"512"`/`"2Gi"`) to MiB.
/// `"2G"`/`"512M"`/`"2Gi"`/`"2048"` → MiB.
///
/// **Public because every backend has to read the SAME field the same way.**
/// It was private, so `delonix-proxmox` grew its own copy — and the copy did
/// not know the k8s `Gi`/`Mi` suffix this one tolerates, so `memory: 2Gi` meant
/// 2 GiB on libvirt and Cloud Hypervisor and 1 GiB on Proxmox, silently. Same
/// discipline as `fw_rule_tail` on the network side: one definition, shared by
/// everyone who reads the format.
pub fn mem_mib(s: &str) -> u64 {
    match parse_mem_mib(s) {
        Some(v) => v,
        // Do not degrade silently: a mistyped value ("2GB", "2 Gi") would give
        // roughly half of the requested RAM without warning. Warn and use a safe default.
        None => {
            tracing::warn!(value = ?s, "invalid memory value; defaulting to 1024 MiB");
            1024
        }
    }
}

/// Stages emitted by `create_with` so a caller can render step-by-step
/// progress. The engine emits ONLY the enum — the user-facing text and its
/// translation stay in `delonix-runtime-bin` (project rule: UI strings live in
/// the bin, not in the mechanism crates).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateStage {
    /// Preparing the per-VM overlay disk (`qemu-img create`).
    Disk,
    /// Ensuring/attaching the network (libvirt NAT net, or the SDN tap).
    Network,
    /// Defining the domain in the hypervisor.
    Define,
    /// Starting the domain.
    Start,
}

/// A stage of `destroy_with`, reported as it STARTS — the teardown twin of
/// [`CreateStage`], so a destroy can show what it is taking away instead of a
/// blinking cursor. Only stages that have something to do are reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestroyStage<'a> {
    /// Powering off and removing the VM from its provider (the backend id).
    Provider(&'a str),
    /// Deleting the per-VM overlay disk.
    Overlay,
    /// Deleting extra disks that belong to the VM (how many).
    ExtraDisks(usize),
    /// Deleting the cloud-init seed and the preserved snapshots.
    SeedAndSnapshots,
    /// Deleting sockets, serial log, pid file and the domain XML.
    RuntimeState,
    /// Removing the record itself.
    Record,
}

/// What a backend produced when booting a VM — persisted in the [`Vm`].
pub struct Boot {
    /// PID of the VMM on the host (Cloud Hypervisor). `None` when managed by a daemon
    /// (libvirt) — there the liveness comes from `is_running`.
    pub pid: Option<i32>,
    /// `tap` interface (or `"user"` for libvirt user-mode networking).
    pub tap: String,
    /// NIC MAC.
    pub mac: String,
    /// Control socket (Cloud Hypervisor API; empty on libvirt).
    pub api_socket: String,
    /// The VM's IP, if known at boot.
    pub ip: Option<String>,
    /// See [`Vm::dhcp_lease_floor`]. Only a backend whose IP comes from a DHCP
    /// lease table that outlives the VM sets it.
    pub lease_floor: Option<String>,
}

/// The virtualization mechanism behind a microVM. Allows having Cloud
/// Hypervisor and libvirt/KVM side by side (chosen per VM).
pub trait VmBackend {
    /// Stable identifier persisted in the [`Vm`].
    fn id(&self) -> &'static str;
    /// `true` if the backend has the required tools installed.
    fn available(&self) -> bool;
    /// Creates the network (if applicable) and boots the VM from the `overlay`. The overlay
    /// creation and idempotency are handled by `create`. `on` receives the
    /// sub-stages (network/define/start) for a progress UI.
    fn boot(
        &self,
        vmdir: &Path,
        cfg: &VmConfig,
        overlay: &str,
        on: &dyn Fn(CreateStage),
    ) -> delonix_model::Result<Boot>;
    /// Is the VM still alive?
    fn is_running(&self, vm: &Vm) -> bool;
    /// Current IP of the VM (may change/resolve later via DHCP).
    fn ip(&self, vm: &Vm) -> Option<String>;

    /// The handle this backend now knows the VM by, when it differs from the
    /// one in the record — `None` when nothing changed or the backend cannot
    /// tell. Never does I/O: it reports what an earlier call in this process
    /// already learnt (a remote backend that found a VM moved to another node
    /// of its cluster, ADR-0053 decision 3), so `status` can persist it
    /// without a round trip per VM on every `vm ls`. Default: `None`.
    fn current_handle(&self, _vm: &Vm) -> Option<String> {
        None
    }

    /// Is [`VmBackend::ip`] a PREDICTION rather than an OBSERVATION?
    ///
    /// Default `false`: libvirt reads a real DHCP lease, so an address there is
    /// evidence that the guest booted far enough to ask for one. Cloud
    /// Hypervisor overrides it — its address is computed from the MAC before
    /// the guest runs at all, so it is evidence of nothing.
    ///
    /// Whoever waits for a boot needs this to know when "it has an IP" is an
    /// answer and when it is only an arithmetic identity. It lives on the
    /// backend rather than in a `backend.contains("cloud-hypervisor")` at the
    /// call site for the reason ADR-0008 gives: the knowledge belongs to the
    /// backend that does the predicting.
    fn ip_is_predicted(&self) -> bool {
        false
    }
    /// Stops the VM and frees the network resources. Returns `Err` when the backend
    /// REFUSED the cleanup (e.g. libvirt) — the caller decides whether to abort (so as not to
    /// delete the local record of a VM that is still defined in the hypervisor) or
    /// to ignore it (`vm rm --force`).
    fn stop(&self, vmdir: &Path, vm: &Vm) -> delonix_model::Result<()>;

    /// Releases everything the VM owns, because its record is going away
    /// (`vm rm`). Default: [`Self::stop`] — which is exactly right for the two
    /// local backends and is why nothing existing changes.
    ///
    /// **The two are the same operation locally and NOT the same remotely**,
    /// and conflating them destroyed data. Locally the disk is the engine's: a
    /// libvirt `undefine` leaves `<root>/vms/<name>.qcow2` untouched, so `stop`
    /// can free the hypervisor's side and `rm` deletes the file afterwards. On
    /// a remote node the disk belongs to the node, and the only call that frees
    /// the VM also frees its disk — so a backend that implemented `stop` as
    /// "stop and destroy" made `delonix vm stop` erase the guest, while the
    /// CLI's own next-steps block promises `stop it (keeps the disk)`.
    ///
    /// A backend that owns nothing beyond what `stop` releases should leave
    /// this alone.
    fn destroy(&self, vmdir: &Path, vm: &Vm) -> delonix_model::Result<()> {
        self.stop(vmdir, vm)
    }

    /// Suspends a RUNNING VM's vCPUs, guest memory intact — the same notion
    /// as `container pause`'s cgroup freezer, not [`VmBackend::snapshot`]
    /// (which persists a checkpoint to disk; this never touches storage).
    /// Default: unsupported (fail closed) — a backend overrides this only
    /// once it actually has a mechanism, never as a silent no-op.
    fn pause(&self, _vmdir: &Path, _vm: &Vm) -> delonix_model::Result<()> {
        Err(unsupported_pause(self.id(), "pause"))
    }
    /// Resumes a VM suspended with [`VmBackend::pause`]. Default: unsupported.
    fn unpause(&self, _vmdir: &Path, _vm: &Vm) -> delonix_model::Result<()> {
        Err(unsupported_pause(self.id(), "unpause"))
    }

    /// `vm resize`: gives a STOPPED VM `vcpus` and `memory_mib` for its next
    /// boot (`vm.resize.cold`). Called only after the engine has confirmed the
    /// record says the VM is not running and validated both numbers; the
    /// engine rewrites the record once this returns `Ok`.
    ///
    /// Default: unsupported (fail closed). A backend whose definition is
    /// rebuilt from the record at every boot overrides it with nothing to do,
    /// and says so; one that keeps its own definition elsewhere (a remote
    /// node) changes it there and reads it back.
    fn resize_cold(
        &self,
        _vmdir: &Path,
        _vm: &Vm,
        _vcpus: u32,
        _memory_mib: u64,
    ) -> delonix_model::Result<()> {
        Err(unsupported_pause(self.id(), "resize"))
    }

    /// `vm move --node`: moves the VM to `target`, another node of the SAME
    /// cluster (ADR-0053 decision 1) — the same VM and the same record, only
    /// its node changes. `live` asks for an online move (the VM keeps
    /// running). Called after the engine has checked from its record that
    /// the power state matches `live`; returns the handle the VM is known by
    /// on `target`, which the engine writes to the record.
    ///
    /// Default: unsupported (fail closed). A local backend has no cluster: it
    /// refuses with the verb that relocates a VM between hosts (`vm migrate`,
    /// ADR-0031) named in the message.
    fn move_to_node(
        &self,
        _vmdir: &Path,
        _vm: &Vm,
        _target: &str,
        _live: bool,
    ) -> delonix_model::Result<String> {
        Err(Error::UnsupportedByBackend(format!(
            "moving a VM between nodes is not supported on the '{}' backend: it has no cluster \
             — to relocate a VM to another delonix host use `delonix vm migrate`",
            self.id()
        ))
        .into())
    }

    /// `vm cloud-init`: gives a STOPPED VM this cloud-init intent for its
    /// next boot, and proves the guest will read it (the backend's own
    /// rendering, not the call's answer). Called only after the engine has
    /// validated the intent and confirmed from its record that the VM is
    /// stopped; the engine rewrites the record once this returns `Ok`.
    ///
    /// Default: unsupported (fail closed). The local backends keep it that way
    /// on purpose: their seed is an ISO built at create, and a guest only
    /// re-runs cloud-init for a new `instance-id` — changing the seed without
    /// that would be reported as applied and ignored by the guest.
    fn update_cloud_init(
        &self,
        _vmdir: &Path,
        _vm: &Vm,
        _intent: &CloudInitIntent,
    ) -> delonix_model::Result<()> {
        Err(unsupported_pause(self.id(), "cloud-init change"))
    }

    /// Checked once, right after [`VmBackend::stop`] has already confirmed the
    /// vmm gone and the caller has already persisted `Status::Stopped` — this
    /// is a POST-CONDITION check, not a precondition, so an `Err` here must
    /// never be read as "the stop failed" (the record is already correct by
    /// the time this runs). Default: nothing to check.
    ///
    /// Exists for exactly one known failure mode (BUG-VM-001, cloud-hypervisor
    /// only): a real guest write in flight at the moment of `stop` can leave
    /// the qcow2 corrupted even though the vmm exited cleanly. Nothing this
    /// engine controls can repair that; what it owes the operator is not
    /// making them discover it three commands later from an unrelated
    /// `restore`/`snapshot` error.
    fn disk_health(&self, _vmdir: &Path, _vm: &Vm) -> delonix_model::Result<()> {
        Ok(())
    }

    /// Brings an already-created VM back up, instead of creating one.
    ///
    /// `Ok(None)` — the default — means "I have no way to resume; create it the
    /// usual way", which is the truth for both local backends: their `boot` is
    /// idempotent because the per-VM overlay is on this filesystem and gets
    /// reused.
    ///
    /// A remote backend has no such luck. Its `boot` asks the node for the next
    /// free id, so a `vm start` on a stopped VM would build a SECOND one and
    /// leave the first orphaned on the node with nothing pointing at it —
    /// silently, since the record is then rewritten to the new handle. Here it
    /// can start the VM its record already names.
    ///
    /// Called only when a record exists and the VM is not running.
    fn resume(&self, _vmdir: &Path, _vm: &Vm) -> delonix_model::Result<Option<Boot>> {
        Ok(None)
    }

    /// Takes a named snapshot of the VM. On libvirt this is a **system checkpoint**
    /// (`virsh snapshot-create-as`): for a running domain it captures memory + disk
    /// state; `restore` reverts to it. Default: unsupported — a backend that does not
    /// override this fails closed with a clear message (never a silent no-op).
    fn snapshot(&self, _vmdir: &Path, _vm: &Vm, _name: &str) -> delonix_model::Result<()> {
        Err(unsupported_snapshot(self.id(), "snapshot"))
    }
    /// Reverts the VM to a named snapshot (libvirt: `virsh snapshot-revert`).
    /// Default: unsupported (fail closed).
    fn restore(&self, _vmdir: &Path, _vm: &Vm, _name: &str) -> delonix_model::Result<()> {
        Err(unsupported_snapshot(self.id(), "restore"))
    }
    /// Lists the VM's snapshot names. Default: unsupported (fail closed).
    ///
    /// Takes `vmdir` because a stopped VM's snapshots may live only on OUR
    /// side: libvirt's metadata does not survive the undefine that `stop`
    /// does, so the list of a stopped VM is read from what
    /// [`VmBackend::preserve_snapshots`] wrote there.
    fn snapshots(&self, _vmdir: &Path, _vm: &Vm) -> delonix_model::Result<Vec<String>> {
        Err(unsupported_snapshot(self.id(), "snapshots"))
    }
    /// Deletes a named snapshot — the state in the disk AND whatever metadata
    /// points at it. Default: unsupported (fail closed).
    fn delete_snapshot(&self, _vmdir: &Path, _vm: &Vm, _name: &str) -> delonix_model::Result<()> {
        Err(unsupported_snapshot(self.id(), "snapshot rm"))
    }

    /// Replaces what the engine wrote to ONE direction of the VM's own
    /// firewall with `policy` (ADR-0052). Rules this engine did not write are
    /// left where they are. Default: unsupported (fail closed). Neither local
    /// backend answers it today: a Cloud Hypervisor VM has anti-spoofing and
    /// namespace isolation on its tap but no per-VM rule chain, and a libvirt
    /// VM lives on `virbr0`, outside the SDN. Saying "applied" there would be
    /// the worst way a firewall can fail.
    fn apply_firewall(
        &self,
        _vmdir: &Path,
        _vm: &Vm,
        _policy: &firewall::Policy,
    ) -> delonix_model::Result<()> {
        Err(unsupported_firewall(self.id(), "apply"))
    }
    /// Reads back what [`VmBackend::apply_firewall`] would compare against:
    /// the direction's default verdict and the rules this engine wrote, as
    /// the node has them. Default: unsupported (fail closed).
    fn read_firewall(
        &self,
        _vmdir: &Path,
        _vm: &Vm,
        _direction: firewall::Direction,
    ) -> delonix_model::Result<firewall::Policy> {
        Err(unsupported_firewall(self.id(), "read"))
    }

    /// Saves whatever snapshot state STOPPING this VM would otherwise destroy,
    /// and returns the names saved. Called by `stop` BEFORE
    /// [`VmBackend::stop`], so a failure here aborts the stop with nothing lost
    /// yet. Default: nothing to preserve (a backend whose snapshots survive a
    /// stop, or which has none, keeps its behaviour byte for byte).
    ///
    /// This exists because of what libvirt's `undefine --snapshots-metadata`
    /// does: the snapshot DATA stays in the qcow2 (measured), only libvirt's
    /// bookkeeping is deleted — so a `vm stop`/`vm start` left `vm snapshots`
    /// empty with rc=0 and `vm restore` answering "Domain snapshot not found",
    /// for snapshots that were still there on the disk the whole time.
    fn preserve_snapshots(&self, _vmdir: &Path, _vm: &Vm) -> delonix_model::Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// `true` when the backend owns its own disks and `create` must NOT
    /// prepare one.
    ///
    /// The default is `false`, which is what both local backends are and what
    /// every existing implementation keeps without changing a line: `create`
    /// resolves `cfg.disk` on THIS filesystem and builds a thin qcow2 overlay
    /// for the VM, and `boot` receives that overlay's path.
    ///
    /// A backend whose hypervisor is on another machine cannot use any of it —
    /// the base image lives on that node, and a local overlay backs nothing
    /// there. Worse, `create` would fail on the local `canonicalize` before the
    /// backend was ever asked. With `true`, `boot` receives `cfg.disk`
    /// unchanged and decides for itself what it names on the far side.
    ///
    /// This exists because the alternative was uploading a local overlay on
    /// every create — a second disk model, and slow — purely to satisfy a
    /// signature (ADR-0008).
    fn manages_own_storage(&self) -> bool {
        false
    }

    /// `true` when auto-detection may pick this backend with nobody asking for
    /// it by name.
    ///
    /// Local backends answer `available()` with a `which`, which is cheap and
    /// truthful. A REMOTE backend cannot: the only honest answer needs a
    /// network round trip to a node that may not even be configured, and
    /// auto-detection is not a place to make HTTP requests. So a remote backend
    /// returns `false` here and is chosen explicitly (`--backend`,
    /// `DELONIX_VM_BACKEND`, `vm default-backend`) or not at all.
    fn auto_selectable(&self) -> bool {
        true
    }
}

/// Fail-closed error for a backend that does not implement pause/unpause.
///
/// Returns the SHARED type directly (not this crate's own `Result`): its two
/// callers are `VmBackend` default method bodies, and that trait's signatures
/// stay on `delonix_model::Result` — see the module doc comment on why.
fn unsupported_pause(backend: &str, op: &str) -> delonix_model::Error {
    Error::UnsupportedByBackend(format!("{op} is not supported on the '{backend}' backend")).into()
}

/// Fail-closed error for a backend that does not implement snapshot/restore
/// (today: cloud-hypervisor — its restore relaunches a fresh vmm, a different
/// lifecycle than libvirt's in-place revert, and needs `ch-remote`; deferred).
fn unsupported_snapshot(backend: &str, op: &str) -> delonix_model::Error {
    Error::UnsupportedByBackend(format!(
        "{op} is not supported on the '{backend}' backend yet — use the libvirt backend"
    ))
    .into()
}

/// Fail-closed error for a backend with no VM firewall of its own (ADR-0052).
fn unsupported_firewall(backend: &str, op: &str) -> delonix_model::Error {
    Error::UnsupportedByBackend(format!(
        "a VM firewall {op} is not supported on the '{backend}' backend — a `NetworkPolicy` with \
         `scope: vm` needs a backend whose node filters the VM (today: proxmox)"
    ))
    .into()
}

/// How a registered backend is built when somebody selects it.
///
/// A closure and not a `fn` pointer because a REMOTE backend needs
/// configuration — an endpoint, a node name, a credential — and
/// `fn() -> Box<dyn VmBackend>` has nowhere to receive it. That gap is
/// precisely what kept ADR-0008's decision 2 from landing: a crate that
/// depends on `delonix-vm` (as any backend must, for the trait) could not put
/// itself into a `static` table here.
///
/// `Send + Sync` because the table is process-wide. It constrains the CLOSURE,
/// not the trait: a backend implementation is untouched by this.
///
/// It returns `Result` so a backend whose construction can fail (a remote one
/// authenticating) reports why, instead of a factory that must panic or lie.
pub type BackendFactory = Box<dyn Fn() -> Result<Box<dyn VmBackend>> + Send + Sync>;

/// Builds the backend's capability report (ADR-0050). Called by `provider ls`,
/// never at registration: like [`BackendFactory`] it may probe the host, and
/// for a remote backend it must NOT connect — it declares, it does not verify.
pub type ReportFactory = Box<dyn Fn() -> crate::capability::ProviderReport + Send + Sync>;

/// One backend this build knows about: its canonical id (the value persisted in
/// [`Vm::backend`]), the aliases accepted on input, and how to build one.
pub struct BackendRegistration {
    /// Canonical id. Must equal what the built backend's [`VmBackend::id`]
    /// returns — it is what gets persisted in the record and looked up later.
    pub id: &'static str,
    /// Extra spellings accepted from a user; never repeats `id`.
    pub aliases: &'static [&'static str],
    /// Whether auto-detection may pick this backend with nobody naming it.
    ///
    /// **A copy of [`VmBackend::auto_selectable`], and deliberately so**:
    /// auto-detection has to answer this WITHOUT building the backend.
    /// Construction is where a remote backend authenticates, so asking the
    /// built object would make the walk do the network round trip the flag
    /// exists to prevent. `register_backend` checks the two agree.
    pub auto_selectable: bool,
    pub new: BackendFactory,
    /// The backend's answer to every catalog entry (ADR-0050). Required: a
    /// backend that cannot say what it supports is a backend nobody can
    /// select by requirement.
    pub report: ReportFactory,
}

impl std::fmt::Debug for BackendRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendRegistration")
            .field("id", &self.id)
            .field("aliases", &self.aliases)
            .field("auto_selectable", &self.auto_selectable)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn unsupported_snapshot_names_the_backend_and_op() {
        let e = super::unsupported_snapshot("cloud-hypervisor", "restore").to_string();
        assert!(e.contains("restore"), "{e}");
        assert!(e.contains("cloud-hypervisor"), "{e}");
        assert!(e.contains("libvirt"), "{e}");
    }

    #[test]
    fn unsupported_pause_names_the_backend_and_op() {
        let e = super::unsupported_pause("proxmox", "pause").to_string();
        assert!(e.contains("pause"), "{e}");
        assert!(e.contains("proxmox"), "{e}");
    }
}
