//! The VM use cases (ADR-0044 P4b.3b, `docs/discovery/61`): create, stop,
//! start, status, list, remove and the day-2 verbs, as methods of a
//! [`VmEngine`] that receives its record store, its backends, the local disk
//! work, the cloud-init seed and the network from the composition root. The
//! engine never opens a store, runs a command or names a backend: `delonix-vm`
//! assembles one per call over its `JsonStore`, its registry, `qemu-img` and
//! `cloud-localds`, and keeps its public functions as thin wrappers.

use crate::capability::Capability;
use crate::ports::{LocalDiskImages, SeedBuilder, VmBackends, VmNetwork};
use crate::vm_backend::{
    mem_mib, names_a_declared_network, parse_mem_mib, CloudInitIntent, CreateStage, DestroyStage,
    GuestInfo, HotplugOutcome, MoveOptions, VmBackend, VmConfig,
};
use crate::vm_error::{Error, Result};
use crate::vm_firewall as firewall;
use crate::{Vm, VmBootSpec};
use delonix_model::ports::StateRepository;
use delonix_model::records::Status;
use delonix_node::proc_starttime;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Refuses `network` unless the node's `net` declares it — the refusal
/// [`VmEngine::create_with`] makes before the overlay, public so a caller that
/// must answer BEFORE it starts the create (the node API acknowledges an
/// operation first) says the same thing with the same number.
///
/// Fails closed: with no network registered there is nothing to ask, and a
/// create that cannot check is not one that may go ahead. The caller decides
/// whether `network` is a question at all ([`names_a_declared_network`]).
pub fn refuse_undeclared_network(net: Option<&dyn VmNetwork>, network: &str) -> Result<()> {
    let net = net.ok_or_else(|| Error::Command {
        context: "vm",
        message: "no VM network provider is registered in this process".into(),
    })?;
    if net.is_declared(network)? {
        return Ok(());
    }
    Err(Error::NetworkNotDeclared(format!(
        "network '{network}': it is not declared on this node — create it first with \
         `delonix network create {network}`, or leave --network out for the default network"
    )))
}

/// `--disk-size`'s unit, everywhere in this crate: whole GiB.
const GIB_BYTES: u64 = 1024 * 1024 * 1024;

/// The VM use cases over the ports one call needs.
pub struct VmEngine<'a, R, B, D, S> {
    /// The state root; the VMs live under `<root>/vms`.
    pub root: &'a Path,
    pub repo: R,
    pub backends: B,
    pub disks: D,
    pub seed: S,
    /// The node's network, when one is registered in this process.
    pub network: Option<&'a dyn VmNetwork>,
}

