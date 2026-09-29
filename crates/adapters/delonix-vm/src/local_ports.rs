//! This adapter's implementations of the ports the VM orchestration calls
//! (`docs/discovery/61`, P4b.3a): the registry of backends, the local disk
//! overlay and the cloud-init seed. The orchestration reaches its backends,
//! `qemu-img` and `cloud-localds` only through these, so moving it into
//! `delonix-compute` (P4b.3b) is a move and not a rewrite. When the two local
//! backends leave this crate (P4b.4), the composition root implements the
//! first and a `delonix-guestfs` adapter the other two.

use delonix_compute::capability::Capability;
use delonix_compute::ports::{LocalDiskImages, SeedBuilder, VmBackends};
use delonix_compute::vm_backend::{CreateStage, VmBackend, VmConfig};
use delonix_compute::Vm;
use std::path::{Path, PathBuf};

/// The registry of this adapter, as [`VmBackends`].
pub struct RegistryBackends;

impl VmBackends for RegistryBackends {
    fn for_vm(&self, vm: &Vm) -> delonix_model::Result<Box<dyn VmBackend>> {
        Ok(super::backend_for(vm)?)
    }

    fn select(
        &self,
        root: &Path,
        cfg: &VmConfig,
        required: &[Capability],
    ) -> delonix_model::Result<Box<dyn VmBackend>> {
        Ok(super::select_for_create(root, cfg, required)?)
    }

    fn require(&self, backend_id: &str, required: &[Capability]) -> delonix_model::Result<()> {
        Ok(super::require_capabilities(backend_id, required)?)
    }

    fn declares(&self, backend_id: &str, cap: Capability) -> bool {
        super::backend_declares(backend_id, cap)
    }

    /// Only libvirt keeps a VM after its record is gone: a domain an old `rm`
    /// left behind. The other local backend's VM is a process that dies with
    /// its record.
    fn unrecorded(&self, name: &str) -> Option<&'static str> {
        super::libvirt_domain_uri(name).map(|_| "libvirt")
    }

    fn stop_unrecorded(&self, name: &str) -> delonix_model::Result<bool> {
        match super::libvirt_domain_uri(name) {
            Some(uri) => {
                super::libvirt_poweroff(uri, name)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn remove_unrecorded(&self, name: &str) -> delonix_model::Result<()> {
        Ok(super::libvirt_cleanup(name)?)
    }
}

/// The overlay built with `qemu-img` over a base image on this host.
pub struct QemuImgDisks;

impl LocalDiskImages for QemuImgDisks {
    fn overlay(
        &self,
        vmdir: &Path,
        name: &str,
        base: &str,
        size_gib: Option<u32>,
        on: &dyn Fn(CreateStage),
    ) -> delonix_model::Result<(PathBuf, PathBuf)> {
        Ok(prepare_local_overlay(vmdir, name, base, size_gib, on)?)
    }
}

/// The NoCloud seed built with `cloud-localds`.
pub struct CloudLocaldsSeed;

impl SeedBuilder for CloudLocaldsSeed {
    fn seed(&self, root: &Path, cfg: &VmConfig) -> delonix_model::Result<PathBuf> {
        super::cloudinit::generate_seed_iso(
            root,
            &cfg.name,
            cfg.hostname.as_deref(),
            cfg.ci_user.as_deref(),
            &cfg.ssh_keys,
            None,
            &cfg.volumes,
        )
        .map_err(|e| super::Error::from(e).into())
    }
}

/// Resolves `disk` on THIS filesystem and builds the VM's thin qcow2
/// overlay from it. Extracted from `create_with` so a backend that owns its
/// storage can skip the whole thing (`manages_own_storage`) instead of the
/// engine doing local disk work for a hypervisor on another machine.
fn prepare_local_overlay(
    vmdir: &Path,
    name: &str,
    disk: &str,
    disk_size_gib: Option<u32>,
    on: &dyn Fn(CreateStage),
) -> super::Result<(PathBuf, PathBuf)> {
    let disk_path = std::fs::canonicalize(disk).map_err(|e| {
        super::Error::from(delonix_model::Error::not_found_or_io(e, || {
            format!("VM image {disk}")
        }))
    })?;
    let overlay = vmdir.join(format!("{name}.qcow2"));
    if !overlay.exists() {
        on(CreateStage::Disk);
        let bf = super::disk_backing_format(&disk_path);
        let mut argv: Vec<String> = vec![
            "create".into(),
            "-f".into(),
            "qcow2".into(),
            "-b".into(),
            disk_path.to_string_lossy().into_owned(),
            "-F".into(),
            bf,
            overlay.to_string_lossy().into_owned(),
        ];
        // Tamanho pedido para o nó. O `qemu-img create` aceita-o depois do
        // ficheiro e o guest cresce a raiz no arranque (growpart do cloud-init,
        // medido). Sem isto o overlay herda o tamanho da base — que é
        // deliberadamente o PISO da golden.
        if let Some(gib) = disk_size_gib {
            let pedido = u64::from(gib) * 1024 * 1024 * 1024;
            // Um overlay não pode ser menor que o seu backing file: o
            // `qemu-img` aceita-o em algumas versões e o resultado é uma VM que
            // arranca e corrompe o filesystem. Recusar por nome e com os dois
            // números é a diferença entre um erro e um mistério.
            if let Some(base_bytes) = super::disk_virtual_size_bytes(&disk_path) {
                if pedido < base_bytes {
                    return Err(super::Error::DiskTooSmall(format!(
                        "--disk-size {gib}G é menor que a imagem base ({} GiB): um overlay qcow2 \
                         não encolhe o seu backing file",
                        base_bytes / (1024 * 1024 * 1024)
                    )));
                }
            }
            argv.push(format!("{gib}G"));
        }
        super::run_quiet(
            "qemu-img",
            &argv.iter().map(String::as_str).collect::<Vec<_>>(),
        )?;
    }
    Ok((disk_path, overlay))
}
