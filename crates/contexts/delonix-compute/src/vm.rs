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
    mem_mib, parse_mem_mib, CloudInitIntent, CreateStage, DestroyStage, GuestInfo, MoveOptions,
    VmBackend, VmConfig,
};
use crate::vm_error::{Error, Result};
use crate::vm_firewall as firewall;
use crate::{Vm, VmBootSpec};
use delonix_model::ports::StateRepository;
use delonix_model::records::Status;
use delonix_node::proc_starttime;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
            let iso = self.seed.seed(self.root, cfg)?;
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
                    // Clean up the overlay only when WE made it. With
                    // `manages_own_storage`, `overlay` IS `cfg.disk` verbatim — the name
                    // the caller wrote for something on the far node — and removing it
                    // means this engine deleting a file it did not create. For today's
                    // Proxmox backend that name is `local-lvm:8` and the unlink simply
                    // fails, but the rule cannot rest on the spelling a backend happens
                    // to use: a remote backend whose disk reference IS a local path
                    // would lose the user's base image on a failed boot.
                    if restarting.is_none() && !own_storage {
                        let _ = std::fs::remove_file(&overlay);
                    }
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
        vm.devices = cfg.devices.clone();
        vm.boot = boot_spec_of(cfg);
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
    /// Changes a STOPPED VM's vCPUs and/or memory for its next boot — the cold
    /// resize (`vm.resize.cold`, see [`VmBackend::resize_cold`]).
    ///
    /// Everything that can be refused is refused before the backend is asked:
    /// nothing to change, zero vCPUs, a memory value that does not parse (the
    /// lenient [`mem_mib`] would read `2GB` as 1 GiB and this would report it
    /// done), and a VM that is running or paused — a guest that only sees the
    /// change after its next reboot has not been resized yet. The record is
    /// rewritten only after the backend returns `Ok`, so a refused or failed
    /// resize leaves it saying what the VM actually has.
    ///
    /// Returns the updated record.
    pub fn resize(&self, name: &str, vcpus: Option<u32>, memory: Option<&str>) -> Result<Vm> {
        if vcpus.is_none() && memory.is_none() {
            return Err(Error::InvalidResize(format!(
                "nothing to resize on VM '{name}': give --vcpus and/or --memory"
            )));
        }
        if vcpus == Some(0) {
            return Err(Error::InvalidResize(format!(
                "VM '{name}' cannot have 0 vCPUs"
            )));
        }
        let new_mib = match memory {
            Some(m) => Some(parse_mem_mib(m).ok_or_else(|| {
                Error::InvalidResize(format!(
                    "memory '{m}' is not a size: use a number with an optional M/G suffix (512M, 4G, 4Gi)"
                ))
            })?),
            None => None,
        };
        let vmdir = self.vmdir();
        let st = &self.repo;
        let mut vm = self.load_vm(name)?;
        if matches!(vm.status, Status::Running | Status::Paused) {
            return Err(Error::ResizeNeedsStopped(format!(
                "VM '{name}' is {:?}: `vm resize` is a cold resize — stop it first (`delonix vm stop {name}`)",
                vm.status
            )));
        }
        let target_vcpus = vcpus.unwrap_or(vm.vcpus.max(1));
        let target_mib = new_mib.unwrap_or_else(|| mem_mib(&vm.memory));
        self.backends
            .for_vm(&vm)?
            .resize_cold(&vmdir, &vm, target_vcpus, target_mib)?;
        vm.vcpus = target_vcpus;
        if let Some(m) = memory {
            vm.memory = m.trim().to_string();
        }
        st.set(name, &vm).map_err(Error::from)?;
        Ok(vm)
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
        network: _,
        namespace: _,
        restart_policy: _,
        devices: _,
        backend: _,
        net_mode: _,
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
pub fn config_from(vm: &Vm) -> VmConfig {
    let b = &vm.boot;
    VmConfig {
        name: vm.name.clone(),
        disk: vm.disk.clone(),
        vcpus: vm.vcpus,
        memory: vm.memory.clone(),
        network: vm.network.clone(),
        namespace: Some(vm.namespace.clone()),
        restart_policy: vm.restart_policy.clone(),
        devices: vm.devices.clone(),
        backend: Some(vm.backend.clone()),
        // Not persisted: the requirement was checked against the backend the
        // record names when the VM was created (see `boot_spec_of`).
        required_capabilities: Vec::new(),
        // For libvirt, `Vm.tap` is not a real tap: `LibvirtBackend::boot` stores
        // the net mode string there. For Cloud Hypervisor it IS a device name
        // and must not be misread as one.
        net_mode: (vm.backend == "libvirt").then(|| vm.tap.clone()),
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
            Ok((PathBuf::from(base), vmdir.join(format!("{name}.qcow2"))))
        }
    }

    struct FakeSeed(Rc<Seen>);

    impl SeedBuilder for FakeSeed {
        fn seed(&self, root: &Path, cfg: &VmConfig) -> delonix_model::Result<PathBuf> {
            self.0.calls.borrow_mut().push(format!("seed {}", cfg.name));
            Ok(root.join(format!("{}-seed.iso", cfg.name)))
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
}