impl<R, B, D, S> VmEngine<'_, R, B, D, S>
where
    R: StateRepository<Vm>,
    B: VmBackends,
    D: LocalDiskImages,
    S: SeedBuilder,
{
    fn vmdir(&self) -> PathBuf {
        self.root.join("vms")
    }

    /// The registered network, or the reason there is none — a VM on the SDN
    /// cannot be attached without one, and saying so beats a VM with no network.
    fn net(&self) -> Result<&dyn VmNetwork> {
        self.network.ok_or_else(|| Error::Command {
            context: "vm",
            message: "no VM network provider is registered in this process".into(),
        })
    }

    /// `true` if the `restart_policy` asks for an automatic restart but the
    /// backend does not declare `vm.restart-policy.native` (ADR-0050).
    fn restart_policy_unsupervised(&self, backend_id: &str, policy: Option<&str>) -> bool {
        matches!(policy, Some("always") | Some("on-failure"))
            && !self
                .backends
                .declares(backend_id, Capability::VmRestartPolicyNative)
    }

    pub fn create(&self, cfg: &VmConfig) -> Result<Vm> {
        self.create_with(cfg, &|_| {})
    }
    /// [`create`] with a progress callback: `on` fires once per [`CreateStage`] as
    /// the VM is built (disk → network → define → start), so the CLI can render
    /// step-by-step progress. The engine emits only the enum; the text lives in the bin.
    ///
    /// Backend precedence when `cfg.backend` is `None`: `DELONIX_VM_BACKEND` env
    /// var, then [`get_default_backend`] (persisted, [`set_default_backend`]),
    /// then the capability heuristic (volumes ⇒ libvirt; cloud image without a
    /// kernel ⇒ libvirt if available). Lives here (not just in the CLI) so every
    /// consumer of this API — `stack apply`/`cluster kubeadm` included — inherits
    /// it for free.
    pub fn create_with(&self, cfg: &VmConfig, on: &dyn Fn(CreateStage)) -> Result<Vm> {
        if !valid_vm_name(&cfg.name) {
            return Err(Error::InvalidName(format!(
                "invalid VM name '{}' — use letters, digits, '.', '_' or '-' (no '/', '..', or leading '-')",
                cfg.name
            )));
        }
        // Requirements are NAMES until here; an unknown one is refused before
        // any store is opened or any backend asked (ADR-0050 D6).
        let required = resolve_required_capabilities(&cfg.required_capabilities)?;
        let vmdir = self.vmdir();
        std::fs::create_dir_all(&vmdir)?;
        let st = &self.repo;

        let restarting = st.get(&cfg.name).ok();
        // Anti-clobber (two VM subsystems in the SAME folder): if a
        // `<name>.json` that does NOT parse as a declarative Vm already exists, it is a direct-QEMU
        // record (`vm run`) — refuse instead of overwriting it and leaving that VM orphaned.
        if restarting.is_none() && vmdir.join(format!("{}.json", cfg.name)).exists() {
            return Err(Error::RecordConflict(format!(
                "a VM '{}' created by `vm run` (direct-QEMU) already exists. Remove it first \
                 (`vm rm {}`) or use another name — the two subsystems share the vms/ folder.",
                cfg.name, cfg.name
            )));
        }
        // On restart, honor the backend the VM already used; otherwise choose now.
        let backend: Box<dyn VmBackend> = match &restarting {
            Some(ex) => {
                // Resolved ONCE. It used to be built twice, which is free for a
                // local backend and a second authentication for a remote one.
                let b = self.backends.for_vm(ex)?;
                // A requirement holds on a restart too: the backend is the record's,
                // and if it cannot do what the caller now requires the answer is
                // the same refusal, not a VM that came back without it.
                self.backends.require(b.id(), &required)?;
                if b.is_running(ex) {
                    return Ok(ex.clone()); // already running — idempotent
                }
                b
            }
            None => self.backends.select(self.root, cfg, &required)?,
        };

        // A held create is only honest where the firewall can be closed before the
        // guest runs; otherwise the VM would come up open under a policy that
        // says it is closed.
        if cfg.policy_hold && restarting.is_none() && !backend.holds_at_boot() {
            return Err(Error::UnsupportedByBackend(format!(
                "the '{}' backend cannot create a VM closed (policy hold): it has no firewall of \
                 its own that can be in force before the guest starts. A `scope: vm` policy is \
                 for a backend that has one (proxmox)",
                backend.id()
            )));
        }

        // Admission: refuses to boot if there is no RAM on the host (anti-overcommit).
        // Only the VMs that will REALLY boot (not the idempotent already-running one above).
        admission_check(cfg)?;

        // Namespace isolation is enforceable only where the VM is on OUR dataplane.
        // Refuse rather than accept-and-ignore — see `vm_namespace_supported`.
        // The anti-spoofing opt-out exists only where the filter does (ADR-0055).
        self.backends.admit(backend.id(), cfg)?;

        let ns = vm_namespace_of(cfg);
        if ns != "default"
            && !self
                .backends
                .declares(backend.id(), Capability::VmNamespaceIsolation)
        {
            return Err(Error::NamespaceUnsupported(format!(
                "namespace '{ns}' is not enforceable on the '{}' backend: its VMs live on the host's \
                 libvirt bridge, outside the Delonix SDN, so nothing here can isolate them. Use \
                 `--backend cloud-hypervisor` (its VMs share the containers' SDN), or drop \
                 `--namespace`",
                backend.id()
            )));
        }

        // A backend that owns its storage gets `cfg.disk` verbatim and nothing is
        // prepared here: the local canonicalize would fail on an image that lives
        // on the remote node, before the backend was ever asked (ADR-0008).
        // `disk_path` is what the record keeps as the VM's base image; `overlay` is
        // what `boot` is handed. For a backend that owns its storage they are the
        // same string the caller wrote — this engine does not get to reinterpret a
        // name that means something on the far node.
        let own_storage = backend.manages_own_storage();

        // A VM on a network nobody declared is refused here, before the
        // overlay — the first thing that leaves something on disk. Measured
        // (2026-10-21): `vm create --network cf-nope --backend libvirt` built the
        // overlay, defined the domain and only then died on libvirt's own «no
        // network with matching name», after a warning promising a fallback to
        // `default` that never happened; Cloud Hypervisor instead CREATED the
        // network on the fly, so a typo became a network. The `--help` and
        // `kind: VM` (`stack validate`) already said the network must exist.
        //
        // Local backends only: a backend that owns its storage speaks the far
        // node's network vocabulary and refuses what it cannot map itself
        // (Proxmox).
        //
        // A create over a STOPPED record asks too, unless it names the network
        // the record already has: the recover path boots with `cfg`, not with
        // the record, so `vm create <stopped> --network <typo>` moved the VM and
        // Cloud Hypervisor created the typo (found by the cli-tester on this
        // very change). Only the unchanged network is spared — a VM that already
        // booted on it keeps booting, even on a network an older version made.
        let changes_network = match &restarting {
            Some(ex) => ex.network != cfg.network,
            None => true,
        };
        if changes_network && !own_storage && names_a_declared_network(&cfg.network) {
            refuse_undeclared_network(self.network, &cfg.network)?;
        }

        // A create of a NEW VM that fails takes back what it wrote: with no
        // record, nothing else ever will — `vm ls` does not list the leftovers
        // and `rm` answers «no such VM» and keeps them (it deletes nothing it
        // cannot prove is a VM's, see `remove_inner`). The per-VM directory is
        // taken back only if this call brought it into being: one already there
        // holds a `--user-data` seed the caller packaged, or what an earlier
        // attempt left, and is not this call's to remove.
        let fresh = restarting.is_none();
        let seed_dir = vmdir.join(&cfg.name);
        let seed_dir_is_ours = fresh && !seed_dir.exists();
        let undo = |overlay: &Path| {
            if !fresh {
                return;
            }
            // Only when WE made it. With `manages_own_storage`, `overlay` IS
            // `cfg.disk` verbatim — the name the caller wrote for something on
            // the far node — and removing it means this engine deleting a file
            // it did not create. For today's Proxmox backend that name is
            // `local-lvm:8` and the unlink simply fails, but the rule cannot
            // rest on the spelling a backend happens to use: a remote backend
            // whose disk reference IS a local path would lose the user's base
            // image on a failed boot.
            if !own_storage {
                let _ = std::fs::remove_file(overlay);
            }
            if seed_dir_is_ours {
                let _ = std::fs::remove_dir_all(&seed_dir);
            }
        };
        let (disk_path, overlay) = if own_storage {
            (
                std::path::PathBuf::from(&cfg.disk),
                std::path::PathBuf::from(&cfg.disk),
            )
        } else {
            self.disks
                .overlay(&vmdir, &cfg.name, &cfg.disk, cfg.disk_size_gib, on)?
        };

        // REALIZE the cloud-init intent, for the backends that need it realized as a
        // file. A local backend has no vocabulary for "this VM should have this
        // hostname and these keys" other than a NoCloud ISO, so the engine builds
        // one here; a backend that owns its storage is handed the intent verbatim
        // and maps it to whatever the far node speaks (Proxmox: `--ciuser`/
        // `--sshkeys`), which is the entire reason these fields exist.
        //
        // This used to be the CALLER's job, and that is what kept cloud-init local:
        // every consumer built its own ISO — the CLI, `cluster kubeadm`, and other
        // programs in copies of their own — so a remote backend could only ever
        // receive a path it could not open.
        //
        // An explicit `seed` always wins: someone who built their own is not asking
        // us to guess. An appliance (`cloud_init: Some(false)`) gets nothing — an
        // ISO nobody reads, on a drive that changes the guest's device list.
        let realized;
        let cfg = if cfg.seed.is_none() && cfg.cloud_init != Some(false) && !own_storage {
            let iso = self
                .seed
                .seed(self.root, cfg)
                .inspect_err(|_| undo(&overlay))?;
            realized = VmConfig {
                seed: Some(iso.to_string_lossy().into_owned()),
                ..cfg.clone()
            };
            &realized
        } else {
            cfg
        };

        // An EXISTING, stopped VM gets a chance to be resumed before anything is
        // created. Both local backends answer `None` here (their `boot` is already
        // idempotent — it reuses the per-VM overlay on this filesystem), so this is
        // invisible to them. A remote backend's `boot` asks the node for the next
        // free id, so without this a `vm start` built a SECOND VM and orphaned the
        // first, with the record rewritten to the new handle and nothing left
        // pointing at the old one.
        let resumed = match &restarting {
            Some(ex) => backend.resume(&vmdir, ex)?,
            None => None,
        };

        let boot = match resumed {
            Some(b) => b,
            None => match backend.boot(&vmdir, cfg, &overlay.to_string_lossy(), on) {
                Ok(b) => b,
                Err(e) => {
                    undo(&overlay);
                    return Err(e.into());
                }
            },
        };

        let mut vm = Vm::new(
            cfg.name.clone(),
            disk_path.to_string_lossy().into_owned(),
            overlay.to_string_lossy().into_owned(),
            cfg.vcpus.max(1),
            cfg.memory.clone(),
            cfg.network.clone(),
            boot.tap,
            boot.mac,
            boot.api_socket,
        );
        vm.pid = boot.pid;
        // Registado JUNTO do pid, e a partir do MESMO pid: é o par que torna o
        // `stop` capaz de distinguir este VMM de um pid reciclado mais tarde.
        vm.pid_starttime = boot.pid.and_then(proc_starttime);
        vm.status = Status::Running;
        vm.restart_policy = cfg.restart_policy.clone();
        vm.namespace = ns.clone();
        vm.ip = boot.ip;
        vm.dhcp_lease_floor = boot.lease_floor;
        vm.backend = backend.id().to_string();
        // A restart rebuilds the record from `cfg`, which describes the boot
        // shape and nothing else: the labels and annotations the record
        // carried (the stack's ownership stamp, the last-applied spec, the
        // declared login, a policy hold) are not in it. Dropping them made a
        // `vm stop` + `vm start` — and the reconciler's own reboot class
        // (ADR-0012) — leave a stack's VM unowned, so the next plan proposed
        // adopting it and a `--prune` no longer saw it.
        if let Some(ex) = &restarting {
            vm.labels = ex.labels.clone();
            vm.annotations = ex.annotations.clone();
        }
        if cfg.policy_hold && restarting.is_none() {
            vm.annotations
                .insert(POLICY_HOLD_ANNOTATION.to_string(), "1".to_string());
        }
        vm.devices = cfg.devices.clone();
        vm.boot = boot_spec_of(cfg);
        vm.vcpus_max = cfg.vcpus_max;
        vm.memory_max_mib = cfg.memory_max_mib;
        vm.started_unix = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );
        // restart_policy HONESTY: only a backend that declares
        // `vm.restart-policy.native` materializes it (libvirt: `<on_crash>restart`
        // in the XML). On Cloud Hypervisor there is no supervisor on the host — warn
        // instead of silently accepting a policy that is not enforced instantly.
        if self.restart_policy_unsupervised(backend.id(), vm.restart_policy.as_deref()) {
            tracing::warn!(
                vm = %cfg.name,
                backend = %backend.id(),
                restart_policy = %vm.restart_policy.as_deref().unwrap_or(""),
                "restart_policy '{}' on VM '{}' (backend {}) is NOT supervised on the host: the restart \
                 happens on the next `delonix apply`/reconcile (auto-heal), not instantly on \
                 crash. For immediate restart use `--backend libvirt`.",
                vm.restart_policy.as_deref().unwrap_or(""),
                cfg.name,
                backend.id()
            );
        }
        st.set(&cfg.name, &vm).map_err(Error::from)?;
        Ok(vm)
    }
    /// Removes a VM: stops the VMM (via its backend), and deletes overlay/state.
    ///
    /// If the backend cleanup fails (e.g. libvirt refuses the undefine), the local
    /// record stays **INTACT** and the error propagates — the old version deleted the record
    /// anyway and the VM was orphaned in libvirt, invisible to `vm ls`/`vm stop`. It also covers
    /// the reverse: no local record but with an orphaned domain in libvirt (an
    /// old interrupted `rm`), `remove` cleans up the domain anyway.
    pub fn remove(&self, name: &str) -> Result<()> {
        self.remove_inner(name, false, false, &|_| {}).map(|_| ())
    }
    /// Like [`remove`], but deletes the local state EVEN if the backend cleanup
    /// fails (the `vm rm --force`) — the user takes on resolving the rest in libvirt.
    pub fn remove_force(&self, name: &str) -> Result<()> {
        self.remove_inner(name, true, false, &|_| {}).map(|_| ())
    }
    /// The full teardown of a VM: the provider's own destroy (libvirt domain with
    /// managed-save/snapshot metadata/NVRAM and the DHCP reservation, or a Proxmox
    /// VM purged from the node with its disks), then every local artifact —
    /// overlay, seed, sockets, serial log, pid, domain XML, the per-VM directory
    /// with its preserved snapshots, and extra disks in the state directory.
    ///
    /// `purge_disks` widens the last step to extra disks OUTSIDE the state
    /// directory; without it they are listed in [`Destroyed::kept`]. `force`
    /// deletes the local state even when the provider refuses.
    pub fn destroy(&self, name: &str, force: bool, purge_disks: bool) -> Result<Destroyed> {
        self.remove_inner(name, force, purge_disks, &|_| {})
    }
    /// [`destroy`] that reports each [`DestroyStage`] as it starts.
    pub fn destroy_with(
        &self,
        name: &str,
        force: bool,
        purge_disks: bool,
        on: &dyn Fn(DestroyStage),
    ) -> Result<Destroyed> {
        self.remove_inner(name, force, purge_disks, on)
    }
    fn remove_inner(
        &self,
        name: &str,
        force: bool,
        purge_disks: bool,
        on: &dyn Fn(DestroyStage),
    ) -> Result<Destroyed> {
        // A name that `create` would refuse cannot exist — and above all, it cannot
        // flow into the paths deleted below (the seed dir's `remove_dir_all`).
        if !valid_vm_name(name) {
            return Err(Error::VmNotFound(name.to_string()));
        }
        let vmdir = self.vmdir();
        let st = &self.repo;
        let mut record: Option<Vm> = None;
        let mut provider_released: Option<String> = None;
        let existed = match st.get(name) {
            Ok(vm) => {
                // `destroy`, not `stop`: the record is going away, so whatever the
                // backend still owns has to go with it. They are the same call for
                // the local backends (the default), and deliberately not for a
                // remote one, whose disk lives on the node.
                on(DestroyStage::Provider(&vm.backend));
                let backend = self.backends.for_vm(&vm).map_err(Error::from);
                provider_released = backend
                    .as_ref()
                    .ok()
                    .filter(|b| b.manages_own_storage())
                    .map(|b| b.id().to_string());
                if let Err(e) = backend.and_then(|b| b.destroy(&vmdir, &vm).map_err(Error::from)) {
                    if !force {
                        return Err(e); // record intact — the rm can be retried
                    }
                }
                record = Some(vm);
                true
            }
            Err(_) => {
                // No record: there may be an orphaned libvirt domain with this name —
                // clean it up, and the ingress tap for safety.
                let orphan = self.backends.unrecorded(name);
                if let Some(held_by) = orphan {
                    on(DestroyStage::Provider(held_by));
                }
                if let Err(e) = self.backends.remove_unrecorded(name) {
                    if !force {
                        return Err(e.into());
                    }
                }
                // No record, so no address to trust — the tap goes, the firewall
                // teardown is skipped rather than guessed at. Best effort, as it
                // always was: a process with no network registered has no tap to
                // remove, and the answer to an `rm` of a name that does not exist
                // stays «no such VM», not a complaint about the network.
                if let Ok(net) = self.net() {
                    net.detach_tap(name, None);
                }
                orphan.is_some()
            }
        };
        // CHECK BEFORE DELETING. This block used to run BEFORE the `existed` test,
        // so `vm rm <name>` on a VM that has no record and no libvirt domain deleted
        // `<name>.qcow2` and the whole seed directory and THEN returned "no such VM"
        // with a non-zero exit — an operator reading that error reasonably concludes
        // nothing happened, while a multi-gigabyte disk image has just been removed.
        // Reproduced live: a stray `auditghost.qcow2` was destroyed by a command that
        // reported failure. Same shape as the volume `rm` that unlinked its metadata
        // before failing: destroy nothing until we know the object is really ours to
        // destroy.
        if !existed {
            // Neither a local record nor a domain in libvirt — the `st.remove` below is
            // idempotent (absence is not an error) and would say Ok; an `rm` of something that
            // does not exist should say so, like docker.
            return Err(Error::VmNotFound(name.to_string()));
        }
        let mut out = Destroyed {
            provider_released,
            ..Destroyed::default()
        };
        let rm_file = |out: &mut Destroyed, p: &Path, label: String| {
            let sz = path_size(p);
            if std::fs::remove_file(p).is_ok() {
                out.freed_bytes += sz;
                out.removed.push(label);
            }
        };
        // The overlay first: it is the bulk of what is freed.
        if vmdir.join(format!("{name}.qcow2")).exists() {
            on(DestroyStage::Overlay);
            rm_file(
                &mut out,
                &vmdir.join(format!("{name}.qcow2")),
                format!("{name}.qcow2"),
            );
        }
        // Extra disks and shares named by the record. Only what lives inside the
        // state directory is provably ours; the rest is reported, and removed only
        // when the operator said so.
        if let Some(vm) = &record {
            let root = std::fs::canonicalize(&vmdir).unwrap_or_else(|_| vmdir.clone());
            let mut doomed: Vec<&str> = Vec::new();
            for d in &vm.boot.extra_disks {
                let p = Path::new(&d.source);
                if d.device == "cdrom" || !p.exists() {
                    continue;
                }
                let inside = std::fs::canonicalize(p)
                    .map(|c| c.starts_with(&root))
                    .unwrap_or(false);
                if inside || purge_disks {
                    doomed.push(&d.source);
                } else {
                    out.kept.push(format!(
                        "{} (extra disk outside the state directory; --purge-disks removes it)",
                        d.source
                    ));
                }
            }
            if !doomed.is_empty() {
                on(DestroyStage::ExtraDisks(doomed.len()));
                for src in doomed {
                    rm_file(&mut out, Path::new(src), src.to_string());
                }
            }
            for v in &vm.boot.volumes {
                out.kept.push(format!(
                    "{} (shared volume — remove it with `volume rm`)",
                    v.source
                ));
            }
        }
        // The cloud-init seed directory (`vms/<name>/`, from `generate_seed_iso`)
        // and the preserved snapshots inside it also belong to the VM.
        let dir = vmdir.join(name);
        if dir.exists() {
            on(DestroyStage::SeedAndSnapshots);
            let sz = path_size(&dir);
            if std::fs::remove_dir_all(&dir).is_ok() && sz > 0 {
                out.freed_bytes += sz;
                out.removed.push(format!("{name}/"));
            }
        }
        let leftovers = ["sock", "sock.lock", "serial", "log", "pid", "xml"];
        if leftovers
            .iter()
            .any(|e| vmdir.join(format!("{name}.{e}")).exists())
        {
            on(DestroyStage::RuntimeState);
            for ext in leftovers {
                rm_file(
                    &mut out,
                    &vmdir.join(format!("{name}.{ext}")),
                    format!("{name}.{ext}"),
                );
            }
        }
        on(DestroyStage::Record);
        st.remove(name).map_err(Error::from)?;
        // The store's per-record lock file (`.<name>.lock`) is the last trace: it
        // outlived every destroy, so «removes everything» was not quite true.
        // Nothing holds it once the record is gone.
        let _ = std::fs::remove_file(vmdir.join(format!(".{name}.lock")));
        Ok(out)
    }
    /// Stops the VM via ITS backend (CH/libvirt) but **preserves** the record and disk
    /// (resumable). Unlike `remove`, it deletes nothing. Fixes the case where
    /// the CLI's `vm stop` (direct-QEMU scheme) did not know how to stop a declarative
    /// libvirt VM (pid null → the domain stayed alive, orphaned).
    pub fn stop(&self, name: &str) -> Result<()> {
        let vmdir = self.vmdir();
        let st = &self.repo;
        let mut vm = match st.get(name) {
            Ok(vm) => vm,
            // No local record, but with a domain in libvirt (orphaned from an old
            // `rm`): power it off anyway — the intent is unambiguous and answering
            // "no such VM" for a VM that libvirt lists would be a lie.
            Err(e) if e.is_not_found() => {
                return if self.backends.stop_unrecorded(name)? {
                    Ok(())
                } else {
                    Err(Error::VmNotFound(name.to_string()))
                };
            }
            Err(e) => return Err(Error::from(e)),
        };
        let backend = self.backends.for_vm(&vm)?;
        // BEFORE the stop, and its failure aborts the stop: on libvirt the stop
        // undefines the domain, and the undefine deletes the snapshot metadata.
        // `remove` deliberately does NOT come through here — there the whole
        // per-VM directory goes anyway.
        backend.preserve_snapshots(&vmdir, &vm)?;
        backend.stop(&vmdir, &vm)?;
        vm.status = Status::Stopped;
        vm.pid = None;
        vm.started_unix = None;
        st.set(name, &vm).map_err(Error::from)?;
        // AFTER the save: the vmm is confirmed gone and the record already says
        // so correctly either way — an `Err` from here is a diagnosis on top of a
        // stop that already happened, never a reason to leave the record lying
        // about a dead vmm being `Running`. See `VmBackend::disk_health`.
        Ok(backend.disk_health(&vmdir, &vm)?)
    }
    /// Loads a VM record, mapping the shared `NotFound` to the VM-specific
    /// `VmNotFound` ("no such VM: …") — same idiom as `stop`/`status`.
    fn load_vm(&self, name: &str) -> Result<Vm> {
        self.repo.get(name).map_err(|e| {
            if e.is_not_found() {
                Error::VmNotFound(name.to_string())
            } else {
                Error::from(e)
            }
        })
    }
    /// Suspends a RUNNING VM's vCPUs (see [`VmBackend::pause`]). Refuses a VM
    /// that is not currently `Running`, rather than a silent no-op — the caller
    /// would otherwise have no way to tell "already paused" from "just paused".
    pub fn pause(&self, name: &str) -> Result<()> {
        let vmdir = self.vmdir();
        let st = &self.repo;
        let mut vm = self.load_vm(name)?;
        if vm.status != Status::Running {
            return Err(Error::NotRunningForOp(format!(
                "VM '{name}' is not running (status: {:?}) — nothing to pause",
                vm.status
            )));
        }
        self.backends.for_vm(&vm)?.pause(&vmdir, &vm)?;
        vm.status = Status::Paused;
        st.set(name, &vm).map_err(Error::from)
    }
    /// Resumes a VM suspended with [`pause`]. Refuses a VM that is not currently
    /// `Paused`.
    pub fn unpause(&self, name: &str) -> Result<()> {
        let vmdir = self.vmdir();
        let st = &self.repo;
        let mut vm = self.load_vm(name)?;
        if vm.status != Status::Paused {
            return Err(Error::NotRunningForOp(format!(
                "VM '{name}' is not paused (status: {:?}) — nothing to resume",
                vm.status
            )));
        }
        self.backends.for_vm(&vm)?.unpause(&vmdir, &vm)?;
        vm.status = Status::Running;
        st.set(name, &vm).map_err(Error::from)
    }
    /// Changes a STOPPED VM's cloud-init — hostname, user and/or SSH keys — for
    /// its next boot (see [`VmBackend::update_cloud_init`]).
    ///
    /// Everything refusable is refused before a backend is asked, with the record
    /// untouched: nothing to change, a hostname that is not a DNS label, a user
    /// that is not a login name, a key that is empty or spans lines, an appliance
    /// (which does not run cloud-init), and a VM that is running or paused. `keys`
    /// REPLACE the record's when given; a field not given keeps its value, and the
    /// backend receives the whole merged intent. The record is rewritten only
    /// after the backend returns `Ok`.
    pub fn set_cloud_init(
        &self,
        name: &str,
        hostname: Option<&str>,
        ci_user: Option<&str>,
        keys: Option<Vec<String>>,
    ) -> Result<Vm> {
        let bad = |m: String| Error::InvalidCloudInitChange(m);
        if hostname.is_none() && ci_user.is_none() && keys.is_none() {
            return Err(bad(format!(
                "nothing to change in VM '{name}''s cloud-init: give --hostname, --user and/or --ssh-key"
            )));
        }
        if let Some(h) = hostname.filter(|h| !valid_hostname(h)) {
            return Err(bad(format!(
                "hostname '{h}' is not a DNS label (letters, digits, '-', not at either end, at most 63)"
            )));
        }
        if let Some(u) = ci_user.filter(|u| !valid_login(u)) {
            return Err(bad(format!("user '{u}' is not a login name")));
        }
        if let Some(k) = &keys {
            if k.is_empty() {
                return Err(bad("give at least one --ssh-key".to_string()));
            }
            if let Some((i, _)) = k
                .iter()
                .enumerate()
                .find(|(_, key)| key.trim().is_empty() || key.contains('\n'))
            {
                return Err(bad(format!("ssh key #{} is empty or spans lines", i + 1)));
            }
        }
        let vmdir = self.vmdir();
        let st = &self.repo;
        let mut vm = self.load_vm(name)?;
        if vm.boot.cloud_init == Some(false) {
            return Err(bad(format!(
                "VM '{name}' runs an appliance image, which does not run cloud-init"
            )));
        }
        if matches!(vm.status, Status::Running | Status::Paused) {
            return Err(Error::CloudInitNeedsStopped(format!(
                "VM '{name}' is {:?}: the guest reads cloud-init at boot — stop it first (`delonix vm stop {name}`)",
                vm.status
            )));
        }
        let intent = CloudInitIntent {
            hostname: hostname
                .map(str::to_string)
                .or_else(|| vm.boot.hostname.clone()),
            ci_user: ci_user
                .map(str::to_string)
                .or_else(|| vm.boot.ci_user.clone()),
            ssh_keys: keys
                .map(|k| k.into_iter().map(|s| s.trim().to_string()).collect())
                .unwrap_or_else(|| vm.boot.ssh_keys.clone()),
        };
        self.backends
            .for_vm(&vm)?
            .update_cloud_init(&vmdir, &vm, &intent)?;
        vm.boot.hostname = intent.hostname;
        vm.boot.ci_user = intent.ci_user;
        vm.boot.ssh_keys = intent.ssh_keys;
        st.set(name, &vm).map_err(Error::from)?;
        Ok(vm)
    }
    /// Changes a STOPPED VM's vCPUs, memory and/or boot disk size for its
    /// next boot — the cold resize (`vm.resize.cold`/`vm.disk.resize`, see
    /// [`VmBackend::resize_cold`]/[`VmBackend::resize_disk`]).
    ///
    /// Everything that can be refused is refused before the backend is
    /// asked: nothing to change, zero vCPUs, a memory value that does not
    /// parse (the lenient [`mem_mib`] would read `2GB` as 1 GiB and this
    /// would report it done), zero GiB of disk, and a VM that is running or
    /// paused — a guest that only sees the change after its next reboot has
    /// not been resized yet. The disk is grown FIRST, before vcpus/memory:
    /// growing a disk is the one part of this that touches a file on disk
    /// and can fail for reasons nothing else here can (space, a backend's
    /// own grow-only guard), and a half-applied resize should at least never
    /// have claimed a vcpu/memory change that then didn't happen.
    ///
    /// Shrinking the disk is never offered here — not refused silently, not
    /// offered at all: there is no flag for it. A filesystem does not
    /// retreat with its block device, and every backend that implements
    /// `resize_disk` refuses a shrink before touching anything.
    ///
    /// Returns the updated record. The record carries no field for the
    /// disk's size — it lives in the backend's own disk, queried live
    /// (`vm describe`/the backend's `resize_disk`), the same way the engine
    /// never duplicates a container's rootfs size either.
    pub fn resize(
        &self,
        name: &str,
        vcpus: Option<u32>,
        memory: Option<&str>,
        disk_size_gib: Option<u32>,
    ) -> Result<Vm> {
        self.resize_shape(
            name,
            &ResizeRequest {
                vcpus,
                memory: memory.map(str::to_string),
                disk_size_gib,
                ..ResizeRequest::default()
            },
        )
    }

    /// The cold resize with the hotplug ceilings too (ADR-0068 open question
    /// 2: the ceiling changes only with the VM stopped, and holds from the
    /// next boot). One path for `vm resize` and for the reconciler's reboot
    /// class (ADR-0012), so the two cannot disagree about what a stopped VM
    /// may become.
    ///
    /// The backend is handed the record AS IT WILL BE (new ceilings included)
    /// and the record is written only after it answers — the order every
    /// resize here already uses.
    pub fn resize_shape(&self, name: &str, req: &ResizeRequest) -> Result<Vm> {
        // An empty request is the CALLER's mistake, whatever the VM: refused
        // before the lookup, so `vm resize <unknown>` with no flag answers
        // "nothing to resize" (class 1), as it always did, not "no such VM".
        refuse_empty(name, req)?;
        let vmdir = self.vmdir();
        let st = &self.repo;
        let mut vm = self.load_vm(name)?;
        let (target_vcpus, target_mib, vcpus_max, memory_max_mib) = check_shape(name, &vm, req)?;
        if matches!(vm.status, Status::Running | Status::Paused) {
            return Err(Error::ResizeNeedsStopped(format!(
                "VM '{name}' is {:?}: `vm resize` is a cold resize — stop it first (`delonix vm stop {name}`)",
                vm.status
            )));
        }
        let backend = self.backends.for_vm(&vm)?;
        if let Some(gib) = req.disk_size_gib {
            backend.resize_disk(&vmdir, &vm, u64::from(gib) * GIB_BYTES)?;
        }
        let mut shaped = vm.clone();
        shaped.vcpus = target_vcpus;
        if let Some(m) = &req.memory {
            shaped.memory = m.trim().to_string();
        }
        shaped.vcpus_max = vcpus_max;
        shaped.memory_max_mib = memory_max_mib;
        backend.resize_cold(&vmdir, &shaped, target_vcpus, target_mib)?;
        vm = shaped;
        st.set(name, &vm).map_err(Error::from)?;
        Ok(vm)
    }

    /// Whether [`Self::resize_shape`] would take `req` — every rule it
    /// applies except the power state, and nothing changed. A caller that
    /// has to STOP the VM first asks this before the stop, so a shape that
    /// would be refused never costs the downtime.
    pub fn check_resize_shape(&self, name: &str, req: &ResizeRequest) -> Result<()> {
        refuse_empty(name, req)?;
        let vm = self.load_vm(name)?;
        check_shape(name, &vm, req).map(|_| ())
    }

    /// `vm update <name> [--vcpus N] [--memory M]` (ADR-0068 D1) — ONE verb
    /// for both states: on a RUNNING VM it hot-adds, live, up to the
    /// declared ceiling (D3); on a STOPPED one it is exactly [`Self::resize`]
    /// (the cold path, unchanged), so nothing has to ask which verb applies.
    ///
    /// Only ADD (D10): a target at or below what the VM already has is
    /// refused, naming `vm resize` (stopped) as the way to decrease — hot
    /// removal is a separate, later capability, not a silent downgrade to
    /// the cold path. Disks and NICs go through [`Self::change_device`].
    ///
    /// Each field that changes is written to the record as soon as the
    /// backend confirms it (D6, "hypervisor first, read-back, record last,
    /// one operation at a time" — the same order [`Self::resize`] already
    /// uses, no transaction across the two fields), so a `Partial` CPU add
    /// still leaves memory free to be asked for separately, and a failure on
    /// the second field never rolls back the first.
    ///
    /// Returns the updated record and, for a RUNNING VM, the
    /// [`HotplugOutcome`] of every field that was asked for — empty for the
    /// cold path, which has nothing live to report.
    pub fn update(
        &self,
        name: &str,
        vcpus: Option<u32>,
        memory: Option<&str>,
        wait: Duration,
    ) -> Result<(Vm, Vec<(&'static str, HotplugOutcome)>)> {
        if vcpus.is_none() && memory.is_none() {
            return Err(Error::InvalidResize(format!(
                "nothing to update on VM '{name}': give --vcpus, --memory, --disk-add, \
                 --nic-add, --disk-rm and/or --nic-rm"
            )));
        }
        let target_mib = match memory {
            Some(m) => Some(parse_mem_mib(m).ok_or_else(|| {
                Error::InvalidResize(format!(
                    "memory '{m}' is not a size: use a number with an optional M/G suffix (512M, 4G, 4Gi)"
                ))
            })?),
            None => None,
        };
        let vm = self.load_vm(name)?;
        if vm.status != Status::Running {
            let vm = self.resize(name, vcpus, memory, None)?;
            return Ok((vm, Vec::new()));
        }
        let vmdir = self.vmdir();
        let st = &self.repo;
        let backend = self.backends.for_vm(&vm)?;
        let mut vm = vm;
        let mut outcomes = Vec::new();
        if let Some(target) = vcpus {
            if target <= vm.vcpus {
                return Err(Error::InvalidResize(format!(
                    "VM '{name}' already has {} vCPUs — `vm update` only adds; use `vm resize` \
                     on a stopped VM for a decrease",
                    vm.vcpus
                )));
            }
            let ceiling = vm.vcpus_max.unwrap_or(vm.vcpus);
            if target > ceiling {
                return Err(above_ceiling(
                    name,
                    "vCPUs",
                    &target.to_string(),
                    &ceiling.to_string(),
                    "--vcpus-max",
                ));
            }
            let (assigned, outcome) = backend.hotplug_add_cpu(&vmdir, &vm, target, wait)?;
            vm.vcpus = assigned;
            st.set(name, &vm).map_err(Error::from)?;
            outcomes.push(("vcpus", outcome));
        }
        if let Some(target_mib) = target_mib {
            let current_mib = mem_mib(&vm.memory);
            if target_mib <= current_mib {
                return Err(Error::InvalidResize(format!(
                    "VM '{name}' already has {current_mib} MiB — `vm update` only adds; use \
                     `vm resize` on a stopped VM for a decrease"
                )));
            }
            let ceiling = vm.memory_max_mib.unwrap_or(current_mib);
            if target_mib > ceiling {
                return Err(above_ceiling(
                    name,
                    "MiB of memory",
                    &target_mib.to_string(),
                    &ceiling.to_string(),
                    "--memory-max",
                ));
            }
            let (assigned_mib, outcome) =
                backend.hotplug_add_memory(&vmdir, &vm, target_mib, wait)?;
            vm.memory = format!("{assigned_mib}M");
            st.set(name, &vm).map_err(Error::from)?;
            outcomes.push(("memory", outcome));
        }
        Ok((vm, outcomes))
    }
    /// `vm update --disk-add/--nic-add/--disk-rm/--nic-rm` (ADR-0068 D7/D10):
    /// one device change. On a RUNNING VM an ADD is hot-plugged — hypervisor
    /// first, read back, record last (D6), with the target/MAC the hypervisor
    /// gave pinned in the record so a restart keeps it where the guest saw it;
    /// a REMOVAL is refused by name (D10: hot removal needs the guest to
    /// release the device first, and a lost answer would leave a disk the
    /// guest still writes to detached). On a STOPPED VM both change the
    /// record, and the backend is told ([`VmBackend::change_devices_cold`]).
    /// A paused VM is refused: unpause or stop it first.
    pub fn change_device(
        &self,
        name: &str,
        change: DeviceChange,
        wait: Duration,
    ) -> Result<(Vm, Option<HotplugOutcome>)> {
        let mut vm = self.load_vm(name)?;
        let vmdir = self.vmdir();
        let backend = self.backends.for_vm(&vm)?;
        match vm.status {
            Status::Paused => Err(Error::ResizeNeedsStopped(format!(
                "VM '{name}' is paused: unpause it (`delonix vm unpause {name}`) or stop it \
                 before changing its devices"
            ))),
            Status::Running => {
                let outcome = match change {
                    DeviceChange::AddDisk(d) => {
                        check_new_disk(&vm, &d)?;
                        let base = vm.clone();
                        let mut recorded: Option<Vm> = None;
                        let (d, o) =
                            backend.hotplug_add_disk(&vmdir, &base, &d, wait, &mut |att| {
                                let mut next = base.clone();
                                next.boot.extra_disks.push(att.clone());
                                self.repo.set(name, &next).map_err(Error::from)?;
                                recorded = Some(next);
                                Ok(())
                            })?;
                        vm = recorded.unwrap_or_else(|| {
                            let mut next = base.clone();
                            next.boot.extra_disks.push(d);
                            next
                        });
                        o
                    }
                    DeviceChange::AddNic(n) => {
                        check_new_nic(&vm, &n)?;
                        let base = vm.clone();
                        let mut recorded: Option<Vm> = None;
                        let (n, o) =
                            backend.hotplug_add_nic(&vmdir, &base, &n, wait, &mut |att| {
                                let mut next = base.clone();
                                next.boot.extra_nics.push(att.clone());
                                self.repo.set(name, &next).map_err(Error::from)?;
                                recorded = Some(next);
                                Ok(())
                            })?;
                        vm = recorded.unwrap_or_else(|| {
                            let mut next = base.clone();
                            next.boot.extra_nics.push(n);
                            next
                        });
                        o
                    }
                    DeviceChange::RemoveDisk(_) | DeviceChange::RemoveNic(_) => {
                        return Err(Error::ResizeNeedsStopped(format!(
                            "VM '{name}' is running: a disk or NIC is not removed live \
                             (ADR-0068 D10) — `delonix vm stop {name}`, remove it, and start it"
                        )))
                    }
                };
                self.repo.set(name, &vm).map_err(Error::from)?;
                Ok((vm, Some(outcome)))
            }
            _ => {
                let mut next = vm.clone();
                apply_device_change(&mut next, change)?;
                backend.change_devices_cold(&vmdir, &vm, &mut next)?;
                vm = next;
                self.repo.set(name, &vm).map_err(Error::from)?;
                Ok((vm, None))
            }
        }
    }

    /// Moves VM `name` to `target`, another node of its cluster (`vm move --node`,
    /// ADR-0053 decision 1; see [`VmBackend::move_to_node`]).
    ///
    /// The target is always the caller's: there is no default and no selection.
    /// Refused before the backend is asked: an empty target, and a power state
    /// that does not match `live` as the record says it — `--live` on a stopped
    /// VM, no `--live` on a running one, and a paused VM either way (unpause it or
    /// stop it first). The backend asks its node the same question again, because
    /// a record can be out of date. A target storage without `--with-local-disks`
    /// is refused too: it names where COPIED disks land, and without the flag
    /// nothing is copied. The record takes the handle the backend returns only
    /// after the move is proved, so a refused or failed move leaves it naming the
    /// node the VM is still on.
    ///
    /// Returns the updated record.
    pub fn move_to_node(&self, name: &str, target: &str, opts: &MoveOptions) -> Result<Vm> {
        let target = target.trim();
        if target.is_empty() {
            return Err(Error::InvalidMoveTarget(format!(
                "no node to move VM '{name}' to: give `--node <node>`"
            )));
        }
        match opts.target_storage.as_deref().map(str::trim) {
            Some("") => {
                return Err(Error::InvalidMoveTarget(format!(
                    "an empty target storage for VM '{name}': name a storage of node '{target}'"
                )))
            }
            Some(_) if !opts.with_local_disks => {
                return Err(Error::InvalidMoveTarget(format!(
                    "`--target-storage` names where copied disks land, and without \
                     `--with-local-disks` VM '{name}' has none copied"
                )))
            }
            _ => {}
        }
        let live = opts.live;
        let vmdir = self.vmdir();
        let st = &self.repo;
        let mut vm = self.load_vm(name)?;
        if let Some(why) = move_power_refusal(&vm.status, live) {
            return Err(Error::MoveRefused(format!("VM '{name}' {why}")));
        }
        let handle = self
            .backends
            .for_vm(&vm)?
            .move_to_node(&vmdir, &vm, target, opts)?;
        vm.api_socket = handle;
        st.set(name, &vm).map_err(Error::from)?;
        Ok(vm)
    }
    /// What the guest of VM `name` reports about itself through its agent
    /// (`vm.guest-agent`, see [`VmBackend::guest_info`]). `Ok(None)` when the
    /// record says the VM is not running — there is no guest to ask — or the
    /// backend has no agent answer to give.
    pub fn guest_info(&self, name: &str) -> Result<Option<GuestInfo>> {
        let vm = self.load_vm(name)?;
        if vm.status != Status::Running {
            return Ok(None);
        }
        Ok(self.backends.for_vm(&vm)?.guest_info(&vm)?)
    }
    /// Takes a named snapshot of VM `name` (see [`VmBackend::snapshot`]). On libvirt a
    /// running VM's snapshot is a system checkpoint (memory + disk).
    pub fn snapshot(&self, name: &str, snap: &str) -> Result<()> {
        if !valid_vm_name(snap) {
            return Err(Error::InvalidSnapshotName(format!(
                "invalid snapshot name: {snap}"
            )));
        }
        let vmdir = self.vmdir();
        let vm = self.load_vm(name)?;
        Ok(self.backends.for_vm(&vm)?.snapshot(&vmdir, &vm, snap)?)
    }
    /// Reverts VM `name` to the named snapshot (see [`VmBackend::restore`]).
    pub fn restore(&self, name: &str, snap: &str) -> Result<()> {
        if !valid_vm_name(snap) {
            return Err(Error::InvalidSnapshotName(format!(
                "invalid snapshot name: {snap}"
            )));
        }
        let vmdir = self.vmdir();
        let vm = self.load_vm(name)?;
        self.backends.for_vm(&vm)?.restore(&vmdir, &vm, snap)?;
        // A revert changes what the VM IS: a checkpoint taken running brings a
        // stopped VM back up, one taken offline puts a running VM down. `status` is
        // the reconciler this engine already has, under the store lock — calling it
        // beats a second one here that could disagree with it.
        self.status(name).map(|_| ())
    }
    /// Copies a RUNNING VM's disk to `dest` without stopping it (see
    /// [`VmBackend::backup_disk_live`]); a backend that cannot refuses by name.
    pub fn backup_disk_live(&self, name: &str, dest: &Path, quiesce: bool) -> Result<()> {
        let vm = self.load_vm(name)?;
        Ok(self
            .backends
            .for_vm(&vm)?
            .backup_disk_live(&self.vmdir(), &vm, dest, quiesce)?)
    }
    /// Lists VM `name`'s snapshot names (see [`VmBackend::snapshots`]).
    pub fn snapshots(&self, name: &str) -> Result<Vec<String>> {
        let vm = self.load_vm(name)?;
        Ok(self.backends.for_vm(&vm)?.snapshots(&self.vmdir(), &vm)?)
    }
    /// Deletes VM `name`'s snapshot `snap` (see [`VmBackend::delete_snapshot`]).
    pub fn delete_snapshot(&self, name: &str, snap: &str) -> Result<()> {
        if !valid_vm_name(snap) {
            return Err(Error::InvalidSnapshotName(format!(
                "invalid snapshot name: {snap}"
            )));
        }
        let vmdir = self.vmdir();
        let vm = self.load_vm(name)?;
        Ok(self
            .backends
            .for_vm(&vm)?
            .delete_snapshot(&vmdir, &vm, snap)?)
    }
    /// Applies one direction of VM `name`'s own firewall (see
    /// [`VmBackend::apply_firewall`]).
    pub fn apply_firewall(&self, name: &str, policy: &firewall::Policy) -> Result<()> {
        let vmdir = self.vmdir();
        let vm = self.load_vm(name)?;
        Ok(self
            .backends
            .for_vm(&vm)?
            .apply_firewall(&vmdir, &vm, policy)?)
    }
    /// Reads one direction of VM `name`'s own firewall back (see
    /// [`VmBackend::read_firewall`]).
    pub fn read_firewall(
        &self,
        name: &str,
        direction: firewall::Direction,
    ) -> Result<firewall::Policy> {
        let vmdir = self.vmdir();
        let vm = self.load_vm(name)?;
        Ok(self
            .backends
            .for_vm(&vm)?
            .read_firewall(&vmdir, &vm, direction)?)
    }
    /// Starts an existing, stopped VM — idempotent (already running = no-op,
    /// same as `create`'s auto-heal, which this delegates to). Reboots reusing
    /// the SAME per-VM overlay (disk state preserved) with the base
    /// disk/vcpus/memory/network/backend recorded at its last `create`/`start`,
    /// PLUS the boot shape ([`VmBootSpec`]: kernel/seed/volumes/static IP/VNC/TPM/
    /// CPU topology/extra disks and NICs/…). Until that block was persisted this
    /// rebooted a materially different machine and said nothing — see
    /// [`VmBootSpec`] for the measurement.
    ///
    /// The one thing still not recovered is a VM whose record predates the block:
    /// `boot` is empty there, and empty means *unknown*, not *none*. Such a VM
    /// keeps its old behaviour until the next `vm create` (idempotent) stamps the
    /// real shape.
    pub fn start(&self, name: &str) -> Result<Vm> {
        let st = &self.repo;
        let vm = st.get(name).map_err(|e| {
            if e.is_not_found() {
                Error::VmNotFound(name.to_string())
            } else {
                Error::from(e)
            }
        })?;
        self.create(&config_from(&vm))
    }
    /// Stops (if running) then starts — always a real reboot, unlike `start`
    /// (which no-ops when already running). Same recovered-fields caveat as
    /// `start`/[`config_from`].
    pub fn restart(&self, name: &str) -> Result<Vm> {
        let st = &self.repo;
        let vm = st.get(name).map_err(|e| {
            if e.is_not_found() {
                Error::VmNotFound(name.to_string())
            } else {
                Error::from(e)
            }
        })?;
        if self.backends.for_vm(&vm)?.is_running(&vm) {
            self.stop(name)?;
        }
        self.create(&config_from(&vm))
    }
    /// Current state of a VM, with `status`/`ip` reconciled by its backend.
    pub fn status(&self, name: &str) -> Result<Vm> {
        let st = &self.repo;
        // load() first just to resolve the NotFound->VmNotFound mapping before
        // taking the lock (update() would otherwise surface the generic NotFound).
        st.get(name).map_err(|e| {
            if e.is_not_found() {
                Error::VmNotFound(name.to_string())
            } else {
                Error::from(e)
            }
        })?;
        // Everything from the backend query to the decision runs INSIDE the
        // locked read-modify-write (`JsonStore::update`) — this used to be a bare
        // load->mutate->save with no lock, racing the background metrics refresh
        // (dash/delonix-mgmt) against a concurrent `vm start/stop/create` on the
        // same VM: a narrow but real lost-update window on the IP/status field.
        // The backend is resolved BEFORE the lock: the closure returns `bool`
        // (changed / unchanged) and has nowhere to put an error, and a record this
        // build cannot resolve is not something to discover halfway through a
        // read-modify-write. `load()` above already read the record, so this costs
        // nothing extra.
        let named = st.get(name).map_err(Error::from)?;
        let backend = self.backends.for_vm(&named)?;
        st.update(name, |vm| {
            let old_ip = vm.ip.clone();
            let old_status = vm.status.clone();
            // Records written before `pid_starttime` existed carry `None`, which
            // `safe_to_signal` treats as the old behaviour — so the guard is inert
            // for every VM already on disk until it is booted again. Adopt it here,
            // where we are already reading the process, and only on proof.
            let adopted = adopt_pid_starttime(vm);
            if backend.is_running(vm) {
                // A PAUSED VMM is still "alive" to `is_running` (the process is
                // there, only its vCPUs are frozen) — a routine `vm ls`/`status()`
                // must not silently thaw the record back to `Running` just
                // because the process answers. Only `unpause`/`stop` move it out
                // of `Paused`; measured live: without this guard, `vm ls` right
                // after `vm pause` reported `Running` again, and a second `vm
                // pause` failed against a VMM that was never actually paused.
                if vm.status != Status::Paused {
                    vm.status = Status::Running;
                }
                vm.ip = backend.ip(vm).or_else(|| vm.ip.clone());
            } else {
                // A powered-off VM = Stopped (the guest may have done a clean shutdown;
                // unlike containers, the VM is autonomous — a crash is not assumed).
                vm.status = Status::Stopped;
                vm.pid = None;
                // The guest powered itself off outside our own `stop()` (e.g. `shutdown
                // now` from inside) — reconcile `started_unix` the same way `stop()`
                // does, so UPTIME doesn't keep counting a boot that already ended.
                vm.started_unix = None;
            }
            // Persist a freshly-learnt IP (a nat VM only gets its DHCP lease well after
            // `create` saved the record): the record is what the holder's internal DNS
            // reads to resolve `<vm-name>` for containers — a stale null IP there means
            // the name never resolves. Only writes when something actually changed.
            //
            // The status comparison is on the WHOLE status, not on "was it
            // Running": `Paused -> Stopped` (a paused VMM that really died) is a
            // change too, and the old `was_running != is_running` saw both sides as
            // "not running" and left `Paused` on disk while `vm ls` said `Stopped`
            // — so `vm unpause` went on to aim at a VM that no longer existed.
            // A remote backend that found the VM on another node of its cluster
            // (ADR-0053 decision 3) says so here; the record takes the new handle,
            // or every later command would ask the old node again.
            let relocated = match backend.current_handle(vm) {
                Some(h) if h != vm.api_socket => {
                    vm.api_socket = h;
                    true
                }
                _ => false,
            };
            adopted || relocated || vm.ip != old_ip || vm.status != old_status
        })
        .map_err(Error::from)
    }
    /// Lists all VMs, with reconciled state.
    pub fn list(&self) -> Result<Vec<Vm>> {
        let st = &self.repo;
        let mut out = Vec::new();
        for vm in st.list().map_err(Error::from)? {
            out.push(self.status(&vm.name).unwrap_or(vm));
        }
        Ok(out)
    }
}

