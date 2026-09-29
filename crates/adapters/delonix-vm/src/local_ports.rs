//! This adapter's implementations of the ports the VM orchestration calls
//! (`docs/discovery/61`, P4b.3a): the registry of backends, the local disk
//! overlay and the cloud-init seed. The orchestration reaches its backends,
//! `qemu-img` and `cloud-localds` only through these. Since P4b.4 this crate
//! is the VM composition root and holds no backend; the disk and seed ports
//! stay here, with the `qemu-img` helpers they need, until the application
//! layer (P5) absorbs the composition.

use delonix_compute::capability::Capability;
use delonix_compute::ports::{LocalDiskImages, SeedBuilder, VmBackends};
use delonix_compute::vm_backend::{CreateStage, VmBackend, VmConfig};
use delonix_compute::Vm;
use std::path::{Path, PathBuf};
use std::process::Command;

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

    fn admit(&self, backend_id: &str, cfg: &VmConfig) -> delonix_model::Result<()> {
        Ok(delonix_provider_libvirt::check_allow_mac_spoofing(
            cfg, backend_id,
        )?)
    }

    /// Only libvirt keeps a VM after its record is gone: a domain an old `rm`
    /// left behind. The other local backend's VM is a process that dies with
    /// its record.
    fn unrecorded(&self, name: &str) -> Option<&'static str> {
        delonix_provider_libvirt::libvirt_domain_uri(name).map(|_| "libvirt")
    }

    fn stop_unrecorded(&self, name: &str) -> delonix_model::Result<bool> {
        match delonix_provider_libvirt::libvirt_domain_uri(name) {
            Some(uri) => {
                delonix_provider_libvirt::libvirt_poweroff(uri, name)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn remove_unrecorded(&self, name: &str) -> delonix_model::Result<()> {
        Ok(delonix_provider_libvirt::libvirt_cleanup(name)?)
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
        let bf = disk_backing_format(&disk_path);
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
            if let Some(base_bytes) = disk_virtual_size_bytes(&disk_path) {
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
        run_quiet(
            "qemu-img",
            &argv.iter().map(String::as_str).collect::<Vec<_>>(),
        )?;
    }
    Ok((disk_path, overlay))
}

/// Extracts the format from the `file format: <fmt>` line of the HUMAN output of
/// `qemu-img info`. Pure function (testable without `qemu-img`).
///
/// NB: the human output is used on purpose — the modern `--output=json` nests a
/// `children` node with the protocol layer's `"format": "file"` BEFORE the
/// top-level `"format"`, and a naive parse would catch "file" instead of "qcow2". The
/// human output has a single `file format:` line (the top-level one).
fn parse_qemu_format(info: &str) -> Option<String> {
    for line in info.lines() {
        if let Some(rest) = line.trim().strip_prefix("file format:") {
            let f = rest.trim();
            if !f.is_empty() {
                return Some(f.to_string());
            }
        }
    }
    None
}

/// Extracts the virtual size IN BYTES from the `virtual size: … (N bytes)` line
/// of `qemu-img info`. Pure function (testable without `qemu-img`).
///
/// Reads the parenthesised byte count and not the human figure: `2.2 GiB` is
/// rounded, and a per-node disk quota compared against a rounded number is a
/// quota that lets through what it meant to refuse.
fn parse_qemu_virtual_size_bytes(info: &str) -> Option<u64> {
    for line in info.lines() {
        if let Some(rest) = line.trim().strip_prefix("virtual size:") {
            let inside = rest.split('(').nth(1)?;
            let digits: String = inside.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                return digits.parse().ok();
            }
        }
    }
    None
}

/// Virtual size of a disk, in bytes, via `qemu-img info`.
fn disk_virtual_size_bytes(disk: &Path) -> Option<u64> {
    let out = stable_cmd("qemu-img").arg("info").arg(disk).output().ok()?;
    parse_qemu_virtual_size_bytes(&String::from_utf8_lossy(&out.stdout))
}

/// The REAL format of the base disk via `qemu-img info` — does NOT trust the extension.
/// Ubuntu/Debian cloud images are distributed as `*.img` but are **qcow2**
/// internally; an overlay created with `-F raw` over a qcow2 backing makes the
/// guest read the qcow2 as raw → corrupted / non-booting disk, silently.
/// Falls back to the extension heuristic if `qemu-img info` is not available.
pub fn disk_backing_format(disk: &Path) -> String {
    // `qemu-img info` is PARSED (`parse_qemu_format`) — same locale exposure
    // as the `virsh` state strings; see `stable_cmd`.
    if let Ok(out) = stable_cmd("qemu-img").arg("info").arg(disk).output() {
        if out.status.success() {
            if let Some(fmt) = std::str::from_utf8(&out.stdout)
                .ok()
                .and_then(parse_qemu_format)
            {
                return fmt;
            }
        }
    }
    if disk.extension().and_then(|e| e.to_str()) == Some("qcow2") {
        "qcow2".into()
    } else {
        "raw".into()
    }
}

/// Runs an external tool (e.g. `qemu-img`/`virsh`) CAPTURING stdout+stderr
/// (nothing leaks raw to the terminal) — surfacing the captured stderr in the
/// error. The `create` progress UI wants clean staged lines, not the raw
/// `Formatting '...qcow2'` / `Domain 'x' defined` chatter of `qemu-img`/`virsh`.
fn run_quiet(prog: &str, args: &[&str]) -> super::Result<()> {
    let out = stable_cmd(prog)
        .args(args)
        .output()
        .map_err(|e| super::Error::Command {
            context: "vm-tool",
            message: format!("{prog}: {e}"),
        })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim().trim_start_matches("error: ").trim();
        return Err(super::Error::Command {
            context: "vm-tool",
            message: if err.is_empty() {
                format!("{prog} failed")
            } else {
                format!("{prog}: {err}")
            },
        });
    }
    Ok(())
}

/// Builds a `Command` whose output this crate PARSES, pinned to the `C` locale.
///
/// BUG FIXED HERE (latent, and it bites precisely in this product's home
/// market). `virsh` is a gettext program — confirmed on this host: its binary
/// exports `bindtextdomain`/`dcgettext` and carries `"shut off"` as a
/// translatable msgid. Meanwhile this crate decides a domain's liveness by
/// comparing that output against ENGLISH literals:
///
/// ```text
/// libvirt_poweroff:      state == "shut off"
/// LibvirtBackend::is_running:  s == "running"
/// ```
///
/// On a host with libvirt's l10n catalogues installed and `LANG=pt_PT` — an
/// ordinary Angolan/Portuguese production host — `virsh domstate` answers in
/// Portuguese and BOTH comparisons silently go false. A running VM reports as
/// stopped (`vm ls` lies, `wait_for_boot` never converges) and
/// `libvirt_poweroff` fires `destroy` at an already-off domain, which is exactly
/// the raw-stderr failure v0.11 fixed from the other end.
///
/// Pinning the locale is the right layer: it makes the tool's output a stable
/// MACHINE interface, rather than teaching every call site to recognise N
/// translations. `LANG` is set too — `LC_ALL` alone is enough for glibc, but
/// belt-and-braces costs nothing and covers tools that read `LANG` directly.
///
/// This is also why it lives on the shared helpers rather than on the `virsh`
/// call sites: `qemu-img`, `losetup` and friends are parsed the same way and
/// have the same exposure.
pub(crate) fn stable_cmd(prog: &str) -> Command {
    let mut c = Command::new(prog);
    c.env("LC_ALL", "C").env("LANG", "C");
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    /// REGRESSION: toda a ferramenta cujo OUTPUT este crate parseia tem de
    /// correr com locale fixo.
    ///
    /// `virsh` é um programa gettext (confirmado neste host: exporta
    /// `bindtextdomain`/`dcgettext` e carrega `"shut off"` como msgid
    /// traduzível), e o crate decide se um domínio está vivo comparando
    /// `virsh domstate` com literais ingleses. Num host com os catálogos
    /// instalados e `LANG=pt_PT`, uma VM a correr passa a reportar-se como
    /// parada. Tirar o `.env("LC_ALL", "C")` do `stable_cmd` faz este teste
    /// falhar.
    #[test]
    fn stable_cmd_fixa_o_locale_para_o_output_ser_estavel() {
        let cmd = stable_cmd("virsh");
        let envs: std::collections::HashMap<_, _> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            envs.get("LC_ALL").and_then(|v| v.as_deref()),
            Some("C"),
            "sem LC_ALL=C o `virsh domstate` responde traduzido e a comparação com \
             \"running\"/\"shut off\" falha em silêncio"
        );
        assert_eq!(envs.get("LANG").and_then(|v| v.as_deref()), Some("C"));
    }

    /// O tamanho virtual lê-se dos BYTES entre parênteses, não do número
    /// humano: `2.2 GiB` é arredondado, e uma quota comparada com um número
    /// arredondado é uma quota que deixa passar o que queria recusar.
    #[test]
    fn tamanho_virtual_le_se_dos_bytes_e_nao_do_numero_humano() {
        let info = "image: g.qcow2\nfile format: qcow2\nvirtual size: 10 GiB (10737418240 bytes)\ndisk size: 690 MiB\n";
        assert_eq!(parse_qemu_virtual_size_bytes(info), Some(10_737_418_240));

        // 2.2 GiB arredondado esconde 161 MiB — daí ler os bytes.
        let arred = "virtual size: 2.2 GiB (2361393152 bytes)\n";
        assert_eq!(parse_qemu_virtual_size_bytes(arred), Some(2_361_393_152));

        // Sem os parênteses (formatos antigos), não se inventa um número.
        assert_eq!(parse_qemu_virtual_size_bytes("virtual size: 8 MiB\n"), None);
        assert_eq!(parse_qemu_virtual_size_bytes("file format: qcow2\n"), None);
    }

    #[test]
    fn parse_qemu_format_extrai_formato_real() {
        // Human output of `qemu-img info` for a `.img` that is qcow2 inside
        // (the core of the backing-format bug).
        let info = "image: jammy.img\nfile format: qcow2\nvirtual size: 2.2 GiB (2361393152 bytes)\ndisk size: 614 MiB\n";
        assert_eq!(parse_qemu_format(info).as_deref(), Some("qcow2"));
        let raw = "image: disco.img\nfile format: raw\nvirtual size: 8 MiB\n";
        assert_eq!(parse_qemu_format(raw).as_deref(), Some("raw"));
        assert_eq!(parse_qemu_format("image: x\nvirtual size: 8 MiB\n"), None);
    }
}