/// Annotation on a VM created closed (`VmConfig::policy_hold`) and not yet
/// released: its own firewall is default-deny until the policy layers ran.
pub const POLICY_HOLD_ANNOTATION: &str = "delonix.io/policy-hold";

/// VM ADMISSION control: refuses to boot a VM if the requested memory does not
/// fit in the host's `MemAvailable` minus a safety reserve. Unlike
/// containers (with a budget in `delonix.slice`), a VM is a process
/// (cloud-hypervisor/qemu) that consumes host RAM DIRECTLY; without this
/// check, scheduling 30×2GB on a 32GB host would drown/OOM-kill the host. Since
/// `MemAvailable` already discounts the running VMs, the Nth VM that does not fit is
/// refused naturally. Reserve tunable via `DELONIX_VM_RESERVE_MIB`
/// (default 2048). Best-effort: if `/proc/meminfo` is unreadable, it does not block.
fn admission_check(cfg: &VmConfig) -> Result<()> {
    admission_verdict(
        cfg,
        delonix_node::mem_available_mib(),
        std::env::var("DELONIX_VM_RESERVE_MIB").ok().as_deref(),
    )
}

/// What a [`destroy`] took away, and what it deliberately left.
#[derive(Debug, Default, Clone)]
pub struct Destroyed {
    /// Bytes of local files and directories removed (a remote disk is freed by
    /// the node, which does not report a size).
    pub freed_bytes: u64,
    /// Every local artifact removed: overlay, seed, snapshots, sockets, extra disks.
    pub removed: Vec<String>,
    /// The backend id when its storage lives on the provider (a remote node),
    /// so the disks it released are NOT in `removed`/`freed_bytes` — those count
    /// local files only, and reporting «0 B freed» for a VM whose disks and
    /// snapshots went away on the node reads as «nothing was freed».
    pub provider_released: Option<String>,
    /// Things the record points to that are NOT the VM's to delete (a 9p share
    /// is somebody's data; an extra disk outside the state directory may be an
    /// image the operator supplied). Named, never silently left.
    pub kept: Vec<String>,
}

/// What a cold resize asks for (`vm resize`, and the reconciler's reboot
/// class). `None` leaves a field as it is; for a ceiling, `Some(None)` clears
/// it back to «the maximum is the boot size» and `Some(Some(n))` sets it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResizeRequest {
    pub vcpus: Option<u32>,
    pub memory: Option<String>,
    pub disk_size_gib: Option<u32>,
    pub vcpus_max: Option<Option<u32>>,
    pub memory_max_mib: Option<Option<u64>>,
}

/// Every rule a cold reshape applies, on the record as it is: nothing to do,
/// zero vCPUs, a zero disk size, a memory that is not a size, and the
/// ceilings below the boot size. Returns the target shape `(vcpus, MiB,
/// vcpus_max, memory_max_mib)`. Pure — [`VmEngine::resize_shape`] and
/// [`VmEngine::check_resize_shape`] share it, so the check before a stop and
/// the reshape after it can never disagree.
/// A request that changes nothing. Pure, and checked before the VM is even
/// looked up.
fn refuse_empty(name: &str, req: &ResizeRequest) -> Result<()> {
    if req.vcpus.is_none()
        && req.memory.is_none()
        && req.disk_size_gib.is_none()
        && req.vcpus_max.is_none()
        && req.memory_max_mib.is_none()
    {
        return Err(Error::InvalidResize(format!(
            "nothing to resize on VM '{name}': give --vcpus, --memory, --disk-size, \
             --vcpus-max and/or --memory-max"
        )));
    }
    Ok(())
}

fn check_shape(
    name: &str,
    vm: &Vm,
    req: &ResizeRequest,
) -> Result<(u32, u64, Option<u32>, Option<u64>)> {
    refuse_empty(name, req)?;
    if req.vcpus == Some(0) || req.vcpus_max == Some(Some(0)) {
        return Err(Error::InvalidResize(format!(
            "VM '{name}' cannot have 0 vCPUs"
        )));
    }
    if req.disk_size_gib == Some(0) {
        return Err(Error::InvalidResize(format!(
            "VM '{name}': --disk-size 0 is not a size — drop the flag to leave the disk alone"
        )));
    }
    let new_mib = match &req.memory {
        Some(m) => Some(parse_mem_mib(m).ok_or_else(|| {
            Error::InvalidResize(format!(
                "memory '{m}' is not a size: use a number with an optional M/G suffix (512M, 4G, 4Gi)"
            ))
        })?),
        None => None,
    };
    let target_vcpus = req.vcpus.unwrap_or(vm.vcpus.max(1));
    let target_mib = new_mib.unwrap_or_else(|| mem_mib(&vm.memory));
    let vcpus_max = req.vcpus_max.unwrap_or(vm.vcpus_max);
    let memory_max_mib = req.memory_max_mib.unwrap_or(vm.memory_max_mib);
    check_ceilings(name, target_vcpus, target_mib, vcpus_max, memory_max_mib)?;
    Ok((target_vcpus, target_mib, vcpus_max, memory_max_mib))
}

/// A ceiling below what the VM boots with is not a ceiling (ADR-0068 D3):
/// refused, naming both numbers, never clamped.
fn check_ceilings(
    name: &str,
    vcpus: u32,
    memory_mib: u64,
    vcpus_max: Option<u32>,
    memory_max_mib: Option<u64>,
) -> Result<()> {
    if let Some(max) = vcpus_max {
        if max < vcpus {
            return Err(Error::InvalidResize(format!(
                "VM '{name}': the vCPU ceiling ({max}) cannot be below the vCPUs it boots with \
                 ({vcpus}) — raise --vcpus-max, or lower --vcpus"
            )));
        }
    }
    if let Some(max) = memory_max_mib {
        if max < memory_mib {
            return Err(Error::InvalidResize(format!(
                "VM '{name}': the memory ceiling ({max} MiB) cannot be below the memory it boots \
                 with ({memory_mib} MiB) — raise --memory-max, or lower --memory"
            )));
        }
    }
    Ok(())
}

/// A live add above the ceiling declared at create is refused, naming the
/// ceiling and the command that changes it (ADR-0068 D3) — never clamped.
fn above_ceiling(name: &str, what: &str, target: &str, ceiling: &str, flag: &str) -> Error {
    Error::InvalidResize(format!(
        "VM '{name}' can take at most {ceiling} {what} live (asked for {target}): the hotplug \
         ceiling is fixed at boot — `delonix vm stop {name}`, then `delonix vm resize {name} \
         {flag} <n>`, and start it"
    ))
}

/// Ensures the microVM (idempotent): if it already exists and is alive, does nothing; if
/// it exists but died, re-boots reusing the overlay (auto-heal) with the SAME
/// backend; otherwise, chooses the backend (explicit/auto), creates the overlay and boots.
/// Validates a VM's NAME before using it in file PATHS, in the
/// cloud-init `hostname` and in the `virsh` argv. Audit finding: the name
/// (coming from the CLI OR from `metadata.name` of an UNTRUSTED manifest via
/// `stack apply -f`) flowed raw into `state_root/vms/<name>` (seed) and into the
/// overlay `<name>.qcow2` — a `metadata.name: "../../.ssh/authorized_keys"`
/// wrote/overwrote files OUTSIDE the state directory, as the
/// user. It also prevents a name starting with `-` (which `virsh` would read
/// as an option) and control characters (injection in the cloud-init YAML).
/// Strict whitelist: `[A-Za-z0-9._-]`, non-empty, does not start with `-`/`.`,
/// no `..`. Same spirit as the `valid_*` of the `cluster` audit.
pub fn valid_vm_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && !name.starts_with('.')
        && name != ".."
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}
fn path_size(p: &Path) -> u64 {
    let Ok(md) = std::fs::symlink_metadata(p) else {
        return 0;
    };
    if md.is_dir() {
        std::fs::read_dir(p)
            .map(|rd| rd.flatten().map(|e| path_size(&e.path())).sum())
            .unwrap_or(0)
    } else {
        md.len()
    }
}
/// Whether `h` is a DNS label a guest accepts as its hostname: letters,
/// digits and '-', not at either end, 1 to 63 characters. Pure.
fn valid_hostname(h: &str) -> bool {
    (1..=63).contains(&h.len())
        && h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !h.starts_with('-')
        && !h.ends_with('-')
}
/// Whether `u` is a login name cloud-init can create: a lowercase letter or
/// '_' first, then lowercase letters, digits, '_' or '-', at most 32. Pure.
fn valid_login(u: &str) -> bool {
    let mut b = u.bytes();
    matches!(b.next(), Some(c) if c.is_ascii_lowercase() || c == b'_')
        && u.len() <= 32
        && b.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}
/// Why a move of a VM in `status` is refused for `live`, or `None`. Pure.
fn move_power_refusal(status: &Status, live: bool) -> Option<String> {
    match (status, live) {
        (Status::Paused, _) => Some(
            "is paused: a move needs it running (`--live`) or stopped — unpause it or stop it first"
                .into(),
        ),
        (Status::Running, false) => Some(
            "is running: move it with `--live`, or stop it first for an offline move".into(),
        ),
        (Status::Running, true) => None,
        (_, true) => Some(
            "is not running: `--live` moves a running VM — drop `--live` for an offline move".into(),
        ),
        (_, false) => None,
    }
}
/// Reconstructs the subset of [`VmConfig`] reliably recoverable from a
/// persisted [`Vm`] record, for [`start`]/[`restart`]. `Vm` does NOT persist
/// everything `VmConfig` needs to boot — only what survives past the initial
/// `create`: base disk, vcpus, memory, network, restart policy, passthrough
/// devices, and (libvirt only) the net mode, smuggled into `Vm.tap` at boot
/// (see the `LibvirtBackend::boot` `tap: cfg.net_mode…` assignment). Fields
/// that only ever existed as `vm create` flags — custom kernel/initrd,
/// cloud-init seed, 9p volumes, static IP, VNC, and the advanced libvirt
/// knobs (machine/CPU model/topology/TPM/video/boot order/extra disks or
/// NICs/raw XML) — are lost once `create` returns, so they are NOT restored.
/// The half of a [`VmConfig`] that the flat [`Vm`] fields do NOT already carry,
/// ready to persist.
///
/// **Destructures `VmConfig` exhaustively on purpose.** Adding a field to
/// `VmConfig` breaks the build right here, which forces whoever adds it to
/// decide whether it has to survive a `vm start` — instead of it being
/// forgotten in silence, which is precisely how the record came to persist ten
/// fields out of thirty. Same discipline as the exhaustive `match` in
/// `cmd::exitcode::for_error`: the compiler asks the question so a human does
/// not have to remember to.
pub fn boot_spec_of(cfg: &VmConfig) -> VmBootSpec {
    let VmConfig {
        // Already carried by a flat `Vm` field, or derived at boot time — the
        // record round-trips these without help (see `config_from`).
        name: _,
        disk: _,
        vcpus: _,
        memory: _,
        vcpus_max: _,
        memory_max_mib: _,
        network: _,
        namespace: _,
        restart_policy: _,
        devices: _,
        backend: _,
        net_mode: _,
        // NOT restored, and the reason is narrow: this subnet only ever feeds
        // `ensure_libvirt_network`'s DEFINE branch. By the time a `vm start`
        // runs, the network it describes already exists, so that branch is not
        // taken and the value would be dead. `network` IS persisted, and
        // `boot` re-derives `bridge` from it, so the VM goes back onto the same
        // network it was created on.
        //
        // The one case this gives up on: if the libvirt network is removed out
        // of band between `create` and `start`, the restart cannot redefine it
        // (no subnet) and `ensure_libvirt_network` warns and leaves it. The
        // domain then names a network the daemon does not have and `virsh
        // start` fails — loudly, which is the point. Persisting the subnet
        // would need a new flat `Vm` field; worth doing if that case shows up
        // in practice.
        network_subnet: _,
        // Consumido em `prepare_local_overlay` no momento da criação: depois de
        // o overlay existir, o tamanho é uma propriedade DELE e não há nada a
        // reaplicar no arranque (e `prepare_local_overlay` salta um overlay que
        // já existe). Por isso não entra no `VmBootSpec`.
        disk_size_gib: _,
        // A request-time gate (ADR-0050 D6), consumed in `create_with` BEFORE
        // the backend is chosen: once the record names a backend that passed
        // it, there is nothing to reapply on `vm start`, and the record does
        // not keep it.
        required_capabilities: _,
        // Consumed at the FIRST create (the firewall it closes lives on the node and
        // outlives a restart; the record keeps the annotation until released).
        policy_hold: _,
        // Everything below used to exist only for the duration of `vm create`.
        kernel,
        initrd,
        firmware,
        cmdline,
        seed,
        hostname,
        ci_user,
        ssh_keys,
        cloud_init,
        hugepages,
        cpu_affinity,
        bridge,
        volumes,
        vnc,
        serial_capture,
        static_ip,
        allow_mac_spoofing,
        machine,
        cpu_model,
        cpu_topology,
        tpm,
        video,
        boot_order,
        extra_disks,
        extra_nics,
        libvirt_xml_overlay,
        libvirt_xml,
    } = cfg;
    VmBootSpec {
        kernel: kernel.clone(),
        initrd: initrd.clone(),
        firmware: firmware.clone(),
        cmdline: cmdline.clone(),
        seed: seed.clone(),
        hostname: hostname.clone(),
        ci_user: ci_user.clone(),
        ssh_keys: ssh_keys.clone(),
        cloud_init: *cloud_init,
        hugepages: *hugepages,
        cpu_affinity: cpu_affinity.clone(),
        bridge: bridge.clone(),
        volumes: volumes.clone(),
        vnc: *vnc,
        serial_capture: *serial_capture,
        static_ip: static_ip.clone(),
        allow_mac_spoofing: *allow_mac_spoofing,
        machine: machine.clone(),
        cpu_model: cpu_model.clone(),
        cpu_topology: cpu_topology.clone(),
        tpm: *tpm,
        video: video.clone(),
        boot_order: boot_order.clone(),
        extra_disks: extra_disks.clone(),
        extra_nics: extra_nics.clone(),
        libvirt_xml_overlay: libvirt_xml_overlay.clone(),
        libvirt_xml: libvirt_xml.clone(),
    }
}
/// Rebuilds the `VmConfig` of an existing VM from its record — what
/// `start`/`restart` reboot with.
///
/// Written WITHOUT `..Default::default()` for the same reason `boot_spec_of`
/// destructures: the fallback was how twenty-one fields quietly became
/// defaults on every restart. Spelling every field out means a new one cannot
/// be silently dropped here either.
/// One device change of `vm update` (ADR-0068 D7/D10). A disk is removed
/// by its pinned target (`vdc`) or its source path; a NIC by its MAC.
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceChange {
    AddDisk(crate::ExtraDisk),
    AddNic(crate::ExtraNic),
    RemoveDisk(String),
    RemoveNic(String),
}

/// A disk the VM already has (same source, or same pinned target) is not
/// added twice. Pure.
fn check_new_disk(vm: &Vm, d: &crate::ExtraDisk) -> Result<()> {
    if d.source.trim().is_empty() {
        return Err(Error::InvalidResize(format!(
            "VM '{}': a disk to add needs a source",
            vm.name
        )));
    }
    let dup = vm.boot.extra_disks.iter().any(|x| {
        x.source == d.source || (d.target.is_some() && x.target.is_some() && x.target == d.target)
    });
    if dup {
        return Err(Error::InvalidResize(format!(
            "VM '{}' already has the disk '{}'",
            vm.name,
            d.target.as_deref().unwrap_or(&d.source)
        )));
    }
    Ok(())
}

/// A NIC whose MAC the VM already carries is not added twice. Pure.
fn check_new_nic(vm: &Vm, n: &crate::ExtraNic) -> Result<()> {
    let same = |m: &str| n.mac.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(m));
    if same(&vm.mac)
        || vm
            .boot
            .extra_nics
            .iter()
            .any(|x| x.mac.as_deref().is_some_and(same))
    {
        return Err(Error::InvalidResize(format!(
            "VM '{}' already has a NIC with MAC {}",
            vm.name,
            n.mac.as_deref().unwrap_or("")
        )));
    }
    Ok(())
}

/// The record as a device change on a STOPPED VM leaves it. Pure.
pub fn apply_device_change(vm: &mut Vm, change: DeviceChange) -> Result<()> {
    match change {
        DeviceChange::AddDisk(d) => {
            check_new_disk(vm, &d)?;
            vm.boot.extra_disks.push(d);
        }
        DeviceChange::AddNic(n) => {
            check_new_nic(vm, &n)?;
            vm.boot.extra_nics.push(n);
        }
        DeviceChange::RemoveDisk(key) => {
            let before = vm.boot.extra_disks.len();
            vm.boot
                .extra_disks
                .retain(|d| d.source != key && d.target.as_deref() != Some(key.as_str()));
            if vm.boot.extra_disks.len() == before {
                return Err(Error::InvalidResize(format!(
                    "VM '{}' has no extra disk '{key}' (name its target, e.g. vdc, or its source)",
                    vm.name
                )));
            }
        }
        DeviceChange::RemoveNic(mac) => {
            let before = vm.boot.extra_nics.len();
            vm.boot.extra_nics.retain(|n| {
                !n.mac
                    .as_deref()
                    .is_some_and(|m| m.eq_ignore_ascii_case(&mac))
            });
            if vm.boot.extra_nics.len() == before {
                return Err(Error::InvalidResize(format!(
                    "VM '{}' has no extra NIC with MAC {mac} (the primary NIC is not removable)",
                    vm.name
                )));
            }
        }
    }
    Ok(())
}

pub fn config_from(vm: &Vm) -> VmConfig {
    let b = &vm.boot;
    VmConfig {
        name: vm.name.clone(),
        disk: vm.disk.clone(),
        vcpus: vm.vcpus,
        memory: vm.memory.clone(),
        vcpus_max: vm.vcpus_max,
        memory_max_mib: vm.memory_max_mib,
        network: vm.network.clone(),
        namespace: Some(vm.namespace.clone()),
        restart_policy: vm.restart_policy.clone(),
        devices: vm.devices.clone(),
        backend: Some(vm.backend.clone()),
        // Not persisted: the requirement was checked against the backend the
        // record names when the VM was created (see `boot_spec_of`).
        required_capabilities: Vec::new(),
        policy_hold: false,
        // For libvirt, `Vm.tap` is not a real tap: `LibvirtBackend::boot` stores
        // the net mode string there. For Cloud Hypervisor it IS a device name
        // and must not be misread as one.
        net_mode: (vm.backend == "libvirt").then(|| vm.tap.clone()),
        // `None` on purpose — see the `network_subnet` note in `boot_spec_of`:
        // only the DEFINE branch reads it, and a restart's network already
        // exists. `network` above is what puts the VM back on it.
        network_subnet: None,
        // `None` de propósito: o overlay já tem o tamanho com que nasceu, e um
        // restart não o redimensiona. Reafirmar aqui um número seria convidar um
        // resize acidental em cada arranque.
        disk_size_gib: None,
        kernel: b.kernel.clone(),
        initrd: b.initrd.clone(),
        firmware: b.firmware.clone(),
        cmdline: b.cmdline.clone(),
        seed: b.seed.clone(),
        hostname: b.hostname.clone(),
        ci_user: b.ci_user.clone(),
        ssh_keys: b.ssh_keys.clone(),
        cloud_init: b.cloud_init,
        hugepages: b.hugepages,
        cpu_affinity: b.cpu_affinity.clone(),
        bridge: b.bridge.clone(),
        volumes: b.volumes.clone(),
        vnc: b.vnc,
        serial_capture: b.serial_capture,
        static_ip: b.static_ip.clone(),
        allow_mac_spoofing: b.allow_mac_spoofing,
        machine: b.machine.clone(),
        cpu_model: b.cpu_model.clone(),
        cpu_topology: b.cpu_topology.clone(),
        tpm: b.tpm,
        video: b.video.clone(),
        boot_order: b.boot_order.clone(),
        extra_disks: b.extra_disks.clone(),
        extra_nics: b.extra_nics.clone(),
        libvirt_xml_overlay: b.libvirt_xml_overlay.clone(),
        libvirt_xml: b.libvirt_xml.clone(),
    }
}
/// Is `argv` the `cloud-hypervisor` serving THIS VM? PURE.
///
/// The api-socket path is the ownership token: we choose it, it is unique per
/// VM, and the VMM carries it in its own argv. Same idiom the slirp reaper uses
/// — the tool's name alone identifies a TOOL, never an instance.
pub fn argv_is_vmm_for(argv: &[String], api_socket: &str) -> bool {
    if api_socket.is_empty() {
        return false;
    }
    argv.first()
        .is_some_and(|a| a.ends_with("cloud-hypervisor"))
        && argv.iter().any(|a| a == api_socket)
}
/// Adopts `pid_starttime` for a record written before the field existed.
///
/// **Only when the pid is PROVABLY this VM's VMM**, by a means independent of
/// the starttime itself: the process's argv has to name this VM's api-socket.
/// Stamping on liveness alone would be worse than the gap it closes — it would
/// carve a recycled pid's starttime into the record and make every later check
/// agree with it, turning a missing guard into a confidently wrong one.
///
/// Returns `true` when it stamped (the caller persists).
pub fn adopt_pid_starttime(vm: &mut Vm) -> bool {
    if vm.pid_starttime.is_some() {
        return false;
    }
    let Some(pid) = vm.pid.filter(|&p| p > 0) else {
        return false;
    };
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let argv: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|c| !c.is_empty())
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect();
    if !argv_is_vmm_for(&argv, &vm.api_socket) {
        return false;
    }
    vm.pid_starttime = proc_starttime(pid);
    vm.pid_starttime.is_some()
}
/// The admission rule, with the host's available memory and the reserve passed in —
/// testable without reading `/proc/meminfo` (which made the old test depend on the
/// machine) or writing the process environment (which raced parallel tests).
pub fn admission_verdict(
    cfg: &VmConfig,
    available_mib: Option<u64>,
    reserve_raw: Option<&str>,
) -> Result<()> {
    let avail = match available_mib {
        Some(a) => a,
        None => return Ok(()),
    };
    let reserve = reserve_raw.and_then(|v| v.parse().ok()).unwrap_or(2048u64);
    let want = mem_mib(&cfg.memory);
    if want.saturating_add(reserve) > avail {
        return Err(Error::AdmissionRefused(format!(
            "host protection: VM '{}' asks for {want} MiB but the host only has {avail} MiB \
             available (reserve {reserve} MiB). Stop VMs/containers, reduce the memory, \
             or lower DELONIX_VM_RESERVE_MIB (at your own risk).",
            cfg.name
        )));
    }
    Ok(())
}
/// The VM's requested isolation namespace, normalized (`None`/empty = `default`).
pub fn vm_namespace_of(cfg: &VmConfig) -> String {
    match cfg.namespace.as_deref() {
        None | Some("") => "default".to_string(),
        Some(ns) => ns.to_string(),
    }
}
/// The names in `VmConfig::required_capabilities`, resolved against the
/// catalog. **Before any backend is touched**: an unknown name is an invalid
/// argument, and reading it as "no provider supports it" would send the
/// caller looking for a provider instead of for the typo.
pub fn resolve_required_capabilities(names: &[String]) -> Result<Vec<Capability>> {
    let mut out = Vec::with_capacity(names.len());
    for n in names {
        let n = n.trim();
        match Capability::from_name(n) {
            Some(c) => {
                if !out.contains(&c) {
                    out.push(c);
                }
            }
            None => {
                return Err(Error::UnknownCapability(format!(
                    "unknown capability '{n}': the catalog (version {}) has no entry by that \
                     name — `delonix provider ls` lists the names",
                    crate::capability::CATALOG_VERSION
                )))
            }
        }
    }
    Ok(out)
}

/// Path of the UNIX socket of the serial console of a Cloud Hypervisor VM
/// (`<base>/vms/<name>.console`). `delonix vm console` connects here.
pub fn console_socket(base: &Path, name: &str) -> PathBuf {
    base.join("vms").join(format!("{name}.console"))
}

/// Where a capture-mode VM's serial console is written (`<base>/vms/<name>.serial`).
///
/// THE formula, with one owner. It used to be spelled out separately in `boot_ch`
/// and in a reader in another program, which is exactly how the two came to
/// disagree: the interactive console moved the writer to a socket and the reader
/// went on opening a file nobody wrote. Same discipline as `fw_rule_tail` — the
/// writer and the reader share the format or they drift.
pub fn serial_log_path(base: &Path, name: &str) -> PathBuf {
    base.join("vms").join(format!("{name}.serial"))
}

#[cfg(test)]
mod tests {
    //! The use cases against fake ports: before P4b.3b these paths could only
    //! be tested against a real state root and a real hypervisor.
    use super::*;
    use crate::vm_backend::Boot;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    #[derive(Default)]
    struct MemRepo(RefCell<BTreeMap<String, Vm>>);

    impl StateRepository<Vm> for MemRepo {
        fn get(&self, id: &str) -> delonix_model::Result<Vm> {
            self.0
                .borrow()
                .get(id)
                .cloned()
                .ok_or_else(|| delonix_model::Error::NotFound(format!("vm {id}")))
        }
        fn list(&self) -> delonix_model::Result<Vec<Vm>> {
            Ok(self.0.borrow().values().cloned().collect())
        }
        fn set(&self, id: &str, value: &Vm) -> delonix_model::Result<()> {
            self.0.borrow_mut().insert(id.to_string(), value.clone());
            Ok(())
        }
        fn update<F>(&self, id: &str, f: F) -> delonix_model::Result<Vm>
        where
            F: FnOnce(&mut Vm) -> bool,
        {
            let mut vm = self.get(id)?;
            if f(&mut vm) {
                self.set(id, &vm)?;
            }
            Ok(vm)
        }
        fn remove(&self, id: &str) -> delonix_model::Result<()> {
            self.0.borrow_mut().remove(id);
            Ok(())
        }
    }

    /// What the fakes saw, shared between the port and the boxed backends it
    /// hands out.
    #[derive(Default)]
    struct Seen {
        calls: RefCell<Vec<String>>,
        running: RefCell<bool>,
        own_storage: bool,
        unrecorded: bool,
        /// What this backend answers to [`VmBackend::holds_at_boot`].
        holds_at_boot: bool,
        /// `Some` makes `hotplug_add_cpu`/`hotplug_add_memory` succeed,
        /// echoing the target back with this outcome (ADR-0068); `None`
        /// keeps them refusing, the same shape as the trait's own
        /// fail-closed default, to exercise the refusal path too.
        hotplug_outcome: RefCell<Option<HotplugOutcome>>,
        /// The record `resize_cold` was handed — the ceilings it must apply
        /// travel in it (ADR-0068 open question 2).
        resized: RefCell<Option<Vm>>,
        /// Makes `hotplug_add_disk` fail after it reported the attachment.
        fail_after_attach: RefCell<bool>,
        /// Makes the overlay and the seed land on disk where the real ports
        /// put them (`vms/<name>.qcow2`, `vms/<name>/`), so a test can see
        /// what a failed create leaves behind.
        writes_files: bool,
        /// Makes the seed fail after it wrote its directory, the way
        /// `cloud-localds` failing leaves `user-data` and `meta-data`.
        fail_seed: bool,
        /// Makes `boot` fail, the way a libvirt domain that does not start does.
        fail_boot: bool,
    }

    struct FakeBackend(Rc<Seen>);

    impl VmBackend for FakeBackend {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn available(&self) -> bool {
            true
        }
        fn manages_own_storage(&self) -> bool {
            self.0.own_storage
        }
        fn holds_at_boot(&self) -> bool {
            self.0.holds_at_boot
        }
        fn boot(
            &self,
            _vmdir: &Path,
            cfg: &VmConfig,
            overlay: &str,
            _on: &dyn Fn(CreateStage),
        ) -> delonix_model::Result<Boot> {
            self.0
                .calls
                .borrow_mut()
                .push(format!("boot {} {overlay} seed={:?}", cfg.name, cfg.seed));
            if self.0.fail_boot {
                return Err(delonix_model::Error::Invalid(
                    "failed to start the domain".into(),
                ));
            }
            *self.0.running.borrow_mut() = true;
            Ok(Boot {
                pid: None,
                tap: "tap0".into(),
                mac: "52:54:00:00:00:01".into(),
                api_socket: "handle".into(),
                ip: None,
                lease_floor: None,
            })
        }
        fn is_running(&self, _vm: &Vm) -> bool {
            *self.0.running.borrow()
        }
        fn ip(&self, _vm: &Vm) -> Option<String> {
            None
        }
        fn stop(&self, _vmdir: &Path, vm: &Vm) -> delonix_model::Result<()> {
            self.0.calls.borrow_mut().push(format!("stop {}", vm.name));
            *self.0.running.borrow_mut() = false;
            Ok(())
        }
        fn resize_cold(
            &self,
            _vmdir: &Path,
            vm: &Vm,
            vcpus: u32,
            memory_mib: u64,
        ) -> delonix_model::Result<()> {
            self.0
                .calls
                .borrow_mut()
                .push(format!("resize_cold {} {vcpus} {memory_mib}", vm.name));
            *self.0.resized.borrow_mut() = Some(vm.clone());
            Ok(())
        }
        fn hotplug_add_disk(
            &self,
            _vmdir: &Path,
            vm: &Vm,
            disk: &crate::ExtraDisk,
            _wait: Duration,
            attached: &mut dyn FnMut(&crate::ExtraDisk) -> delonix_model::Result<()>,
        ) -> delonix_model::Result<(crate::ExtraDisk, HotplugOutcome)> {
            self.0
                .calls
                .borrow_mut()
                .push(format!("hotplug_add_disk {} {}", vm.name, disk.source));
            let mut d = disk.clone();
            d.target = Some("vdz".into());
            attached(&d)?;
            // A guest wait the command is killed in: `fail_after_attach`
            // makes the call fail AFTER the read-back confirmed the disk.
            if *self.0.fail_after_attach.borrow() {
                return Err(delonix_model::Error::Invalid(
                    "killed during the wait".into(),
                ));
            }
            Ok((d, HotplugOutcome::Complete))
        }
        fn hotplug_add_nic(
            &self,
            _vmdir: &Path,
            vm: &Vm,
            nic: &crate::ExtraNic,
            _wait: Duration,
            attached: &mut dyn FnMut(&crate::ExtraNic) -> delonix_model::Result<()>,
        ) -> delonix_model::Result<(crate::ExtraNic, HotplugOutcome)> {
            self.0
                .calls
                .borrow_mut()
                .push(format!("hotplug_add_nic {}", vm.name));
            let mut n = nic.clone();
            n.mac = Some("52:54:00:aa:bb:cc".into());
            attached(&n)?;
            Ok((
                n,
                HotplugOutcome::Partial {
                    reason: "no agent".into(),
                },
            ))
        }
        fn change_devices_cold(
            &self,
            _vmdir: &Path,
            _old: &Vm,
            vm: &mut Vm,
        ) -> delonix_model::Result<()> {
            self.0.calls.borrow_mut().push(format!(
                "change_devices_cold {} disks={} nics={}",
                vm.name,
                vm.boot.extra_disks.len(),
                vm.boot.extra_nics.len()
            ));
            Ok(())
        }
        fn hotplug_add_cpu(
            &self,
            _vmdir: &Path,
            vm: &Vm,
            target_vcpus: u32,
            _wait: Duration,
        ) -> delonix_model::Result<(u32, HotplugOutcome)> {
            self.0
                .calls
                .borrow_mut()
                .push(format!("hotplug_add_cpu {} -> {target_vcpus}", vm.name));
            match self.0.hotplug_outcome.borrow().clone() {
                Some(outcome) => Ok((target_vcpus, outcome)),
                None => Err(delonix_model::Error::Invalid(
                    "hotplug cpu add not configured on this fake".into(),
                )),
            }
        }
        fn hotplug_add_memory(
            &self,
            _vmdir: &Path,
            vm: &Vm,
            target_mib: u64,
            _wait: Duration,
        ) -> delonix_model::Result<(u64, HotplugOutcome)> {
            self.0
                .calls
                .borrow_mut()
                .push(format!("hotplug_add_memory {} -> {target_mib}", vm.name));
            match self.0.hotplug_outcome.borrow().clone() {
                Some(outcome) => Ok((target_mib, outcome)),
                None => Err(delonix_model::Error::Invalid(
                    "hotplug memory add not configured on this fake".into(),
                )),
            }
        }
    }

    struct FakeBackends(Rc<Seen>);

    impl VmBackends for FakeBackends {
        fn for_vm(&self, _vm: &Vm) -> delonix_model::Result<Box<dyn VmBackend>> {
            Ok(Box::new(FakeBackend(self.0.clone())))
        }
        fn select(
            &self,
            _root: &Path,
            _cfg: &VmConfig,
            _required: &[Capability],
        ) -> delonix_model::Result<Box<dyn VmBackend>> {
            Ok(Box::new(FakeBackend(self.0.clone())))
        }
        fn require(&self, _id: &str, _required: &[Capability]) -> delonix_model::Result<()> {
            Ok(())
        }
        fn declares(&self, _id: &str, _cap: Capability) -> bool {
            false
        }
        fn admit(&self, _id: &str, _cfg: &VmConfig) -> delonix_model::Result<()> {
            Ok(())
        }
        fn unrecorded(&self, _name: &str) -> Option<&'static str> {
            self.0.unrecorded.then_some("fake")
        }
        fn stop_unrecorded(&self, name: &str) -> delonix_model::Result<bool> {
            self.0
                .calls
                .borrow_mut()
                .push(format!("stop_unrecorded {name}"));
            Ok(self.0.unrecorded)
        }
        fn remove_unrecorded(&self, name: &str) -> delonix_model::Result<()> {
            self.0
                .calls
                .borrow_mut()
                .push(format!("remove_unrecorded {name}"));
            Ok(())
        }
    }

    struct FakeDisks(Rc<Seen>);

    impl LocalDiskImages for FakeDisks {
        fn overlay(
            &self,
            vmdir: &Path,
            name: &str,
            base: &str,
            _size_gib: Option<u32>,
            _on: &dyn Fn(CreateStage),
        ) -> delonix_model::Result<(PathBuf, PathBuf)> {
            self.0.calls.borrow_mut().push(format!("overlay {name}"));
            let overlay = vmdir.join(format!("{name}.qcow2"));
            if self.0.writes_files {
                std::fs::write(&overlay, b"qcow2").unwrap();
            }
            Ok((PathBuf::from(base), overlay))
        }
    }

    struct FakeSeed(Rc<Seen>);

    impl SeedBuilder for FakeSeed {
        fn seed(&self, root: &Path, cfg: &VmConfig) -> delonix_model::Result<PathBuf> {
            self.0.calls.borrow_mut().push(format!("seed {}", cfg.name));
            if !self.0.writes_files {
                return Ok(root.join(format!("{}-seed.iso", cfg.name)));
            }
            let dir = root.join("vms").join(&cfg.name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("user-data"), b"#cloud-config\n").unwrap();
            if self.0.fail_seed {
                return Err(delonix_model::Error::Invalid("cloud-localds failed".into()));
            }
            std::fs::write(dir.join("seed.iso"), b"iso").unwrap();
            Ok(dir.join("seed.iso"))
        }
    }

    fn engine(
        root: &Path,
        seen: Rc<Seen>,
    ) -> VmEngine<'_, MemRepo, FakeBackends, FakeDisks, FakeSeed> {
        VmEngine {
            root,
            repo: MemRepo::default(),
            backends: FakeBackends(seen.clone()),
            disks: FakeDisks(seen.clone()),
            seed: FakeSeed(seen),
            network: None,
        }
    }

    fn cfg(name: &str) -> VmConfig {
        VmConfig {
            name: name.into(),
            disk: "/images/base.qcow2".into(),
            memory: "64M".into(),
            vcpus: 1,
            ..Default::default()
        }
    }

    fn calls(seen: &Seen) -> Vec<String> {
        seen.calls.borrow().clone()
    }

    /// A local backend: the overlay and the seed are built, the backend boots
    /// the overlay with the seed attached, and the record says Running.
    #[test]
    fn a_new_vm_on_a_local_backend_gets_an_overlay_and_a_seed() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let e = engine(root.path(), seen.clone());
        let vm = e.create(&cfg("web")).expect("create");
        assert_eq!(vm.status, Status::Running);
        assert_eq!(vm.backend, "fake");
        let c = calls(&seen);
        assert_eq!(c[0], "overlay web");
        assert_eq!(c[1], "seed web");
        assert!(c[2].starts_with("boot web "), "{c:?}");
        assert!(
            c[2].contains("web.qcow2") && c[2].contains("web-seed.iso"),
            "{c:?}"
        );
        assert_eq!(e.repo.get("web").unwrap().status, Status::Running);
    }

    /// A node network that declares exactly `.0`. The attach half is never
    /// reached by these tests (the fake backend boots without one).
    struct DeclaredNets(&'static [&'static str]);

    impl VmNetwork for DeclaredNets {
        fn ensure_network(&self, _name: &str) -> delonix_model::Result<()> {
            Ok(())
        }
        fn is_declared(&self, name: &str) -> delonix_model::Result<bool> {
            Ok(self.0.contains(&name))
        }
        fn attach_tap(
            &self,
            _vm: &str,
            _network: &str,
            _mac: &str,
            _namespace: &str,
        ) -> delonix_model::Result<String> {
            Ok("tap0".into())
        }
        fn lease_ip(&self, _network: &str, _mac: &str) -> Option<String> {
            None
        }
        fn detach_tap(&self, _vm: &str, _lease: Option<&str>) {}
        fn join_argv(&self) -> Option<Vec<String>> {
            None
        }
    }

    /// A network nobody declared is refused as «not found» (exit 4), naming
    /// the network and the command that declares it — and BEFORE the overlay:
    /// the defect was a create that built the disk and defined the domain,
    /// then died on the hypervisor's own «no network with matching name».
    #[test]
    fn an_undeclared_network_is_refused_before_anything_is_built() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let nets = DeclaredNets(&["lab-net"]);
        let e = VmEngine {
            network: Some(&nets),
            ..engine(root.path(), seen.clone())
        };
        let err = e
            .create(&VmConfig {
                network: "cf-nope".into(),
                ..cfg("web")
            })
            .expect_err("an undeclared network must be refused");
        assert!(err.is_not_found(), "{err}");
        let shown = delonix_model::Error::from(err);
        assert_eq!(shown.number(), 4505, "{shown}");
        let msg = shown.to_string();
        assert!(
            msg.contains("'cf-nope'") && msg.contains("delonix network create cf-nope"),
            "{msg}"
        );
        assert!(calls(&seen).is_empty(), "{:?}", calls(&seen));
        assert!(e.repo.get("web").is_err(), "no record may be left");
    }

    /// A create over a STOPPED VM that names a different, undeclared network
    /// is refused like a new one — the recover path boots with the new `cfg`,
    /// so skipping the question there moved the VM onto the typo (and Cloud
    /// Hypervisor created it). The record's own network is not asked again.
    #[test]
    fn a_stopped_vm_is_not_recovered_onto_an_undeclared_network() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let declared = DeclaredNets(&["lab-net"]);
        let e = VmEngine {
            network: Some(&declared),
            ..engine(root.path(), seen.clone())
        };
        let on_lab = VmConfig {
            network: "lab-net".into(),
            ..cfg("web")
        };
        e.create(&on_lab).expect("create on a declared network");
        *seen.running.borrow_mut() = false; // stopped
        let boots = |s: &Seen| calls(s).iter().filter(|c| c.starts_with("boot ")).count();
        let before = boots(&seen);

        let err = e
            .create(&VmConfig {
                network: "cf-nope".into(),
                ..cfg("web")
            })
            .expect_err("a stopped VM must not be recovered onto an undeclared network");
        assert_eq!(delonix_model::Error::from(err).number(), 4505);
        assert_eq!(boots(&seen), before, "{:?}", calls(&seen));
        assert_eq!(e.repo.get("web").unwrap().network, "lab-net");

        // Its own network is not a question — even once it is no longer
        // declared (a network an older version made, adopted on boot).
        let none = DeclaredNets(&[]);
        let e = VmEngine {
            network: Some(&none),
            ..e
        };
        e.create(&on_lab)
            .expect("recovering on the record's own network");
        assert_eq!(boots(&seen), before + 1, "{:?}", calls(&seen));
    }

    /// The declared network goes through; the default-network values are not
    /// questions at all (#901's set), even in a process with no network
    /// registered; and a backend that owns its storage is not asked either.
    #[test]
    fn only_a_local_create_on_a_named_network_asks_whether_it_is_declared() {
        let root = tempfile::tempdir().unwrap();
        let nets = DeclaredNets(&["lab-net"]);
        let seen = Rc::new(Seen::default());
        let e = VmEngine {
            network: Some(&nets),
            ..engine(root.path(), seen.clone())
        };
        e.create(&VmConfig {
            network: "lab-net".into(),
            ..cfg("declared")
        })
        .expect("a declared network is accepted");
        for (i, default) in ["", "ingress", "bridge", "default"].iter().enumerate() {
            let e = engine(root.path(), Rc::new(Seen::default()));
            e.create(&VmConfig {
                network: (*default).into(),
                ..cfg(&format!("d{i}"))
            })
            .unwrap_or_else(|err| panic!("{default:?}: {err}"));
        }
        let remote = Rc::new(Seen {
            own_storage: true,
            ..Default::default()
        });
        engine(root.path(), remote)
            .create(&VmConfig {
                network: "vmbr9".into(),
                ..cfg("far")
            })
            .expect("a remote backend decides its own network vocabulary");
    }

    /// A backend that owns its storage (a remote node) is handed the disk
    /// reference verbatim: no local overlay, no seed.
    #[test]
    fn a_backend_that_owns_its_storage_gets_no_local_disk_work() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen {
            own_storage: true,
            ..Default::default()
        });
        let e = engine(root.path(), seen.clone());
        e.create(&cfg("db")).expect("create");
        let c = calls(&seen);
        assert_eq!(c.len(), 1, "{c:?}");
        assert!(c[0].starts_with("boot db /images/base.qcow2"), "{c:?}");
    }

    /// A held create on a backend with no firewall of its own is refused BEFORE
    /// anything is built. The alternative is the defect the hold exists to
    /// prevent: a VM up and open under a policy that says it is closed.
    #[test]
    fn a_held_create_is_refused_by_a_backend_that_cannot_hold_at_boot() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let e = engine(root.path(), seen.clone());
        let err = e
            .create(&VmConfig {
                policy_hold: true,
                ..cfg("web")
            })
            .expect_err("a backend that cannot hold must refuse");
        // The MESSAGE, not the variant: what the operator reads is the contract,
        // and `arch_fitness` counts a match on an error variant outside the
        // foundation as debt.
        let msg = err.to_string();
        assert!(
            msg.contains("cannot create a VM closed") && msg.contains("proxmox"),
            "{msg}"
        );
        // And nothing was built: no overlay, no seed, no boot, no record.
        assert!(calls(&seen).is_empty(), "{:?}", calls(&seen));
        assert!(e.repo.get("web").is_err());
    }

    /// The same create on a backend that CAN hold goes through: the refusal is
    /// about the backend's capability, not about the hold itself.
    #[test]
    fn a_held_create_goes_through_on_a_backend_that_holds_at_boot() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen {
            holds_at_boot: true,
            ..Default::default()
        });
        let e = engine(root.path(), seen.clone());
        let vm = e
            .create(&VmConfig {
                policy_hold: true,
                ..cfg("web")
            })
            .expect("a holding backend accepts a held create");
        assert_eq!(vm.status, Status::Running);
        assert!(
            calls(&seen).iter().any(|c| c.starts_with("boot web ")),
            "{:?}",
            calls(&seen)
        );
    }

    /// A name `create` would refuse never reaches a port.
    #[test]
    fn an_invalid_name_is_refused_before_any_port() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let e = engine(root.path(), seen.clone());
        assert!(e.create(&cfg("../escape")).is_err());
        assert!(calls(&seen).is_empty());
        assert!(e.repo.list().unwrap().is_empty());
    }

    /// A NEW VM whose boot fails takes back everything its create wrote: the
    /// overlay AND the per-VM seed directory. The directory used to stay —
    /// with `user-data` holding the injected key — and with no record, `vm ls`
    /// did not list it and `rm` answered «no such VM» (4501) and kept it.
    #[test]
    fn a_failed_boot_of_a_new_vm_removes_the_overlay_and_the_seed_directory() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen {
            writes_files: true,
            fail_boot: true,
            ..Default::default()
        });
        let e = engine(root.path(), seen.clone());
        e.create(&cfg("web")).expect_err("the boot fails");
        let vms = root.path().join("vms");
        assert!(!vms.join("web.qcow2").exists(), "overlay left behind");
        assert!(!vms.join("web").exists(), "seed directory left behind");
        assert!(e.repo.get("web").is_err());
        assert_eq!(e.remove("web").unwrap_err().number(), 4501);
    }

    /// A seed that fails after it wrote its directory takes the overlay built a
    /// moment earlier with it — the next create of the name would otherwise
    /// reuse that overlay whatever base image it asks for.
    #[test]
    fn a_failed_seed_removes_the_overlay_and_what_the_seed_wrote() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen {
            writes_files: true,
            fail_seed: true,
            ..Default::default()
        });
        let e = engine(root.path(), seen.clone());
        e.create(&cfg("web")).expect_err("the seed fails");
        let vms = root.path().join("vms");
        assert!(!vms.join("web.qcow2").exists(), "overlay left behind");
        assert!(!vms.join("web").exists(), "seed directory left behind");
        assert!(
            !calls(&seen).iter().any(|c| c.starts_with("boot ")),
            "{:?}",
            calls(&seen)
        );
    }

    /// A per-VM directory that was there BEFORE the create — a `--user-data`
    /// seed the caller packaged into it, or what an earlier attempt left — is
    /// not this create's to remove, even when its boot fails.
    #[test]
    fn a_failed_boot_keeps_a_seed_directory_that_was_already_there() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("vms").join("web");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("caller-file"), b"keep").unwrap();
        let seen = Rc::new(Seen {
            writes_files: true,
            fail_boot: true,
            ..Default::default()
        });
        let e = engine(root.path(), seen.clone());
        e.create(&cfg("web")).expect_err("the boot fails");
        assert!(dir.join("caller-file").exists());
        assert!(!root.path().join("vms").join("web.qcow2").exists());
    }

    /// A RESTART whose boot fails deletes nothing: the overlay and the seed
    /// are the recorded VM's, and the record stays to be started again.
    #[test]
    fn a_failed_restart_keeps_the_vms_overlay_and_seed() {
        let root = tempfile::tempdir().unwrap();
        let ok = Rc::new(Seen {
            writes_files: true,
            ..Default::default()
        });
        let e = engine(root.path(), ok.clone());
        e.create(&cfg("web")).expect("create");
        e.stop("web").expect("stop");
        let failing = Rc::new(Seen {
            writes_files: true,
            fail_boot: true,
            ..Default::default()
        });
        let again = VmEngine {
            backends: FakeBackends(failing.clone()),
            disks: FakeDisks(failing.clone()),
            seed: FakeSeed(failing),
            ..e
        };
        again.create(&cfg("web")).expect_err("the restart fails");
        let vms = root.path().join("vms");
        assert!(vms.join("web.qcow2").exists());
        assert!(vms.join("web").join("seed.iso").exists());
        assert_eq!(again.repo.get("web").unwrap().status, Status::Stopped);
    }

    /// `stop` of a name no record describes asks the backends for an
    /// unrecorded VM: none is «no such VM» (4501), one is powered off.
    #[test]
    fn stopping_an_unrecorded_name_asks_the_backends() {
        let root = tempfile::tempdir().unwrap();
        let none = Rc::new(Seen::default());
        let e = engine(root.path(), none.clone());
        let err = e.stop("ghost").unwrap_err();
        assert_eq!(err.number(), 4501, "{err}");
        assert_eq!(calls(&none), vec!["stop_unrecorded ghost"]);

        let held = Rc::new(Seen {
            unrecorded: true,
            ..Default::default()
        });
        engine(root.path(), held.clone())
            .stop("ghost")
            .expect("powered off");
        assert_eq!(calls(&held), vec!["stop_unrecorded ghost"]);
    }

    /// `status` reconciles a record the backend no longer runs: Stopped, no
    /// pid, written back.
    #[test]
    fn status_reconciles_a_vm_whose_backend_stopped_it() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let e = engine(root.path(), seen.clone());
        e.create(&cfg("gone")).expect("create");
        *seen.running.borrow_mut() = false;
        let vm = e.status("gone").expect("status");
        assert_eq!(vm.status, Status::Stopped);
        assert_eq!(vm.pid, None);
        assert_eq!(e.repo.get("gone").unwrap().status, Status::Stopped);
    }

    /// `stop` of a recorded VM stops it on its backend and records Stopped.
    #[test]
    fn stop_records_the_vm_stopped() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let e = engine(root.path(), seen.clone());
        e.create(&cfg("app")).expect("create");
        e.stop("app").expect("stop");
        assert!(calls(&seen).contains(&"stop app".to_string()));
        assert_eq!(e.repo.get("app").unwrap().status, Status::Stopped);
    }

    // ---- `update` (ADR-0068 D1) --------------------------------------------

    /// Nothing to change is refused before any port is asked anything —
    /// the same guard `resize` already has.
    #[test]
    fn update_refuses_when_nothing_is_asked_for() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let e = engine(root.path(), seen.clone());
        e.create(&cfg("app")).expect("create");
        let before = calls(&seen).len();
        let err = e
            .update("app", None, None, Duration::from_secs(1))
            .unwrap_err();
        assert!(err.to_string().contains("nothing to update"), "{err}");
        assert_eq!(calls(&seen).len(), before, "{:?}", calls(&seen));
    }

    /// On a STOPPED VM, `update` is exactly `resize`'s cold path: no
    /// `HotplugOutcome` to report, and the backend's `resize_cold` is what
    /// gets called — never `hotplug_add_cpu`/`hotplug_add_memory`.
    #[test]
    fn update_on_a_stopped_vm_is_the_cold_resize() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let e = engine(root.path(), seen.clone());
        e.create(&cfg("app")).expect("create");
        e.stop("app").expect("stop");
        let (vm, outcomes) = e
            .update("app", Some(2), Some("128M"), Duration::from_secs(1))
            .expect("cold update");
        assert!(outcomes.is_empty(), "{outcomes:?}");
        assert_eq!(vm.vcpus, 2);
        assert_eq!(vm.memory, "128M");
        assert!(
            calls(&seen).contains(&"resize_cold app 2 128".to_string()),
            "{:?}",
            calls(&seen)
        );
        assert!(
            !calls(&seen).iter().any(|c| c.starts_with("hotplug_add_")),
            "{:?}",
            calls(&seen)
        );
    }

    /// On a RUNNING VM, `update` hot-adds each field through the backend,
    /// writes the record after EACH one (D6 — no transaction across the
    /// two), and returns the `HotplugOutcome` of every field asked for.
    #[test]
    fn update_on_a_running_vm_adds_live_and_reports_the_outcome() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let e = engine(root.path(), seen.clone());
        // vcpus=1, memory=64M, Running — with the headroom a live add needs.
        let mut c = cfg("app");
        c.vcpus_max = Some(4);
        c.memory_max_mib = Some(256);
        e.create(&c).expect("create");
        *seen.hotplug_outcome.borrow_mut() = Some(HotplugOutcome::Complete);
        let (vm, outcomes) = e
            .update("app", Some(3), Some("128M"), Duration::from_millis(1))
            .expect("live update");
        assert_eq!(vm.vcpus, 3);
        assert_eq!(vm.memory, "128M");
        assert_eq!(
            outcomes,
            vec![
                ("vcpus", HotplugOutcome::Complete),
                ("memory", HotplugOutcome::Complete),
            ]
        );
        assert!(
            calls(&seen).contains(&"hotplug_add_cpu app -> 3".to_string()),
            "{:?}",
            calls(&seen)
        );
        assert!(
            calls(&seen).contains(&"hotplug_add_memory app -> 128".to_string()),
            "{:?}",
            calls(&seen)
        );
        // Persisted, not just returned.
        assert_eq!(e.repo.get("app").unwrap().vcpus, 3);
        assert_eq!(e.repo.get("app").unwrap().memory, "128M");
    }

    /// A `Partial` outcome is not downgraded to an error, and the record
    /// STILL takes the hypervisor-assigned value (D6: a restart must not
    /// drop it) — the caller decides what a partial outcome means.
    #[test]
    fn a_partial_outcome_still_updates_the_record() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let e = engine(root.path(), seen.clone());
        let mut c = cfg("app");
        c.vcpus_max = Some(2);
        e.create(&c).expect("create");
        *seen.hotplug_outcome.borrow_mut() = Some(HotplugOutcome::Partial {
            reason: "guest activation not observable".into(),
        });
        let (vm, outcomes) = e
            .update("app", Some(2), None, Duration::from_millis(1))
            .expect("partial is Ok, not Err");
        assert_eq!(vm.vcpus, 2);
        assert_eq!(e.repo.get("app").unwrap().vcpus, 2);
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0].1, HotplugOutcome::Partial { .. }));
    }

    /// A target at or below what the VM already has is refused — removal is
    /// a separate, later capability (D10), not a silent no-op on `update`.
    #[test]
    fn update_refuses_a_cpu_or_memory_decrease() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let e = engine(root.path(), seen.clone());
        e.create(&cfg("app")).expect("create"); // vcpus=1, memory=64M.
        let before = calls(&seen).len();
        let err = e
            .update("app", Some(1), None, Duration::from_secs(1))
            .unwrap_err();
        assert!(
            err.to_string().contains("already has 1 vCPUs")
                && err.to_string().contains("vm resize"),
            "{err}"
        );
        let err = e
            .update("app", None, Some("32M"), Duration::from_secs(1))
            .unwrap_err();
        assert!(
            err.to_string().contains("already has 64 MiB") && err.to_string().contains("vm resize"),
            "{err}"
        );
        // Neither refusal reached the backend.
        assert_eq!(calls(&seen).len(), before, "{:?}", calls(&seen));
    }

    // ---- ceilings and restarts (ADR-0068 Q2, ADR-0012) -----------------------

    /// A restart rebuilds the record from the boot shape — and used to drop
    /// the labels and annotations with it, so a stack's VM came back unowned.
    #[test]
    fn a_restart_keeps_the_records_labels_and_annotations() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let e = engine(root.path(), seen.clone());
        e.create(&cfg("app")).expect("create");
        e.repo
            .update("app", |vm| {
                vm.labels.insert("delonix.io/stack".into(), "shop".into());
                vm.annotations
                    .insert("delonix.io/last-applied".into(), "{}".into());
                true
            })
            .unwrap();
        e.stop("app").expect("stop");
        let vm = e.start("app").expect("start");
        assert_eq!(vm.status, Status::Running);
        let rec = e.repo.get("app").unwrap();
        assert_eq!(
            rec.labels.get("delonix.io/stack").map(String::as_str),
            Some("shop")
        );
        assert!(rec.annotations.contains_key("delonix.io/last-applied"));
    }

    /// The cold resize sets and clears the ceilings, and the backend is
    /// handed the record as it will be — the ceilings included.
    #[test]
    fn resize_shape_sets_and_clears_the_ceilings_and_hands_them_to_the_backend() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let e = engine(root.path(), seen.clone());
        e.create(&cfg("app")).expect("create");
        e.stop("app").expect("stop");
        let vm = e
            .resize_shape(
                "app",
                &ResizeRequest {
                    vcpus: Some(2),
                    vcpus_max: Some(Some(4)),
                    memory_max_mib: Some(Some(256)),
                    ..ResizeRequest::default()
                },
            )
            .expect("resize");
        assert_eq!(
            (vm.vcpus, vm.vcpus_max, vm.memory_max_mib),
            (2, Some(4), Some(256))
        );
        let handed = seen.resized.borrow().clone().expect("backend asked");
        assert_eq!(
            (handed.vcpus_max, handed.memory_max_mib),
            (Some(4), Some(256))
        );
        assert_eq!(e.repo.get("app").unwrap().vcpus_max, Some(4));
        let vm = e
            .resize_shape(
                "app",
                &ResizeRequest {
                    vcpus_max: Some(None),
                    memory_max_mib: Some(None),
                    ..ResizeRequest::default()
                },
            )
            .expect("clear");
        assert_eq!((vm.vcpus_max, vm.memory_max_mib), (None, None));
    }

    /// A ceiling below the boot size is refused before the backend is asked.
    #[test]
    fn a_ceiling_below_the_boot_size_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let e = engine(root.path(), seen.clone());
        e.create(&cfg("app")).expect("create");
        e.stop("app").expect("stop");
        let before = calls(&seen).len();
        let err = e
            .resize_shape(
                "app",
                &ResizeRequest {
                    vcpus: Some(4),
                    vcpus_max: Some(Some(2)),
                    ..ResizeRequest::default()
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("ceiling (2)"), "{err}");
        let err = e
            .resize_shape(
                "app",
                &ResizeRequest {
                    memory_max_mib: Some(Some(32)),
                    ..ResizeRequest::default()
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("32 MiB"), "{err}");
        assert_eq!(calls(&seen).len(), before, "{:?}", calls(&seen));
    }

    /// A live add above the declared ceiling is refused by the engine, naming
    /// the ceiling and the command that changes it — the backend is never
    /// asked to clamp.
    #[test]
    fn a_live_add_above_the_ceiling_is_refused_before_the_backend() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let e = engine(root.path(), seen.clone());
        let mut c = cfg("app");
        c.vcpus_max = Some(2);
        c.memory_max_mib = Some(128);
        e.create(&c).expect("create");
        *seen.hotplug_outcome.borrow_mut() = Some(HotplugOutcome::Complete);
        let before = calls(&seen).len();
        let err = e
            .update("app", Some(3), None, Duration::from_millis(1))
            .unwrap_err();
        assert!(
            err.to_string().contains("at most 2 vCPUs") && err.to_string().contains("--vcpus-max"),
            "{err}"
        );
        let err = e
            .update("app", None, Some("256M"), Duration::from_millis(1))
            .unwrap_err();
        assert!(err.to_string().contains("--memory-max"), "{err}");
        assert_eq!(calls(&seen).len(), before, "{:?}", calls(&seen));
        // Inside the ceiling it goes through.
        let (vm, _) = e
            .update("app", Some(2), Some("128M"), Duration::from_millis(1))
            .expect("inside the ceiling");
        assert_eq!(vm.vcpus, 2);
    }

    /// An empty resize is refused before the lookup: `vm resize <unknown>`
    /// with no flag is "nothing to resize" (class 1), never "no such VM".
    #[test]
    fn an_empty_resize_is_refused_before_the_vm_is_looked_up() {
        let root = tempfile::tempdir().unwrap();
        let e = engine(root.path(), Rc::new(Seen::default()));
        let err = e
            .resize_shape("vm-nada", &ResizeRequest::default())
            .unwrap_err();
        assert!(matches!(err, Error::InvalidResize(_)), "{err:?}");
        let err = e
            .check_resize_shape("vm-nada", &ResizeRequest::default())
            .unwrap_err();
        assert!(matches!(err, Error::InvalidResize(_)), "{err:?}");
    }

    /// The check a reboot runs BEFORE its stop: every reshape rule, on a
    /// RUNNING VM, and nothing changed.
    #[test]
    fn the_shape_check_refuses_on_a_running_vm_and_changes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let e = engine(root.path(), seen.clone());
        e.create(&cfg("app")).expect("create");
        let bad = ResizeRequest {
            vcpus_max: Some(Some(0)),
            ..Default::default()
        };
        assert!(e.check_resize_shape("app", &bad).is_err());
        let below = ResizeRequest {
            vcpus: Some(4),
            vcpus_max: Some(Some(2)),
            ..Default::default()
        };
        assert!(e.check_resize_shape("app", &below).is_err());
        let fine = ResizeRequest {
            vcpus: Some(4),
            vcpus_max: Some(Some(8)),
            ..Default::default()
        };
        e.check_resize_shape("app", &fine)
            .expect("the power state is not the check's business");
        assert!(seen.resized.borrow().is_none(), "nothing was reshaped");
    }

    /// ADR-0068 D6/D7: a hot-added disk lands in the record with the target
    /// the hypervisor gave it PINNED, so the next boot keeps it where the
    /// guest saw it; a NIC with its MAC pinned and its partial outcome.
    #[test]
    fn a_hot_added_device_reaches_the_record_pinned() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let e = engine(root.path(), seen.clone());
        e.create(&cfg("app")).expect("create");
        let disk = crate::ExtraDisk {
            source: "/data/d.qcow2".into(),
            ..Default::default()
        };
        let (vm, o) = e
            .change_device("app", DeviceChange::AddDisk(disk.clone()), Duration::ZERO)
            .unwrap();
        assert_eq!(o, Some(HotplugOutcome::Complete));
        assert_eq!(vm.boot.extra_disks[0].target.as_deref(), Some("vdz"));
        assert_eq!(e.load_vm("app").unwrap().boot.extra_disks.len(), 1);
        let (vm, o) = e
            .change_device(
                "app",
                DeviceChange::AddNic(crate::ExtraNic {
                    kind: "network".into(),
                    source: Some("default".into()),
                    ..Default::default()
                }),
                Duration::ZERO,
            )
            .unwrap();
        assert!(matches!(o, Some(HotplugOutcome::Partial { .. })));
        assert_eq!(
            vm.boot.extra_nics[0].mac.as_deref(),
            Some("52:54:00:aa:bb:cc")
        );
        // The same disk twice is refused before the backend.
        let before = calls(&seen).len();
        assert!(e
            .change_device("app", DeviceChange::AddDisk(disk), Duration::ZERO)
            .is_err());
        assert_eq!(calls(&seen).len(), before);
    }

    /// D6: the record names the disk as soon as the hypervisor's read-back
    /// confirms it — a command killed during the guest wait must not leave
    /// the domain with a disk the record does not know.
    #[test]
    fn a_disk_is_recorded_before_the_guest_wait() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let e = engine(root.path(), seen.clone());
        e.create(&cfg("app")).expect("create");
        *seen.fail_after_attach.borrow_mut() = true;
        let disk = crate::ExtraDisk {
            source: "/data/d.qcow2".into(),
            ..Default::default()
        };
        assert!(e
            .change_device("app", DeviceChange::AddDisk(disk), Duration::ZERO)
            .is_err());
        let rec = e.load_vm("app").unwrap();
        assert_eq!(rec.boot.extra_disks.len(), 1, "recorded before the wait");
        assert_eq!(rec.boot.extra_disks[0].target.as_deref(), Some("vdz"));
    }

    /// D10: nothing is removed live; the refusal names `vm stop`, and the
    /// record keeps the disk.
    #[test]
    fn a_device_is_not_removed_live_and_is_removed_cold() {
        let root = tempfile::tempdir().unwrap();
        let seen = Rc::new(Seen::default());
        let e = engine(root.path(), seen.clone());
        e.create(&cfg("app")).expect("create");
        let disk = crate::ExtraDisk {
            source: "/data/d.qcow2".into(),
            ..Default::default()
        };
        e.change_device("app", DeviceChange::AddDisk(disk), Duration::ZERO)
            .unwrap();
        let err = e
            .change_device(
                "app",
                DeviceChange::RemoveDisk("vdz".into()),
                Duration::ZERO,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("D10") && err.to_string().contains("vm stop"),
            "{err}"
        );
        assert_eq!(e.load_vm("app").unwrap().boot.extra_disks.len(), 1);
        e.stop("app").unwrap();
        let (vm, o) = e
            .change_device(
                "app",
                DeviceChange::RemoveDisk("vdz".into()),
                Duration::ZERO,
            )
            .unwrap();
        assert_eq!(o, None, "cold: nothing live to report");
        assert!(vm.boot.extra_disks.is_empty());
        assert!(
            calls(&seen)
                .iter()
                .any(|c| c == "change_devices_cold app disks=0 nics=0"),
            "{:?}",
            calls(&seen)
        );
        // A key the VM does not have is refused, by name.
        let err = e
            .change_device(
                "app",
                DeviceChange::RemoveNic("52:54:00:00:00:09".into()),
                Duration::ZERO,
            )
            .unwrap_err();
        assert!(err.to_string().contains("52:54:00:00:00:09"), "{err}");
    }
}
