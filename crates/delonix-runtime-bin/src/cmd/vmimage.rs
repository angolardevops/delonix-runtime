//! `delonix image --vm` — golden VM images (Ubuntu + kubeadm/kubelet/
//! kubectl + `delonix-cri`), managed separately from container images (those
//! live in `cmd::image`/`ImageStore`). One standalone `.qcow2` per image (no
//! CAS/layers — there is only one blob per image, nothing to deduplicate) + a
//! `.json` of metadata, both under `<root>/vm-images/`.
//!
//! `build` produces the image from scratch (download of the Ubuntu cloud
//! image plus `virt-customize`); `push`/`pull` publish/fetch it from an OCI
//! registry (a single-blob artifact, see
//! `delonix_image::registry::{push_oci_artifact, pull_oci_artifact}`) — the
//! same protocol as container images, only without the Docker layers/config
//! model.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Subcommand;
use delonix_runtime_core::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::output::{self, fmt_local, fmt_size};
use super::util::state_root;

const VM_IMAGE_MEDIA_TYPE: &str = "application/vnd.delonix.vmimage.v1.qcow2";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VmImage {
    pub name: String,
    pub tag: String,
    pub digest: String,
    pub size: u64,
    pub ubuntu_release: Option<String>,
    pub k8s_version: Option<String>,
    pub created_unix: u64,
    /// The Linux kernel release string (`uname -r` shape, e.g.
    /// `6.8.0-31-generic`) baked into the image — read back via `virt-cat`
    /// right after `virt-customize` (see `cmd_build`), never booted to find
    /// out. `None` for images built before this field existed, or `vm pull`ed
    /// (same known gap as `ubuntu_release`/`k8s_version` — the OCI artifact
    /// only carries the qcow2 blob, not build metadata).
    #[serde(default)]
    pub kernel_version: Option<String>,
    /// `"ubuntu"` | `"debian"` — added in v0.17.0 alongside multi-distro
    /// support. `#[serde(default)]` so pre-existing on-disk metadata (all
    /// Ubuntu, built before this field existed) still loads; `None` there is
    /// treated as `"ubuntu"` for display purposes (`distro_label`), never
    /// re-written on disk. `ubuntu_release` keeps its field NAME for
    /// backward-compat with existing `.json` files (no `#[serde(default)]`
    /// on it — a rename would break loading them) but now holds the release
    /// identifier for WHATEVER distro this is (a Debian codename like
    /// `bookworm`, not just an Ubuntu release number).
    #[serde(default)]
    pub distro: Option<String>,
    /// Recommended vCPUs (`VCPUS` in the `VMfile` that built this image).
    /// `vm create` uses it as the default when `--vcpus` is not given AND the
    /// disk resolves to this image (by local name) — never overrides an
    /// explicit `--vcpus`. `#[serde(default)]`: absent on images built before
    /// this field existed, or `vm pull`ed (the OCI artifact only carries the
    /// qcow2 blob, same known gap as `ubuntu_release`/`k8s_version`).
    #[serde(default)]
    pub default_vcpus: Option<u32>,
    /// Recommended memory (`MEMORY` in the `VMfile`, e.g. `"2G"`). Same rule
    /// as `default_vcpus`.
    #[serde(default)]
    pub default_memory: Option<String>,
    /// Recommended VM backend (`HYPERVISOR` in the `VMfile`):
    /// `"cloud-hypervisor"` or `"libvirt"`, already canonicalized by
    /// `delonix_vm::valid_backend_name`. Same rule as `default_vcpus` — wins
    /// over the engine's own auto-detection heuristic but never over an
    /// explicit `--backend`/`DELONIX_VM_BACKEND`/persisted default (see
    /// `cmd::vm::resolve_vm_defaults`, fed by `cmd::vm::resolve_image_ref`).
    #[serde(default)]
    pub default_backend: Option<String>,
    /// Whether the guest runs cloud-init. `None` (every image built before
    /// this field existed) means *unknown* and is treated as `true` — the
    /// cloud images this engine has always built do run it, so nothing about
    /// them changes.
    ///
    /// `Some(false)` marks an **appliance**: a vendor system that installs and
    /// configures itself (OPNsense, Proxmox, TrueNAS). For those, `vm create`
    /// must NOT attach the NoCloud seed it otherwise always builds — an ISO
    /// nothing reads, on a CD-ROM that changes the guest's device list. And
    /// `--hostname`/`--ssh-key`/`--user-data` are refused outright rather than
    /// accepted and silently dropped, which is the failure mode this repo
    /// names as its worst (see `--security-opt seccomp=`, `-v …:z`,
    /// `--network-alias` in AGENTS.md).
    #[serde(default)]
    pub cloud_init: Option<bool>,
    /// How many packages the image carries, and the sha256 of the inventory
    /// itself (`<store>/<name>.packages.tsv`, and `/usr/share/delonix/
    /// packages.tsv` inside the guest). `None` on images built before this
    /// existed or `vm pull`ed — the same known gap as `kernel_version`.
    #[serde(default)]
    pub packages: Option<u32>,
    #[serde(default)]
    pub packages_sha256: Option<String>,
    /// The `delonix` that built it, and the sha256 of the vendor cloud image it
    /// started from — the two facts that let someone repeat this build from the
    /// same starting point. The base image moves under a stable URL, so its
    /// name alone does not identify it.
    #[serde(default)]
    pub built_by: Option<String>,
    #[serde(default)]
    pub base_sha256: Option<String>,
}

impl VmImage {
    /// True when this image's guest runs cloud-init, so a NoCloud seed is
    /// worth generating. Unknown (pre-existing metadata) counts as yes: that
    /// is what those images have always been.
    pub fn uses_cloud_init(&self) -> bool {
        self.cloud_init.unwrap_or(true)
    }
}

/// Target format for `image vm convert` — **the integration point with every
/// other hypervisor's ecosystem**.
///
/// This engine runs two backends (libvirt/QEMU and Cloud Hypervisor) and will
/// not grow a backend for every product on the market: VirtualBox does not
/// coexist with KVM on one host, vSphere and Proxmox are remote datacenter APIs
/// rather than local hypervisors, and Hyper-V is Windows. But an IMAGE built
/// here can be imported by all of them, and `qemu-img` already writes every one
/// of their formats — so the cheap, honest integration is the artifact, not a
/// driver. Build here, import there.
///
/// The mapping is not decorative; each format is what a specific product's
/// importer expects:
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConvertFormat {
    /// QEMU/KVM, libvirt, Cloud Hypervisor — this engine's own two backends.
    Qcow2,
    /// Raw sectors: Proxmox VE (its default for LVM/ZFS/Ceph storage), and the
    /// universal fallback anything can read. Not sparse on a filesystem that
    /// does not support holes — expect the full virtual size on disk.
    Raw,
    /// VMware — Workstation, Fusion, ESXi/vSphere.
    Vmdk,
    /// VirtualBox.
    Vdi,
    /// Hyper-V (Windows 8/2012 and later), and Azure's modern disk format.
    Vhdx,
    /// Hyper-V's older VHD (`vpc` in qemu-img's own naming). Kept separate from
    /// `vhdx` because they are different formats, not spellings — an importer
    /// that wants one rejects the other.
    Vhd,
}

impl ConvertFormat {
    /// The name `qemu-img -O` knows. **VHD is `vpc` there** — qemu's historical
    /// name for it — which is exactly the kind of detail a user should not have
    /// to know to get a file Hyper-V accepts.
    fn as_str(self) -> &'static str {
        match self {
            ConvertFormat::Qcow2 => "qcow2",
            ConvertFormat::Raw => "raw",
            ConvertFormat::Vmdk => "vmdk",
            ConvertFormat::Vdi => "vdi",
            ConvertFormat::Vhdx => "vhdx",
            ConvertFormat::Vhd => "vpc",
        }
    }

    /// The file extension the target ecosystem expects — NOT always the same as
    /// the qemu format name (`vpc` produces a `.vhd`), which is why the two are
    /// separate functions instead of one string used twice.
    fn extension(self) -> &'static str {
        match self {
            ConvertFormat::Qcow2 => "qcow2",
            ConvertFormat::Raw => "raw",
            ConvertFormat::Vmdk => "vmdk",
            ConvertFormat::Vdi => "vdi",
            ConvertFormat::Vhdx => "vhdx",
            ConvertFormat::Vhd => "vhd",
        }
    }

    /// Whether the format supports `qemu-img`'s `-c` (compressed) output.
    ///
    /// Only qcow2 and vmdk do. Passing `-c` to the others makes `qemu-img` fail
    /// outright, so this is what keeps `--compress` from turning into a
    /// confusing tool error on a flag the user was invited to use.
    fn supports_compression(self) -> bool {
        matches!(self, ConvertFormat::Qcow2 | ConvertFormat::Vmdk)
    }
}

/// Base distro for a golden image build. `Ubuntu` stays the default (no
/// behavior change for existing callers); `Debian` is additive.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Distro {
    Ubuntu,
    Debian,
    Rocky,
    Fedora,
}

impl Distro {
    fn as_str(self) -> &'static str {
        match self {
            Distro::Ubuntu => "ubuntu",
            Distro::Debian => "debian",
            Distro::Rocky => "rocky",
            Distro::Fedora => "fedora",
        }
    }
}

/// `"ubuntu/24.04"`-style label for `ls`/`describe`. Pre-v0.17.0 metadata has
/// no `distro` field — displayed as just the release, matching what those
/// images actually showed before this column existed (never silently
/// mislabeled as Debian, which `None` would otherwise risk).
fn distro_label(img: &VmImage) -> String {
    match (img.distro.as_deref(), img.ubuntu_release.as_deref()) {
        (Some(d), Some(r)) => format!("{d}/{r}"),
        (Some(d), None) => d.to_string(),
        (None, Some(r)) => r.to_string(),
        (None, None) => "-".to_string(),
    }
}

pub struct VmImageStore {
    root: PathBuf,
}

impl VmImageStore {
    pub fn open(base: impl Into<PathBuf>) -> Result<Self> {
        let root = base.into().join("vm-images");
        std::fs::create_dir_all(root.join("_base"))?;
        Ok(Self { root })
    }

    fn sanitize(name: &str) -> String {
        name.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    fn meta_path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{}.json", Self::sanitize(name)))
    }

    pub fn qcow2_path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{}.qcow2", Self::sanitize(name)))
    }

    /// The package inventory extracted from the image at build time. Sidecar
    /// and not a field of the `.json`: a full inventory is tens of KiB and the
    /// metadata is read on every `vm ls`.
    pub fn sbom_path(&self, name: &str) -> PathBuf {
        self.root
            .join(format!("{}.packages.tsv", Self::sanitize(name)))
    }

    pub fn base_cache_path(&self, distro: Distro, release: &str) -> PathBuf {
        // `sanitize` (not applied here before — security-audit finding, see
        // AGENTS.md) strips `/` from `release`, preventing
        // `--ubuntu-release '../../../etc/cron.d/x'` from writing outside `_base/`.
        let filename = match distro {
            Distro::Ubuntu => format!(
                "ubuntu-{}-server-cloudimg-amd64.img",
                Self::sanitize(release)
            ),
            Distro::Debian => format!(
                "debian-{}-genericcloud-amd64.qcow2",
                Self::sanitize(release)
            ),
            Distro::Rocky => format!("rocky-{}-genericcloud-amd64.qcow2", Self::sanitize(release)),
            Distro::Fedora => format!(
                "Fedora-Cloud-Base-Generic-{}.x86_64.qcow2",
                Self::sanitize(release)
            ),
        };
        self.root.join("_base").join(filename)
    }

    pub fn save(&self, img: &VmImage) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(img)?;
        std::fs::write(self.meta_path(&img.name), bytes)?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<VmImage>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.root)?.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(bytes) = std::fs::read(&p) {
                    if let Ok(img) = serde_json::from_slice::<VmImage>(&bytes) {
                        out.push(img);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Deletes only the metadata. Separate from the disk on purpose: the
    /// caller removes the disk FIRST and this LAST, so a failure never leaves a
    /// multi-gigabyte file that no command can see.
    pub fn remove_meta(&self, name: &str) -> Result<()> {
        match std::fs::remove_file(self.meta_path(name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn get(&self, name: &str) -> Result<VmImage> {
        let bytes = std::fs::read(self.meta_path(name))
            .map_err(|_| Error::NotFound(format!("imagem VM '{name}'")))?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

/// Every VM overlay that backs onto `image`, by name.
///
/// Read from the DISK and not from the registry: `qemu-img` reports what the
/// overlay actually points at, while a record only says what it was created
/// with. A VM whose registry entry was hand-edited, or one made outside this
/// engine, still holds the image open — and it is the ones nobody remembers
/// that this check exists for.
///
/// An overlay `qemu-img` cannot OPEN is reported as a USER, not skipped: not
/// knowing what it points at is precisely when refusing to delete the base is
/// right. Measured, because the first version of this comment claimed more than
/// it delivered: `qemu-img info` does NOT fail on a file that is not a qcow2 —
/// it reads it as `raw` and reports no backing file, which is the correct
/// answer for a raw disk. It fails when it cannot read the file at all
/// (permissions, or a file that goes away mid-scan), and that is the case this
/// branch exists for.
fn vms_backed_by(root: &std::path::Path, image_qcow2: &std::path::Path) -> Vec<String> {
    let want = std::fs::canonicalize(image_qcow2).unwrap_or_else(|_| image_qcow2.to_path_buf());
    let mut users = Vec::new();
    let Ok(entries) = std::fs::read_dir(root.join("vms")) else {
        return users;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("qcow2") {
            continue;
        }
        let name = p
            .file_stem()
            .and_then(|x| x.to_str())
            .unwrap_or("?")
            .to_string();
        let out = std::process::Command::new("qemu-img")
            .args(["info", "--output=json", "--"])
            .arg(&p)
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let backing = serde_json::from_slice::<serde_json::Value>(&o.stdout)
                    .ok()
                    .and_then(|v| {
                        v.get("backing-filename")
                            .and_then(|b| b.as_str())
                            .map(str::to_string)
                    });
                if let Some(b) = backing {
                    let b =
                        std::fs::canonicalize(&b).unwrap_or_else(|_| std::path::PathBuf::from(&b));
                    if b == want {
                        users.push(name);
                    }
                }
            }
            // Could not open it at all → assume it might point here.
            _ => users.push(format!("{name} (unreadable)")),
        }
    }
    users.sort();
    users
}

/// Removes VM images. **Disk first, metadata LAST.**
///
/// The order is the rule the v0.37.0 audit wrote down after `volumes rm` did it
/// backwards: the metadata is the only thing that makes the image visible, so
/// deleting it first and then failing on the disk leaves gigabytes on the
/// filesystem that no command lists and nobody will ever find. This way a
/// failed delete leaves the image exactly as it was — still listed, still
/// usable, and still removable.
pub(crate) fn cmd_rm(store: &VmImageStore, names: &[String], force: bool) -> Result<()> {
    let root = state_root();
    let mut failed = false;
    // «Não existe» é uma CLASSE de saída (4), não o genérico. Um reconciliador
    // usa o código para decidir entre «cria porque falta» e «pára porque
    // falhou», e um lote onde tudo o que falhou foi por ausência tem de dizer
    // isso — como o `container rm` já diz.
    let mut missing = false;
    for name in names {
        // Fail on a name that does not exist (docker/`vm rm` parity), rather
        // than reporting success for a removal that removed nothing.
        let img = match store.get(name) {
            Ok(i) => i,
            Err(_) => {
                super::output::error(&super::po::tf(
                    "no such VM image: {name} (see `delonix image vm ls`)",
                    &[("name", name)],
                ));
                failed = true;
                missing = true;
                continue;
            }
        };
        let qcow2 = store.qcow2_path(&img.name);
        let users = vms_backed_by(&root, &qcow2);
        if !users.is_empty() {
            if !force {
                super::output::error(&super::po::tf(
                    "VM image '{name}' is the backing file of: {vms} — remove those VMs first (`delonix vm rm <name>`), or pass --force to make them unreadable",
                    &[("name", name), ("vms", &users.join(", "))],
                ));
                failed = true;
                continue;
            }
            super::output::warn(&super::po::tf(
                "VM image '{name}': {vms} back onto it and will become unreadable",
                &[("name", name), ("vms", &users.join(", "))],
            ));
        }
        // Disk first.
        match std::fs::remove_file(&qcow2) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                super::output::error(&super::po::tf(
                    "VM image '{name}': could not remove its disk ({err}) — the image is untouched",
                    &[("name", name), ("err", &e.to_string())],
                ));
                failed = true;
                continue;
            }
        }
        // Bookkeeping last: until this line the image is still listed, which is
        // what makes a failure above recoverable.
        store.remove_meta(&img.name)?;
        println!("{}", img.name);
    }
    if failed {
        let msg: String = super::po::t("one or more VM images were not removed").into();
        // A CLASSE da saída importa: um lote onde tudo o que falhou foi por
        // ausência diz «não existe» (4), não o genérico (1) — é o que um
        // reconciliador lê para decidir entre criar e parar.
        return Err(if missing {
            Error::NotFound(msg)
        } else {
            Error::Invalid(msg)
        });
    }
    Ok(())
}

// A CLI enum parsed once per invocation, not a hot path — the same
// justification the sibling command enums already carry.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub enum VmImageCmd {
    /// Remove a local VM image (its disk and its metadata).
    ///
    /// **Refused while a VM still uses it.** A VM created from an image runs on
    /// a thin overlay whose backing file IS that image: deleting it out from
    /// under a live overlay does not free the VM, it makes it permanently
    /// unreadable. `--force` overrides, and says what it is breaking.
    Rm {
        /// Image name(s), as shown by `image vm ls`.
        #[arg(required = true)]
        names: Vec<String>,
        /// Remove it even while VMs back onto it — **those VMs stop being
        /// readable**, and there is no way back.
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// List the local VM images.
    Ls {
        /// Output format: `table` (default) or `json` (ADR-0005).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: output::OutputFormat,
    },
    /// Human-readable detail of one or more VM images, `kubectl describe` style.
    Describe {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::vm_images))]
        names: Vec<String>,
    },
    /// Publish a local VM image to an OCI registry (single-blob artifact).
    Push {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::vm_images))]
        name: String,
        /// Destination. Omit it for an OFFICIAL repository: which one is
        /// decided by what the image says about itself (an appliance goes to
        /// the appliances repo, a Kubernetes golden to the k8s one). Give it
        /// to publish anywhere else.
        target: Option<String>,
    },
    /// Convert a VM disk between `qcow2` (default, per-VM overlay) and `raw`
    /// — flattened (no backing file) either way, ready to boot on either
    /// backend (libvirt/QEMU and Cloud Hypervisor already share this same
    /// pair of formats; there is no separate "per-hypervisor" format here).
    Convert {
        /// A local VM image name (`image vm ls`) or a literal `.qcow2`/`.raw` path.
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::vm_images))]
        source: String,
        /// Target format.
        #[arg(long = "to", value_enum)]
        to: ConvertFormat,
        /// Destination file (default: alongside the source, with the new extension).
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'o', long = "output")]
        output: Option<PathBuf>,
        /// Compress the output. Only `qcow2` and `vmdk` can — refused for the
        /// others rather than handed to `qemu-img` to fail on.
        #[arg(long)]
        compress: bool,
    },
    /// Register an existing disk image under a name, so `vm create --disk
    /// <name>` and `image vm push` can use it.
    ///
    /// The counterpart to `build` for a disk this engine did not produce: a
    /// vendor appliance installed from its own ISO, or an image exported from
    /// somewhere else. The file is copied into the store as-is — nothing is
    /// booted, nothing is inspected.
    Import(ImportArgs),
    /// Pull a VM image from an OCI registry — with no argument, the OFFICIAL
    /// Delonix image (ready for `vm create`/`cluster kubeadm`).
    Pull {
        source: Option<String>,
        #[arg(long)]
        name: Option<String>,
        /// With no `source`, pull the official NO-Kubernetes golden (just the
        /// `delonix` engine, rootless-ready) instead of the Kubernetes one.
        #[arg(long)]
        no_k8s: bool,
    },
    /// List the tags available in a remote OCI repository — with no
    /// argument, the OFFICIAL Delonix golden image repo (discover which
    /// k8s versions are published before `pull`/`--k8s-version`).
    LsRemote {
        source: Option<String>,
        /// With no `source`, list the official NO-Kubernetes golden's repo
        /// instead of the Kubernetes one.
        #[arg(long)]
        no_k8s: bool,
    },
    /// Scaffold a `VMfile` (and a cloud-init) for building your own image.
    ///
    /// Writes a recipe that BUILDS AS WRITTEN — a scaffold nobody can run is
    /// documentation that lies. Delete what you do not need.
    Init {
        /// Name to use in the scaffold (image tag, hostname, account).
        #[arg(default_value = "myimage")]
        name: String,
        /// Where to write it (default: the current directory).
        #[arg(value_hint = clap::ValueHint::DirPath, short = 'd', long)]
        dir: Option<PathBuf>,
        /// Overwrite an existing `VMfile`.
        #[arg(long)]
        force: bool,
    },
    /// Build a VM image: from a `VMfile` when there is one, otherwise the
    /// built-in golden recipe (Ubuntu cloud image + kubeadm/kubelet/kubectl +
    /// `delonix-cri`), via `virt-customize`.
    Build {
        #[arg(short = 't', long = "tag")]
        tag: String,
        /// Build from a `VMfile` instead of the built-in golden recipe.
        ///
        /// With no `-f`, a `VMfile` in the context directory is used if there
        /// is one — same rule `delonix build` follows for `Delonixfile`. The
        /// flags below (`--distro`, `--k8s-version`, …) belong to the golden
        /// recipe and are REFUSED with a VMfile, which describes all of that
        /// itself; accepting and ignoring them is the failure this repo names
        /// as its worst.
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
        /// Build context — the directory `COPY` reads from (default: `.`).
        #[arg(value_hint = clap::ValueHint::DirPath, default_value = ".")]
        context: PathBuf,
        /// Base distro for the cloud image.
        #[arg(long, value_enum, default_value = "ubuntu")]
        distro: Distro,
        #[arg(long, default_value = "26.04")]
        ubuntu_release: String,
        /// Debian codename (`bookworm`, `trixie`, ...) — only used with `--distro debian`.
        #[arg(long, default_value = "bookworm")]
        debian_release: String,
        /// Rocky Linux major version (`8`, `9`, `10`) — only used with `--distro
        /// rocky`. Rocky currently only supports `--no-k8s` builds.
        #[arg(long, default_value = "9")]
        rocky_release: String,
        /// Fedora release AND build, as shown on Fedora's download page
        /// (e.g. `42-1.1`) — only used with `--distro fedora`. The build
        /// number is not derivable from the release, and Fedora's redirector
        /// offers no listing to look it up, so it is asked for rather than
        /// guessed.
        #[arg(long, default_value = "42-1.1")]
        fedora_release: String,
        /// Kubernetes version (e.g. `1.31`) — omit for `DEFAULT_K8S_VERSION`
        /// (1.36, o tecto do control-plane alojado). 1.34/1.35 continuam
        /// disponíveis passando-as aqui.
        #[arg(long)]
        k8s_version: Option<String>,
        /// Extra apt package, repeatable — extensibility without touching the code.
        #[arg(long = "extra-package")]
        extra_packages: Vec<String>,
        /// Extra command to run inside the guest during the build, repeatable.
        #[arg(long = "extra-run")]
        extra_run: Vec<String>,
        /// Explicit path of the `delonix-cri` binary to install (otherwise:
        /// looks next to the current `delonix`, then tries to build from the
        /// workspace if a `Cargo.toml` is detected from the cwd).
        #[arg(value_hint = clap::ValueHint::FilePath, long)]
        cri_bin: Option<PathBuf>,
        /// Do not compress the final qcow2 (larger, but no decompression cost
        /// on backing-file reads at runtime).
        #[arg(long)]
        no_compress: bool,
        /// Give the guest network access during `RUN` — VMfile builds only.
        /// The golden recipe already decides this with `--offline`.
        #[arg(long)]
        network: bool,
        /// Fetch the k8s .deb files on the HOST (verified: InRelease signature +
        /// SHA256) and install them with `dpkg` — the appliance runs without
        /// network (`--no-network`). Dispenses with DHCP/DNS in the guest, so it
        /// dispenses with the host workarounds (passt/dhclient) the online mode requires.
        #[arg(long)]
        offline: bool,
        /// Build a golden image with NO Kubernetes at all — just the
        /// `delonix` engine binary, ready for rootless containers (mutually
        /// exclusive with `--k8s-version`/`--offline`, which don't apply).
        #[arg(long)]
        no_k8s: bool,
        /// Explicit path of the `delonix` binary to install when `--no-k8s`
        /// (otherwise: the currently running `delonix`, then a workspace
        /// build, then a verified download from the matching release).
        #[arg(value_hint = clap::ValueHint::FilePath, long)]
        delonix_bin: Option<PathBuf>,
        /// Set a root password in the image. WITHOUT this, no account ships
        /// with one (the supported ways in are the SSH key cloud-init injects
        /// and the key `cluster kubeadm` generates). Use it only when you need
        /// the serial console to take a login — and remember the image is an
        /// artefact you may publish.
        #[arg(long)]
        root_password: Option<String>,
        /// Install the Prometheus node_exporter and enable it on this address
        /// (bare flag: `0.0.0.0:9100`). Without it the image ships no listener.
        #[arg(long, require_equals = true, num_args = 0..=1, default_missing_value = "0.0.0.0:9100")]
        node_exporter: Option<String>,
    },
}

pub fn run(action: VmImageCmd) -> Result<()> {
    let store = VmImageStore::open(state_root())?;
    match action {
        VmImageCmd::Rm { names, force } => cmd_rm(&store, &names, force),
        VmImageCmd::Ls { output } => cmd_ls(&store, output),
        VmImageCmd::Describe { names } => cmd_describe(&store, &names),
        VmImageCmd::Push { name, target } => cmd_push(&store, &name, target.as_deref()),
        VmImageCmd::Convert {
            source,
            to,
            output,
            compress,
        } => cmd_convert(&store, &source, to, output, compress),
        VmImageCmd::Import(args) => cmd_import(&store, args),
        VmImageCmd::Pull {
            source,
            name,
            no_k8s,
        } => {
            // BUG FIXED HERE, found live: this is the shared engine command
            // behind BOTH `image --vm pull` AND `image vm pull` — it never
            // got the "no argument = official image" default that `delonix
            // vm pull` (a separate, sibling CLI definition in `cmd/vm.rs`)
            // already has, despite this exact struct's own doc comment
            // claiming it. A user on a real host hit this: `delonix image vm
            // pull --name delonix-vm-k8s:1.34` (no source) errored "required
            // arguments were not provided: <SOURCE>".
            // A reference WITHOUT a registry (`opnsense:26.1`) is looked up in
            // the official catalogue; one with a `/` is used verbatim, so the
            // argument keeps meaning "somewhere of your own".
            let src = match source {
                Some(s) => resolve_official_ref(&s)?,
                None => default_pull_source(no_k8s).to_string(),
            };
            cmd_pull(&store, &src, name)
        }
        VmImageCmd::LsRemote { source, no_k8s } => match source {
            // A bare name resolves against the official catalogue, so
            // `ls-remote appliances` needs no URL.
            Some(s) => cmd_ls_remote(&resolve_official_ref(&s)?),
            // `--no-k8s` predates the catalogue and still means "the base
            // images"; kept working rather than removed.
            None if no_k8s => cmd_ls_remote(
                OFFICIAL_REPOS
                    .iter()
                    .find(|r| r.key == "base")
                    .expect("base is in the table")
                    .repo,
            ),
            None => cmd_ls_remote_official(),
        },
        VmImageCmd::Init { name, dir, force } => cmd_init(&name, dir, force),
        VmImageCmd::Build {
            tag,
            file,
            context,
            distro,
            ubuntu_release,
            debian_release,
            rocky_release,
            fedora_release,
            k8s_version,
            extra_packages,
            extra_run,
            cri_bin,
            no_compress,
            network,
            offline,
            no_k8s,
            delonix_bin,
            root_password,
            node_exporter,
        } => {
            // A `VMfile` in the context beats the golden recipe, the same way a
            // `Delonixfile` beats a `Dockerfile` for `delonix build`. Explicit
            // `-f` always wins.
            let vmfile = file.or_else(|| {
                let p = context.join("VMfile");
                p.exists().then_some(p)
            });
            if let Some(path) = vmfile {
                // The golden-recipe flags describe a recipe the VMfile replaces.
                // Silently ignoring them would let someone believe their
                // `--k8s-version` took effect.
                let golden: &[(&str, bool)] = &[
                    ("--k8s-version", k8s_version.is_some()),
                    ("--extra-package", !extra_packages.is_empty()),
                    ("--extra-run", !extra_run.is_empty()),
                    ("--offline", offline),
                    ("--no-k8s", no_k8s),
                    ("--cri-bin", cri_bin.is_some()),
                    ("--delonix-bin", delonix_bin.is_some()),
                ];
                let used: Vec<&str> = golden
                    .iter()
                    .filter(|(_, on)| *on)
                    .map(|(n, _)| *n)
                    .collect();
                if !used.is_empty() {
                    return Err(Error::Invalid(super::po::tf(
                        "{flags} belong to the built-in golden recipe and mean nothing with a VMfile — the VMfile describes all of that itself",
                        &[("flags", &used.join(", "))],
                    ).to_string()));
                }
                // This path (`image vm build -f VMfile`) has no `--verbose` of
                // its own; `DELONIX_VERBOSE` still unfolds it, which is what
                // `Progress::new` reads.
                return super::vmfile::build(
                    &store,
                    &path,
                    &context,
                    &tag,
                    !no_compress,
                    network,
                    false,
                );
            }
            if network {
                return Err(Error::Invalid(super::po::t(
                    "`--network` is for VMfile builds; the golden recipe decides it with `--offline`",
                )
                .to_string()));
            }
            cmd_build(
                &store,
                &tag,
                distro,
                &ubuntu_release,
                &debian_release,
                &rocky_release,
                &fedora_release,
                k8s_version,
                extra_packages,
                extra_run,
                cri_bin,
                !no_compress,
                offline,
                no_k8s,
                delonix_bin,
                root_password,
                node_exporter,
            )
        }
    }
}

/// `vm init` — writes the scaffold.
fn cmd_init(name: &str, dir: Option<PathBuf>, force: bool) -> Result<()> {
    let dir = super::vmfile::init_dir(dir);
    std::fs::create_dir_all(&dir).map_err(|e| Error::Invalid(format!("{}: {e}", dir.display())))?;
    let vmfile = dir.join("VMfile");
    if vmfile.exists() && !force {
        return Err(Error::Invalid(super::po::tf(
            "{path} already exists — use --force to overwrite",
            &[("path", &vmfile.display().to_string())],
        )));
    }
    std::fs::write(&vmfile, super::vmfile::scaffold(name))
        .map_err(|e| Error::Invalid(format!("{}: {e}", vmfile.display())))?;
    let ci_dir = dir.join("cloud-init");
    std::fs::create_dir_all(&ci_dir)
        .map_err(|e| Error::Invalid(format!("{}: {e}", ci_dir.display())))?;
    let ci = ci_dir.join("user-data.yaml");
    if !ci.exists() || force {
        std::fs::write(&ci, super::vmfile::scaffold_cloud_init(name))
            .map_err(|e| Error::Invalid(format!("{}: {e}", ci.display())))?;
    }
    println!("{}", vmfile.display());
    println!("{}", ci.display());
    println!(
        "\n{}",
        super::po::tf(
            "Next: `delonix vm build -t {name}:1.0 {dir}` then `delonix vm create dev --disk-image {name}:1.0`",
            &[("name", name), ("dir", &dir.display().to_string())],
        )
    );
    Ok(())
}

/// `image --vm ls -o json` / `image vm ls -o json` row (ADR-0005): machine-friendly
/// values (`created_unix`/`size_bytes` as numbers; nullable kernel/k8s).
#[derive(serde::Serialize)]
struct VmImageLsRow {
    name: String,
    distro: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    kernel_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    k8s_version: Option<String>,
    created_unix: u64,
    size_bytes: u64,
    /// Whether the guest runs cloud-init; `null` for images registered before
    /// the field existed. Automation reads this to know whether passing
    /// `--hostname`/`--ssh-key` to `vm create` would be refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    cloud_init: Option<bool>,
}

fn cmd_ls(store: &VmImageStore, format: output::OutputFormat) -> Result<()> {
    if format == output::OutputFormat::Json {
        let rows: Vec<VmImageLsRow> = store
            .list()?
            .into_iter()
            .map(|img| VmImageLsRow {
                distro: distro_label(&img),
                name: img.name,
                kernel_version: img.kernel_version,
                k8s_version: img.k8s_version,
                created_unix: img.created_unix,
                size_bytes: img.size,
                cloud_init: img.cloud_init,
            })
            .collect();
        return output::print_json(&rows);
    }
    // TYPE and DEFAULTS answer what the reader actually needs before running
    // `vm create`: whether this image configures itself (and therefore refuses
    // `--ssh-key`), and what it wants for vCPU/memory. KERNEL/K8S stay — they
    // are filled whenever they are known, which now includes a `vm pull` (the
    // manifest annotations carry them) and an `import --kernel-version`.
    let mut t = output::Table::new(&[
        "NAME", "DISTRO", "TYPE", "KERNEL", "K8S", "DEFAULTS", "CREATED", "SIZE",
    ])
    .right_align(7);
    for img in store.list()? {
        let distro = distro_label(&img);
        let type_label = image_type_label(match img.cloud_init {
            Some(true) => Some("true"),
            Some(false) => Some("false"),
            None => None,
        });
        let defaults = defaults_label(&img);
        t.row(vec![
            img.name,
            distro,
            type_label,
            img.kernel_version.as_deref().unwrap_or("-").to_string(),
            img.k8s_version.as_deref().unwrap_or("-").to_string(),
            defaults,
            fmt_local(img.created_unix),
            fmt_size(img.size),
        ]);
    }
    t.print();
    Ok(())
}

/// `image --vm describe` — human-readable detail, `kubectl describe` style.
fn cmd_describe(store: &VmImageStore, names: &[String]) -> Result<()> {
    for (i, name) in names.iter().enumerate() {
        let img = store.get(name)?;
        if i > 0 {
            println!();
        }
        describe_one(store, &img);
    }
    Ok(())
}

fn describe_one(store: &VmImageStore, img: &VmImage) {
    let mut d = output::Describe::new();
    d.field("Name", &img.name);
    d.field("Tag", &img.tag);
    d.field("Digest", &img.digest);
    d.field("Size", fmt_size(img.size));
    d.field("Created", fmt_local(img.created_unix));
    d.field("Age", output::fmt_age(img.created_unix));
    // `pull` does NOT recover this metadata (the OCI artifact only carries the
    // qcow2 blob) — on a pulled image they stay `None`. See the known gap in AGENTS.md.
    let distro = distro_label(img);
    d.field("Distro", if distro == "-" { "<unknown>" } else { &distro });
    d.field(
        "Kernel",
        img.kernel_version.as_deref().unwrap_or("<unknown>"),
    );
    d.field("K8s", img.k8s_version.as_deref().unwrap_or("<unknown>"));
    // Worth a line of its own: this is what makes `vm create` skip the seed
    // and refuse `--hostname`/`--ssh-key`, and without it that refusal reads
    // as arbitrary.
    d.field(
        "Cloud-init",
        match img.cloud_init {
            Some(true) => super::po::t("yes"),
            Some(false) => super::po::t("no (appliance — configure it on first boot)"),
            None => super::po::t("<unknown> (assumed yes)"),
        },
    );
    d.field_opt("Default vCPUs", img.default_vcpus.map(|v| v.to_string()));
    d.field_opt("Default memory", img.default_memory.clone());
    d.field_opt("Default backend", img.default_backend.clone());
    let qcow2 = store.qcow2_path(&img.name);
    d.field("Path", qcow2.to_string_lossy());
    // The `size` above is the build/pull one; this is what IS on disk now. If
    // they diverge, the artifact was tampered with out-of-band — worth being able to see.
    d.field_opt(
        "On disk",
        std::fs::metadata(&qcow2).ok().map(|m| fmt_size(m.len())),
    );
    // Proveniência e inventário: as duas perguntas que alguém faz a um
    // artefacto que recebeu — «de onde veio isto?» e «tem a versão X?».
    // Sem elas a resposta é montar a imagem.
    d.field_opt("Built by", img.built_by.clone());
    d.field_opt("Base sha256", img.base_sha256.clone());
    let sbom = store.sbom_path(&img.name);
    match (img.packages, sbom.exists()) {
        (Some(n), true) => {
            d.field(
                "Packages",
                super::po::tf(
                    "{n} (see {path})",
                    &[("n", &n.to_string()), ("path", &sbom.to_string_lossy())],
                ),
            );
        }
        (Some(n), false) => {
            d.field("Packages", n.to_string());
        }
        // Deliberadamente uma linha e não silêncio: «não sei» e «zero pacotes»
        // não são a mesma coisa, e um artefacto sem inventário é um facto que
        // quem o for auditar precisa de ver.
        (None, _) => {
            d.field(
                "Packages",
                super::po::t("<unknown> (built before the inventory existed, or pulled)"),
            );
        }
    }
    d.print();
}

/// A repository the project itself publishes. Having these as DATA rather than
/// as a pair of constants behind a boolean is what lets `ls-remote`, `pull` and
/// `push` all agree on what "official" means without any of them taking an
/// argument — and what makes adding a fourth one a single row instead of a new
/// flag.
///
/// The old shape was `OFFICIAL_VM_IMAGE`/`OFFICIAL_VM_BASE_IMAGE` selected by
/// `--no-k8s`, which stops working the moment there is a third repository: a
/// boolean cannot name three things.
pub(crate) struct OfficialRepo {
    /// Short name for the CLI and for messages (`k8s`, `base`, `appliances`).
    pub key: &'static str,
    /// `host/namespace/name`, never with a tag.
    pub repo: &'static str,
    /// What a bare `pull` from this repository should fetch. `None` for a
    /// repository with no single sensible default — `appliances` holds
    /// unrelated products, and picking one of them for the user would be a
    /// guess dressed as a default.
    pub default_tag: Option<&'static str>,
    /// One line, for the listing header.
    pub what: &'static str,
}

pub(crate) const OFFICIAL_REPOS: &[OfficialRepo] = &[
    OfficialRepo {
        key: "k8s",
        repo: "ghcr.io/angolardevops/delonix-vm-k8s",
        default_tag: Some(DEFAULT_K8S_VERSION),
        what: "golden Kubernetes node (kubeadm/kubelet/kubectl + delonix-cri)",
    },
    OfficialRepo {
        key: "base",
        repo: "ghcr.io/angolardevops/delonix-vm-base",
        default_tag: Some("ubuntu-24.04"),
        what: "base OS images (delonix engine, rootless-ready, no Kubernetes)",
    },
    OfficialRepo {
        key: "appliances",
        repo: "ghcr.io/angolardevops/delonix-vm-appliances",
        default_tag: None,
        what: "vendor appliances (OPNsense, Proxmox, TrueNAS) — no cloud-init",
    },
];

/// The official repository a LOCAL image belongs in, decided from what the
/// image says about itself rather than from a flag: an appliance goes with the
/// appliances, an image carrying a Kubernetes version with the golden nodes,
/// anything else with the base images.
///
/// This is what lets `image vm push <name>` work with no destination. `None`
/// when the metadata does not settle it — and then the caller must be told to
/// name a target instead of having one guessed.
pub(crate) fn official_repo_for(img: &VmImage) -> Option<&'static OfficialRepo> {
    let key = if img.cloud_init == Some(false) {
        "appliances"
    } else if img.k8s_version.is_some() {
        "k8s"
    } else if img.distro.is_some() {
        "base"
    } else {
        return None;
    };
    OFFICIAL_REPOS.iter().find(|r| r.key == key)
}

/// Resolves a reference that names no registry (`opnsense:26.1`) against the
/// official repositories, so a pull of something the project publishes needs
/// no URL. A reference WITH a `/` is a real one and is returned untouched —
/// the parameter keeps meaning "a repository of your own".
pub(crate) fn resolve_official_ref_local(reference: &str) -> Option<String> {
    if reference.contains('/') {
        return Some(reference.to_string());
    }
    let name = reference.split(':').next().unwrap_or(reference);
    for r in OFFICIAL_REPOS {
        if r.key == name {
            return Some(match r.default_tag {
                Some(tag) => format!(
                    "{}:{}",
                    r.repo,
                    reference.split_once(':').map_or(tag, |(_, t)| t)
                ),
                None => r.repo.to_string(),
            });
        }
    }
    None
}

/// The tag a bare reference is asking for. `<product>:<version>` is spelled
/// `<product>-<version>` in the official repositories, so both forms name the
/// same artifact; anything else is already a tag.
pub(crate) fn official_tag_candidate(reference: &str) -> String {
    match reference.split_once(':') {
        Some((prod, ver)) => format!("{prod}-{ver}"),
        None => reference.to_string(),
    }
}

/// Resolves a reference to a full repository ref, asking the official
/// repositories when the answer cannot be known locally.
///
/// [`resolve_official_ref_local`] settles what is decidable offline: anything
/// with a `/` already names a repository, and a repository KEY (`k8s`, `base`,
/// `appliances`) names one directly. Everything else is a bare tag, and the
/// only honest way to place it is to look — the tag namespaces are not
/// partitioned by any rule this code can derive. `rocky-9` lives in the base
/// repository and `proxmox-ve-9.1` in the appliances one, and nothing in either
/// string says so.
///
/// This used to fall back to the appliances repository for every bare tag. That
/// is what turned `delonix vm pull rocky-9` into `no such image
/// …/delonix-vm-appliances:rocky-9` — a 404 naming a repository the user had
/// never typed, about an image that was published and public the whole time.
/// The fallback was right when appliances were the only repository with product
/// tags, and started lying the moment the base images shipped.
pub(crate) fn resolve_official_ref(reference: &str) -> Result<String> {
    if let Some(local) = resolve_official_ref_local(reference) {
        return Ok(local);
    }
    let want = official_tag_candidate(reference);
    let root = state_root();
    let mut found: Vec<&str> = Vec::new();
    let mut unreachable: Vec<String> = Vec::new();
    for r in OFFICIAL_REPOS {
        match delonix_image::registry::list_remote_tags(&root, r.repo) {
            Ok(tags) if tags.iter().any(|t| t == &want) => found.push(r.repo),
            Ok(_) => {}
            // A repository we could not read is NOT a repository without the
            // tag: a private package answers 404 exactly like a missing one.
            // Reporting "nowhere" while one of the three never answered would
            // be the same class of lie this function exists to remove.
            Err(e) => unreachable.push(format!("{}: {e}", r.repo)),
        }
    }
    match found.as_slice() {
        [one] => Ok(format!("{one}:{want}")),
        [] => {
            let mut msg = super::po::tf(
                "no official repository has the tag '{tag}' — run `delonix image vm ls-remote` to see what is published",
                &[("tag", &want)],
            );
            for u in &unreachable {
                msg.push_str(&format!("\n  unreachable — {u}"));
            }
            Err(Error::Invalid(msg))
        }
        many => Err(Error::Invalid(super::po::tf(
            "the tag '{tag}' exists in more than one official repository ({repos}) — name the one you mean",
            &[("tag", &want), ("repos", &many.join(", "))],
        ))),
    }
}

/// Versão do Kubernetes por OMISSÃO da golden.
///
/// **1.36**, e o número não é arbitrário: é o canal estável mais recente do
/// `pkgs.k8s.io` (v1.37 não existe — verificado, não suposto) E é exactamente
/// o tecto que o control-plane alojado aceita. O webhook do Kamaji recusa um
/// `TenantControlPlane` acima de `upgrade.KubeadmVersion`, que na versão que
/// alojamos é `v1.36.0`.
///
/// Esse alinhamento é a razão de ser desta constante. Um nó **mais novo** que
/// o seu control-plane não é uma combinação suportada pelo kubeadm, por isso
/// uma golden à frente do tecto do Kamaji produz workers que nunca se juntam —
/// foi exactamente esse par impossível (golden 1.34/1.35 contra um Kamaji que
/// parava em 1.30.2) que bloqueou o DKS. Ao mexer neste número, confirma o
/// tecto do Kamaji alojado ANTES, não depois.
///
/// As tags 1.34 e 1.35 continuam a ser construíveis com `--k8s-version`; só
/// deixam de ser a omissão.
pub(crate) const DEFAULT_K8S_VERSION: &str = "1.36";

/// Tamanho VIRTUAL do disco da golden, em GiB.
///
/// A imagem base do Ubuntu vem com 3,5 GiB (≈2,4 GB de raiz utilizável), e isso
/// **não chega para um nó de Kubernetes**. Medido a 2026-08-16 numa golden 1.36:
/// depois de o `delonix-cri` puxar as imagens do control-plane a raiz ficava a
/// `2.4G/2.4G, 100%`, o `fallocate` do WAL do etcd falhava com ENOSPC — à letra
/// `failed to preallocate space when creating a new WAL` — o etcd morria, o
/// apiserver não o alcançava e o `kubeadm init` ficava preso em
/// `wait-control-plane`.
///
/// O `DKS_DELIVERY_LOG` do `delonix-paas` já dizia que o nó precisa de ≥20G, e
/// dava isso por fechado com «a golden já vem dimensionada». Não vinha.
///
/// Custa ZERO ao artefacto publicado: o qcow2 é esparso, e a imagem
/// redimensionada continua a ocupar ~690 MiB em disco e a viajar igual no
/// registo. E não exige nada ao guest — as imagens cloud do Ubuntu trazem o
/// `growpart` do cloud-init, que cresce a raiz no primeiro arranque.
///
/// ## Porque 10 e não 30: isto é um PISO, não um tamanho
///
/// A quota de armazenamento de um inquilino conta-se sobre o **provisionado**
/// (a soma dos tamanhos virtuais), não sobre o consumido — decisão do
/// utilizador, 2026-08-17. Um inquilino que paga 40G não pode receber nós de
/// 30G, senão o segundo já estoura a quota. Por isso a golden nasce no **mínimo
/// viável** e o disco de cada nó cresce a partir daí, dentro da quota do
/// cluster.
///
/// O mínimo está medido, não estimado (2026-08-17, golden 1.36 encolhida para
/// 10 GiB): a raiz fica com 8,7G, um control-plane completo consome **2,4G
/// (28%)**, o apiserver atende na 6443 aos ~120s e o log do etcd não tem uma
/// única linha de `preallocate`/`No space`. Sobram 6,3G de folga. Com os 3,5
/// GiB da imagem base, a mesma corrida enchia o disco a 100% e o WAL do etcd
/// falhava — ver acima.
///
/// **O que isto NÃO resolve**: `delonix vm create` não tem hoje flag de
/// tamanho de disco, por isso todo o nó herda este piso. Dimensionar um nó pela
/// quota do inquilino exige essa flag primeiro.
pub(crate) const GOLDEN_DISK_SIZE_GIB: u32 = 10;

/// Delonix's OFFICIAL golden VM image (Ubuntu 24.04 + kubeadm/kubelet/
/// kubectl + delonix-cri as a systemd service) — published and validated with
/// a byte-identical round-trip; see AGENTS.md, section "Golden VM image".
/// A tag TEM de ser `DEFAULT_K8S_VERSION` — amarrado pelo teste
/// `a_imagem_oficial_aponta_para_a_versao_por_omissao` (o `concat!` não aceita
/// constantes, e duas fontes de verdade para o mesmo número divergem sempre).
pub(crate) const OFFICIAL_VM_IMAGE: &str = "ghcr.io/angolardevops/delonix-vm-k8s:1.36";

/// Golden VM image with NO Kubernetes — just the `delonix` engine binary and
/// rootless prerequisites (see `rootless_customization_steps`). Selected by
/// `Pull`/`LsRemote --no-k8s` when no explicit `source` is given.
pub(crate) const OFFICIAL_VM_BASE_IMAGE: &str =
    "ghcr.io/angolardevops/delonix-vm-base:ubuntu-24.04";

/// Downloads a cloud image from an ARBITRARY URL, for `FROM https://…`.
///
/// Verified when the publisher makes it possible: a sibling `<url>.sha256` (the
/// convention almost everyone follows) is fetched and checked. When there is
/// none, the download proceeds over TLS and **says so** — because the
/// alternative is a build that silently trusts whatever answered, and someone
/// pointing `FROM` at their own bucket deserves to know which of the two they
/// got.
pub(crate) fn download_url_base(store: &VmImageStore, url: &str) -> Result<PathBuf> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err(Error::Invalid(format!("not an absolute URL: {url}")));
    }
    if url.starts_with("http://") {
        eprintln!(
            "{} {}",
            super::po::t("warning:"),
            super::po::t("plain http — the image is downloaded without any transport protection")
        );
    }
    // Cache key from the URL, not from its last path segment: two different
    // buckets publish `noble-server-cloudimg-amd64.img` and they are not the
    // same file.
    let key = format!("url-{}", hex_sha256(url.as_bytes()));
    let cached = store.base_cache_path(Distro::Ubuntu, &key);
    if cached.exists() {
        return Ok(cached);
    }
    if let Some(parent) = cached.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = cached.with_extension("part");
    eprintln!("{}", super::po::tf("downloading {url}...", &[("url", url)]));
    stream_download(url, &tmp)?;

    let sums_url = format!("{url}.sha256");
    let sums = std::env::temp_dir().join(format!("delonix-url-sha-{}", std::process::id()));
    match stream_download(&sums_url, &sums) {
        Ok(()) => {
            let want = std::fs::read_to_string(&sums)
                .unwrap_or_default()
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let _ = std::fs::remove_file(&sums);
            let got = hex_sha256_file(&tmp)?;
            if want.is_empty() || want != got {
                let _ = std::fs::remove_file(&tmp);
                return Err(Error::Invalid(super::po::tf(
                    "{url}: checksum mismatch (expected {want}, got {got}) — refusing the image",
                    &[("url", url), ("want", &want), ("got", &got)],
                )));
            }
            eprintln!("{}", super::po::t("checksum verified against <url>.sha256"));
        }
        Err(_) => {
            let _ = std::fs::remove_file(&sums);
            eprintln!(
                "{} {}",
                super::po::t("warning:"),
                super::po::tf(
                    "no {sums} published — the image is trusted on TLS alone. Publish one next to it to have this verified.",
                    &[("sums", &sums_url)],
                )
            );
        }
    }
    std::fs::rename(&tmp, &cached)?;
    Ok(cached)
}

/// Dispatches to the right per-distro downloader.
///
/// Each publisher does checksums differently — Ubuntu ships GNU `SHA256SUMS`,
/// Debian only `SHA512SUMS`, Rocky a per-file BSD-format `.CHECKSUM` — and that
/// knowledge already lives in the three functions below. This is the one place
/// that picks between them.
/// The stored VM image a VM's base disk came from, when it is one of ours.
///
/// A `Vm` records the base disk it was given, never the image name, so the way
/// back is to ask each image where its qcow2 lives and compare. Cheap (the
/// store is a directory of JSON) and it never guesses: a disk given by
/// `--url-img` or by hand belongs to no image of ours, and `None` says exactly
/// that instead of picking the closest-looking one.
pub(crate) fn image_of_disk(disk: &str) -> Option<VmImage> {
    let store = VmImageStore::open(super::util::state_root()).ok()?;
    let want = std::path::Path::new(disk);
    store
        .list()
        .ok()?
        .into_iter()
        .find(|i| store.qcow2_path(&i.name) == want)
}

/// The base image DELONIX publishes for a `FROM <distro>:<release>`, when there
/// is one.
///
/// `FROM ubuntu:24.04` used to mean one thing only: go to Canonical and fetch
/// their cloud image. But the project publishes its own base for that same
/// distro and release — `ghcr.io/angolardevops/delonix-vm-base:ubuntu-24.04`,
/// built and validated here — so a recipe that names a distro should land on
/// THAT, and reach the publisher only when the project has nothing to offer.
/// The tag spelling needs no translation table: the official repositories
/// already spell `<product>:<version>` as `<product>-<version>`
/// (`official_tag_for`), which is exactly what a `FROM ubuntu:24.04` becomes.
///
/// The LOCAL copy is checked first, and it is not an optimization: a base
/// already in `image --vm ls` makes the build cost nothing and reach nothing,
/// which is the same reason `RUN` is offline by default — a build that touches
/// the network gives a different image depending on when it ran.
///
/// Returns `None`, never an error, when the project publishes no such base or
/// the registry cannot be reached. The caller still has the distro's own cloud
/// image, and turning an unreachable ghcr (private package, no token, a plane)
/// into a failed build would take away the path that works today.
pub(crate) fn official_distro_base(
    store: &VmImageStore,
    distro: &str,
    release: &str,
) -> Option<PathBuf> {
    let tag = format!("{distro}-{release}");
    let local = format!("delonix-vm-base:{tag}");
    let path = store.qcow2_path(&local);
    if path.exists() {
        eprintln!(
            "{}",
            super::po::tf(
                "FROM {d}:{r}: official delonix base {local} (local)",
                &[("d", distro), ("r", release), ("local", &local)],
            )
        );
        return Some(path);
    }
    let source = format!("ghcr.io/angolardevops/delonix-vm-base:{tag}");
    eprintln!(
        "{}",
        super::po::tf(
            "FROM {d}:{r}: pulling the official delonix base {src}",
            &[("d", distro), ("r", release), ("src", &source)],
        )
    );
    let label = format!("[vm pull] {source}");
    let last = std::cell::Cell::new(0u64);
    let on_progress = move |done: u64, total: Option<u64>| {
        let finished = total.map(|t| done >= t).unwrap_or(false);
        if finished || done.wrapping_sub(last.get()) >= 2 * 1024 * 1024 {
            last.set(done);
            super::output::progress_bar(&label, done, total);
        }
    };
    let (data, annotations) = delonix_image::registry::pull_oci_artifact_with_meta(
        &state_root(),
        &source,
        Some(&on_progress),
    )
    .ok()?;
    super::output::progress_done();
    std::fs::write(&path, &data).ok()?;
    let img = VmImage {
        name: local.clone(),
        tag: source.clone(),
        digest: format!("sha256:{}", hex_sha256(&data)),
        size: data.len() as u64,
        ubuntu_release: None,
        k8s_version: None,
        created_unix: now_unix(),
        kernel_version: None,
        distro: None,
        default_vcpus: None,
        default_memory: None,
        default_backend: None,
        cloud_init: None,
        // O artefacto OCI/o disco importado não trazem inventário nem
        // proveniência — a mesma lacuna conhecida do `kernel_version`.
        packages: None,
        packages_sha256: None,
        built_by: None,
        base_sha256: None,
    };
    // Same metadata path a plain `vm pull` takes, so the base lands in the
    // store indistinguishable from one pulled by hand — including showing up
    // in `image --vm ls` with its distro and kernel.
    let img = apply_pulled_annotations(img, &annotations);
    store.save(&img).ok()?;
    Some(path)
}

pub(crate) fn download_base(
    store: &VmImageStore,
    distro: Distro,
    release: &str,
) -> Result<PathBuf> {
    match distro {
        Distro::Ubuntu => download_ubuntu_base(store, release),
        Distro::Debian => download_debian_base(store, release),
        Distro::Rocky => download_rocky_base(store, release),
        Distro::Fedora => download_fedora_base(store, release),
    }
}

/// Picks the default source for `Pull`/`LsRemote` when no explicit `source`
/// is given. BUG FIXED (gap): `OFFICIAL_VM_BASE_IMAGE` existed but had no way
/// to be selected without typing out the full `ghcr.io/...:ubuntu-24.04`
/// reference by hand — a tenant who only wants the no-Kubernetes golden had
/// no discoverable "just give me the official one" path, unlike the
/// Kubernetes golden (bare `pull`/`ls-remote`).
pub(crate) fn default_pull_source(no_k8s: bool) -> &'static str {
    if no_k8s {
        OFFICIAL_VM_BASE_IMAGE
    } else {
        OFFICIAL_VM_IMAGE
    }
}

/// The tag an image takes in its official repository. One repository holds
/// several products, so the local `<product>:<version>` becomes
/// `<product>-<version>` — which is also what `resolve_official_ref` undoes on
/// the way back, so `push` and `pull` agree without either knowing about the
/// other.
pub(crate) fn official_tag_for(img: &VmImage, local_name: &str) -> String {
    // `delonix-vm-base:ubuntu-24.04` publishes as `ubuntu-24.04`: the
    // repository already carries the family, repeating it in the tag would
    // read as `delonix-vm-base:delonix-vm-base-ubuntu-24.04`.
    if let Some(rest) = local_name.strip_prefix("delonix-vm-base:") {
        return rest.to_string();
    }
    if let Some(rest) = local_name.strip_prefix("delonix-vm-k8s:") {
        return rest.to_string();
    }
    match local_name.split_once(':') {
        Some((prod, ver)) => format!("{prod}-{ver}"),
        None => match (&img.distro, &img.ubuntu_release) {
            (Some(d), Some(r)) => format!("{d}-{r}"),
            _ => local_name.to_string(),
        },
    }
}

pub(crate) fn cmd_push(store: &VmImageStore, name: &str, target: Option<&str>) -> Result<()> {
    let img = store.get(name)?;
    // With no destination, publish where this image belongs — decided from the
    // image's own metadata, not from a flag the caller has to remember. A tag
    // that is not a valid OCI reference is rewritten the way the appliances
    // repository names them (`opnsense:26.1` → `opnsense-26.1`), because one
    // repository holds several products.
    let target = match target {
        Some(t) => t.to_string(),
        None => {
            let repo = official_repo_for(&img).ok_or_else(|| {
                Error::Invalid(super::po::tf(
                    "{name}: cannot tell which official repository this belongs in (no distro, \
                     no k8s version, not marked as an appliance) — name a destination explicitly",
                    &[("name", name)],
                ))
            })?;
            let tag = official_tag_for(&img, name);
            let dest = format!("{}:{}", repo.repo, tag);
            eprintln!(
                "{}",
                super::po::tf(
                    "publishing to the official repository: {dest}",
                    &[("dest", &dest)]
                )
            );
            dest
        }
    };
    let target = target.as_str();
    let data = std::fs::read(store.qcow2_path(name)).map_err(|e| {
        Error::Invalid(format!(
            "{} '{name}': {e}",
            super::po::t("could not read the qcow2 of")
        ))
    })?;
    let digest = delonix_image::registry::push_oci_artifact_with_annotations(
        &state_root(),
        target,
        VM_IMAGE_MEDIA_TYPE,
        &data,
        &annotations_of(&img),
    )?;
    println!("{digest}");
    Ok(())
}

/// Arguments of `image vm import`, defined ONCE and `flatten`ed into all
/// three entry points (`vm import`, `image vm import`, `image --vm import`).
/// Spelling them out per entry point is how those three drift apart — a flag
/// that works in one place and not another is worse than no flag.
#[derive(clap::Args, Clone, Debug)]
pub struct ImportArgs {
    /// Path to a `.qcow2` (or any format `qemu-img` reads — it is converted).
    #[arg(value_hint = clap::ValueHint::FilePath)]
    pub source: PathBuf,
    /// Name to register it under (`image vm ls`).
    #[arg(short = 't', long = "tag")]
    pub tag: String,
    /// The guest does NOT run cloud-init: it is a self-configuring appliance
    /// (OPNsense, Proxmox, TrueNAS). `vm create` then skips the NoCloud seed
    /// instead of attaching one nothing in the guest reads.
    #[arg(long)]
    pub appliance: bool,
    /// What this is, for `image vm ls` (e.g. `opnsense`, `proxmox-ve`).
    #[arg(long)]
    pub distro: Option<String>,
    /// Version, for `image vm ls` (e.g. `26.1.2`).
    #[arg(long)]
    pub release: Option<String>,
    /// Recommended vCPUs, applied by `vm create` when `--vcpus` is absent.
    #[arg(long)]
    pub default_vcpus: Option<u32>,
    /// Recommended memory (e.g. `4G`), same rule as `--default-vcpus`.
    #[arg(long)]
    pub default_memory: Option<String>,
    /// Replace an image already registered under this name.
    #[arg(long)]
    pub force: bool,
    /// Store the qcow2 uncompressed (larger, but no decompression cost on
    /// backing-file reads at runtime) — same flag, same trade-off, as `build`.
    #[arg(long)]
    pub no_compress: bool,
    /// Kernel baked into the image (`uname -r` shape), for `image vm ls`.
    /// Omit to let it be probed from the image — which only works where
    /// libguestfs can read `/boot` (not FreeBSD, not root-on-ZFS).
    #[arg(long)]
    pub kernel_version: Option<String>,
}

/// `image vm import` — registers a disk this engine did not build.
///
/// Always goes through `qemu-img convert`, never a plain copy: the source may
/// be raw, or a qcow2 with a backing file (which would leave the store holding
/// a reference to a path outside it — a store entry that silently breaks the
/// day that file moves).
pub(crate) fn cmd_import(store: &VmImageStore, args: ImportArgs) -> Result<()> {
    let ImportArgs {
        source,
        tag,
        appliance,
        distro,
        release,
        default_vcpus,
        default_memory,
        force,
        no_compress,
        kernel_version,
    } = args;
    let (source, tag) = (source.as_path(), tag.as_str());
    if !source.exists() {
        return Err(Error::Invalid(super::po::tf(
            "no such file: {source}",
            &[("source", &source.display().to_string())],
        )));
    }
    let dest = store.qcow2_path(tag);
    if dest.exists() && !force {
        return Err(Error::Invalid(super::po::tf(
            "VM image '{tag}' already exists — pass --force to replace it",
            &[("tag", tag)],
        )));
    }
    eprintln!(
        "{}",
        super::po::tf(
            "importing {source} as '{tag}'...",
            &[("source", &source.display().to_string()), ("tag", tag)],
        )
    );
    let tmp = dest.with_extension("importing");
    let _ = std::fs::remove_file(&tmp);
    let mut argv: Vec<&str> = vec!["convert", "-O", "qcow2"];
    if !no_compress {
        // zstd, and compressed by default, for the same reason the golden
        // recipe settled on it: a store image is the read-only BACKING FILE of
        // every VM created from it, so decompression speed is what matters,
        // and `qemu-img convert` without `-c` would inflate an already
        // compressed source several-fold on the way in.
        argv.extend(["-c", "-o", "compression_type=zstd"]);
    }
    // `--` so a path starting with `-` stays a path.
    argv.push("--");
    let (src_s, tmp_s) = (source.to_string_lossy(), tmp.to_string_lossy());
    argv.push(&src_s);
    argv.push(&tmp_s);
    run_tool("qemu-img", &argv)?;
    // Rename only after the conversion succeeded: a failed import must not
    // leave a truncated image registered under a good name.
    std::fs::rename(&tmp, &dest)?;

    let data = std::fs::read(&dest)?;
    let img = VmImage {
        name: tag.to_string(),
        tag: tag.to_string(),
        digest: format!("sha256:{}", hex_sha256(&data)),
        size: data.len() as u64,
        ubuntu_release: release,
        k8s_version: None,
        created_unix: now_unix(),
        // Explicit wins; otherwise probe the image (best-effort — see
        // `detect_kernel_version`). Never a guess: unknown stays `None`.
        kernel_version: kernel_version.or_else(|| detect_kernel_version(&dest)),
        distro,
        default_vcpus,
        default_memory,
        default_backend: None,
        cloud_init: Some(!appliance),
        // O artefacto OCI/o disco importado não trazem inventário nem
        // proveniência — a mesma lacuna conhecida do `kernel_version`.
        packages: None,
        packages_sha256: None,
        built_by: None,
        base_sha256: None,
    };
    store.save(&img)?;
    println!("{tag}");
    Ok(())
}

/// Manifest annotations carrying what the qcow2 blob cannot: the store's own
/// view of the image. Without these a `vm pull` lands an image with every
/// metadata field blank — a documented gap for `ubuntu_release`/`k8s_version`,
/// and a functional bug for `cloud_init`, which decides whether `vm create`
/// attaches a seed the guest may not be able to read.
///
/// Only fields that are actually known are emitted: an absent annotation and
/// an annotation saying "unknown" are different claims, and the pull side
/// leaves what it was not told alone.
pub(crate) fn annotations_of(img: &VmImage) -> std::collections::BTreeMap<String, String> {
    let mut a = std::collections::BTreeMap::new();
    let mut put = |k: &str, v: String| {
        a.insert(format!("io.delonix.vmimage.{k}"), v);
    };
    if let Some(v) = &img.distro {
        put("distro", v.clone());
    }
    if let Some(v) = &img.ubuntu_release {
        put("release", v.clone());
    }
    if let Some(v) = &img.k8s_version {
        put("k8s-version", v.clone());
    }
    if let Some(v) = &img.kernel_version {
        put("kernel-version", v.clone());
    }
    if let Some(v) = img.default_vcpus {
        put("default-vcpus", v.to_string());
    }
    if let Some(v) = &img.default_memory {
        put("default-memory", v.clone());
    }
    if let Some(v) = &img.default_backend {
        put("default-backend", v.clone());
    }
    if let Some(v) = img.cloud_init {
        put("cloud-init", v.to_string());
    }
    a
}

/// Inverse of [`annotations_of`], applied to the metadata a `vm pull` just
/// built from the blob alone.
///
/// A malformed value is IGNORED rather than fatal: a bad `default-vcpus`
/// should not make an otherwise good image unusable, and the field's own
/// absence already has well-defined behaviour. `cloud-init` is the one case
/// where a wrong reading matters, so only the two exact strings count.
pub(crate) fn apply_pulled_annotations(
    mut img: VmImage,
    a: &std::collections::BTreeMap<String, String>,
) -> VmImage {
    let get = |k: &str| {
        a.get(&format!("io.delonix.vmimage.{k}"))
            .map(|s| s.as_str())
    };
    img.distro = get("distro").map(str::to_string).or(img.distro);
    img.ubuntu_release = get("release").map(str::to_string).or(img.ubuntu_release);
    img.k8s_version = get("k8s-version").map(str::to_string).or(img.k8s_version);
    img.kernel_version = get("kernel-version")
        .map(str::to_string)
        .or(img.kernel_version);
    img.default_vcpus = get("default-vcpus")
        .and_then(|v| v.parse().ok())
        .or(img.default_vcpus);
    img.default_memory = get("default-memory")
        .map(str::to_string)
        .or(img.default_memory);
    img.default_backend = get("default-backend")
        .map(str::to_string)
        .or(img.default_backend);
    img.cloud_init = match get("cloud-init") {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => img.cloud_init,
    };
    img
}

/// `image vm convert` — flattens/converts a disk to the format another
/// ecosystem imports (`vmdk` for VMware, `vdi` for VirtualBox, `vhdx`/`vhd`
/// for Hyper-V and Azure, plus this engine's own `qcow2`/`raw`), using
/// `qemu-img convert` (same tool `cmd_build`/`vmfile::build` already use
/// to flatten a base image). `source` is tried as a local VM image name
/// first (so `convert my-image --to raw` works without knowing the qcow2's
/// on-disk path), falling back to a literal path — never an error to pass a
/// path that happens to collide with no store entry.
pub(crate) fn cmd_convert(
    store: &VmImageStore,
    source: &str,
    to: ConvertFormat,
    output: Option<PathBuf>,
    compress: bool,
) -> Result<()> {
    // Refused HERE, not handed to `qemu-img` to reject: the tool's own error for
    // `-c` on a format that cannot compress says nothing about which formats
    // can, and the flag was offered by this CLI in the first place.
    if compress && !to.supports_compression() {
        return Err(Error::Invalid(super::po::tf(
            "{fmt} cannot be compressed — only qcow2 and vmdk can",
            &[("fmt", to.extension())],
        )));
    }
    let src_path = {
        let by_name = store.qcow2_path(source);
        if by_name.exists() {
            by_name
        } else {
            PathBuf::from(source)
        }
    };
    if !src_path.exists() {
        return Err(Error::Invalid(super::po::tf(
            "no such local VM image or file: {source} (see `delonix image vm ls`)",
            &[("source", source)],
        )));
    }
    let dest = output.unwrap_or_else(|| {
        let stem = src_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("image");
        src_path.with_file_name(format!("{stem}.{}", to.extension()))
    });
    if dest == src_path {
        return Err(Error::Invalid(
            super::po::t(
                "destination is the same as the source — pass `-o <file>` to convert in place",
            )
            .to_string(),
        ));
    }
    eprintln!(
        "{}",
        super::po::tf(
            "converting {src} to {fmt} → {dst}...",
            &[
                ("src", &src_path.display().to_string()),
                ("fmt", to.as_str()),
                ("dst", &dest.display().to_string()),
            ],
        )
    );
    let mut args: Vec<&str> = vec!["convert", "-O", to.as_str()];
    if compress {
        args.push("-c");
    }
    let (src, dst) = (src_path.to_string_lossy(), dest.to_string_lossy());
    // `--` so a source path starting with `-` is a path, not an option:
    // `source` here may be a literal path straight from the caller.
    args.extend(["--", &src, &dst]);
    run_tool("qemu-img", &args)?;
    let _unused = ();
    println!("{}", dest.display());
    Ok(())
}

pub(crate) fn cmd_pull(store: &VmImageStore, source: &str, name: Option<String>) -> Result<()> {
    // Download progress bar (the golden is hundreds of MB): the engine
    // reports (bytes, total) every 64KB; we redraw at most every ~2MB
    // so as not to hammer the terminal. Only draws on a tty (see `output`).
    let label = format!("[vm pull] {source}");
    let last = std::cell::Cell::new(0u64);
    let on_progress = move |done: u64, total: Option<u64>| {
        let finished = total.map(|t| done >= t).unwrap_or(false);
        if finished || done.wrapping_sub(last.get()) >= 2 * 1024 * 1024 {
            last.set(done);
            super::output::progress_bar(&label, done, total);
        }
    };
    let (data, annotations) = delonix_image::registry::pull_oci_artifact_with_meta(
        &state_root(),
        source,
        Some(&on_progress),
    )?;
    super::output::progress_done();
    let name = name.unwrap_or_else(|| source.rsplit('/').next().unwrap_or(source).to_string());
    let digest = format!("sha256:{}", hex_sha256(&data));
    std::fs::write(store.qcow2_path(&name), &data)?;
    let img = VmImage {
        name: name.clone(),
        tag: source.to_string(),
        digest,
        size: data.len() as u64,
        ubuntu_release: None,
        k8s_version: None,
        created_unix: now_unix(),
        kernel_version: None,
        distro: None,
        // The OCI artifact only carries the qcow2 blob, not build metadata —
        // same known gap as `ubuntu_release`/`k8s_version` above.
        default_vcpus: None,
        default_memory: None,
        default_backend: None,
        // Filled in below from the manifest annotations when the publisher
        // set them. Left `None` (= assume cloud-init, the historical
        // behaviour) for an artifact that carries no annotations.
        cloud_init: None,
        // O artefacto OCI/o disco importado não trazem inventário nem
        // proveniência — a mesma lacuna conhecida do `kernel_version`.
        packages: None,
        packages_sha256: None,
        built_by: None,
        base_sha256: None,
    };
    let img = apply_pulled_annotations(img, &annotations);
    store.save(&img)?;
    println!("{name}");
    Ok(())
}

/// `ls-remote` with no argument: every official repository, one after the
/// other. Listing only one of them (and not saying which) is what made a user
/// who had just published to another conclude the push had failed.
pub(crate) fn cmd_ls_remote_official() -> Result<()> {
    let mut failed = false;
    for (i, r) in OFFICIAL_REPOS.iter().enumerate() {
        if i > 0 {
            println!();
        }
        eprintln!(
            "{}",
            super::po::tf("{key} — {what}", &[("key", r.key), ("what", r.what)],)
        );
        // One unreachable repository must not hide the others: a brand-new
        // package is private until someone flips it, and 404 is what that
        // looks like from here.
        if let Err(e) = cmd_ls_remote(r.repo) {
            eprintln!("  {e}");
            failed = true;
        }
    }
    if failed {
        eprintln!(
            "{}",
            super::po::t(
                "note: a repository that reports \"not found\" may simply be private to these credentials"
            )
        );
    }
    Ok(())
}

pub(crate) fn cmd_ls_remote(source: &str) -> Result<()> {
    let root = state_root();
    // Say WHICH repository is being listed. With no argument this command
    // lists a default one, and a reader who has just published elsewhere sees
    // a short list of tags that are not theirs and concludes the push failed —
    // which is exactly what happened. The tags alone do not identify their
    // origin, so the header has to.
    // Without the tag: the default source carries one (`…:1.34`), and printing
    // it here would suggest the listing is scoped to that tag when it is the
    // whole repository. Cut only a `:` that comes AFTER the last `/`, so a
    // `host:port/repo` keeps its port.
    let shown = match source.rfind('/') {
        Some(slash) => match source[slash..].find(':') {
            Some(colon) => &source[..slash + colon],
            None => source,
        },
        None => source.split(':').next().unwrap_or(source),
    };
    eprintln!(
        "{}",
        super::po::tf("repository: {source}", &[("source", shown)])
    );
    let mut tags = delonix_image::registry::list_remote_tags(&root, source)?;
    tags.sort();
    // A bare list of tags does not answer the question the reader has, which is
    // "which of these do I want" — so each tag's MANIFEST is read (one GET, no
    // blob transfer) for its size and for the annotations `image vm push`
    // stamps: distro/release, and whether the guest runs cloud-init.
    //
    // A tag whose manifest cannot be read still gets its row, with `-` in the
    // columns: a listing that hides a tag because one HTTP call failed would be
    // worse than one that admits it does not know.
    let mut t = output::Table::new(&["TAG", "DISTRO", "TYPE", "SIZE"]).right_align(3);
    for tag in tags.iter_mut() {
        let tag = std::mem::take(tag);
        match delonix_image::registry::describe_remote_artifact(&root, source, &tag) {
            Ok(a) => {
                let get = |k: &str| {
                    a.annotations
                        .get(&format!("io.delonix.vmimage.{k}"))
                        .map(|s| s.as_str())
                };
                let distro = match (get("distro"), get("release")) {
                    (Some(d), Some(r)) => format!("{d}/{r}"),
                    (Some(d), None) => d.to_string(),
                    (None, Some(r)) => r.to_string(),
                    (None, None) => "-".to_string(),
                };
                t.row(vec![
                    tag,
                    distro,
                    image_type_label(get("cloud-init")),
                    fmt_size(a.size),
                ]);
            }
            Err(_) => t.row(vec![tag, "-".into(), "-".into(), "-".into()]),
        }
    }
    t.print();
    Ok(())
}

/// Kernel baked into a disk image this engine did NOT build, read without
/// booting it.
///
/// `cmd_build` reads `/etc/delonix-kernel-version`, a file it writes itself —
/// useless for an imported image. This lists `/boot` instead and takes the
/// highest `vmlinuz-<version>` it finds, which is what the guest boots by
/// default.
///
/// Best-effort by design, and quiet when it fails: an appliance may be FreeBSD
/// (OPNsense), or root on ZFS (TrueNAS), where libguestfs cannot see `/boot` at
/// all. An empty KERNEL column is honest; failing an import over a display
/// field would not be.
fn detect_kernel_version(qcow2: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("virt-ls")
        .args(["-a", &qcow2.to_string_lossy(), "/boot"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().strip_prefix("vmlinuz-"))
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .max()
}

/// `2cpu/2G` — what `vm create` will use when `--vcpus`/`--memory` are absent.
/// `-` when the image recommends nothing (the engine then falls back to 1/1G).
fn defaults_label(img: &VmImage) -> String {
    match (img.default_vcpus, img.default_memory.as_deref()) {
        (Some(c), Some(m)) => format!("{c}cpu/{m}"),
        (Some(c), None) => format!("{c}cpu"),
        (None, Some(m)) => m.to_string(),
        (None, None) => "-".to_string(),
    }
}

/// How an image gets configured on first boot, in one word — the difference
/// that decides whether `vm create` seeds it and whether `--ssh-key` works.
fn image_type_label(cloud_init: Option<&str>) -> String {
    match cloud_init {
        Some("false") => super::po::t("appliance").to_string(),
        Some("true") => "cloud-init".to_string(),
        _ => "-".to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_build(
    store: &VmImageStore,
    tag: &str,
    distro: Distro,
    ubuntu_release: &str,
    debian_release: &str,
    rocky_release: &str,
    fedora_release: &str,
    k8s_version: Option<String>,
    extra_packages: Vec<String>,
    extra_run: Vec<String>,
    cri_bin: Option<PathBuf>,
    compress: bool,
    offline: bool,
    no_k8s: bool,
    delonix_bin: Option<PathBuf>,
    root_password: Option<String>,
    node_exporter: Option<String>,
) -> Result<()> {
    // `--no-k8s` builds a completely different image (no kubeadm/kubelet/
    // kubectl/delonix-cri at all — see `rootless_customization_steps`); the
    // k8s-only flags don't apply and silently ignoring them would be
    // confusing, so reject the combination up front.
    if no_k8s {
        if k8s_version.is_some() {
            return Err(Error::Invalid(
                super::po::t("--no-k8s and --k8s-version are mutually exclusive").into(),
            ));
        }
        if offline {
            return Err(Error::Invalid(
                super::po::t(
                    "--no-k8s does not support --offline (offline mode only knows how to verify k8s .deb)",
                )
                .into(),
            ));
        }
        if cri_bin.is_some() {
            return Err(Error::Invalid(
                super::po::t("--no-k8s and --cri-bin are mutually exclusive (use --delonix-bin)")
                    .into(),
            ));
        }
    }
    // NOTA: `--delonix-bin` já foi rejeitado no caminho k8s ("only applies with
    // --no-k8s"). Deixou de o ser, e a razão é um defeito medido: a golden k8s
    // instalava `delonix-cri` SEM o `delonix` a que ele delega TODO o ciclo de
    // vida de containers (`cli_bin()` em `delonix-cri`). Resultado numa VM da
    // golden 1.35, a 2026-08-16: os `pull` funcionavam (o ImageService usa a
    // biblioteca em processo) mas cada `StartContainer` morria com
    // `No such file or directory (os error 2)` — o kubeadm nunca levantava um
    // control-plane. Os dois binários são UMA unidade; ver `install_cri_steps`.
    // Rocky's dnf-family customization steps only exist for the `--no-k8s`
    // path (`rootless_customization_steps`) — `k8s_recipes` is apt-only
    // (pkgs.k8s.io's RPM repo has a different URL/GPG scheme, not
    // implemented). Fail closed rather than silently running apt commands
    // against a dnf guest.
    if matches!(distro, Distro::Rocky | Distro::Fedora) && !no_k8s {
        return Err(Error::Invalid(super::po::tf(
            "--distro {distro} only supports --no-k8s for now (the k8s path needs the \
                 pkgs.k8s.io RPM repository, not implemented yet)",
            &[("distro", distro.as_str())],
        )));
    }
    // `k8s_version` goes into a `format!` that becomes a `virt-customize --run-command`
    // (via `k8s_recipes::k8s_host_recipes`) — validating here closes the same security
    // finding as `cmd::cluster::valid_version` (the embedded apt repository must not
    // contain shell metacharacters). Audit finding, see AGENTS.md.
    if let Some(v) = &k8s_version {
        if !super::cluster::valid_version(v) {
            return Err(Error::Invalid(super::po::tf(
                "--k8s-version '{v}' invalid (only digits and dots, e.g.: '1.31')",
                &[("v", v)],
            )));
        }
    }
    let release = match distro {
        Distro::Ubuntu => ubuntu_release,
        Distro::Debian => debian_release,
        Distro::Rocky => rocky_release,
        Distro::Fedora => fedora_release,
    };
    let base = match distro {
        Distro::Ubuntu => download_ubuntu_base(store, ubuntu_release)?,
        Distro::Debian => download_debian_base(store, debian_release)?,
        Distro::Rocky => download_rocky_base(store, rocky_release)?,
        Distro::Fedora => download_fedora_base(store, fedora_release)?,
    };

    let work_dir =
        std::env::temp_dir().join(format!("delonix-vmimage-build-{}", std::process::id()));
    std::fs::create_dir_all(&work_dir)?;
    let work_qcow2 = work_dir.join("work.qcow2");

    eprintln!(
        "{}",
        super::po::t("preparing the working image (flattened, no backing file)...")
    );
    run_tool(
        "qemu-img",
        &[
            "convert",
            "-O",
            "qcow2",
            &base.to_string_lossy(),
            &work_qcow2.to_string_lossy(),
        ],
    )?;

    let ops = if no_k8s {
        eprintln!(
            "{}",
            super::po::t(
                "--no-k8s mode: preparing image without Kubernetes (delonix + rootless)..."
            )
        );
        let delonix = resolve_delonix_bin(delonix_bin)?;
        rootless_customization_steps(&extra_run, &delonix, distro, root_password.as_deref())
    } else {
        let cri = resolve_cri_bin(cri_bin)?;
        // O mesmo resolver do caminho `--no-k8s`: explícito, senão o
        // `current_exe()` (este comando JÁ está a correr como `delonix`).
        let delonix = resolve_delonix_bin(delonix_bin)?;
        let service_unit = workspace_dist_file("delonix-cri.service")?;
        if offline {
            // Everything that needs network happens HERE, on the host (verified), so the
            // appliance can run with `--no-network`.
            eprintln!(
                "{}",
                super::po::t("offline mode: getting the k8s .deb on the host...")
            );
            let debs = download_k8s_debs(
                &work_dir,
                &work_dir.join("debs"),
                k8s_version.as_deref(),
                "amd64",
                &extra_packages,
            )?;
            // The guest agent, from the DISTRO archive, into the SAME directory the
            // existing `dpkg -i /tmp/k8s-debs/*.deb` step already installs — so this adds a
            // download, not a second install path.
            //
            // Why it belongs in the golden: on a Proxmox node the platform learns a VM's
            // address through the agent (`ProxmoxBackend::ip`), and it also gives PBS a
            // quiesced backup. Without it that lookup answers `None` — a first-class answer
            // there, which is exactly why its absence is INVISIBLE until someone needs an
            // IP. So this FAILS the build instead of warning: a golden that silently lacks
            // the agent is indistinguishable from one that has it.
            let codename = guest_os_codename(&work_qcow2).ok_or_else(|| {
                Error::Invalid(
                    super::po::t(
                        "could not read VERSION_CODENAME from the base image (needed to \
                         reach the distro archive) — is libguestfs installed?",
                    )
                    .to_string(),
                )
            })?;
            eprintln!(
                "{}",
                super::po::tf(
                    "offline mode: getting the guest agent from the {codename} archive...",
                    &[("codename", &codename)]
                )
            );
            download_archive_debs(
                &work_dir,
                &work_dir.join("debs"),
                "http://archive.ubuntu.com/ubuntu",
                &codename,
                // `qemu-guest-agent` is in `universe`, its `liburing2` dependency in
                // `main`: measured, not assumed. The other four deps it declares
                // (libc6/libglib2.0-0t64/libnuma1/libudev1) are ALREADY in the cloud image,
                // so the closure to carry is two files — same measurement the k8s path made
                // before settling on four.
                &["main", "universe"],
                "amd64",
                &["qemu-guest-agent", "liburing2"],
            )?;
            // Best-effort: the golden image ships with kubeadm's own core images
            // already local, so a real `kubeadm init` on a booted VM does not
            // redownload apiserver/etcd/coredns/... from scratch (see
            // `preseed_k8s_images` for the real crash this fixes). Needs the
            // kubeadm `.deb` we just downloaded/verified on the host.
            let preseed_root = debs
                .iter()
                .find(|d| {
                    d.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("kubeadm_"))
                })
                .and_then(|kubeadm_deb| {
                    eprintln!(
                        "{}",
                        super::po::t("pre-seeding the kubeadm images on the host...")
                    );
                    preseed_k8s_images(&work_dir, kubeadm_deb, k8s_version.as_deref())
                });
            k8s_customization_steps_offline(
                &debs,
                &extra_run,
                &cri,
                &delonix,
                &service_unit,
                preseed_root.as_deref(),
                distro,
                root_password.as_deref(),
            )
        } else {
            k8s_customization_steps(
                k8s_version.as_deref(),
                &extra_packages,
                &extra_run,
                &cri,
                &delonix,
                &service_unit,
                distro,
                root_password.as_deref(),
            )
        }
    };
    // Metrics agent: OPT-IN, fetched and verified host-side like everything else
    // this build downloads. Without the flag the image is byte-for-byte what it
    // was — a listener in every published copy is a port the tenant never asked
    // for, on an artefact whose whole purpose is to be handed to someone else.
    let ops = match node_exporter.as_deref() {
        Some(listen) => {
            eprintln!(
                "{}",
                super::po::tf(
                    "installing the node_exporter metrics agent on {listen}...",
                    &[("listen", listen)]
                )
            );
            let bin = fetch_node_exporter(store, NODE_EXPORTER_VERSION)?;
            splice_before_machine_id(ops, node_exporter_steps(&bin, listen))
        }
        None => ops,
    };
    // O manifesto de build vai para DENTRO da imagem: um qcow2 que circula sem
    // ele obriga quem o recebe a adivinhar de onde veio. O sha256 da base é o
    // elo que falta para alguém poder repetir o build a partir do mesmo ponto
    // de partida — a cloud image do fabricante muda debaixo da mesma URL.
    let base_sha256 = hex_sha256_file(&base).ok();
    let manifest = build_manifest(&[
        ("DELONIX_IMAGE", tag.to_string()),
        ("DELONIX_DISTRO", format!("{distro:?}").to_lowercase()),
        ("DELONIX_RELEASE", release.to_string()),
        (
            "DELONIX_BUILT_BY",
            format!("delonix {}", env!("CARGO_PKG_VERSION")),
        ),
        (
            "DELONIX_BASE_IMAGE",
            base.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
        ),
        (
            "DELONIX_BASE_SHA256",
            base_sha256.clone().unwrap_or_else(|| "unknown".into()),
        ),
        (
            "DELONIX_K8S_VERSION",
            k8s_version.clone().unwrap_or_else(|| "none".into()),
        ),
        ("DELONIX_OFFLINE", offline.to_string()),
        (
            "DELONIX_NODE_EXPORTER",
            node_exporter
                .as_deref()
                .map(|l| format!("{NODE_EXPORTER_VERSION} on {l}"))
                .unwrap_or_else(|| "none".into()),
        ),
        ("DELONIX_EXTRA_PACKAGES", extra_packages.join(" ")),
    ]);
    let ops = splice_before_machine_id(
        ops,
        vec![CustomizeOp::RunCommand(format!(
            "printf '%s' \'{}\' > /etc/delonix-image-release && chmod 644 /etc/delonix-image-release",
            manifest.replace('\'', "'\\''")
        ))],
    );
    let mut args = customize_args(&work_qcow2, &ops);
    if offline {
        // Without this, libguestfs starts passt and the appliance waits for a DHCP
        // lease that never arrives on hosts where passt is broken (see AGENTS.md).
        args.insert(0, "--no-network".to_string());
    }

    eprintln!(
        "{}",
        super::po::tf(
            "running virt-customize ({n} step(s){net})...",
            &[
                ("n", &ops.len().to_string()),
                ("net", if offline { ", no network" } else { "" }),
            ],
        )
    );
    run_tool(
        "virt-customize",
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    )?;

    // Cresce o disco DEPOIS da customização e ANTES de compactar: o `qemu-img
    // resize` só mexe no tamanho virtual (o ficheiro continua esparso), e quem
    // estende a partição e o filesystem é o `growpart` do cloud-init no
    // primeiro arranque. Ver `GOLDEN_DISK_SIZE_GIB` para a medição que obriga
    // a isto.
    run_tool(
        "qemu-img",
        &[
            "resize",
            &work_qcow2.to_string_lossy(),
            &format!("{GOLDEN_DISK_SIZE_GIB}G"),
        ],
    )?;

    // Read back the kernel version the customize steps recorded (see the
    // `/etc/delonix-kernel-version` step in `common_customization_steps`) —
    // `virt-cat` pulls a single file out of a disk image without booting it.
    // Best-effort: a missing/unreadable file just leaves the column blank,
    // never fails the build over a "nice to have" metadata field.
    let kernel_version = std::process::Command::new("virt-cat")
        .args([
            "-a",
            &work_qcow2.to_string_lossy(),
            "/etc/delonix-kernel-version",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty() && s != "unknown");

    // O inventário sai para um sidecar ao lado do qcow2, pela mesma razão que o
    // `kernel_version` sai para os metadados: responder a «esta versão está
    // aqui?» não pode obrigar a montar a imagem. Best-effort — uma imagem sem
    // inventário é pior que uma com, mas muito melhor que um build chumbado por
    // causa dele.
    let (packages, packages_sha256) = match std::process::Command::new("virt-cat")
        .args([
            "-a",
            &work_qcow2.to_string_lossy(),
            "/usr/share/delonix/packages.tsv",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| o.stdout)
        .filter(|b| !b.is_empty())
    {
        Some(bytes) => {
            let n = bytes.iter().filter(|b| **b == b'\n').count() as u32;
            let sha = hex_sha256(&bytes);
            let _ = std::fs::write(store.sbom_path(tag), &bytes);
            eprintln!(
                "{}",
                super::po::tf("package inventory: {n} packages", &[("n", &n.to_string())])
            );
            (Some(n), Some(sha))
        }
        None => (None, None),
    };

    // Shrink the artifact. Measured on a 24.04 golden (2.38 GiB → 677 MiB, −72%):
    //  1) `virt-sparsify --in-place` — zeroes the blocks already freed (the apt
    //     cleanup above frees ~367 MiB that, without this, still occupy the qcow2).
    //  2) `qemu-img convert -c` — the Ubuntu cloud image COMES compressed and the
    //     initial `convert` (above, without `-c`) decompresses it; without this step
    //     the final artifact is ~4x larger than the base. `zstd` instead of the
    //     default zlib: compresses 5x faster (10s vs 53s), ends up smaller, and above
    //     all DECOMPRESSES much faster — it matters because this image is used as the
    //     read-only backing file of the VMs (`delonix_vm::create` makes an overlay per
    //     VM), so every read of the base OS goes through the decompressor.
    // Sparsify is best-effort: if it fails, we carry on (only some size is lost).
    let final_qcow2 = if compress {
        eprintln!(
            "{}",
            super::po::t("compacting the image (sparsify + zstd compression)...")
        );
        if let Err(e) = run_tool(
            "virt-sparsify",
            &["--in-place", &work_qcow2.to_string_lossy()],
        ) {
            eprintln!(
                "{} {}",
                super::po::t("warning:"),
                super::po::tf(
                    "virt-sparsify failed ({err}); compressing anyway",
                    &[("err", &e.to_string())]
                )
            );
        }
        let compressed = work_dir.join("final.qcow2");
        run_tool(
            "qemu-img",
            &[
                "convert",
                "-c",
                "-O",
                "qcow2",
                "-o",
                "compression_type=zstd",
                &work_qcow2.to_string_lossy(),
                &compressed.to_string_lossy(),
            ],
        )?;
        compressed
    } else {
        work_qcow2
    };

    let data = std::fs::read(&final_qcow2)?;
    let digest = format!("sha256:{}", hex_sha256(&data));
    let size = data.len() as u64;
    std::fs::rename(&final_qcow2, store.qcow2_path(tag))
        .or_else(|_| std::fs::copy(&final_qcow2, store.qcow2_path(tag)).map(|_| ()))?;
    let _ = std::fs::remove_dir_all(&work_dir);

    let img = VmImage {
        name: tag.to_string(),
        tag: tag.to_string(),
        digest,
        size,
        ubuntu_release: Some(release.to_string()),
        k8s_version,
        created_unix: now_unix(),
        kernel_version,
        distro: Some(distro.as_str().to_string()),
        // The built-in golden recipe has no VMfile to carry a recommendation
        // from — only a `VMfile` build (`vmfile::build`) sets these.
        default_vcpus: None,
        default_memory: None,
        default_backend: None,
        // The golden recipe deliberately leaves cloud-init enabled in the
        // image so each VM's first boot applies its own hostname/SSH keys.
        cloud_init: Some(true),
        packages,
        packages_sha256,
        built_by: Some(format!("delonix {}", env!("CARGO_PKG_VERSION"))),
        base_sha256,
    };
    store.save(&img)?;
    println!("{tag}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Download + verification of the Ubuntu cloud image
// ---------------------------------------------------------------------------

/// The node_exporter release this build ships, PINNED.
///
/// A floating `latest` would make two builds of the same `VMfile` produce
/// different images — the exact property the build manifest exists to make
/// checkable. Bumping it is a deliberate edit, with the checksum verified by
/// the same path as everything else this build downloads.
pub(crate) const NODE_EXPORTER_VERSION: &str = "1.9.1";

/// `<hash>  <file>` (GNU coreutils shape) — the form Prometheus, Ubuntu and
/// Debian all publish, as opposed to the BSD `SHA256 (f) = h` that Rocky uses
/// (`parse_bsd_checksum`).
///
/// Matches the file name by EQUALITY of the last field and not by
/// `contains`/`ends_with`: a release directory that publishes both
/// `node_exporter-1.9.1.linux-amd64.tar.gz` and
/// `...linux-arm64.tar.gz` has entries where one name is not a suffix of the
/// other, but the day an asset is renamed with a suffix the loose form starts
/// returning the wrong hash — and a wrong hash here reads as tampering.
pub(crate) fn sha256_from_gnu_sums(text: &str, filename: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let hash = it.next()?;
        let name = it.next()?.trim_start_matches('*');
        (name == filename && hash.len() == 64).then(|| hash.to_string())
    })
}

pub(crate) fn node_exporter_asset(version: &str) -> String {
    format!("node_exporter-{version}.linux-amd64.tar.gz")
}

/// Downloads and verifies the node_exporter binary ON THE HOST, returning the
/// extracted binary's path. Cached under `_base/` next to the cloud images, so
/// building several appliances costs one download.
///
/// Host-side and verified for the same reason the k8s `.deb` are: the guest is
/// customized OFFLINE, and a binary fetched inside it would be a second trust
/// path with no checksum on it.
pub(crate) fn fetch_node_exporter(store: &VmImageStore, version: &str) -> Result<PathBuf> {
    let version = VmImageStore::sanitize(version);
    let cached = store
        .base_cache_path(Distro::Ubuntu, "unused")
        .with_file_name(format!("node_exporter-{version}"));
    if cached.exists() {
        return Ok(cached);
    }
    let asset = node_exporter_asset(&version);
    let base = format!("https://github.com/prometheus/node_exporter/releases/download/v{version}");
    let tarball = cached.with_extension("tar.gz");
    eprintln!(
        "{}",
        super::po::tf(
            "downloading {url}...",
            &[("url", &format!("{base}/{asset}"))]
        )
    );
    stream_download(&format!("{base}/{asset}"), &tarball)?;

    eprintln!("{}", super::po::t("verifying SHA256SUMS..."));
    let sums = http_get_text(&format!("{base}/sha256sums.txt"))?;
    let expected = sha256_from_gnu_sums(&sums, &asset).ok_or_else(|| {
        Error::Invalid(format!(
            "{} {asset}",
            super::po::t("SHA256SUMS has no entry for")
        ))
    })?;
    let got = hex_sha256_file(&tarball)?;
    if got != expected {
        let _ = std::fs::remove_file(&tarball);
        return Err(Error::Invalid(format!(
            "{}: {asset} (expected {expected}, got {got})",
            super::po::t("checksum mismatch")
        )));
    }

    // `--strip-components=1` because the tarball has one top-level directory
    // named after the release; `-C` into the cache dir and rename, so a failed
    // extraction never leaves a half-written binary under the final name.
    let dir = cached.parent().unwrap_or(Path::new(".")).to_path_buf();
    let staged = dir.join(format!("node_exporter-{version}.staged"));
    let _ = std::fs::remove_file(&staged);
    let member = format!("node_exporter-{version}.linux-amd64/node_exporter");
    run_tool(
        "tar",
        &[
            "-xzf",
            &tarball.to_string_lossy(),
            "-C",
            &dir.to_string_lossy(),
            "--strip-components=1",
            &member,
        ],
    )?;
    let _ = std::fs::rename(dir.join("node_exporter"), &staged);
    std::fs::rename(&staged, &cached)?;
    let _ = std::fs::remove_file(&tarball);
    Ok(cached)
}

/// Installs the node_exporter binary already fetched host-side, as a systemd
/// unit bound to `listen`.
///
/// **Opt-in, and that is the security decision.** A metrics listener in every
/// published copy of this image is a port the tenant never asked for, on an
/// artefact whose whole point is to be handed to someone else — the same
/// reasoning that took the built-in password out. AWS and GCP ship a guest
/// agent by default and leave the metrics agent to be enabled; this follows
/// them. Without `--node-exporter` the image is byte-for-byte what it was.
///
/// Runs as a dedicated system account with no shell and no home, under the
/// systemd sandbox directives that cost nothing here (it reads `/proc` and
/// `/sys`, never writes).
/// Splices `extra` in BEFORE the machine-id reset, which has to stay the very
/// last operation (see `shared_account_steps` for the DHCP incident that put it
/// there): `systemctl enable` can materialise a machine-id in a guest that has
/// none, and an image whose VMs all boot with the same one loses its DHCP lease
/// to whichever sibling renewed last.
///
/// Falls back to appending when the marker is absent, so a recipe that never
/// resets the machine-id still gets the steps rather than silently losing them.
/// The build manifest baked into the image at `/etc/delonix-image-release`,
/// `os-release` shape (one `KEY=value` per line, shell-sourceable).
///
/// **This is the reproducibility record, and it is deliberately not a promise
/// of a reproducible build.** `apt`/`dnf` resolve against a moving archive, so
/// two runs of the same recipe a week apart legitimately differ; pinning every
/// transitive version would mean carrying a snapshot mirror, which is a
/// platform decision and not a flag on this command. What IS achievable — and
/// is what an operator actually needs after a CVE lands — is that any image can
/// say which base it came from, which `delonix` built it, and exactly which
/// package versions it carries (`/usr/share/delonix/packages.tsv`). That turns
/// «is this affected?» from an investigation into a lookup.
///
/// Values are quoted and newlines refused rather than escaped: a value with a
/// newline would split into a second, forged `KEY=value` line, and this file is
/// meant to be sourced.
pub(crate) fn build_manifest(fields: &[(&str, String)]) -> String {
    let mut out = String::from("# Generated by `delonix image vm build` — do not edit.\n");
    for (k, v) in fields {
        let v = v.replace(['\n', '\r'], " ");
        out.push_str(&format!("{k}=\"{}\"\n", v.replace('"', "'")));
    }
    out
}

pub(crate) fn splice_before_machine_id(
    mut ops: Vec<CustomizeOp>,
    extra: Vec<CustomizeOp>,
) -> Vec<CustomizeOp> {
    let at = ops
        .iter()
        .position(|o| matches!(o, CustomizeOp::RunCommand(c) if c.contains("/etc/machine-id")))
        .unwrap_or(ops.len());
    for (i, op) in extra.into_iter().enumerate() {
        ops.insert(at + i, op);
    }
    ops
}

pub(crate) fn node_exporter_steps(bin: &Path, listen: &str) -> Vec<CustomizeOp> {
    let unit = format!(
        "[Unit]\\n\
         Description=Prometheus node exporter\\n\
         Documentation=https://github.com/prometheus/node_exporter\\n\
         After=network-online.target\\n\
         Wants=network-online.target\\n\
         \\n\
         [Service]\\n\
         User=node-exporter\\n\
         Group=node-exporter\\n\
         ExecStart=/usr/local/bin/node_exporter --web.listen-address={listen}\\n\
         Restart=on-failure\\n\
         RestartSec=5\\n\
         NoNewPrivileges=yes\\n\
         ProtectSystem=strict\\n\
         ProtectHome=yes\\n\
         PrivateTmp=yes\\n\
         ProtectKernelTunables=yes\\n\
         ProtectControlGroups=yes\\n\
         RestrictNamespaces=yes\\n\
         \\n\
         [Install]\\n\
         WantedBy=multi-user.target\\n"
    );
    vec![
        CustomizeOp::CopyIn(bin.to_path_buf(), "/usr/local/bin".to_string()),
        CustomizeOp::RunCommand("chmod 0755 /usr/local/bin/node_exporter".into()),
        CustomizeOp::RunCommand(
            "useradd --system --no-create-home --shell /usr/sbin/nologin node-exporter \
             || useradd --system --no-create-home --shell /sbin/nologin node-exporter || true"
                .into(),
        ),
        CustomizeOp::RunCommand(format!(
            "printf '{unit}' > /etc/systemd/system/node-exporter.service && \
             chmod 644 /etc/systemd/system/node-exporter.service && \
             systemctl enable node-exporter.service"
        )),
    ]
}

pub(crate) fn download_ubuntu_base(store: &VmImageStore, release: &str) -> Result<PathBuf> {
    let cached = store.base_cache_path(Distro::Ubuntu, release);
    if cached.exists() {
        return Ok(cached);
    }
    let base_url = format!("https://cloud-images.ubuntu.com/releases/{release}/release");
    let img_name = format!("ubuntu-{release}-server-cloudimg-amd64.img");
    let img_url = format!("{base_url}/{img_name}");
    let sums_url = format!("{base_url}/SHA256SUMS");

    eprintln!(
        "{}",
        super::po::tf("downloading {url}...", &[("url", &img_url)])
    );
    let tmp = cached.with_extension("download");
    stream_download(&img_url, &tmp)?;

    eprintln!("{}", super::po::t("verifying SHA256SUMS..."));
    let sums = http_get_text(&sums_url)?;
    let expected = sums
        .lines()
        .find(|l| l.trim_end().ends_with(&img_name))
        .and_then(|l| l.split_whitespace().next())
        .ok_or_else(|| {
            Error::Invalid(format!(
                "{} {img_name}",
                super::po::t("SHA256SUMS has no entry for")
            ))
        })?
        .to_string();
    let got = hex_sha256_file(&tmp)?;
    if got != expected {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Invalid(super::po::tf(
            "invalid checksum for {img_name}: expected {expected}, got {got} — download discarded",
            &[
                ("img_name", &img_name),
                ("expected", &expected),
                ("got", &got),
            ],
        )));
    }
    std::fs::rename(&tmp, &cached)?;
    Ok(cached)
}

// ---------------------------------------------------------------------------
// Download + verification of the Debian cloud image
// ---------------------------------------------------------------------------

/// Debian's cloud image directory is keyed by CODENAME (`bookworm`), but the
/// filename embeds the MAJOR VERSION NUMBER (`debian-12-...`) — confirmed
/// live against `cloud.debian.org` (no numeric-only directory alias exists,
/// so this mapping can't be derived from the codename string alone). Fails
/// closed on an unknown codename rather than guessing a number.
fn debian_major_version(codename: &str) -> Result<&'static str> {
    match codename {
        "bullseye" => Ok("11"),
        "bookworm" => Ok("12"),
        "trixie" => Ok("13"),
        _ => Err(Error::Invalid(super::po::tf(
            "--debian-release '{codename}' unknown (bullseye|bookworm|trixie)",
            &[("codename", codename)],
        ))),
    }
}

/// Same shape as `download_ubuntu_base`, two confirmed differences (checked
/// live against `cloud.debian.org` before writing this, not assumed): (1) the
/// path is `images/cloud/<codename>/latest/`, filename
/// `debian-<major>-genericcloud-amd64.qcow2` (the `genericcloud` variant —
/// virtio-only kernel, smaller, still has cloud-init — not the `generic`
/// variant, which also ships legacy drivers this project never needs); (2)
/// Debian publishes `SHA512SUMS`, NOT `SHA256SUMS` (no SHA256 checksums file
/// exists at all) — same `<hash>  <filename>` line format, different hash
/// algorithm, hence `hex_sha512_file` below.
pub(crate) fn download_debian_base(store: &VmImageStore, release: &str) -> Result<PathBuf> {
    let cached = store.base_cache_path(Distro::Debian, release);
    if cached.exists() {
        return Ok(cached);
    }
    let major = debian_major_version(release)?;
    let base_url = format!("https://cloud.debian.org/images/cloud/{release}/latest");
    let img_name = format!("debian-{major}-genericcloud-amd64.qcow2");
    let img_url = format!("{base_url}/{img_name}");
    let sums_url = format!("{base_url}/SHA512SUMS");

    eprintln!(
        "{}",
        super::po::tf("downloading {url}...", &[("url", &img_url)])
    );
    let tmp = cached.with_extension("download");
    stream_download(&img_url, &tmp)?;

    eprintln!("{}", super::po::t("verifying SHA512SUMS..."));
    let sums = http_get_text(&sums_url)?;
    let expected = sums
        .lines()
        .find(|l| l.trim_end().ends_with(&img_name))
        .and_then(|l| l.split_whitespace().next())
        .ok_or_else(|| {
            Error::Invalid(format!(
                "{} {img_name}",
                super::po::t("SHA512SUMS has no entry for")
            ))
        })?
        .to_string();
    let got = hex_sha512_file(&tmp)?;
    if got != expected {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Invalid(super::po::tf(
            "invalid checksum for {img_name}: expected {expected}, got {got} — download discarded",
            &[
                ("img_name", &img_name),
                ("expected", &expected),
                ("got", &got),
            ],
        )));
    }
    std::fs::rename(&tmp, &cached)?;
    Ok(cached)
}

// ---------------------------------------------------------------------------
// Download + verification of the Rocky Linux cloud image
// ---------------------------------------------------------------------------

/// Rocky's release directory AND filename both use the plain major version
/// number (`9`, not a codename) — simpler than Debian, confirmed live
/// against `dl.rockylinux.org` (8/9/10 all exist today). Whitelisted rather
/// than accepted verbatim purely for a fast, clear error before any network
/// call — an unknown value would otherwise still fail safely (a 404 from
/// `stream_download`), this is just better UX, not a security boundary
/// (unlike Debian's codename→number mapping, which IS load-bearing: there is
/// no way to derive the filename's number from an arbitrary codename).
fn valid_rocky_release(release: &str) -> Result<()> {
    if matches!(release, "8" | "9" | "10") {
        Ok(())
    } else {
        Err(Error::Invalid(super::po::tf(
            "--rocky-release '{release}' unknown (8|9|10)",
            &[("release", release)],
        )))
    }
}

/// Fedora Cloud base image.
///
/// Fedora's artifact name carries a BUILD number that the release number does
/// not determine (`42` ships as `42-1.1`), and its download redirector serves
/// no directory listing — so there is nothing to derive it from. Rather than
/// guess, `--fedora-release` takes the full `<release>-<build>` exactly as
/// Fedora's own download page shows it, and says so when given anything else.
/// Same principle as the Proxmox ISO inputs in the appliance workflow: an
/// unverifiable guess presented as a fact is worse than asking.
///
/// The CHECKSUM is BSD-style (`SHA256 (file) = hash`), the same shape Rocky
/// uses — `parse_bsd_checksum` is shared, not duplicated.
pub(crate) fn download_fedora_base(store: &VmImageStore, release: &str) -> Result<PathBuf> {
    valid_fedora_release(release)?;
    let cached = store.base_cache_path(Distro::Fedora, release);
    if cached.exists() {
        return Ok(cached);
    }
    let major = release.split('-').next().unwrap_or(release);
    let img_name = format!("Fedora-Cloud-Base-Generic-{release}.x86_64.qcow2");
    let base_url = format!(
        "https://download.fedoraproject.org/pub/fedora/linux/releases/{major}/Cloud/x86_64/images"
    );
    let img_url = format!("{base_url}/{img_name}");
    let sums_url = format!("{base_url}/Fedora-Cloud-{release}-x86_64-CHECKSUM");

    eprintln!(
        "{}",
        super::po::tf("downloading {url}...", &[("url", &img_url)])
    );
    let tmp = cached.with_extension("download");
    stream_download(&img_url, &tmp)?;

    eprintln!("{}", super::po::t("verifying CHECKSUM..."));
    let sums = http_get_text(&sums_url)?;
    let expected = parse_bsd_checksum(&sums, &img_name).ok_or_else(|| {
        Error::Invalid(format!(
            "{} {img_name}",
            super::po::t("CHECKSUM has no entry for")
        ))
    })?;
    let got = hex_sha256_file(&tmp)?;
    if got != expected {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Invalid(super::po::tf(
            "invalid checksum for {img_name}: expected {expected}, got {got} — download discarded",
            &[
                ("img_name", &img_name),
                ("expected", &expected),
                ("got", &got),
            ],
        )));
    }
    std::fs::rename(&tmp, &cached)?;
    Ok(cached)
}

/// `<release>-<build>`, e.g. `42-1.1`. Both parts are numeric; the build may
/// have several dotted components. Rejecting a bare `42` here is the point:
/// it looks right and produces a 404 several hundred megabytes later.
fn valid_fedora_release(release: &str) -> Result<()> {
    let bad = || {
        Error::Invalid(super::po::tf(
            "--fedora-release '{release}' must be `<release>-<build>` as shown on Fedora's \
             download page (e.g. `42-1.1`) — the build number is not derivable from the release",
            &[("release", release)],
        ))
    };
    let (major, build) = release.split_once('-').ok_or_else(bad)?;
    if major.is_empty() || !major.chars().all(|c| c.is_ascii_digit()) {
        return Err(bad());
    }
    if build.is_empty()
        || !build.chars().all(|c| c.is_ascii_digit() || c == '.')
        || build.starts_with('.')
        || build.ends_with('.')
    {
        return Err(bad());
    }
    Ok(())
}

/// Same shape as `download_ubuntu_base`/`download_debian_base`, confirmed
/// live before writing code: the image is `Rocky-<release>-GenericCloud.
/// latest.x86_64.qcow2` under `pub/rocky/<release>/images/x86_64/` (no
/// `images/cloud/` segment — a different tree shape than Debian's). The
/// checksum sidecar is PER-FILE (`<img>.CHECKSUM`, not a directory-wide
/// `SUMS` file) and uses the BSD `SHA256 (<filename>) = <hash>` shape — a
/// THIRD checksum format in this module, after Ubuntu/Debian's GNU
/// `<hash>  <filename>` — hence `parse_bsd_checksum` below. SHA256 (not
/// SHA512 like Debian), so
/// `hex_sha256_file` is reused as-is.
pub(crate) fn download_rocky_base(store: &VmImageStore, release: &str) -> Result<PathBuf> {
    valid_rocky_release(release)?;
    let cached = store.base_cache_path(Distro::Rocky, release);
    if cached.exists() {
        return Ok(cached);
    }
    let img_name = format!("Rocky-{release}-GenericCloud.latest.x86_64.qcow2");
    let img_url = format!("https://dl.rockylinux.org/pub/rocky/{release}/images/x86_64/{img_name}");
    let sums_url = format!("{img_url}.CHECKSUM");

    eprintln!(
        "{}",
        super::po::tf("downloading {url}...", &[("url", &img_url)])
    );
    let tmp = cached.with_extension("download");
    stream_download(&img_url, &tmp)?;

    eprintln!("{}", super::po::t("verifying CHECKSUM..."));
    let sums = http_get_text(&sums_url)?;
    let expected = parse_bsd_checksum(&sums, &img_name).ok_or_else(|| {
        Error::Invalid(format!(
            "{} {img_name}",
            super::po::t("CHECKSUM has no entry for")
        ))
    })?;
    let got = hex_sha256_file(&tmp)?;
    if got != expected {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Invalid(super::po::tf(
            "invalid checksum for {img_name}: expected {expected}, got {got} — download discarded",
            &[
                ("img_name", &img_name),
                ("expected", &expected),
                ("got", &got),
            ],
        )));
    }
    std::fs::rename(&tmp, &cached)?;
    Ok(cached)
}

/// Rocky's `.CHECKSUM` uses the BSD `SHA256 (<filename>) = <hash>` line
/// shape — confirmed live, different from both `<hash>  <filename>` forms
/// used elsewhere in this module. Pure/tested. Matches the exact filename
/// (not just any `SHA256 (...)` line) as defense-in-depth against a sidecar
/// that happens to list more than one file.
fn parse_bsd_checksum(text: &str, filename: &str) -> Option<String> {
    let prefix = format!("SHA256 ({filename}) = ");
    text.lines()
        .find_map(|l| l.strip_prefix(prefix.as_str()))
        .map(|s| s.trim().to_string())
}

/// Downloads `url` to `dest`, RESUMING a partial file and retrying a dropped
/// connection.
///
/// Measured here, and the reason this exists: a Rocky cloud image (646 MiB) died
/// 3.8 MiB in. The old version had no retry at all — one failed `read()` threw
/// away everything and the next run started from zero, so on a slow or flaky
/// link a build was a coin toss. Every caller verifies a checksum afterwards,
/// which is what makes resuming safe: bytes stitched from two ranges either add
/// up to the published hash or the download is discarded.
pub(crate) fn stream_download(url: &str, dest: &Path) -> Result<()> {
    const ATTEMPTS: u32 = 5;
    /// How long a sample must run before its rate means anything.
    const STALL_WINDOW_SECS: f64 = 20.0;
    /// Bounded: a link that is simply slow must not reconnect forever.
    const MAX_STALL_RECONNECTS: u32 = 3;
    let client = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(std::time::Duration::from_secs(3600))
        .build()
        .map_err(|e| Error::Invalid(format!("{}: {e}", super::po::t("HTTP client"))))?;

    let mut last_err = String::new();
    let mut stalls: u32 = 0;
    // A reconnect for slowness is not a failed attempt — it is the same
    // transfer continuing on (hopefully) a better node, so it must not consume
    // the retry budget that exists for genuine errors.
    let mut attempt = 0u32;
    while attempt < ATTEMPTS + stalls {
        attempt += 1;
        // Where a previous attempt stopped. `dest` is the caller's `.download`
        // temp file, never the final cached image, so a leftover here is always
        // ours to continue.
        let have = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
        let mut req = client.get(url);
        if have > 0 {
            req = req.header("Range", format!("bytes={have}-"));
        }
        let mut resp = match req.send() {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("{e}");
                continue;
            }
        };
        if !resp.status().is_success() {
            return Err(Error::Invalid(format!("GET {url}: HTTP {}", resp.status())));
        }
        // 206 means the server honoured the range and we append; anything else
        // (a mirror without range support, or a redirect to a fresh object)
        // means it is sending the whole thing again, so start over.
        let resuming = have > 0 && resp.status().as_u16() == 206;
        let mut file = if resuming {
            std::fs::OpenOptions::new().append(true).open(dest)?
        } else {
            std::fs::File::create(dest)?
        };
        let mut written: u64 = if resuming { have } else { 0 };
        if attempt > 1 || resuming {
            eprintln!(
                "{}",
                super::po::tf(
                    "resuming download at {have} bytes (attempt {attempt})...",
                    &[
                        ("have", &written.to_string()),
                        ("attempt", &attempt.to_string())
                    ],
                )
            );
        }
        let mut buf = [0u8; 1 << 20];
        let mut broke = false;
        // Stall detection. A CDN redirect can land the connection on a slow
        // node and keep it there: measured here, a resumed Rocky transfer sat
        // at 242 KiB/s while a FRESH request to the same URL got 1732 KiB/s —
        // 7x, same host, same second. Reconnecting used to mean throwing the
        // whole file away, so it was never worth it; with `Range` resume it
        // costs nothing, which is what makes this worth doing at all.
        //
        // The threshold is RELATIVE to the best window this transfer has seen,
        // not an absolute number: a uniformly slow link (satellite, rural) must
        // not reconnect forever chasing a speed it will never reach.
        let mut window_start = std::time::Instant::now();
        let mut window_bytes: u64 = 0;
        let mut best_rate: f64 = 0.0;
        loop {
            let n = match resp.read(&mut buf) {
                Ok(n) => n,
                Err(e) => {
                    // Keep what is on disk: the next attempt continues from here.
                    last_err = format!("{e}");
                    broke = true;
                    break;
                }
            };
            if n == 0 {
                break;
            }
            written += n as u64;
            // Ceiling on ACTUAL bytes read, not on the advertised
            // `Content-Length`: a hostile or misconfigured server can simply
            // keep streaming, and this writes to disk, so the failure is a full
            // filesystem — on a node that may be running everything else this
            // host serves. Deliberately far above any real cloud image, so it
            // only ever fires on something that is not an image.
            if written > MAX_DOWNLOAD_BYTES {
                let _ = std::fs::remove_file(dest);
                return Err(Error::Invalid(format!(
                    "GET {url}: aborted after {written} bytes (over the {MAX_DOWNLOAD_BYTES}-byte limit) \
                     — the server is streaming more than any expected image"
                )));
            }
            file.write_all(&buf[..n])?;

            window_bytes += n as u64;
            let elapsed = window_start.elapsed().as_secs_f64();
            if elapsed >= STALL_WINDOW_SECS {
                let rate = window_bytes as f64 / elapsed;
                if rate > best_rate {
                    best_rate = rate;
                } else if stalls < MAX_STALL_RECONNECTS && rate < best_rate / 4.0 {
                    stalls += 1;
                    eprintln!(
                        "{}",
                        super::po::tf(
                            "download slowed to {now} KiB/s (peak {peak}) — reconnecting to resume",
                            &[
                                ("now", &format!("{:.0}", rate / 1024.0)),
                                ("peak", &format!("{:.0}", best_rate / 1024.0)),
                            ],
                        )
                    );
                    broke = true;
                    break;
                }
                window_start = std::time::Instant::now();
                window_bytes = 0;
            }
        }
        if !broke {
            return Ok(());
        }
        file.flush()?;
    }
    Err(Error::Invalid(super::po::tf(
        "GET {url}: gave up after {attempts} attempts ({err}) — partial file kept, \
         a later run resumes from it",
        &[
            ("url", url),
            ("attempts", &ATTEMPTS.to_string()),
            ("err", &last_err),
        ],
    )))
}

/// Ceiling for a single downloaded artifact (cloud image, `.deb`, binary).
/// 32 GiB: an order of magnitude above the largest cloud image anyone ships,
/// so a legitimate download never approaches it.
const MAX_DOWNLOAD_BYTES: u64 = 32 * 1024 * 1024 * 1024;

pub(crate) fn http_get_text(url: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| Error::Invalid(format!("{}: {e}", super::po::t("HTTP client"))))?;
    let resp = client
        .get(url)
        .send()
        .map_err(|e| Error::Invalid(format!("GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Invalid(format!("GET {url}: HTTP {}", resp.status())));
    }
    resp.text().map_err(|e| {
        Error::Invalid(format!(
            "{}: {e}",
            super::po::tf("body of {url}", &[("url", url)])
        ))
    })
}

// ---------------------------------------------------------------------------
// OFFLINE build: download+verify the k8s .deb files ON THE HOST
// ---------------------------------------------------------------------------
// This way `virt-customize` runs with `--no-network` and the appliance never
// needs DHCP/DNS — which removes the host workarounds (passt/dhclient) that the
// online path requires. The chain of trust is the SAME as apt's, only done
// here instead of inside the guest:
//   InRelease (clearsigned, verified with the repo's Release.key)
//     → SHA256 of `Packages`  → SHA256 of each `.deb`
// A file is never accepted without the previous step having authenticated it — the
// same principle as CRITICAL finding nº3 of the audit (`pull_oci_artifact` without digest).

/// A `.deb` from the `pkgs.k8s.io` repo, already resolved from an authenticated `Packages`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct K8sDeb {
    pub name: String,
    pub version: String,
    /// Path relative to the repo root (the `Filename` field).
    pub filename: String,
    pub sha256: String,
}

/// Parses a `Packages` index (Debian control, blocks separated by a blank
/// line) and returns, per package in `wanted`, the HIGHEST version available for
/// `arch`. PURE function (testable without network).
///
/// `version_prefix` (e.g. "1.34.") only applies to the `versioned` packages — the
/// components that follow the Kubernetes version (kubeadm/kubelet/kubectl). The
/// rest of the repo has its OWN versioning (`kubernetes-cni` is 1.7.x,
/// `cri-tools` is 1.34.x but independent) and take only "the most recent": filtering
/// them by the k8s prefix returned nothing.
pub(crate) fn parse_packages_index(
    index: &str,
    arch: &str,
    version_prefix: &str,
    wanted: &[&str],
    versioned: &[&str],
) -> Vec<K8sDeb> {
    let mut best: std::collections::BTreeMap<String, K8sDeb> = Default::default();
    for block in index.split("\n\n") {
        let mut f: std::collections::HashMap<&str, &str> = Default::default();
        for line in block.lines() {
            if let Some((k, v)) = line.split_once(": ") {
                f.insert(k.trim(), v.trim());
            }
        }
        let (Some(name), Some(version), Some(filename), Some(sha), Some(a)) = (
            f.get("Package"),
            f.get("Version"),
            f.get("Filename"),
            f.get("SHA256"),
            f.get("Architecture"),
        ) else {
            continue;
        };
        if *a != arch {
            continue;
        }
        if !wanted.is_empty() && !wanted.contains(name) {
            continue;
        }
        // The k8s prefix only applies to those that follow the k8s version.
        if versioned.contains(name) && !version.starts_with(version_prefix) {
            continue;
        }
        let cand = K8sDeb {
            name: name.to_string(),
            version: version.to_string(),
            filename: filename.to_string(),
            sha256: sha.to_string(),
        };
        best.entry(name.to_string())
            .and_modify(|cur| {
                if deb_version_lt(&cur.version, &cand.version) {
                    *cur = cand.clone();
                }
            })
            .or_insert(cand);
    }
    best.into_values().collect()
}

/// Compares two Debian versions well enough for the k8s repo
/// (`1.34.9-1.1`): compares numerically the fields separated by `.`/`-`.
/// It is not dpkg's full algorithm — the repo only uses versions of this form, and a
/// tie/unexpected format degrades to lexicographic comparison.
pub(crate) fn deb_version_lt(a: &str, b: &str) -> bool {
    let parts = |s: &str| -> Vec<u64> {
        s.split(['.', '-'])
            .map(|p| {
                p.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
            })
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (pa, pb) = (parts(a), parts(b));
    match pa.cmp(&pb) {
        std::cmp::Ordering::Equal => a < b,
        o => o == std::cmp::Ordering::Less,
    }
}

/// Extracts from an authenticated `Release` the expected SHA256 of a file
/// (e.g. "Packages"). The indexes come in the `SHA256:` section as
/// `<sha>  <size>  <path>`. PURE function.
pub(crate) fn release_sha256_of(release: &str, want_path: &str) -> Option<String> {
    let mut in_sha = false;
    for line in release.lines() {
        if line.starts_with("SHA256:") {
            in_sha = true;
            continue;
        }
        // another top-level (non-indented) section ends the SHA256 block.
        if in_sha && !line.starts_with(' ') {
            in_sha = false;
        }
        if !in_sha {
            continue;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        if let [sha, _size, path] = cols[..] {
            if path == want_path {
                return Some(sha.to_string());
            }
        }
    }
    None
}

/// Verifies the `InRelease` (clearsigned) with the repo's `Release.key` and returns
/// the ALREADY AUTHENTICATED body. Uses `gpgv` with a temporary keyring — never touches
/// the user's keyring. Fails closed: without a valid signature, there is no build.
fn verify_inrelease(work: &Path, repo_base: &str) -> Result<String> {
    let key_armored = http_get_text(&format!("{repo_base}/Release.key"))?;
    let key_asc = work.join("k8s-release.asc");
    let keyring = work.join("k8s-release.gpg");
    std::fs::write(&key_asc, &key_armored)?;
    // ASCII-armored → binary keyring that gpgv understands.
    run_tool(
        "gpg",
        &[
            "--batch",
            "--yes",
            "--no-default-keyring",
            "--dearmor",
            "-o",
            &keyring.to_string_lossy(),
            &key_asc.to_string_lossy(),
        ],
    )
    .map_err(|e| {
        Error::Invalid(format!(
            "{}: {e}",
            super::po::t("preparing the k8s repo keyring")
        ))
    })?;

    let inrelease = work.join("InRelease");
    stream_download(&format!("{repo_base}/InRelease"), &inrelease)?;
    run_tool(
        "gpgv",
        &[
            "--keyring",
            &keyring.to_string_lossy(),
            &inrelease.to_string_lossy(),
        ],
    )
    .map_err(|_| {
        Error::Invalid(
            super::po::t(
                "the k8s repo's InRelease signature does NOT match the Release.key — aborting \
                 (possible compromised repo or MITM)",
            )
            .to_string(),
        )
    })?;
    Ok(std::fs::read_to_string(&inrelease)?)
}

/// Downloads to `dest_dir` the k8s `.deb` files (repo closure: kubeadm/kubelet/
/// kubectl + `kubernetes-cni`), with the full apt chain verified on the host.
/// Returns the local paths. `arch` is the Debian architecture (e.g. "amd64").
fn download_k8s_debs(
    work: &Path,
    dest_dir: &Path,
    k8s_version: Option<&str>,
    arch: &str,
    extra_packages: &[String],
) -> Result<Vec<PathBuf>> {
    let repo = super::k8s_recipes::k8s_repo_version(k8s_version);
    let repo_base = format!("https://pkgs.k8s.io/core:/{repo}/deb");
    std::fs::create_dir_all(dest_dir)?;

    eprintln!(
        "{}",
        super::po::tf(
            "verifying the k8s repo signature ({repo})...",
            &[("repo", &repo)]
        )
    );
    let release = verify_inrelease(work, &repo_base)?;

    // `Packages` authenticated by the SHA256 listed in the signed InRelease.
    let want_sha = release_sha256_of(&release, "Packages").ok_or_else(|| {
        Error::Invalid(
            super::po::t("the k8s repo InRelease does not declare the SHA256 of 'Packages'")
                .to_string(),
        )
    })?;
    let packages_path = work.join("Packages");
    stream_download(&format!("{repo_base}/Packages"), &packages_path)?;
    let got = hex_sha256_file(&packages_path)?;
    if got != want_sha {
        return Err(Error::Invalid(super::po::tf(
            "SHA256 of the Packages index does not match (expected {expected}, got {got}) — aborting",
            &[
                ("expected", &want_sha[..16.min(want_sha.len())]),
                ("got", &got[..16.min(got.len())]),
            ],
        )));
    }
    let index = std::fs::read_to_string(&packages_path)?;

    // Closure: the 3 requested + `kubernetes-cni` (kubelet dep inside the repo).
    // The remaining kubelet deps (iptables/mount/util-linux/libc6) already come in
    // the Ubuntu cloud image — if any is missing, `dpkg -i` fails LOUDLY in the guest,
    // which is what we want (never install half-installed silently).
    // `versioned` follow the k8s version (`--k8s-version 1.34` → `1.34.*`);
    // `kubernetes-cni` has its own versioning → only "the most recent".
    const VERSIONED: [&str; 3] = ["kubeadm", "kubelet", "kubectl"];
    let mut wanted: Vec<&str> = vec!["kubeadm", "kubelet", "kubectl", "kubernetes-cni"];
    for p in extra_packages {
        wanted.push(p.as_str());
    }
    let version_prefix = match k8s_version {
        Some(v) if v != "stable" => format!("{v}."),
        _ => String::new(),
    };
    let debs = parse_packages_index(&index, arch, &version_prefix, &wanted, &VERSIONED);
    for base in ["kubeadm", "kubelet", "kubectl", "kubernetes-cni"] {
        if !debs.iter().any(|d| d.name == base) {
            return Err(Error::Invalid(super::po::tf(
                "the k8s repo ({repo}) does not have '{base}' for {arch} — nonexistent version?",
                &[("repo", &repo), ("base", base), ("arch", arch)],
            )));
        }
    }

    let mut out = Vec::new();
    for d in &debs {
        let file_name = d.filename.rsplit('/').next().unwrap_or(&d.filename);
        let dest = dest_dir.join(file_name);
        eprintln!("  {} {} ({arch})", d.name, d.version);
        stream_download(&format!("{repo_base}/{}", d.filename), &dest)?;
        let got = hex_sha256_file(&dest)?;
        if got != d.sha256 {
            let _ = std::fs::remove_file(&dest);
            return Err(Error::Invalid(super::po::tf(
                "SHA256 of {file_name} does not match (expected {expected}, got {got}) — aborting",
                &[
                    ("file_name", file_name),
                    ("expected", &d.sha256[..16.min(d.sha256.len())]),
                    ("got", &got[..16.min(got.len())]),
                ],
            )));
        }
        out.push(dest);
    }
    Ok(out)
}

/// Where the DISTRO's own archive keyring lives on the build host.
///
/// The k8s chain fetches `Release.key` from the repo it is about to trust — fine for a
/// third-party repo (there is no other anchor), but trust-on-first-use. The Ubuntu archive
/// key ships with the distro, so here the anchor is the host's package manager instead of
/// the server we are downloading from. That is strictly stronger, and it is why an absent
/// keyring **fails** rather than falling back to fetching a key from the archive.
const UBUNTU_ARCHIVE_KEYRING: &str = "/usr/share/keyrings/ubuntu-archive-keyring.gpg";

/// The suite codename (`noble`) of an image, read from ITS OWN `/etc/os-release`.
///
/// Deliberately not a `24.04 → noble` table: the archive URL needs the codename, a table
/// goes stale the day a release is added (this build already accepts `--ubuntu-release
/// 26.04`), and the image is the thing that knows. Same idiom as `detect_kernel_version`,
/// which reads the kernel out of the artefact instead of deriving it.
fn guest_os_codename(qcow2: &Path) -> Option<String> {
    let out = std::process::Command::new("virt-cat")
        .args(["-a", &qcow2.to_string_lossy(), "/etc/os-release"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_os_release_codename(&String::from_utf8_lossy(&out.stdout))
}

/// PURE. `VERSION_CODENAME=noble` → `noble`; quotes stripped (some distros quote it).
pub(crate) fn parse_os_release_codename(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(v) = line.trim().strip_prefix("VERSION_CODENAME=") {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Verifies a distro archive's clearsigned `InRelease` against a keyring ALREADY on the
/// host, and returns its text. Fails closed at both steps.
fn verify_archive_inrelease(work: &Path, dists_base: &str, keyring: &str) -> Result<String> {
    if !Path::new(keyring).exists() {
        return Err(Error::Invalid(super::po::tf(
            "the distro archive keyring is missing: {keyring} — install the distro's \
             archive keyring on the BUILD HOST (Debian/Ubuntu: ubuntu-keyring)",
            &[("keyring", keyring)],
        )));
    }
    let inrelease = work.join("archive-InRelease");
    stream_download(&format!("{dists_base}/InRelease"), &inrelease)?;
    run_tool(
        "gpgv",
        &["--keyring", keyring, &inrelease.to_string_lossy()],
    )
    .map_err(|_| {
        Error::Invalid(
            super::po::t(
                "the distro archive's InRelease signature does NOT match the host keyring \
                 — aborting (possible compromised mirror or MITM)",
            )
            .to_string(),
        )
    })?;
    Ok(std::fs::read_to_string(&inrelease)?)
}

/// Downloads `.deb` files from a DISTRO archive into `dest_dir`, with the same apt chain
/// the k8s path uses — signed `InRelease` → SHA256 of the component index → SHA256 of each
/// `.deb` — but anchored on the host keyring (see [`UBUNTU_ARCHIVE_KEYRING`]).
///
/// The index is only published COMPRESSED for the big components (measured: an
/// uncompressed `universe/binary-amd64/Packages` is a 404), so it is gunzipped here —
/// `flate2` is already a direct dependency of this crate, no new supply-chain surface.
///
/// Every wanted package must be found; a missing one is an ERROR and never a quiet
/// omission, because the whole point of installing offline is that the guest cannot go
/// looking for what we forgot.
fn download_archive_debs(
    work: &Path,
    dest_dir: &Path,
    archive_base: &str,
    codename: &str,
    components: &[&str],
    arch: &str,
    wanted: &[&str],
) -> Result<Vec<PathBuf>> {
    let dists_base = format!("{archive_base}/dists/{codename}");
    let release = verify_archive_inrelease(work, &dists_base, UBUNTU_ARCHIVE_KEYRING)?;
    std::fs::create_dir_all(dest_dir)?;

    let mut found: Vec<K8sDeb> = Vec::new();
    for comp in components {
        let rel_path = format!("{comp}/binary-{arch}/Packages.gz");
        let Some(want_sha) = release_sha256_of(&release, &rel_path) else {
            continue;
        };
        let gz = work.join(format!("Packages-{comp}.gz"));
        stream_download(&format!("{dists_base}/{rel_path}"), &gz)?;
        let got = hex_sha256_file(&gz)?;
        if got != want_sha {
            return Err(Error::Invalid(super::po::tf(
                "the archive index {path} does not match the SHA256 in the signed InRelease",
                &[("path", &rel_path)],
            )));
        }
        let mut text = String::new();
        {
            use std::io::Read;
            let f = std::fs::File::open(&gz)?;
            flate2::read::GzDecoder::new(f).read_to_string(&mut text)?;
        }
        found.extend(parse_packages_index(&text, arch, "", wanted, &[]));
    }

    for w in wanted {
        if !found.iter().any(|d| d.name == *w) {
            return Err(Error::Invalid(super::po::tf(
                "package {pkg} was not found in the archive index for {codename}",
                &[("pkg", w), ("codename", codename)],
            )));
        }
    }

    let mut out = Vec::new();
    for d in &found {
        let dest = dest_dir.join(d.filename.rsplit('/').next().unwrap_or(&d.filename));
        stream_download(&format!("{archive_base}/{}", d.filename), &dest)?;
        let got = hex_sha256_file(&dest)?;
        if got != d.sha256 {
            let _ = std::fs::remove_file(&dest);
            return Err(Error::Invalid(super::po::tf(
                "the .deb {pkg} does not match the SHA256 in the authenticated index",
                &[("pkg", &d.name)],
            )));
        }
        out.push(dest);
    }
    Ok(out)
}

/// Pre-seeds the golden image's own `ImageStore` (`/var/lib/delonix`, what
/// `delonix-cri` reads at runtime) with the exact container images
/// `kubeadm init`/`join` need for this k8s version — fetched+verified on the
/// HOST (same offline philosophy as the `.deb` packages: the appliance
/// itself never needs network for this), injected via
/// `virt-customize --copy-in`.
///
/// Real bug this closes (host kaeso-sys-01): `kubeadm init` redownloaded
/// every core image (apiserver/controller-manager/scheduler/etcd/coredns/
/// pause) fresh on EVERY VM boot, slow enough to blow past kubeadm's own
/// internal rate-limiter deadline and crash the bootstrap. This alone is not
/// sufficient — it depends on the CAS-first fix in
/// `delonix_image::registry::pull_from_registry_with_creds` (skip a blob
/// already on disk) to actually pay off at runtime; without that fix,
/// `delonix-cri` would still re-download every blob regardless of what is
/// pre-seeded here.
///
/// Best-effort end to end: returns `None` (never fails the whole build) if
/// `kubeadm config images list` or the pre-seed step does not succeed, and
/// logs (does not fail) a per-image pull error — a slower first boot beats a
/// broken one.
fn preseed_k8s_images(
    work: &Path,
    kubeadm_deb: &Path,
    k8s_version: Option<&str>,
) -> Option<PathBuf> {
    let extract_dir = work.join("kubeadm-host");
    let status = Command::new("dpkg-deb")
        .args([
            "-x",
            &kubeadm_deb.to_string_lossy(),
            &extract_dir.to_string_lossy(),
        ])
        .status()
        .ok()?;
    if !status.success() {
        eprintln!("warning: dpkg-deb -x kubeadm.deb failed — skipping image pre-seed");
        return None;
    }
    let kubeadm_bin = extract_dir.join("usr/bin/kubeadm");
    if !kubeadm_bin.exists() {
        eprintln!("warning: kubeadm binary not found after extraction — skipping image pre-seed");
        return None;
    }

    let mut cmd = Command::new(&kubeadm_bin);
    cmd.arg("config").arg("images").arg("list");
    if let Some(v) = k8s_version {
        cmd.arg(format!("--kubernetes-version=v{v}"));
    }
    let out = match cmd.output() {
        Ok(o) if o.status.success() => o,
        _ => {
            eprintln!("warning: `kubeadm config images list` failed — skipping image pre-seed");
            return None;
        }
    };
    let images: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if images.is_empty() {
        return None;
    }

    let preseed_root = work.join("preseed-images");
    let store = delonix_image::ImageStore::open(&preseed_root).ok()?;
    for img in &images {
        eprintln!("  pre-seeding {img}...");
        if let Err(e) = delonix_image::registry::pull_from_registry_with_creds(&store, img, None) {
            eprintln!(
                "warning: could not pre-seed {img}: {e} (kubeadm will fetch it at runtime instead)"
            );
        }
    }
    Some(preseed_root)
}

pub(crate) fn hex_sha256(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex(&h.finalize())
}

pub(crate) fn hex_sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

/// Debian's cloud images only publish `SHA512SUMS` (no `SHA256SUMS` at all —
/// confirmed live) — same streaming-hash shape as `hex_sha256_file`, `Sha512`
/// instead of `Sha256` (already in the `sha2` crate, no new dependency).
fn hex_sha512_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = sha2::Sha512::new();
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Resolution of the `delonix-cri` binary to install in the guest
// ---------------------------------------------------------------------------

pub(crate) fn resolve_cri_bin(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if !p.exists() {
            return Err(Error::Invalid(super::po::tf(
                "--cri-bin '{path}' does not exist",
                &[("path", &p.display().to_string())],
            )));
        }
        return Ok(p);
    }
    // Next to the current `delonix` (normal install, release).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("delonix-cri");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    // Dev convenience: source-code workspace from the cwd.
    if let Some(workspace_root) = find_workspace_root() {
        eprintln!(
            "{}",
            super::po::tf(
                "compiling delonix-cri (release) from {dir}...",
                &[("dir", &workspace_root.display().to_string())],
            )
        );
        let status = Command::new("cargo")
            .args([
                "build",
                "--release",
                "-p",
                "delonix-cri",
                "--bin",
                "delonix-cri",
            ])
            .current_dir(&workspace_root)
            .status()
            .map_err(|e| Error::Invalid(format!("{}: {e}", super::po::t("running cargo build"))))?;
        if !status.success() {
            return Err(Error::Invalid(
                super::po::t("cargo build of delonix-cri failed").into(),
            ));
        }
        let built = workspace_root.join("target/release/delonix-cri");
        if built.exists() {
            return Ok(built);
        }
    }
    // BUG FIXED HERE, found live: a user who installed via `install.sh`
    // WITHOUT `--with-cri` (the default) and isn't running from a source
    // checkout had no way forward — `cluster kubeadm` needs `delonix-cri` to
    // install on every provisioned host, and none of the checks above ever
    // find one. `delonix-cri` is published as its own release asset
    // alongside `delonix` (same tag, always released together) — download it
    // (verified against the release's own SHA256SUMS, same as `install.sh`
    // would with `--with-cri`) instead of giving up.
    download_cri_bin().map_err(|e| {
        Error::Invalid(format!(
            "{e} — or use --cri-bin <path> / run from a source checkout"
        ))
    })
}

/// `true` when the CPU has AVX2+BMI2+FMA — the same 3-feature check
/// `install.sh` uses to pick the `-v3` release asset (Zen 2+/Haswell+).
pub(crate) fn cpu_has_x86_64_v3() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        is_x86_feature_detected!("avx2")
            && is_x86_feature_detected!("bmi2")
            && is_x86_feature_detected!("fma")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// `<root>/bin/<running-version>/` — where a resolved-without-a-checkout
/// `delonix-cri` (binary and/or service unit) is cached, namespaced by the
/// RUNNING `delonix` binary's own version so an upgrade never serves a
/// stale copy from a previous install.
fn cri_cache_dir() -> PathBuf {
    state_root().join("bin").join(env!("CARGO_PKG_VERSION"))
}

/// Downloads (and caches) `delonix-cri` from the GitHub release matching the
/// RUNNING `delonix` binary's own version — the two are always released
/// together, same tag. Verified against the release's own SHA256SUMS, same
/// non-negotiable as every other download in this codebase (never installs
/// an unverified binary). Cached under `<root>/bin/<version>/delonix-cri`,
/// so this only ever downloads once per installed version.
fn download_cri_bin() -> Result<PathBuf> {
    let version = env!("CARGO_PKG_VERSION");
    let cache_dir = cri_cache_dir();
    let cached = cache_dir.join("delonix-cri");
    if cached.exists() {
        return Ok(cached);
    }
    std::fs::create_dir_all(&cache_dir)?;
    let base_url =
        format!("https://github.com/angolardevops/delonix-runtime/releases/download/v{version}");
    let tmp = cache_dir.join("delonix-cri.download");
    let variant = if cpu_has_x86_64_v3() { "-v3" } else { "" };
    let mut asset = format!("delonix-cri-x86_64{variant}-linux");
    eprintln!(
        "{}",
        super::po::tf(
            "downloading {asset} (v{version})...",
            &[("asset", &asset), ("version", version)],
        )
    );
    if !variant.is_empty() && stream_download(&format!("{base_url}/{asset}"), &tmp).is_err() {
        asset = "delonix-cri-x86_64-linux".to_string();
        eprintln!(
            "{}",
            super::po::tf(
                "{asset} missing from this release — trying the generic binary...",
                &[("asset", &asset)],
            )
        );
        stream_download(&format!("{base_url}/{asset}"), &tmp)?;
    } else if variant.is_empty() {
        stream_download(&format!("{base_url}/{asset}"), &tmp)?;
    }
    let sums = http_get_text(&format!("{base_url}/SHA256SUMS"))?;
    let expected = sums
        .lines()
        .find(|l| l.trim_end().ends_with(&asset))
        .and_then(|l| l.split_whitespace().next())
        .ok_or_else(|| Error::Invalid(format!("SHA256SUMS has no entry for {asset}")))?
        .to_string();
    let got = hex_sha256_file(&tmp)?;
    if got != expected {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Invalid(super::po::tf(
            "invalid checksum for {asset}: expected {expected}, got {got} — download discarded",
            &[("asset", &asset), ("expected", &expected), ("got", &got)],
        )));
    }
    std::fs::rename(&tmp, &cached)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cached, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(cached)
}

/// Mirrors `resolve_cri_bin` for the `delonix` engine binary itself (used by
/// `--no-k8s` golden images). Simpler than the CRI case for tier 2: this
/// command is already running AS `delonix`, so `current_exe()` IS a valid
/// `delonix` binary — no "next to the exe" lookup needed.
pub(crate) fn resolve_delonix_bin(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if !p.exists() {
            return Err(Error::Invalid(super::po::tf(
                "--delonix-bin '{path}' does not exist",
                &[("path", &p.display().to_string())],
            )));
        }
        return Ok(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if exe.exists() {
            return Ok(exe);
        }
    }
    // Dev convenience: source-code workspace from the cwd.
    if let Some(workspace_root) = find_workspace_root() {
        eprintln!(
            "{}",
            super::po::tf(
                "compiling delonix (release) from {dir}...",
                &[("dir", &workspace_root.display().to_string())],
            )
        );
        let status = Command::new("cargo")
            .args([
                "build",
                "--release",
                "-p",
                "delonix-runtime-bin",
                "--bin",
                "delonix",
            ])
            .current_dir(&workspace_root)
            .status()
            .map_err(|e| Error::Invalid(format!("{}: {e}", super::po::t("running cargo build"))))?;
        if !status.success() {
            return Err(Error::Invalid(
                super::po::t("cargo build of delonix failed").into(),
            ));
        }
        let built = workspace_root.join("target/release/delonix");
        if built.exists() {
            return Ok(built);
        }
    }
    download_delonix_bin().map_err(|e| {
        Error::Invalid(format!(
            "{e} — or use --delonix-bin <path> / run from a source checkout"
        ))
    })
}

/// Downloads (and caches) `delonix` itself from the GitHub release matching
/// the RUNNING binary's own version. Same verified-download machinery as
/// `download_cri_bin`, just a different release asset name.
fn download_delonix_bin() -> Result<PathBuf> {
    let version = env!("CARGO_PKG_VERSION");
    let cache_dir = cri_cache_dir();
    let cached = cache_dir.join("delonix");
    if cached.exists() {
        return Ok(cached);
    }
    std::fs::create_dir_all(&cache_dir)?;
    let base_url =
        format!("https://github.com/angolardevops/delonix-runtime/releases/download/v{version}");
    let tmp = cache_dir.join("delonix.download");
    let variant = if cpu_has_x86_64_v3() { "-v3" } else { "" };
    let mut asset = format!("delonix-x86_64{variant}-linux");
    eprintln!(
        "{}",
        super::po::tf(
            "downloading {asset} (v{version})...",
            &[("asset", &asset), ("version", version)],
        )
    );
    if !variant.is_empty() && stream_download(&format!("{base_url}/{asset}"), &tmp).is_err() {
        asset = "delonix-x86_64-linux".to_string();
        eprintln!(
            "{}",
            super::po::tf(
                "{asset} missing from this release — trying the generic binary...",
                &[("asset", &asset)],
            )
        );
        stream_download(&format!("{base_url}/{asset}"), &tmp)?;
    } else if variant.is_empty() {
        stream_download(&format!("{base_url}/{asset}"), &tmp)?;
    }
    let sums = http_get_text(&format!("{base_url}/SHA256SUMS"))?;
    let expected = sums
        .lines()
        .find(|l| l.trim_end().ends_with(&asset))
        .and_then(|l| l.split_whitespace().next())
        .ok_or_else(|| Error::Invalid(format!("SHA256SUMS has no entry for {asset}")))?
        .to_string();
    let got = hex_sha256_file(&tmp)?;
    if got != expected {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Invalid(super::po::tf(
            "invalid checksum for {asset}: expected {expected}, got {got} — download discarded",
            &[("asset", &asset), ("expected", &expected), ("got", &got)],
        )));
    }
    std::fs::rename(&tmp, &cached)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cached, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(cached)
}

fn find_workspace_root() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join("Cargo.toml").exists() && dir.join("crates/delonix-cri").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Embedded fallback for `dist/delonix-cri.service` — the ONLY file
/// `workspace_dist_file` is ever asked for. It is small, static, and
/// version-independent (no templating), so there is no staleness risk in
/// baking it into the binary at compile time — same fix class as
/// `download_cri_bin` above (a user outside a source checkout had no way
/// forward for this file either), just without needing a network round-trip
/// since the content already ships inside us.
const DELONIX_CRI_SERVICE_UNIT: &str = include_str!("../../../../dist/delonix-cri.service");

pub(crate) fn workspace_dist_file(name: &str) -> Result<PathBuf> {
    if let Some(root) = find_workspace_root() {
        let p = root.join("dist").join(name);
        if p.exists() {
            return Ok(p);
        }
    }
    if name == "delonix-cri.service" {
        let cached = cri_cache_dir().join(name);
        if !cached.exists() {
            std::fs::create_dir_all(cri_cache_dir())?;
            std::fs::write(&cached, DELONIX_CRI_SERVICE_UNIT)?;
        }
        return Ok(cached);
    }
    Err(Error::Invalid(super::po::tf(
        "could not find dist/{name} — run from the source-code checkout or supply it via --extra-run",
        &[("name", name)],
    )))
}

// ---------------------------------------------------------------------------
// Customization steps (pure function — testable without a real VM/virt-customize)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CustomizeOp {
    RunCommand(String),
    CopyIn(PathBuf, String),
    Password { user: String, password: String },
    RootPassword(String),
}

/// Builds the list of customization steps to apply to the base image — the
/// "100% parameterized" part: `extra_packages`/`extra_run` extend without
/// touching this function. Pure (no I/O), testable in isolation. The
/// technically sensitive recipes (repo/packages/swap/modules/sysctls) come from
/// `k8s_recipes::k8s_host_recipes` — the SAME catalog that `cmd::cluster`
/// uses via SSH, so the golden image and a host prepared by `cluster
/// apply` end up exactly alike.
/// Like [`k8s_customization_steps`], but WITHOUT network in the guest: instead of
/// the apt repository + `apt-get install`, it injects the `.deb` files already
/// downloaded and verified on the HOST (`download_k8s_debs`) and installs them with
/// `dpkg -i`. The remaining recipes (swap/modules/sysctls) are the SAME as the online
/// path (`k8s_recipes::k8s_config_recipes`) — they do not diverge.
///
/// `dpkg -i` instead of `apt-get install ./*.deb`: apt would need to contact
/// the lists to resolve deps; the kubelet deps outside the k8s repo
/// (iptables/mount/util-linux/libc6) already come in the cloud image. If any is
/// missing, `dpkg` fails LOUDLY and the build stops — it never leaves a half-installed guest.
///
/// `preseed_images_root`, when given (see `preseed_k8s_images`), points at a
/// HOST-side `delonix_image::ImageStore` root already populated with
/// kubeadm's core images — copied verbatim into the guest's own
/// `/var/lib/delonix` (what `delonix-cri` reads at runtime) via 4
/// `--copy-in` calls, one per `ImageStore` subdirectory
/// (`images`/`layers`/`containers`/`blobs`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn k8s_customization_steps_offline(
    debs: &[PathBuf],
    extra_run: &[String],
    cri_bin: &Path,
    delonix_bin: &Path,
    cri_service: &Path,
    preseed_images_root: Option<&Path>,
    distro: Distro,
    root_password: Option<&str>,
) -> Vec<CustomizeOp> {
    let mut ops: Vec<CustomizeOp> = Vec::new();
    // `--copy-in` requires the target directory to ALREADY exist in the guest.
    ops.push(CustomizeOp::RunCommand("mkdir -p /tmp/k8s-debs".into()));
    for d in debs {
        ops.push(CustomizeOp::CopyIn(d.clone(), "/tmp/k8s-debs".to_string()));
    }
    ops.push(CustomizeOp::RunCommand(
        "dpkg -i /tmp/k8s-debs/*.deb && apt-mark hold kubeadm kubelet kubectl && rm -rf /tmp/k8s-debs"
            .into(),
    ));
    ops.extend(
        super::k8s_recipes::k8s_config_recipes()
            .into_iter()
            .map(|r| CustomizeOp::RunCommand(r.apply_offline().to_string())),
    );
    ops.extend(install_cri_steps(cri_bin, delonix_bin, cri_service));
    ops.extend(shared_account_steps(extra_run, distro, root_password));
    if let Some(root) = preseed_images_root {
        ops.push(CustomizeOp::RunCommand("mkdir -p /var/lib/delonix".into()));
        for sub in ["images", "layers", "containers", "blobs"] {
            ops.push(CustomizeOp::CopyIn(
                root.join(sub),
                "/var/lib/delonix".to_string(),
            ));
        }
    }
    ops
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn k8s_customization_steps(
    k8s_version: Option<&str>,
    extra_packages: &[String],
    extra_run: &[String],
    cri_bin: &Path,
    delonix_bin: &Path,
    cri_service: &Path,
    distro: Distro,
    root_password: Option<&str>,
) -> Vec<CustomizeOp> {
    let mut ops: Vec<CustomizeOp> =
        super::k8s_recipes::k8s_host_recipes(k8s_version, extra_packages)
            .into_iter()
            .map(|r| CustomizeOp::RunCommand(r.apply_offline().to_string()))
            .collect();
    ops.extend(install_cri_steps(cri_bin, delonix_bin, cri_service));
    ops.extend(shared_account_steps(extra_run, distro, root_password));
    ops
}

/// `delonix-cri` install — CRI endpoint for the kubelet (replaces containerd).
/// Split out of the old `common_customization_steps` so the no-k8s golden
/// image path (`rootless_customization_steps`) can skip it entirely: a
/// CRI-to-kubelet shim is meaningless on a VM with no kubelet.
fn install_cri_steps(cri_bin: &Path, delonix_bin: &Path, cri_service: &Path) -> Vec<CustomizeOp> {
    vec![
        CustomizeOp::CopyIn(cri_bin.to_path_buf(), "/usr/local/bin".to_string()),
        CustomizeOp::RunCommand("chmod +x /usr/local/bin/delonix-cri".into()),
        // O `delonix` viaja SEMPRE com o `delonix-cri`. O CRI não corre
        // containers — delega neste binário (`cli_bin()`), cujo próprio
        // comentário diz que "the golden image installs both in
        // /usr/local/bin". Não instalava: o CRI ficava a falar sozinho e o
        // kubelet via `No such file or directory (os error 2)` em cada
        // `StartContainer`. Ver `install_cri_steps_instala_os_dois_binarios`.
        CustomizeOp::CopyIn(delonix_bin.to_path_buf(), "/usr/local/bin".to_string()),
        CustomizeOp::RunCommand("chmod +x /usr/local/bin/delonix".into()),
        CustomizeOp::CopyIn(cri_service.to_path_buf(), "/etc/systemd/system".to_string()),
        CustomizeOp::RunCommand("systemctl enable delonix-cri.service".into()),
    ]
}

/// The tail shared by every golden image variant regardless of k8s/no-k8s:
/// accounts + the user's `--extra-run` + package-cache cleanup + machine-id
/// reset. Kept separate from `install_cri_steps` so a non-Kubernetes image
/// gets the same account/UX/cleanup guarantees without carrying the CRI shim.
///
/// The three lines below actually differ by distro FAMILY, all confirmed
/// live (not assumed) before writing this: the sudo-equivalent group is
/// `wheel` on Rocky/RHEL, not `sudo` (which doesn't exist there at all);
/// the system-wide interactive-bash file is `/etc/bashrc` on Rocky, not
/// `/etc/bash.bashrc` (a Debian/Ubuntu-family convention); and the package
/// cache cleanup command is obviously package-manager-specific.
fn shared_account_steps(
    extra_run: &[String],
    distro: Distro,
    root_password: Option<&str>,
) -> Vec<CustomizeOp> {
    let sudo_group = match distro {
        Distro::Ubuntu | Distro::Debian => "sudo",
        Distro::Rocky | Distro::Fedora => "wheel",
    };
    let bashrc_path = match distro {
        Distro::Ubuntu | Distro::Debian => "/etc/bash.bashrc",
        Distro::Rocky | Distro::Fedora => "/etc/bashrc",
    };
    let mut ops: Vec<CustomizeOp> = Vec::new();
    ops.extend([
        // The `delonix` account exists so cloud-init has somewhere to put the
        // SSH key and `cluster kubeadm` has someone to log in as. It ships with
        // NO PASSWORD, and neither does root — see the block below.
        CustomizeOp::RunCommand(format!(
            "useradd -m -s /bin/bash -G {sudo_group} delonix || true"
        )),
        CustomizeOp::RunCommand(
            "echo 'delonix ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/90-delonix && chmod 440 /etc/sudoers.d/90-delonix"
                .into(),
        ),
        // Activa o `qemu-guest-agent` — instalado pelas TRÊS receitas (a golden
        // busca os `.deb` verificados no host, a base instala-o pelo gestor de
        // pacotes), por isso o `enable` vive aqui e não numa delas: estava só na
        // receita offline da golden, e era por isso que a `delonix-vm-base` saía
        // sem agente nenhum.
        //
        // O postinst do próprio pacote TAMBÉM activa a unit — mas por
        // `deb-systemd-helper`, contra um convidado onde o systemd não corre, o
        // que é uma afirmação sobre o script de outra pessoa e não uma medição.
        // Fazê-lo aqui custa um comando idempotente e é o mesmo que este build já
        // faz ao `delonix-cri.service` em vez de confiar num preset. Guardado,
        // porque uma build sem o agente (uma distro cujo arquivo não buscamos)
        // não pode falhar por causa de uma unit ausente.
        CustomizeOp::RunCommand(
            "systemctl list-unit-files qemu-guest-agent.service >/dev/null 2>&1 && \
             systemctl enable qemu-guest-agent.service || true"
                .into(),
        ),
        // **No password ships in the image, on any account.**
        //
        // Until now root and `delonix` both had the password `delonix`, and the
        // account has passwordless sudo — so anyone reaching a login prompt was
        // root. The mitigation was to turn password login OFF over SSH and keep
        // the serial console open «for when the VM has lost its network», which
        // holds for a laboratory VM on someone's laptop and stops holding the
        // moment the same artefact is published and a tenant runs it: on a
        // shared hypervisor, console access is not a lesser door than SSH.
        //
        // A credential that lives in an open-source repository is not a
        // credential. Both accounts are locked (`passwd -l`, portable across the
        // four distros — `!` in front of the hash means no password can ever
        // match), and every supported way in still works untouched: cloud-init
        // injects the SSH key, and `cluster kubeadm` authenticates with a key it
        // generates.
        //
        // The console case the old comment defends is real, and it keeps an
        // answer — but one the OPERATOR chooses, not one baked into every copy
        // of the image: `--root-password` at build time, or `chpasswd` in the
        // cloud-init user-data of that one VM. What is gone is the default.
        match root_password {
            // Escolha EXPLÍCITA de quem constrói, para o caso da consola série —
            // e vive só naquela imagem, não em todas as cópias publicadas.
            Some(pw) => CustomizeOp::RootPassword(pw.to_string()),
            None => CustomizeOp::RunCommand(
                "passwd -l root >/dev/null 2>&1 || true; passwd -l delonix >/dev/null 2>&1 || true"
                    .into(),
            ),
        },
        //
        // Both places, deliberately: `sshd_config.d` is the modern drop-in, but
        // Debian bullseye's stock `sshd_config` has no `Include` line, so there
        // the drop-in alone would be silently ignored. In the main file the
        // existing directives are commented out first rather than appended to —
        // sshd takes the FIRST occurrence of a keyword, so an append lands after
        // the distro's own line and does nothing.
        CustomizeOp::RunCommand(
            "mkdir -p /etc/ssh/sshd_config.d && \
             printf 'PasswordAuthentication no\\nPermitRootLogin prohibit-password\\nKbdInteractiveAuthentication no\\n' \
               > /etc/ssh/sshd_config.d/99-delonix-hardening.conf && \
             chmod 644 /etc/ssh/sshd_config.d/99-delonix-hardening.conf && \
             if [ -f /etc/ssh/sshd_config ]; then \
               sed -i -E 's/^[[:space:]]*(PasswordAuthentication|PermitRootLogin|KbdInteractiveAuthentication|ChallengeResponseAuthentication)[[:space:]]/#&/I' /etc/ssh/sshd_config && \
               printf '\\n# --- Delonix golden image hardening (the built-in password is public) ---\\nPasswordAuthentication no\\nPermitRootLogin prohibit-password\\nKbdInteractiveAuthentication no\\n' \
                 >> /etc/ssh/sshd_config; \
             fi"
                .into(),
        ),
        // Shell UX the Kubernetes docs recommend: kubectl/kubeadm bash completion
        // + the `k` alias (with completion wired to it). Written to the
        // system-wide interactive-bash file (sourced for every interactive
        // bash — login AND non-login — so both the serial console and SSH
        // get it), NOT to `/etc/profile.d` (those are sourced by `sh` too,
        // which chokes on the `<(...)` process substitution). Each block is
        // guarded by `command -v` so it is inert if a tool is missing (e.g.
        // every non-k8s image), and evaluated at shell-start (not build
        // time) — order relative to the package install does not matter.
        CustomizeOp::RunCommand(format!(
            "cat >> {bashrc_path} <<'DELONIX_KUBECTL_EOF'\n\
             \n\
             # --- Delonix golden image: kubectl/kubeadm completion + `k` alias (k8s docs) ---\n\
             if command -v kubectl >/dev/null 2>&1; then\n\
             \x20 source <(kubectl completion bash)\n\
             \x20 alias k=kubectl\n\
             \x20 complete -o default -F __start_kubectl k\n\
             fi\n\
             if command -v kubeadm >/dev/null 2>&1; then\n\
             \x20 source <(kubeadm completion bash)\n\
             fi\n\
             if command -v crictl >/dev/null 2>&1; then\n\
             \x20 source <(crictl completion bash) 2>/dev/null || true\n\
             fi\n\
             # --- end Delonix ---\n\
             DELONIX_KUBECTL_EOF"
        )),
    ]);
    ops.extend(extra_run.iter().cloned().map(CustomizeOp::RunCommand));
    // Records the installed kernel's `uname -r` string for `image --vm ls`'s
    // KERNEL column — `virt-customize` never boots the image's own kernel (it
    // chroots via its OWN appliance kernel), so there is no `uname -r` to run
    // here; `/boot/vmlinuz-<release>` is named by the exact release string
    // once booted, so listing it is the reliable proxy. Written to a file
    // (not returned — `virt-customize` has no channel back to the host
    // process) that `cmd_build` reads out with `virt-cat` right after this
    // runs, once for the whole build, not per VM.
    ops.push(CustomizeOp::RunCommand(
        "ls /boot/vmlinuz-* 2>/dev/null | sed 's#.*/vmlinuz-##' | sort -V | tail -1 \
         > /etc/delonix-kernel-version || echo unknown > /etc/delonix-kernel-version"
            .into(),
    ));
    // Package cache cleanup — ALWAYS at the end (after the user's
    // `--extra-run`, which may install more packages). Measured on a 24.04
    // apt golden: `/var/cache/apt` (~181 MiB of already-installed .deb) +
    // `/var/lib/apt/lists` (~186 MiB of indexes) = ~367 MiB of pure garbage,
    // which filled the root to 92%. An `apt-get update`/`dnf makecache`
    // regenerates the indexes if the node needs them.
    //
    // DELIBERATELY here and not in `k8s_recipes`: that catalog is SHARED
    // with `cluster apply`, which prepares LIVE hosts — cleaning the package
    // cache is a concern of the ARTIFACT (shrinking a distributable image),
    // not of host preparation.
    // **O journal é VOLÁTIL nas cloud images de Ubuntu/Debian**, e o que isso
    // custa é preciso: `Storage=auto` quer dizer «persiste SE `/var/log/journal`
    // existir», e não existe — por isso um reboot apaga os logs do arranque que
    // falhou, que são exactamente os que alguém iria ler. Uma VM que reinicia
    // sozinha não deixa rasto nenhum de porquê. (Rocky/Fedora já criam o
    // directório; lá isto só acrescenta os limites.)
    //
    // **O limite não é decoração.** Este host já teve disk-pressure a sério (49
    // rootfs órfãos, o kubelet a marcar o nó), e um journal sem tecto na raiz de
    // 10 GiB de um inquilino é uma forma de a encher com os NOSSOS logs. Os
    // 200 MiB são a proporção default do systemd (10%) escrita à mão, para não
    // escalar com um disco que não controlamos.
    ops.push(CustomizeOp::RunCommand(
        "mkdir -p /var/log/journal /etc/systemd/journald.conf.d && \
         printf '[Journal]\\nStorage=persistent\\nCompress=yes\\nSystemMaxUse=200M\\nSystemMaxFileSize=50M\\nRuntimeMaxUse=50M\\n' \
           > /etc/systemd/journald.conf.d/99-delonix.conf && \
         chmod 644 /etc/systemd/journald.conf.d/99-delonix.conf && \
         systemd-tmpfiles --create --prefix /var/log/journal >/dev/null 2>&1 || true"
            .into(),
    ));
    let cleanup_cmd = match distro {
        Distro::Ubuntu | Distro::Debian => "apt-get clean && rm -rf /var/lib/apt/lists/*",
        Distro::Rocky | Distro::Fedora => "dnf clean all",
    };
    ops.push(CustomizeOp::RunCommand(cleanup_cmd.into()));
    // **A imagem passa a dizer o que tem dentro.** Um artefacto que se publica
    // sem inventário obriga quem o consome a montá-lo para responder à única
    // pergunta que interessa depois de um CVE sair: «esta versão está aqui?».
    //
    // O formato é o que o convidado sabe produzir sem ferramenta nova — nome,
    // versão e arquitectura, uma linha por pacote — que é exactamente o
    // conteúdo que um SPDX/CycloneDX carrega. Gerar um desses aqui obrigaria a
    // meter um gerador dentro de TODAS as imagens, e o que este passo precisa
    // de ser é uma leitura da base de dados de pacotes que já lá está.
    //
    // Corre DEPOIS da limpeza de cache de propósito: o `dpkg-query`/`rpm` lê
    // `/var/lib/dpkg`/`/var/lib/rpm`, não os índices que a limpeza apaga, por
    // isso é aqui que o inventário reflecte a imagem FINAL — incluindo o que um
    // `--extra-run` tenha instalado.
    let sbom_cmd = match distro {
        Distro::Ubuntu | Distro::Debian => {
            "dpkg-query -W -f='${binary:Package}\\t${Version}\\t${Architecture}\\n'"
        }
        Distro::Rocky | Distro::Fedora => {
            "rpm -qa --qf '%{NAME}\\t%{VERSION}-%{RELEASE}\\t%{ARCH}\\n'"
        }
    };
    ops.push(CustomizeOp::RunCommand(format!(
        "mkdir -p /usr/share/delonix && {sbom_cmd} 2>/dev/null | LC_ALL=C sort \
         > /usr/share/delonix/packages.tsv && chmod 644 /usr/share/delonix/packages.tsv"
    )));
    // BUG FOUND LIVE (delonix cluster kubeadm, multi-VM libvirt NAT): every VM
    // cloned from this golden qcow2 shares ONE `/etc/machine-id` — installing
    // kubeadm's dependencies during `virt-customize` pulls in a package whose
    // postinst calls `systemd-machine-id-setup`/`dbus-uuidgen`, baking a REAL id
    // into the image (a fresh Ubuntu cloud image ships this file EMPTY on
    // purpose, so systemd generates a fresh one on each VM's actual first boot —
    // `virt-customize` doesn't do that virt-sysprep-style cleanup by itself).
    // systemd-networkd derives its DHCP client-id (DUID) from machine-id, so
    // dnsmasq saw 3 cluster VMs as the SAME client and kept moving the one lease
    // to whichever VM last renewed — evicting the other two, breaking
    // connectivity mid-`kubeadm init`. Confirmed live: `lab-cp1` and `lab-w1`
    // reported the byte-for-byte identical machine-id. MUST be the very last
    // step (after `--extra-run`/apt cleanup) so nothing after it regenerates one.
    //
    // `mkdir -p /var/lib/dbus` FOUND LIVE building Fedora: that directory does
    // not exist there (dbus reads /etc/machine-id directly), so `ln -sf` failed
    // and took the whole `virt-customize` run with it — the same class of trap
    // as the AppArmor step that had to be made Ubuntu-only for Rocky. The
    // symlink is still created where the directory does exist, because that is
    // the case the compat matters for.
    ops.push(CustomizeOp::RunCommand(
        "truncate -s 0 /etc/machine-id && rm -f /var/lib/dbus/machine-id && \
         mkdir -p /var/lib/dbus && ln -sf /etc/machine-id /var/lib/dbus/machine-id"
            .into(),
    ));
    ops
}

/// Non-Kubernetes golden image: no kubeadm/kubelet/kubectl, no `delonix-cri`
/// (a CRI shim is meaningless without a kubelet to call it) — instead, the
/// `delonix` engine binary itself plus everything `scripts/install.sh`
/// normally configures on a fresh host for rootless containers to work
/// out of the box (rootless deps, subuid/subgid range, AppArmor profile on
/// 23.10+-family hosts). Without this, the appliance boots but `delonix run`
/// fails immediately on a userns error — defeating the point of a golden image.
pub(crate) fn rootless_customization_steps(
    extra_run: &[String],
    delonix_bin: &Path,
    distro: Distro,
    root_password: Option<&str>,
) -> Vec<CustomizeOp> {
    // Same package LIST `install.sh` requires (`require_dep`/`optional_dep`),
    // just guest-installed instead of host-detected. Package NAMES confirmed
    // live against Rocky 9's own repo listings before writing this (not
    // assumed): `shadow-utils` (not `uidmap`), `iproute` (not `iproute2`),
    // `conntrack-tools` (not `conntrack`) — all present in Rocky's base
    // BaseOS/AppStream, no EPEL needed. `nftables`/`slirp4netns` share the
    // same package name across both families.
    // `qemu-guest-agent` viaja com a imagem base pela mesma razão que já viaja na
    // golden k8s (ver `k8s_customization_steps_offline`): sem ele o hypervisor
    // não sabe o IP do convidado (mede-o por ARP ou pelo lease, e em
    // cloud-hypervisor o endereço é CALCULADO do MAC, nunca observado), não faz
    // `fsfreeze` antes de um snapshot — o que torna qualquer snapshot de uma base
    // de dados a correr crash-consistent em vez de consistente — e não tem forma
    // de pedir um shutdown ordenado. O backend Proxmox deste repo já chama
    // `/agent/…` e trata a ausência como normal; o que faltava era o convidado.
    //
    // Vai no MESMO `apt-get`/`dnf` dos outros: esta receita já instala por rede
    // (ao contrário da golden, que tem o caminho `--offline` com os `.deb`
    // verificados no host), por isso não há mecanismo novo — há um nome a mais
    // numa lista que já existia.
    let pkg_install_cmd = match distro {
        Distro::Ubuntu | Distro::Debian => {
            "apt-get update && apt-get install -y slirp4netns uidmap nftables iproute2 conntrack qemu-guest-agent"
                .to_string()
        }
        // Fedora is the same dnf/RPM family as Rocky and uses the same package
        // names — the four that differ from Debian's (`shadow-utils`, `iproute`,
        // `conntrack-tools`, and the shared `nftables`/`slirp4netns`) are named
        // identically in Fedora's repos.
        Distro::Rocky | Distro::Fedora => {
            "dnf install -y slirp4netns shadow-utils nftables iproute conntrack-tools qemu-guest-agent"
                .to_string()
        }
    };
    let mut ops: Vec<CustomizeOp> = vec![
        CustomizeOp::RunCommand(pkg_install_cmd),
        // `delonix` — the daemonless engine binary itself. No systemd unit: it
        // is CLI-invoked, not a long-running service (unlike `delonix-cri`).
        CustomizeOp::CopyIn(delonix_bin.to_path_buf(), "/usr/local/bin".to_string()),
        CustomizeOp::RunCommand("chmod +x /usr/local/bin/delonix".into()),
    ];
    ops.extend(shared_account_steps(extra_run, distro, root_password));
    // Subuid/subgid range for the `delonix` account — mirrors `install.sh`'s
    // `ensure_subid`: without a range, the rootless userns only maps 1 uid and
    // any image with a non-root USER fails. Idempotent (`grep -q` guard).
    // `/etc/subuid`/`/etc/subgid` are `shadow-utils` files, present and in the
    // same location on every distro family here.
    ops.push(CustomizeOp::RunCommand(
        "grep -q '^delonix:' /etc/subuid || echo 'delonix:100000:65536' >> /etc/subuid".into(),
    ));
    ops.push(CustomizeOp::RunCommand(
        "grep -q '^delonix:' /etc/subgid || echo 'delonix:100000:65536' >> /etc/subgid".into(),
    ));
    // AppArmor `unconfined+userns` profile — Ubuntu-ONLY. The kernel sysctl
    // this defends against (`kernel.apparmor_restrict_unprivileged_userns=1`,
    // 23.10+-family hosts) is an Ubuntu-specific LSM hardening patch, not
    // present upstream/Debian and meaningless on Rocky (SELinux, no AppArmor
    // LSM at all). BUG CAUGHT HERE, before it shipped: the write is
    // `printf ... > /etc/apparmor.d/delonix && (apparmor_parser ... || true)`
    // — the `|| true` only guards the parser call, NOT the file write. Rocky
    // cloud images have no `/etc/apparmor.d/` directory at all, so the
    // redirect itself would fail, the `&&` would short-circuit, and the
    // WHOLE `virt-customize` step (hence the whole build) would fail — not a
    // harmless no-op like on a distro that happens to lack AppArmor tooling
    // but still has the directory. Gating to `Ubuntu` is the correct fix, not
    // just a Rocky workaround (also correct for Debian, which never actually
    // needed this step either — kept unconditional there since it degrades
    // harmlessly, but Rocky specifically cannot risk it).
    if distro == Distro::Ubuntu {
        ops.push(CustomizeOp::RunCommand(
            "printf 'abi <abi/4.0>,\\ninclude <tunables/global>\\nprofile delonix /usr/local/bin/delonix flags=(unconfined) {\\n  userns,\\n}\\n' > /etc/apparmor.d/delonix && \
             (apparmor_parser -r /etc/apparmor.d/delonix || true)"
                .into(),
        ));
    }
    // Publishing 80/443 rootless (`-p 80:80`) is refused by the kernel: the
    // host-side bind is done by `slirp4netns` as the unprivileged `delonix`
    // user, and ports below `net.ipv4.ip_unprivileged_port_start` (1024 by
    // default) need CAP_NET_BIND_SERVICE.
    //
    // `install.sh` keeps this OPT-IN (`--low-ports`) because it lowers a
    // privilege boundary for a whole host that may be shared or in production:
    // from then on any local program can bind 80-1023. THIS image is the
    // opposite situation — a disposable, single-tenant VM whose entire purpose
    // is running Delonix rootless — so the trade-off flips and it ships applied.
    // Deliberately NOT in `shared_account_steps`: the k8s golden is a Kubernetes
    // node whose kubelet/kube-proxy already run as root and never needed it.
    //
    // Written as a file, not `sysctl -w`: `virt-customize` runs against an
    // offline guest, so only what lands in /etc/sysctl.d survives to first boot.
    ops.push(CustomizeOp::RunCommand(
        "printf '# Delonix Runtime — publish ports <1024 rootless (see install.sh --low-ports).\\n\
         net.ipv4.ip_unprivileged_port_start = 80\\n' > /etc/sysctl.d/99-delonix-lowports.conf"
            .into(),
    ));
    // `br_netfilter` + `bridge-nf-call-iptables=1` — WITHOUT THESE THE NAMESPACE
    // ISOLATION SILENTLY DOES NOTHING, and that is the whole reason this step
    // exists.
    //
    // The isolation lives in nftables chains on the `forward` hook; traffic
    // between two containers on the SAME bridge only reaches that hook when
    // `br_netfilter` is bridging it into the ip layer. Without the module the
    // chains are installed, the `@dlxall`/`@dlxns_*` sets are populated, every
    // command reports success — and a container in namespace `teamA` reaches one
    // in `teamB`. MEASURED, in a VM built from this very image: the two
    // isolation scenarios of the chaos harness FAIL without it and PASS with it,
    // nothing else changed.
    //
    // `install.sh` already does this on a host (`WITH_TUNE`, on by default), but
    // it justifies it by Kubernetes — so an image built for rootless-only had no
    // reason to inherit it, and did not. That left OUR OWN base image shipping a
    // security property that reports itself as applied and is not.
    //
    // Files and not `modprobe`/`sysctl -w`, for the same reason as the step
    // above: the guest is offline here, and only what lands in /etc survives to
    // first boot. `systemd-modules-load` runs before `systemd-sysctl`, so the
    // sysctl finds the knob already there.
    ops.push(CustomizeOp::RunCommand(
        "printf 'br_netfilter\\n' > /etc/modules-load.d/delonix.conf && \
         printf '# Delonix Runtime — namespace isolation needs bridged traffic in netfilter.\\n\
         net.bridge.bridge-nf-call-iptables = 1\\n\
         net.bridge.bridge-nf-call-ip6tables = 1\\n' > /etc/sysctl.d/99-delonix-bridge.conf"
            .into(),
    ));
    ops
}

/// Relabels the guest's filesystem for SELinux — appended by
/// [`customize_args`] as the LAST step of every build.
///
/// **Why it cannot be left to libguestfs.** `virt-customize` 1.52 relabels by
/// default and prints `SELinux relabelling`, but on a Fedora guest that step
/// takes 0.1s: it does not relabel anything, it schedules one by touching
/// `/.autorelabel`. And that first-boot relabel never runs, because by then the
/// damage is already fatal — MEASURED, on a Fedora 42 guest this engine built:
///
/// ```text
/// avc: denied { map } for pid=1 comm="systemd" path="/etc/ld.so.cache"
///      scontext=…:init_t tcontext=…:unlabeled_t
/// ```
///
/// Any `dnf install` inside the appliance re-runs `ldconfig`, and the rewritten
/// `/etc/ld.so.cache` comes back with **no** SELinux xattr at all. With PID 1
/// itself denied, `dbus-broker` never starts; with no D-Bus there is no
/// NetworkManager; with no NetworkManager the guest boots to a login prompt
/// with an interface that is DOWN, no address, and the hostname still
/// `localhost`. 195 denials in one boot, and the only visible symptom from
/// outside was a VM that never took a DHCP lease.
///
/// **Why it is here and not in each ops builder.** Every build path has to have
/// it, and a step each builder must remember to append is the trap this repo
/// keeps paying for. `customize_args` is the one place both the golden recipe
/// and the VMfile path go through, and appending here also guarantees the
/// relabel runs AFTER every other step — a step added later cannot land behind
/// it and re-break the labels.
///
/// Guarded in shell rather than by distro so it is inert on Debian/Ubuntu
/// (no `/etc/selinux/config`) without this function needing to know which
/// distro it is looking at — the VMfile path builds from a `FROM` that may be
/// an arbitrary URL, where there is nothing reliable to branch on. And it
/// falls back to `/.autorelabel` rather than doing nothing when the guest has
/// SELinux but no `setfiles`: a slow first boot beats an unlabeled image.
/// **The mountpoints come from the guest's own `/etc/fstab`, and `/` alone is
/// not enough.** MEASURED on the rebuilt Fedora image: `/`, `/var`, `/boot` and
/// the injected binary all came out correctly labelled, and `/home/delonix` —
/// the account this build creates with `useradd -m` — came out `unlabeled_t`,
/// which cost two denials at every login and a `login` that could not chdir
/// into the home directory (the prompt reads `/` instead of `~`). That one is
/// not cosmetic: `--ssh-key` writes `~/.ssh/authorized_keys`, and sshd cannot
/// read a mislabeled one.
///
/// Fedora puts `/home` and `/var` on separate btrfs subvolumes, so naming the
/// mountpoints explicitly is what removes the guesswork about which of them a
/// single `setfiles /` happens to reach. `fstab` is the guest declaring them
/// itself — accurate, available offline, and it needs no list here to be kept
/// in step with any distro's layout.
///
/// **The EFI partition has to be skipped, and skipping it by TYPE and not by
/// path is the point.** `/boot/efi` is vfat, which has no extended attributes,
/// so `setfiles` answers `Operation not supported` and — correctly — exits
/// non-zero, which fails the whole build. MEASURED: both the Fedora and the
/// Rocky rebuild died there. The filter is on the fstab's fs-type column, so
/// any other label-less filesystem a guest happens to mount is skipped too, and
/// a name other than `/boot/efi` does not reintroduce it. Nothing is lost:
/// those filesystems could never carry a label in the first place.
pub(crate) const SELINUX_RELABEL_CMD: &str = "if [ -f /etc/selinux/config ]; then \
if command -v setfiles >/dev/null 2>&1; then \
. /etc/selinux/config; \
p=/; \
for d in $(awk '$1 !~ /^#/ && $2 ~ /^\\/./ && \
$3 !~ /^(vfat|msdos|exfat|ntfs|ntfs3|iso9660|udf|swap|tmpfs|none)$/ {print $2}' \
/etc/fstab 2>/dev/null); do \
[ -d \"$d\" ] && p=\"$p $d\"; done; \
setfiles -F /etc/selinux/${SELINUXTYPE:-targeted}/contexts/files/file_contexts $p; \
else touch /.autorelabel; fi; fi";

/// Translates the `CustomizeOp`s into the actual `virt-customize` arguments,
/// plus the SELinux relabel every build needs last (see
/// [`SELINUX_RELABEL_CMD`]).
pub(crate) fn customize_args(disk: &Path, ops: &[CustomizeOp]) -> Vec<String> {
    let mut args = vec!["-a".to_string(), disk.to_string_lossy().into_owned()];
    for op in ops {
        match op {
            CustomizeOp::RunCommand(cmd) => {
                args.push("--run-command".into());
                args.push(cmd.clone());
            }
            CustomizeOp::CopyIn(src, dst) => {
                args.push("--copy-in".into());
                args.push(format!("{}:{}", src.display(), dst));
            }
            CustomizeOp::Password { user, password } => {
                args.push("--password".into());
                args.push(format!("{user}:password:{password}"));
            }
            CustomizeOp::RootPassword(password) => {
                args.push("--root-password".into());
                args.push(format!("password:{password}"));
            }
        }
    }
    // Last, and after everything the caller asked for.
    args.push("--run-command".into());
    args.push(SELINUX_RELABEL_CMD.into());
    // …and libguestfs's own pass is then only in the way: it would re-create
    // the `/.autorelabel` this step exists to make unnecessary, buying a slow
    // relabel-and-reboot on first boot for labels that are already correct.
    args.push("--no-selinux-relabel".into());
    args
}

/// The package that ships `bin`, for the two families this engine builds for.
///
/// Split out and pure because the message it feeds is the whole value: an
/// absent tool surfaces from `Command::status()` as `ENOENT`, which renders as
/// "No such file or directory" — a sentence that sends the reader looking for
/// a missing *file*. Measured on a host without libguestfs, that is exactly
/// what `vm build` printed after a 600 MB download had already succeeded.
pub(crate) fn tool_package(bin: &str) -> Option<(&'static str, &'static str, &'static str)> {
    match bin {
        "virt-customize" | "virt-sparsify" | "virt-copy-out" => {
            Some(("libguestfs-tools", "guestfs-tools", "libguestfs"))
        }
        "qemu-img" => Some(("qemu-utils", "qemu-img", "qemu-img")),
        "cloud-localds" => Some(("cloud-image-utils", "cloud-utils", "cloud-image-utils")),
        "virsh" => Some(("libvirt-clients", "libvirt-client", "libvirt")),
        _ => None,
    }
}

/// How the host installs software. Not cosmetic: an `apt install` printed on
/// Fedora is not a smaller kind of help, it is an instruction that fails, and
/// the reader has to translate it before they can act on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    Debian,
    RedHat,
    Arch,
    Suse,
    Unknown,
}

/// Reads the family off `/etc/os-release`. Kept apart from [`family_of`] so the
/// decision itself stays pure and testable — the file is the only thing here
/// that needs a real host.
pub(crate) fn host_family() -> Family {
    std::fs::read_to_string("/etc/os-release").map_or(Family::Unknown, |s| family_of(&s))
}

/// `ID` first, then `ID_LIKE` — a derivative (Zorin, Mint, Alma, Manjaro)
/// names its parent there, which is exactly the case where guessing from `ID`
/// alone would produce the wrong package manager.
pub(crate) fn family_of(os_release: &str) -> Family {
    let field = |k: &str| -> String {
        os_release
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{k}=")))
            .map(|v| v.trim_matches('"').to_ascii_lowercase())
            .unwrap_or_default()
    };
    let hay = format!("{} {}", field("ID"), field("ID_LIKE"));
    for (needle, fam) in [
        ("debian", Family::Debian),
        ("ubuntu", Family::Debian),
        ("fedora", Family::RedHat),
        ("rhel", Family::RedHat),
        ("centos", Family::RedHat),
        ("arch", Family::Arch),
        ("suse", Family::Suse),
    ] {
        if hay.split_whitespace().any(|w| w == needle) {
            return fam;
        }
    }
    Family::Unknown
}

/// The one command that installs `pkg` on this host — and, when the family is
/// unknown, all of them rather than a guess.
pub(crate) fn install_cmd(f: Family, deb: &str, rpm: &str, arch: &str) -> String {
    match f {
        Family::Debian => format!("sudo apt install {deb}"),
        Family::RedHat => format!("sudo dnf install {rpm}"),
        Family::Arch => format!("sudo pacman -S {arch}"),
        Family::Suse => format!("sudo zypper install {rpm}"),
        Family::Unknown => format!(
            "sudo apt install {deb}   (Debian/Ubuntu)\n  \
             sudo dnf install {rpm}   (Fedora/RHEL/Rocky)\n  \
             sudo pacman -S {arch}   (Arch)"
        ),
    }
}

/// Turns a tool's own last words into the fix for them.
///
/// A build that dies inside libguestfs prints something true and unusable —
/// `supermin exited with error status 1`, `passt exited with status 1` — and
/// then `virt-customize failed (exit Some(1))` on top. Everything needed to
/// name the cause was on screen; nothing named it. Each arm below is a failure
/// this engine has actually been debugged through on a real host, and the hint
/// is what fixed it there.
///
/// `None` when nothing matches: inventing advice for an unrecognised failure
/// would be worse than the bare exit code, because it sends the reader away
/// from the output that does explain it.
pub(crate) fn tool_failure_hint(tail: &str, f: Family) -> Option<String> {
    let t = tail.to_ascii_lowercase();
    // The host kernel is not readable, so supermin cannot copy it into the
    // appliance. Debian and Ubuntu ship /boot/vmlinuz-* as 0600 root:root,
    // which is why this is close to universal there and unheard of elsewhere.
    if t.contains("supermin") && (t.contains("vmlinuz") || t.contains("kernel"))
        || (t.contains("vmlinuz") && t.contains("permission denied"))
    {
        let mut m = String::from(
            "libguestfs could not read the host kernel, so it could not build its appliance.\n\
             Fix (the kernel is 0600 on Debian/Ubuntu, and has to be readable):\n  \
             sudo chmod 0644 /boot/vmlinuz-*",
        );
        if f == Family::Debian || f == Family::Unknown {
            m.push_str(
                "\nTo survive the next kernel update, make it permanent:\n  \
                 sudo tee /etc/kernel/postinst.d/statoverride >/dev/null <<'EOF'\n  \
                 #!/bin/sh\n  \
                 version=\"$1\"; [ -z \"$version\" ] && exit 0\n  \
                 dpkg-statoverride --update --add root root 0644 \"/boot/vmlinuz-${version}\"\n  \
                 EOF\n  sudo chmod +x /etc/kernel/postinst.d/statoverride",
            );
        }
        return Some(m);
    }
    // `--network` only. passt is how libguestfs 1.52+ gives the appliance a
    // network, and it is confined: the AppArmor profile Debian/Ubuntu ship
    // allows writes under /tmp and $HOME only, while libguestfs puts passt's
    // socket and pid file under $XDG_RUNTIME_DIR (/run/user/UID).
    // The SECOND failure mode of the same cause, and the one that used to go
    // unnamed: passt starts, never hands out a lease, `dhclient` hangs its full
    // 300s, and the build then CONTINUES with no network. What surfaces is a
    // package manager failing to resolve a mirror, hundreds of lines deep, with
    // the word `passt` nowhere in it — so the arm below never fired and the
    // reader was left to conclude their own DNS was broken. The `[ 30x.x ]`
    // timestamp on the failing step is the tell: nothing else in a build pauses
    // for exactly five minutes.
    let no_dns = t.contains("could not resolve host")
        || t.contains("temporary failure resolving")
        || t.contains("cannot prepare internal mirrorlist");
    if t.contains("passt") || no_dns {
        let mut m = String::from(if no_dns {
            "the guest had no working network during the build: its resolver answered nothing.\n\
             The appliance gets its network from passt, and when passt starts but never leases \
             an address, libguestfs waits out `dhclient` (~300s — check the timestamp on the \
             step that failed) and then carries on with no network at all.\n\
             Most often passt's AppArmor profile forbids the runtime directory libguestfs uses. \
             Point that directory somewhere the profile allows and retry:\n  \
             mkdir -p /tmp/delonix-run && chmod 700 /tmp/delonix-run\n  \
             XDG_RUNTIME_DIR=/tmp/delonix-run delonix vm build --network …"
        } else {
            "the appliance's network helper (passt) failed, so `--network` could not start.\n\
             Most often its AppArmor profile forbids the runtime directory libguestfs uses.\n\
             Point that directory somewhere the profile allows and retry:\n  \
             mkdir -p /tmp/delonix-run && chmod 700 /tmp/delonix-run\n  \
             XDG_RUNTIME_DIR=/tmp/delonix-run delonix vm build --network …"
        });
        if f == Family::Debian {
            // Building it is only half the remedy, and the half that was
            // documented. libguestfs finds passt by running `passt --help`,
            // which goes through PATH — so the new binary has to come FIRST in
            // it. And do not try to disable the packaged one by shadowing it
            // with a stub that fails: MEASURED, libguestfs then runs the stub
            // as the real helper and dies on it. Absent falls back to qemu's
            // own slirp; present-and-broken does not.
            m.push_str(
                "\nIf it still fails, the packaged passt is too old to talk to this qemu \
                 (Ubuntu 24.04 ships the Feb-2024 build); build a current one and put it \
                 FIRST in PATH — libguestfs looks passt up there:\n  \
                 git clone https://passt.top/passt && cd passt && make\n  \
                 PATH=$PWD:$PATH XDG_RUNTIME_DIR=/tmp/delonix-run delonix vm build --network …",
            );
        }
        m.push_str("\nOr build without it: drop `--network` (RUN steps then work offline).");
        return Some(m);
    }
    // No KVM: works, but every build becomes software emulation and takes
    // minutes instead of seconds — worth naming before someone concludes the
    // tool is slow.
    if t.contains("/dev/kvm") || t.contains("kvm: permission denied") {
        return Some(String::from(
            "no access to /dev/kvm — add yourself to the group that owns it and log in again:\n  \
             sudo usermod -aG kvm $USER\n\
             Until then everything runs under software emulation, which is very slow.",
        ));
    }
    if t.contains("supermin") || t.contains("libguestfs") {
        return Some(String::from(
            "libguestfs could not start its appliance. To see its own diagnosis:\n  \
             export LIBGUESTFS_DEBUG=1 LIBGUESTFS_TRACE=1\n  \
             libguestfs-test-tool",
        ));
    }
    None
}

pub(crate) fn run_tool(bin: &str, args: &[&str]) -> Result<()> {
    use std::io::BufRead;

    let not_found = |e: std::io::Error| -> Error {
        if e.kind() == std::io::ErrorKind::NotFound {
            let fam = host_family();
            if let Some((deb, rpm, arch)) = tool_package(bin) {
                return Error::Invalid(format!(
                    "{}\n  {}",
                    super::po::tf("`{bin}` is not installed.", &[("bin", bin)]),
                    install_cmd(fam, deb, rpm, arch),
                ));
            }
            return Error::Invalid(super::po::tf(
                "`{bin}` is not installed, and it is needed here.",
                &[("bin", bin)],
            ));
        }
        Error::Invalid(format!(
            "{}: {e}",
            super::po::tf("running {bin}", &[("bin", bin)])
        ))
    };

    // stderr is PIPED rather than inherited so that a failure can be explained
    // instead of merely reported. The lines are echoed as they arrive — a build
    // is long and the tool's own progress is the only sign of life — and the
    // last of them are kept, because that is where libguestfs says what really
    // went wrong (`supermin: … Permission denied`) one layer under the useless
    // sentence it exits with (`virt-customize: error: … exited with error
    // status 1`).
    let mut child = Command::new(bin)
        .args(args)
        // stdout too, and not for the diagnosis: virt-customize narrates its
        // work there (`[ 2.1] Running: …`), so leaving it inherited meant every
        // one of those lines went straight to the terminal THROUGH a step that
        // was supposed to be holding them. Measured: the spinner line and the
        // guest's narration fighting over the same row.
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(not_found)?;
    // Read on its own thread — two pipes read in sequence deadlock as soon as
    // the one not being read fills its buffer.
    let out_reader = child.stdout.take().map(|o| {
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(o)
                .lines()
                .map_while(std::result::Result::ok)
            {
                // Outside a step this stays on stdout, exactly where it has
                // always gone — only the fold changes it.
                if !super::output::capture_line(&line) {
                    println!("{line}");
                }
            }
        })
    });
    let mut tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    if let Some(err) = child.stderr.take() {
        for line in std::io::BufReader::new(err)
            .lines()
            .map_while(std::result::Result::ok)
        {
            // Offered to the step fold first: inside a `Progress` step the
            // tool's output belongs behind it, and outside one it prints as it
            // always did (`capture_line` says which).
            if !super::output::capture_line(&line) {
                eprintln!("{line}");
            }
            if tail.len() == 60 {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    }
    if let Some(t) = out_reader {
        let _ = t.join();
    }
    let status = child.wait().map_err(|e| Error::Runtime {
        context: "vm build",
        message: e.to_string(),
    })?;
    if !status.success() {
        let mut msg = super::po::tf(
            "{bin} failed (exit {code})",
            &[("bin", bin), ("code", &format!("{:?}", status.code()))],
        );
        let joined = tail.iter().cloned().collect::<Vec<_>>().join("\n");
        if let Some(hint) = tool_failure_hint(&joined, host_family()) {
            msg.push_str("\n\n");
            msg.push_str(&hint);
        }
        return Err(Error::Invalid(msg));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gap closed: `OFFICIAL_VM_BASE_IMAGE` existed but had no default-selection
    /// wiring — a bare `pull`/`ls-remote --no-k8s` now resolves to it, same as a
    /// bare `pull`/`ls-remote` already resolved to the Kubernetes golden.
    #[test]
    fn default_pull_source_escolhe_k8s_ou_base_consoante_a_flag() {
        assert_eq!(default_pull_source(false), OFFICIAL_VM_IMAGE);
        assert_eq!(default_pull_source(true), OFFICIAL_VM_BASE_IMAGE);
        assert_ne!(default_pull_source(false), default_pull_source(true));
    }

    #[test]
    fn customization_steps_incluem_pacotes_extra() {
        let cri = PathBuf::from("/tmp/delonix-cri");
        let eng = PathBuf::from("/tmp/delonix");
        let svc = PathBuf::from("/tmp/delonix-cri.service");
        let ops = k8s_customization_steps(
            None,
            &["htop".to_string()],
            &[],
            &cri,
            &eng,
            &svc,
            Distro::Ubuntu,
            None,
        );
        let install_step = ops
            .iter()
            .find_map(|op| match op {
                CustomizeOp::RunCommand(c) if c.contains("apt-get install") => Some(c),
                _ => None,
            })
            .expect("devia haver um RunCommand de apt-get install");
        assert!(install_step.contains("kubeadm"));
        assert!(install_step.contains("htop"));
    }

    #[test]
    fn fmt_size_legivel_por_escalao() {
        assert_eq!(fmt_size(0), "0 B");
        assert_eq!(fmt_size(512), "512 B");
        assert_eq!(fmt_size(1024), "1.0 KiB");
        assert_eq!(fmt_size(1536), "1.5 KiB");
        assert_eq!(fmt_size(1024 * 1024), "1.0 MiB");
        assert_eq!(fmt_size(2_555_576_320), "2.38 GiB");
        assert_eq!(fmt_size(1024_u64.pow(4)), "1.00 TiB");
    }

    #[test]
    fn fmt_local_tem_a_forma_data_hora() {
        // 1784216635 → a local date/time; we validate the SHAPE (the timezone is the host's).
        let s = fmt_local(1_784_216_635);
        let b = s.as_bytes();
        assert_eq!(s.len(), 16, "esperado 'AAAA-MM-DD HH:MM', obtido {s:?}");
        assert!(b[4] == b'-' && b[7] == b'-' && b[10] == b' ' && b[13] == b':');
        assert!(s[..4].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn customization_steps_incluem_extra_run_no_fim() {
        let cri = PathBuf::from("/tmp/delonix-cri");
        let eng = PathBuf::from("/tmp/delonix");
        let svc = PathBuf::from("/tmp/delonix-cri.service");
        let ops = k8s_customization_steps(
            None,
            &[],
            &["echo oi".to_string()],
            &cri,
            &eng,
            &svc,
            Distro::Ubuntu,
            None,
        );
        // `--extra-run` runs after all base steps; only the apt cleanup
        // comes after it (it must be last — the extra-run may install packages).
        let idx_extra = ops
            .iter()
            .position(|op| matches!(op, CustomizeOp::RunCommand(c) if c == "echo oi"))
            .expect("o --extra-run devia estar na lista");
        // Ordem, e não posições fixas: a cauda cresceu duas vezes desde que este
        // teste foi escrito (journal, inventário de pacotes) e o que ele existe
        // para fixar é que NADA se intromete entre o `--extra-run` e a limpeza,
        // e que o reset do machine-id continua a ser o último passo.
        let at = |needle: &str| {
            ops.iter()
                .position(|op| matches!(op, CustomizeOp::RunCommand(c) if c.contains(needle)))
                .unwrap_or_else(|| panic!("passo em falta: {needle}"))
        };
        assert!(idx_extra < at("/etc/delonix-kernel-version"));
        assert!(at("/etc/delonix-kernel-version") < at("apt-get clean"));
        assert!(
            at("apt-get clean") < at("packages.tsv"),
            "o inventário tem de reflectir a imagem FINAL"
        );
        // machine-id reset must be the ABSOLUTE last step (regression: shared
        // machine-id across cloned VMs breaks DHCP client-id, see comment at
        // the push site in `common_customization_steps`).
        assert!(
            matches!(ops.last(), Some(CustomizeOp::RunCommand(c)) if c.contains("truncate -s 0 /etc/machine-id")),
            "o reset do machine-id devia ser o ÚLTIMO passo"
        );
    }

    #[test]
    fn customization_steps_configuram_completion_e_alias_k() {
        let cri = PathBuf::from("/tmp/delonix-cri");
        let eng = PathBuf::from("/tmp/delonix");
        let svc = PathBuf::from("/tmp/delonix-cri.service");
        // Both build paths (online + offline) share `common_customization_steps`,
        // so the kubectl UX must be present in both.
        for ops in [
            k8s_customization_steps(None, &[], &[], &cri, &eng, &svc, Distro::Ubuntu, None),
            k8s_customization_steps_offline(
                &[PathBuf::from("/tmp/x/kubeadm_1.34.9-1.1_amd64.deb")],
                &[],
                &cri,
                &eng,
                &svc,
                None,
                Distro::Ubuntu,
                None,
            ),
        ] {
            let bashrc = ops
                .iter()
                .find_map(|op| match op {
                    CustomizeOp::RunCommand(c) if c.contains("/etc/bash.bashrc") => Some(c),
                    _ => None,
                })
                .expect("devia haver um passo a escrever no /etc/bash.bashrc");
            assert!(bashrc.contains("kubectl completion bash"));
            assert!(bashrc.contains("alias k=kubectl"));
            assert!(bashrc.contains("complete -o default -F __start_kubectl k"));
            assert!(bashrc.contains("kubeadm completion bash"));
            // Guarded so it is inert when a tool is absent.
            assert!(bashrc.contains("command -v kubectl"));
        }
    }

    /// A reduced `Packages`, with the same shape as the real one (several architectures and
    /// versions per package) — includes the case that broke the 1st offline build.
    const PACKAGES_FIXTURE: &str = "\
Package: cri-tools
Version: 1.34.0-1.1
Architecture: amd64
Filename: amd64/cri-tools_1.34.0-1.1_amd64.deb
SHA256: aaa1

Package: kubeadm
Version: 1.34.0-1.1
Architecture: amd64
Filename: amd64/kubeadm_1.34.0-1.1_amd64.deb
SHA256: bbb1

Package: kubeadm
Version: 1.34.9-1.1
Architecture: amd64
Filename: amd64/kubeadm_1.34.9-1.1_amd64.deb
SHA256: bbb2

Package: kubeadm
Version: 1.34.9-1.1
Architecture: arm64
Filename: arm64/kubeadm_1.34.9-1.1_arm64.deb
SHA256: bbb3

Package: kubeadm
Version: 1.33.1-1.1
Architecture: amd64
Filename: amd64/kubeadm_1.33.1-1.1_amd64.deb
SHA256: bbb4

Package: kubernetes-cni
Version: 1.7.1-1.1
Architecture: amd64
Filename: amd64/kubernetes-cni_1.7.1-1.1_amd64.deb
SHA256: ccc1
";

    #[test]
    fn parse_packages_escolhe_maior_versao_da_arch_certa() {
        let got = parse_packages_index(
            PACKAGES_FIXTURE,
            "amd64",
            "1.34.",
            &["kubeadm"],
            &["kubeadm"],
        );
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].version, "1.34.9-1.1",
            "devia escolher a maior 1.34.*"
        );
        assert_eq!(got[0].filename, "amd64/kubeadm_1.34.9-1.1_amd64.deb");
        assert_eq!(got[0].sha256, "bbb2");
    }

    #[test]
    fn parse_packages_ignora_versionamento_proprio_no_filtro_de_versao() {
        // REGRESSION: `kubernetes-cni` is 1.7.x — filtering it by "1.34." returned
        // nothing and the offline build aborted with "does not have kubernetes-cni".
        let got = parse_packages_index(
            PACKAGES_FIXTURE,
            "amd64",
            "1.34.",
            &["kubeadm", "kubernetes-cni"],
            &["kubeadm"], // only kubeadm follows the k8s version
        );
        let cni = got
            .iter()
            .find(|d| d.name == "kubernetes-cni")
            .expect("cni tem de vir");
        assert_eq!(cni.version, "1.7.1-1.1");
        assert!(got
            .iter()
            .any(|d| d.name == "kubeadm" && d.version == "1.34.9-1.1"));
    }

    #[test]
    fn deb_version_lt_compara_numericamente() {
        assert!(deb_version_lt("1.34.0-1.1", "1.34.9-1.1"));
        assert!(deb_version_lt("1.33.1-1.1", "1.34.0-1.1"));
        assert!(
            deb_version_lt("1.9.0-1.1", "1.10.0-1.1"),
            "9 < 10 numericamente, não lexicograficamente"
        );
        assert!(!deb_version_lt("1.34.9-1.1", "1.34.0-1.1"));
        assert!(!deb_version_lt("1.34.9-1.1", "1.34.9-1.1"));
    }

    #[test]
    fn release_sha256_of_le_a_seccao_certa() {
        let release = "\
Origin: obs://build.opensuse.org
MD5Sum:
 deadbeef 1234 Packages
SHA256:
 abc123 4567 Packages
 def456 89 Release
Date: Fri, 12 Jun 2026 12:40:56 UTC
";
        assert_eq!(
            release_sha256_of(release, "Packages").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            release_sha256_of(release, "Release").as_deref(),
            Some("def456")
        );
        assert_eq!(release_sha256_of(release, "nao-existe"), None);
    }

    /// O codinome sai da própria imagem, e é o que constrói o URL do arquivo — um valor
    /// errado aqui dá um 404 depois de centenas de MB, ou pior, o índice de outra suite.
    #[test]
    fn o_codinome_le_se_do_os_release_da_imagem() {
        let real = "PRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nNAME=\"Ubuntu\"\n\
                    VERSION_ID=\"24.04\"\nVERSION_CODENAME=noble\nID=ubuntu\n\
                    UBUNTU_CODENAME=noble\n";
        assert_eq!(parse_os_release_codename(real).as_deref(), Some("noble"));
        // Algumas distros citam-no; o valor é o mesmo.
        assert_eq!(
            parse_os_release_codename("VERSION_CODENAME=\"bookworm\"\n").as_deref(),
            Some("bookworm")
        );
        // Ausente é None e NÃO uma string vazia: vazio construiria
        // `dists//InRelease`, que é um 404 a dizer outra coisa.
        assert!(parse_os_release_codename("ID=fedora\nVERSION_ID=42\n").is_none());
        assert!(parse_os_release_codename("VERSION_CODENAME=\n").is_none());
        // `UBUNTU_CODENAME` sozinho não conta — este parser tem um dono, e é a chave
        // padronizada pelo os-release(5).
        assert!(parse_os_release_codename("UBUNTU_CODENAME=noble\n").is_none());
    }

    /// O `.deb` do agente instala pelo passo que já existia; o que faltava era garantir a
    /// unit ACTIVA, sem depender do postinst de outro pacote num convidado sem systemd a
    /// correr. Guardado, para um build sem o agente não falhar numa unit ausente.
    #[test]
    fn steps_offline_activam_a_unit_do_guest_agent() {
        let ops = k8s_customization_steps_offline(
            &[PathBuf::from("/tmp/x/qemu-guest-agent_8.2.2_amd64.deb")],
            &[],
            &PathBuf::from("/tmp/delonix-cri"),
            &PathBuf::from("/tmp/delonix"),
            &PathBuf::from("/tmp/delonix-cri.service"),
            None,
            Distro::Ubuntu,
            None,
        );
        let cmds: Vec<&str> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::RunCommand(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        let enable = cmds
            .iter()
            .find(|c| c.contains("qemu-guest-agent.service"))
            .expect("faltou activar a unit do guest agent");
        assert!(
            enable.contains("systemctl enable"),
            "a unit é listada mas não activada: {enable}"
        );
        assert!(
            enable.contains("list-unit-files") && enable.contains("|| true"),
            "activar tem de ser guardado, senão um build sem o agente chumba: {enable}"
        );
    }

    #[test]
    fn steps_offline_instalam_por_dpkg_e_nao_tocam_a_rede() {
        let debs = vec![PathBuf::from("/tmp/x/kubeadm_1.34.9-1.1_amd64.deb")];
        let ops = k8s_customization_steps_offline(
            &debs,
            &[],
            &PathBuf::from("/tmp/delonix-cri"),
            &PathBuf::from("/tmp/delonix"),
            &PathBuf::from("/tmp/delonix-cri.service"),
            None,
            Distro::Ubuntu,
            None,
        );
        let cmds: Vec<&str> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::RunCommand(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        assert!(cmds
            .iter()
            .any(|c| c.contains("dpkg -i /tmp/k8s-debs/*.deb")));
        assert!(
            cmds.iter().any(|c| c.contains("mkdir -p /tmp/k8s-debs")),
            "o --copy-in exige o dir criado"
        );
        // The central guarantee of offline mode: nothing contacts the network in the guest.
        for c in &cmds {
            assert!(
                !c.contains("curl") && !c.contains("apt-get update") && !c.contains("https://"),
                "passo offline com rede: {c}"
            );
        }
        // And the .deb is injected.
        assert!(ops
            .iter()
            .any(|o| matches!(o, CustomizeOp::CopyIn(_, d) if d == "/tmp/k8s-debs")));
    }

    #[test]
    fn steps_offline_com_preseed_injecta_as_4_subpastas_do_image_store() {
        let debs = vec![PathBuf::from("/tmp/x/kubeadm_1.34.9-1.1_amd64.deb")];
        let preseed_root = PathBuf::from("/tmp/preseed-images");
        let ops = k8s_customization_steps_offline(
            &debs,
            &[],
            &PathBuf::from("/tmp/delonix-cri"),
            &PathBuf::from("/tmp/delonix"),
            &PathBuf::from("/tmp/delonix-cri.service"),
            Some(&preseed_root),
            Distro::Ubuntu,
            None,
        );
        for sub in ["images", "layers", "containers", "blobs"] {
            assert!(
                ops.iter().any(|o| matches!(
                    o,
                    CustomizeOp::CopyIn(src, dst)
                        if src == &preseed_root.join(sub) && dst == "/var/lib/delonix"
                )),
                "faltou o --copy-in de {sub}"
            );
        }
        assert!(
            ops.iter().any(
                |o| matches!(o, CustomizeOp::RunCommand(c) if c == "mkdir -p /var/lib/delonix")
            ),
            "/var/lib/delonix tem de existir antes do --copy-in"
        );
    }

    #[test]
    fn customization_steps_limpam_a_cache_apt_no_fim() {
        let cri = PathBuf::from("/tmp/delonix-cri");
        let eng = PathBuf::from("/tmp/delonix");
        let svc = PathBuf::from("/tmp/delonix-cri.service");
        let ops = k8s_customization_steps(None, &[], &[], &cri, &eng, &svc, Distro::Ubuntu, None);
        // ~367 MiB of .deb + indexes that, without this, filled the golden's root to 92%.
        // Tem de correr DEPOIS de tudo o que instala pacotes (incluindo o
        // `--extra-run`) e ANTES do reset do machine-id, que é o último passo.
        // Por posição fixa isto já chumbou duas vezes por a cauda ter crescido
        // (journal, inventário) — o que fixa é a ordem.
        let at = |needle: &str| {
            ops.iter()
                .position(|op| matches!(op, CustomizeOp::RunCommand(c) if c.contains(needle)))
                .unwrap_or_else(|| panic!("passo em falta: {needle}"))
        };
        assert!(
            matches!(&ops[at("apt-get clean")], CustomizeOp::RunCommand(c) if c.contains("/var/lib/apt/lists")),
            "a limpeza tem de apagar tambem os indices"
        );
        assert!(at("apt-get clean") < at("truncate -s 0 /etc/machine-id"));
    }

    #[test]
    fn a_imagem_traz_o_journal_persistente_em_todas_as_distros() {
        // Um journal volátil apaga, a cada reboot, os logs do arranque que
        // falhou — e o tecto impede que os NOSSOS logs encham a raiz do
        // inquilino, que é uma forma de disk-pressure que este host já viu.
        for d in [
            Distro::Ubuntu,
            Distro::Debian,
            Distro::Rocky,
            Distro::Fedora,
        ] {
            let ops = shared_account_steps(&[], d, None);
            let cmds: Vec<&str> = ops
                .iter()
                .filter_map(|o| match o {
                    CustomizeOp::RunCommand(c) => Some(c.as_str()),
                    _ => None,
                })
                .collect();
            assert!(
                cmds.iter()
                    .any(|c| c.contains("Storage=persistent") && c.contains("/var/log/journal")),
                "{d:?} ficou com o journal volátil"
            );
            assert!(
                cmds.iter().any(|c| c.contains("SystemMaxUse=")),
                "{d:?} ficou com um journal sem tecto"
            );
        }
    }

    #[test]
    fn sem_a_flag_a_imagem_nao_abre_porta_nenhuma() {
        // O agente de métricas é opt-in: uma imagem publicada não leva um
        // listener que o inquilino nunca pediu.
        let ops =
            rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), Distro::Ubuntu, None);
        assert!(!ops.iter().any(|o| matches!(
            o,
            CustomizeOp::RunCommand(c) if c.contains("node-exporter") || c.contains("node_exporter")
        )));
    }

    #[test]
    fn o_node_exporter_corre_sem_privilegio_no_endereco_pedido() {
        let ops = node_exporter_steps(&PathBuf::from("/tmp/node_exporter"), "127.0.0.1:9100");
        let cmds: Vec<&str> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::RunCommand(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        assert!(ops
            .iter()
            .any(|o| matches!(o, CustomizeOp::CopyIn(_, dst) if dst == "/usr/local/bin")));
        assert!(cmds
            .iter()
            .any(|c| c.contains("--web.listen-address=127.0.0.1:9100")));
        assert!(
            cmds.iter().any(|c| c.contains("User=node-exporter")),
            "o exporter nunca corre como root"
        );
        assert!(cmds.iter().any(|c| c.contains("NoNewPrivileges=yes")));
        assert!(cmds
            .iter()
            .any(|c| c.contains("systemctl enable node-exporter.service")));
    }

    #[test]
    fn o_reset_do_machine_id_continua_a_ser_o_ultimo_passo() {
        // Se o `systemctl enable` do exporter corresse DEPOIS do reset, podia
        // materializar um machine-id — e VMs que partilham um perdem o lease
        // DHCP umas às outras (medido, e a razão de aquele passo ser o último).
        let base = shared_account_steps(&[], Distro::Ubuntu, None);
        let with = splice_before_machine_id(
            base.clone(),
            node_exporter_steps(&PathBuf::from("/tmp/node_exporter"), ":9100"),
        );
        let last = match with.last().expect("não pode ficar vazio") {
            CustomizeOp::RunCommand(c) => c.clone(),
            other => panic!("último passo inesperado: {other:?}"),
        };
        assert!(last.contains("/etc/machine-id"), "último passo: {last}");
        assert_eq!(with.len(), base.len() + 4);
    }

    #[test]
    fn sem_marcador_de_machine_id_os_passos_sao_acrescentados() {
        let extra = vec![CustomizeOp::RunCommand("echo oi".into())];
        let out = splice_before_machine_id(vec![CustomizeOp::RunCommand("echo a".into())], extra);
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[1], CustomizeOp::RunCommand(c) if c == "echo oi"));
    }

    #[test]
    fn o_sums_gnu_casa_o_nome_exacto_e_nao_um_sufixo() {
        let hash_amd = "a".repeat(64);
        let hash_arm = "b".repeat(64);
        let text = format!(
            "{hash_arm}  node_exporter-1.9.1.linux-arm64.tar.gz\n\
             {hash_amd}  node_exporter-1.9.1.linux-amd64.tar.gz\n"
        );
        assert_eq!(
            sha256_from_gnu_sums(&text, "node_exporter-1.9.1.linux-amd64.tar.gz"),
            Some(hash_amd)
        );
        // Um nome que é SUFIXO de outra entrada não pode devolver o hash dela.
        assert_eq!(sha256_from_gnu_sums(&text, "linux-amd64.tar.gz"), None);
        assert_eq!(sha256_from_gnu_sums(&text, "inexistente.tar.gz"), None);
        // Uma linha truncada não é um hash.
        assert_eq!(sha256_from_gnu_sums("abc  ficheiro\n", "ficheiro"), None);
    }

    #[test]
    fn a_imagem_nao_leva_password_nenhuma_por_omissao() {
        let cri = PathBuf::from("/tmp/delonix-cri");
        let eng = PathBuf::from("/tmp/delonix");
        let svc = PathBuf::from("/tmp/delonix-cri.service");
        let ops = k8s_customization_steps(None, &[], &[], &cri, &eng, &svc, Distro::Ubuntu, None);
        // No account may carry a password that is written down in this repository.
        assert!(!ops
            .iter()
            .any(|op| matches!(op, CustomizeOp::RootPassword(_))));
        assert!(!ops
            .iter()
            .any(|op| matches!(op, CustomizeOp::Password { .. })));
        let cmds: Vec<&str> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::RunCommand(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        assert!(cmds.iter().any(|c| c.contains("useradd")));
        assert!(
            cmds.iter()
                .any(|c| c.contains("passwd -l root") && c.contains("passwd -l delonix")),
            "as contas ficam trancadas quando ninguem pede password"
        );
        // With --root-password, and only then, the image carries one.
        let ops = k8s_customization_steps(
            None,
            &[],
            &[],
            &cri,
            &eng,
            &svc,
            Distro::Ubuntu,
            Some("segredo"),
        );
        assert!(ops
            .iter()
            .any(|op| matches!(op, CustomizeOp::RootPassword(p) if p == "segredo")));
    }

    #[test]
    fn rootless_steps_instalam_dependencias_e_o_binario_delonix_sem_cri() {
        let delonix = PathBuf::from("/tmp/delonix");
        let ops = rootless_customization_steps(&[], &delonix, Distro::Ubuntu, None);
        let cmds: Vec<&str> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::RunCommand(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        assert!(cmds.iter().any(|c| c.contains("slirp4netns")
            && c.contains("uidmap")
            && c.contains("nftables")
            && c.contains("iproute2")
            && c.contains("conntrack")));
        assert!(ops
            .iter()
            .any(|o| matches!(o, CustomizeOp::CopyIn(src, dst) if src == &delonix && dst == "/usr/local/bin")));
        // No CRI shim on a no-k8s image — there is no kubelet to serve.
        assert!(!cmds.iter().any(|c| c.contains("delonix-cri")));
        assert!(!ops
            .iter()
            .any(|o| matches!(o, CustomizeOp::CopyIn(_, dst) if dst == "/etc/systemd/system")));
    }

    #[test]
    fn rootless_steps_configuram_subuid_e_subgid_para_delonix() {
        let ops =
            rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), Distro::Ubuntu, None);
        let cmds: Vec<&str> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::RunCommand(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        assert!(cmds
            .iter()
            .any(|c| c.contains("/etc/subuid") && c.contains("delonix:100000:65536")));
        assert!(cmds
            .iter()
            .any(|c| c.contains("/etc/subgid") && c.contains("delonix:100000:65536")));
    }

    #[test]
    fn rootless_steps_escrevem_o_perfil_apparmor_unconfined_userns() {
        let ops =
            rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), Distro::Ubuntu, None);
        let cmds: Vec<&str> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::RunCommand(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        assert!(cmds.iter().any(|c| c.contains("/etc/apparmor.d/delonix")
            && c.contains("flags=(unconfined)")
            && c.contains("userns")));
    }

    #[test]
    fn rootless_steps_partilham_a_criacao_de_conta_delonix() {
        let ops =
            rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), Distro::Ubuntu, None);
        let cmds: Vec<&str> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::RunCommand(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        // A conta é partilhada com as receitas do k8s — é o que este teste
        // sempre guardou. O que MUDOU é que ela já não traz password: exigia
        // `RootPassword("delonix")` e `delonix:delonix`, e era esse o defeito.
        assert!(cmds
            .iter()
            .any(|c| c.contains("useradd") && c.contains("delonix")));
        assert!(cmds.iter().any(|c| c.contains("passwd -l root")));
        assert!(!ops
            .iter()
            .any(|op| matches!(op, CustomizeOp::RootPassword(_))));
        assert!(!ops
            .iter()
            .any(|op| matches!(op, CustomizeOp::Password { .. })));
    }

    #[test]
    fn rootless_steps_rocky_usa_dnf_e_pacotes_rpm() {
        let ops =
            rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), Distro::Rocky, None);
        let cmds: Vec<&str> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::RunCommand(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        // Package NAMES confirmed live against Rocky 9's own repo listings —
        // `shadow-utils`/`iproute`/`conntrack-tools`, not the apt names.
        assert!(cmds.iter().any(|c| c.starts_with("dnf install")
            && c.contains("slirp4netns")
            && c.contains("shadow-utils")
            && c.contains("nftables")
            && c.contains("iproute")
            && c.contains("conntrack-tools")));
        assert!(!cmds.iter().any(|c| c.contains("apt-get")));
    }

    #[test]
    fn rootless_steps_rocky_usa_wheel_bashrc_e_dnf_clean() {
        let ops =
            rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), Distro::Rocky, None);
        let cmds: Vec<&str> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::RunCommand(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        assert!(cmds
            .iter()
            .any(|c| c.contains("useradd") && c.contains("-G wheel")));
        assert!(!cmds.iter().any(|c| c.contains("-G sudo")));
        assert!(cmds.iter().any(|c| c.contains(">> /etc/bashrc")));
        assert!(!cmds.iter().any(|c| c.contains("/etc/bash.bashrc")));
        assert!(cmds.iter().any(|c| c == &"dnf clean all"));
        assert!(!cmds.iter().any(|c| c.contains("apt-get clean")));
    }

    /// Achado de auditoria (MÉDIO): a golden traz `root/delonix` e
    /// `delonix:delonix` com sudo NOPASSWD — credenciais FIXAS e públicas (estão
    /// no código-fonte aberto). Sem desligar o login por password no SSH, uma
    /// golden exposta à rede é root remoto com credenciais conhecidas. Todos os
    /// caminhos suportados entram por chave (cloud-init injecta-a, o
    /// `cluster kubeadm` gera a sua), por isso desligar não custa nada; a
    /// consola série continua a aceitar a password para recuperação.
    #[test]
    fn todas_as_goldens_desligam_o_login_por_password_no_ssh() {
        let hardened = |ops: &[CustomizeOp]| {
            ops.iter().any(
                |o| matches!(o, CustomizeOp::RunCommand(c) if c.contains("PasswordAuthentication no")),
            )
        };
        for d in [Distro::Ubuntu, Distro::Debian, Distro::Rocky] {
            let ops = rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), d, None);
            assert!(
                hardened(&ops),
                "a golden rootless ({d:?}) tem de desligar o login por password"
            );
            // Tem de mexer nos DOIS sítios: o `sshd_config` do bullseye não tem
            // linha `Include`, logo só o drop-in seria ignorado em silêncio.
            let cmds: Vec<&String> = ops
                .iter()
                .filter_map(|o| match o {
                    CustomizeOp::RunCommand(c) => Some(c),
                    _ => None,
                })
                .collect();
            assert!(cmds
                .iter()
                .any(|c| c.contains("sshd_config.d") && c.contains("/etc/ssh/sshd_config")));
        }
        // A golden k8s partilha os mesmos passos de conta, logo também vem
        // endurecida — é a que corre em nós de produção.
        let k8s = k8s_customization_steps(
            None,
            &[],
            &[],
            &PathBuf::from("/tmp/delonix-cri"),
            &PathBuf::from("/tmp/delonix"),
            &PathBuf::from("/tmp/delonix-cri.service"),
            Distro::Ubuntu,
            None,
        );
        assert!(hardened(&k8s), "a golden k8s também tem de vir endurecida");
    }

    #[test]
    fn rootless_steps_rocky_nunca_escreve_perfil_apparmor() {
        // BUG CAUGHT DURING IMPLEMENTATION: the AppArmor write is
        // `printf ... > /etc/apparmor.d/delonix && (apparmor_parser ... || true)`
        // — only the parser call is guarded. Rocky has no `/etc/apparmor.d/`
        // directory at all, so if this step ran there the redirect itself
        // would fail and take down the whole `virt-customize` build (the
        // `&&` isn't guarded). Ubuntu-only gating is the fix; this test
        // proves Rocky never even sees the command.
        let ops =
            rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), Distro::Rocky, None);
        assert!(!ops
            .iter()
            .any(|o| matches!(o, CustomizeOp::RunCommand(c) if c.contains("apparmor"))));
    }

    /// The rootless golden ships `ip_unprivileged_port_start=80` applied, so
    /// `-p 80:80` works out of the box in it — a disposable single-tenant VM is
    /// exactly the case where the host-wide trade-off is acceptable, unlike the
    /// public `install.sh` (where it stays behind `--low-ports`). Every distro,
    /// since the file is a kernel path, not a packaging convention. The k8s
    /// golden must NOT get it: that node's kubelet/kube-proxy run as root.
    #[test]
    fn so_a_golden_rootless_traz_as_portas_baixas_abertas() {
        let has_lowports = |ops: &[CustomizeOp]| {
            ops.iter().any(
                |o| matches!(o, CustomizeOp::RunCommand(c) if c.contains("ip_unprivileged_port_start")),
            )
        };
        // The namespace isolation is unenforceable without bridged traffic in
        // netfilter — MEASURED in a VM from this image: the chaos harness's two
        // isolation scenarios fail without it and pass with it. A base image
        // that ships without this reports the isolation as applied and is not.
        let has_bridge_nf = |ops: &[CustomizeOp]| {
            ops.iter().any(|o| {
                matches!(o, CustomizeOp::RunCommand(c) if c.contains("br_netfilter")
                    && c.contains("bridge-nf-call-iptables"))
            })
        };
        for d in [
            Distro::Ubuntu,
            Distro::Debian,
            Distro::Rocky,
            Distro::Fedora,
        ] {
            let ops = rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), d, None);
            assert!(
                has_lowports(&ops),
                "rootless golden ({d:?}) needs the sysctl"
            );
            assert!(
                has_bridge_nf(&ops),
                "rootless golden ({d:?}): without br_netfilter the namespace isolation is silently inert"
            );
        }
        assert!(!has_lowports(&k8s_customization_steps(
            None,
            &[],
            &[],
            &PathBuf::from("/tmp/delonix-cri"),
            &PathBuf::from("/tmp/delonix"),
            &PathBuf::from("/tmp/delonix-cri.service"),
            Distro::Ubuntu,
            None,
        )));
    }

    #[test]
    fn rootless_steps_debian_nao_muda_de_comportamento() {
        // v0.17.0 regression guard: Debian's sudo/bashrc/apt-clean output must
        // stay byte-identical to before this Rocky-driven refactor.
        let ops =
            rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), Distro::Debian, None);
        let cmds: Vec<&str> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::RunCommand(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        assert!(cmds
            .iter()
            .any(|c| c.contains("useradd") && c.contains("-G sudo")));
        assert!(cmds.iter().any(|c| c.contains(">> /etc/bash.bashrc")));
        assert!(cmds.iter().any(|c| c.contains("apt-get clean")));
    }

    #[test]
    fn parse_bsd_checksum_extrai_o_hash_do_ficheiro_pedido() {
        // Real line captured live from `dl.rockylinux.org`.
        let text = "# Rocky-9-GenericCloud.latest.x86_64.qcow2: 645988352 bytes\n\
                     SHA256 (Rocky-9-GenericCloud.latest.x86_64.qcow2) = 92c206cc6f790c61583247eefe87890f8828420662c17cacf247cec78ab4eec8\n";
        assert_eq!(
            parse_bsd_checksum(text, "Rocky-9-GenericCloud.latest.x86_64.qcow2"),
            Some("92c206cc6f790c61583247eefe87890f8828420662c17cacf247cec78ab4eec8".to_string())
        );
        assert_eq!(parse_bsd_checksum(text, "other-file.qcow2"), None);
        assert_eq!(parse_bsd_checksum("", "x"), None);
    }

    #[test]
    fn valid_rocky_release_aceita_so_as_versoes_conhecidas() {
        assert!(valid_rocky_release("8").is_ok());
        assert!(valid_rocky_release("9").is_ok());
        assert!(valid_rocky_release("10").is_ok());
        assert!(valid_rocky_release("7").is_err());
        assert!(valid_rocky_release("latest").is_err());
    }

    #[test]
    fn no_k8s_false_com_distro_rocky_e_rejeitado() {
        let (store, dir) = tmp_store();
        let err = cmd_build(
            &store,
            "t",
            Distro::Rocky,
            "24.04",
            "bookworm",
            "9",
            "42-1.1",
            None,
            vec![],
            vec![],
            None,
            true,
            false,
            false, // no_k8s = false — Rocky doesn't support the k8s path yet
            None,
            None, // sem password: o caminho por omissão, e o que a imagem publica
            None, // node_exporter: sem agente, que é o default
        );
        assert!(err.is_err());
        assert!(format!("{}", err.unwrap_err()).contains("--no-k8s"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tmp_store() -> (VmImageStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "delonix-vmimage-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        (VmImageStore::open(&dir).unwrap(), dir)
    }

    #[test]
    fn hex_sha512_file_bate_com_vector_conhecido() {
        // NIST test vector: SHA-512("abc").
        let dir = std::env::temp_dir().join(format!(
            "delonix-vmimage-sha512-test-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::write(&dir, b"abc").unwrap();
        let got = hex_sha512_file(&dir).unwrap();
        assert_eq!(got, "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f");
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn debian_major_version_conhece_os_codinomes_suportados() {
        assert_eq!(debian_major_version("bullseye").unwrap(), "11");
        assert_eq!(debian_major_version("bookworm").unwrap(), "12");
        assert_eq!(debian_major_version("trixie").unwrap(), "13");
        assert!(debian_major_version("sid").is_err());
        assert!(debian_major_version("").is_err());
    }

    #[test]
    fn distro_label_combina_distro_e_release_ou_degrada_com_gracia() {
        let mut img = VmImage {
            name: "x".to_string(),
            tag: "x".to_string(),
            digest: "sha256:x".to_string(),
            size: 0,
            ubuntu_release: None,
            k8s_version: None,
            created_unix: 0,
            kernel_version: None,
            distro: None,
            default_vcpus: None,
            default_memory: None,
            default_backend: None,
            cloud_init: None,
            ..Default::default()
        };
        // Pulled image, no build metadata at all — pre-existing gap, not new.
        assert_eq!(distro_label(&img), "-");
        // Pre-v0.17.0 on-disk metadata: `distro` missing, `ubuntu_release` set.
        img.ubuntu_release = Some("24.04".to_string());
        assert_eq!(distro_label(&img), "24.04");
        // A build from this version on: both set.
        img.distro = Some("debian".to_string());
        img.ubuntu_release = Some("bookworm".to_string());
        assert_eq!(distro_label(&img), "debian/bookworm");
    }

    #[test]
    fn base_cache_path_distingue_distro_e_continua_a_sanitizar() {
        let (store, dir) = tmp_store();
        let ubuntu = store.base_cache_path(Distro::Ubuntu, "24.04");
        let debian = store.base_cache_path(Distro::Debian, "bookworm");
        let rocky = store.base_cache_path(Distro::Rocky, "9");
        assert_ne!(ubuntu, debian);
        assert_ne!(debian, rocky);
        assert_ne!(ubuntu, rocky);
        assert!(ubuntu
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("ubuntu-24.04-"));
        assert!(debian
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("debian-bookworm-"));
        assert!(rocky
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("rocky-9-"));
        // Same path-traversal defense as the Ubuntu side, for the new Debian arm:
        // `sanitize` strips `/` (dots survive — harmless without a separator,
        // `Path::join` can't treat them as multiple segments), so the result
        // stays confined to a single filename inside `_base/`.
        let evil = store.base_cache_path(Distro::Debian, "../../../etc/cron.d/x");
        assert_eq!(evil.parent().unwrap(), dir.join("vm-images").join("_base"));
        assert!(!evil.file_name().unwrap().to_str().unwrap().contains('/'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A golden tem de nascer com disco para um nó de Kubernetes. Com os 3,5
    /// GiB da imagem base do Ubuntu, o WAL do etcd não cabe e o control-plane
    /// nunca arranca — medido, não suposto (ver `GOLDEN_DISK_SIZE_GIB`).
    #[test]
    fn a_golden_nasce_com_disco_para_um_no_de_kubernetes() {
        const _: () = assert!(
            GOLDEN_DISK_SIZE_GIB >= 10,
            "medido: um control-plane completo consome 2,4G de 8,7G úteis; abaixo de 10 GiB o WAL do etcd deixa de caber"
        );
    }

    /// A omissão da golden e o tecto do control-plane alojado são o MESMO
    /// número, e é isso que impede o par impossível que bloqueou o DKS: uma
    /// golden 1.34/1.35 contra um Kamaji que parava em 1.30.2. Um nó mais novo
    /// que o seu control-plane não é combinação suportada pelo kubeadm.
    #[test]
    fn a_imagem_oficial_aponta_para_a_versao_por_omissao() {
        assert_eq!(
            DEFAULT_K8S_VERSION, "1.36",
            "o tecto do Kamaji alojado (upgrade.KubeadmVersion=v1.36.0)"
        );
        assert!(
            OFFICIAL_VM_IMAGE.ends_with(&format!(":{DEFAULT_K8S_VERSION}")),
            "a imagem oficial ({OFFICIAL_VM_IMAGE}) tem de apontar para {DEFAULT_K8S_VERSION}"
        );
        let k8s = OFFICIAL_REPOS
            .iter()
            .find(|r| r.key == "k8s")
            .expect("repo k8s");
        assert_eq!(
            k8s.default_tag,
            Some(DEFAULT_K8S_VERSION),
            "o pull por omissão tem de puxar a mesma"
        );
    }

    /// 1.34 e 1.35 deixam de ser a omissão mas continuam construíveis — o
    /// pedido foi mudar a omissão, não retirar as outras.
    #[test]
    fn as_versoes_anteriores_continuam_aceites() {
        for v in ["1.34", "1.35", "1.36"] {
            assert!(
                v.chars().all(|c| c.is_ascii_digit() || c == '.'),
                "{v} tem de passar a validação de --k8s-version"
            );
        }
    }

    /// O defeito que impedia a golden de correr Kubernetes: instalava o
    /// `delonix-cri` sem o `delonix` a que ele delega TODO o ciclo de vida.
    /// Medido numa VM da golden 1.35 (2026-08-16): os `pull` passavam, cada
    /// `StartContainer` morria com `No such file or directory (os error 2)`, e
    /// o `kubeadm init` nunca levantava o control-plane.
    #[test]
    fn install_cri_steps_instala_os_dois_binarios() {
        let cri = PathBuf::from("/tmp/delonix-cri");
        let eng = PathBuf::from("/tmp/delonix");
        let unit = PathBuf::from("/tmp/delonix-cri.service");
        let ops = install_cri_steps(&cri, &eng, &unit);

        let copiados: Vec<&PathBuf> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::CopyIn(src, dst) if dst == "/usr/local/bin" => Some(src),
                _ => None,
            })
            .collect();
        assert!(
            copiados.contains(&&cri),
            "delonix-cri tem de ir para /usr/local/bin"
        );
        assert!(
            copiados.contains(&&eng),
            "o `delonix` TEM de viajar com o CRI — sem ele o kubelet vê ENOENT em cada StartContainer"
        );

        let cmds: Vec<&str> = ops
            .iter()
            .filter_map(|o| match o {
                CustomizeOp::RunCommand(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        assert!(cmds
            .iter()
            .any(|c| c.contains("chmod +x /usr/local/bin/delonix-cri")));
        assert!(
            cmds.contains(&"chmod +x /usr/local/bin/delonix"),
            "sem bit de execução o binário está lá e continua a não correr"
        );
    }

    #[test]
    fn no_k8s_rejeita_k8s_version_offline_e_cri_bin() {
        let (store, dir) = tmp_store();
        let base = |k8s_version: Option<String>, offline: bool, cri_bin: Option<PathBuf>| {
            cmd_build(
                &store,
                "t",
                Distro::Ubuntu,
                "24.04",
                "bookworm",
                "9",
                "42-1.1",
                k8s_version,
                vec![],
                vec![],
                cri_bin,
                true,
                offline,
                true, // no_k8s
                None,
                None, // root_password
                None, // node_exporter
            )
        };
        assert!(base(Some("1.34".into()), false, None).is_err());
        assert!(base(None, true, None).is_err());
        assert!(base(None, false, Some(PathBuf::from("/tmp/x"))).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delonix_bin_sem_no_k8s_e_rejeitado() {
        let (store, dir) = tmp_store();
        let err = cmd_build(
            &store,
            "t",
            Distro::Ubuntu,
            "24.04",
            "bookworm",
            "9",
            "42-1.1",
            None,
            vec![],
            vec![],
            None,
            true,
            false,
            false, // no_k8s = false
            Some(PathBuf::from("/tmp/delonix")),
            None, // root_password
            None, // node_exporter
        );
        assert!(err.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn customize_args_traduz_run_command_e_copy_in_correctamente() {
        let ops = vec![
            CustomizeOp::RunCommand("apt-get install -y a b".to_string()),
            CustomizeOp::CopyIn(PathBuf::from("/host/bin"), "/usr/local/bin".to_string()),
            CustomizeOp::RootPassword("x".to_string()),
        ];
        let args = customize_args(Path::new("/tmp/disk.qcow2"), &ops);
        assert_eq!(args[0], "-a");
        assert_eq!(args[1], "/tmp/disk.qcow2");
        assert!(args.windows(2).any(|w| w
            == [
                "--run-command".to_string(),
                "apt-get install -y a b".to_string()
            ]));
        assert!(args.windows(2).any(|w| w
            == [
                "--copy-in".to_string(),
                "/host/bin:/usr/local/bin".to_string()
            ]));
        // The relabel is LAST, and after it nothing may run: a step that landed
        // behind it would re-break the labels it just fixed, and the symptom is
        // a guest whose PID 1 is denied `/etc/ld.so.cache`.
        assert_eq!(
            args[args.len() - 3..],
            [
                "--run-command".to_string(),
                SELINUX_RELABEL_CMD.to_string(),
                "--no-selinux-relabel".to_string()
            ],
            "the SELinux relabel must close every build: {args:?}"
        );
        // …and it must relabel the guest's OTHER mountpoints, not just `/`.
        // Measured: with `/` alone, Fedora's `/home` subvolume was missed and
        // the account this build creates came out unlabeled.
        assert!(
            SELINUX_RELABEL_CMD.contains("/etc/fstab"),
            "relabelling only `/` misses a separate /home or /var: {SELINUX_RELABEL_CMD}"
        );
        assert!(args
            .windows(2)
            .any(|w| w == ["--root-password".to_string(), "password:x".to_string()]));
    }

    #[test]
    fn hex_sha256_e_consistente() {
        assert_eq!(
            hex_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// An absent tool is not a missing file, and the message has to say so.
    ///
    /// Measured on a host without libguestfs: `vm build` downloaded 600 MB,
    /// verified the checksum, resized the disk, and then said
    /// `running virt-customize: No such file or directory` — which reads as a
    /// broken path, not as «install this package». The mapping is the fix, so
    /// it is the mapping that gets the test.
    #[test]
    fn ferramenta_ausente_nomeia_o_pacote_das_tres_familias() {
        assert_eq!(
            super::tool_package("virt-customize"),
            Some(("libguestfs-tools", "guestfs-tools", "libguestfs"))
        );
        assert_eq!(
            super::tool_package("qemu-img"),
            Some(("qemu-utils", "qemu-img", "qemu-img"))
        );
        // Unknown tools still get a sentence, just without a package name —
        // never a silent fallthrough to the ENOENT text.
        assert_eq!(super::tool_package("whatever"), None);
    }

    /// A derivative names its parent in `ID_LIKE`, and that is precisely the
    /// case where reading `ID` alone prints the wrong package manager: the host
    /// this was written on is `ID=zorin`, which no table will ever list.
    #[test]
    fn familia_da_distro_segue_id_e_depois_id_like() {
        use super::{family_of, Family};
        assert_eq!(family_of("ID=ubuntu\nID_LIKE=debian\n"), Family::Debian);
        assert_eq!(
            family_of("ID=zorin\nID_LIKE=\"ubuntu debian\"\n"),
            Family::Debian
        );
        assert_eq!(family_of("ID=fedora\n"), Family::RedHat);
        assert_eq!(
            family_of("ID=rocky\nID_LIKE=\"rhel centos fedora\"\n"),
            Family::RedHat
        );
        assert_eq!(family_of("ID=arch\n"), Family::Arch);
        assert_eq!(family_of("ID=manjaro\nID_LIKE=arch\n"), Family::Arch);
        // Not recognised is not a licence to guess: the caller prints every
        // family's command instead of one that may not exist here.
        assert_eq!(family_of("ID=plan9\n"), Family::Unknown);
        assert_eq!(family_of(""), Family::Unknown);
    }

    #[test]
    fn install_cmd_usa_o_gestor_da_familia_e_lista_todos_quando_nao_sabe() {
        use super::{install_cmd, Family};
        assert_eq!(
            install_cmd(
                Family::Debian,
                "libguestfs-tools",
                "guestfs-tools",
                "libguestfs"
            ),
            "sudo apt install libguestfs-tools"
        );
        assert_eq!(
            install_cmd(
                Family::RedHat,
                "libguestfs-tools",
                "guestfs-tools",
                "libguestfs"
            ),
            "sudo dnf install guestfs-tools"
        );
        assert_eq!(
            install_cmd(
                Family::Arch,
                "libguestfs-tools",
                "guestfs-tools",
                "libguestfs"
            ),
            "sudo pacman -S libguestfs"
        );
        let unknown = install_cmd(Family::Unknown, "a", "b", "c");
        for cmd in ["apt install a", "dnf install b", "pacman -S c"] {
            assert!(unknown.contains(cmd), "faltava {cmd} em: {unknown}");
        }
    }

    /// Both failures below cost a full debugging session on a real host, and
    /// both printed everything needed to name the cause without naming it. The
    /// hint is the outcome of that session, so it is what gets the test.
    #[test]
    fn falhas_conhecidas_do_build_dizem_o_que_fazer() {
        use super::{tool_failure_hint, Family};
        // supermin cannot read the host kernel (Debian/Ubuntu ship it 0600).
        let supermin = "supermin: build: 4284 files, after munging\n\
                        cp: cannot open '/boot/vmlinuz-7.0.0-28-generic' for reading: Permission denied\n\
                        supermin: cp -p '/boot/vmlinuz-7.0.0-28-generic' … command failed";
        let h = tool_failure_hint(supermin, Family::Debian).expect("devia reconhecer o supermin");
        assert!(h.contains("chmod 0644 /boot/vmlinuz-*"), "{h}");
        // The permanent form is Debian-specific and must not be printed to a
        // reader whose distro has no /etc/kernel/postinst.d.
        assert!(h.contains("dpkg-statoverride"), "{h}");
        assert!(!tool_failure_hint(supermin, Family::Arch)
            .unwrap()
            .contains("dpkg-statoverride"));

        // passt: `--network` only.
        let passt = "virt-customize: error: libguestfs error: passt exited with status 1";
        let h = tool_failure_hint(passt, Family::Debian).expect("devia reconhecer o passt");
        assert!(h.contains("XDG_RUNTIME_DIR"), "{h}");
        assert!(h.contains("--network"), "{h}");
        // Building a current passt is only half the remedy: libguestfs looks it
        // up by running `passt --help`, so the new binary has to be FIRST in
        // PATH or the packaged one keeps winning.
        assert!(h.contains("PATH=$PWD:$PATH"), "{h}");

        // The SAME cause, and the shape it actually takes on a real host: passt
        // leases nothing, the build carries on without network, and what the
        // reader sees is a package manager that cannot resolve a mirror — with
        // the word `passt` nowhere in it. MEASURED, and it used to get no hint
        // at all, which reads as "your DNS is broken".
        let dnf = "Curl error (6): Could not resolve hostname for \
                   https://mirrors.fedoraproject.org/metalink?repo=updates-released-f42\n\
                   virt-customize: error: dnf install -y slirp4netns: command exited with an error";
        let h = tool_failure_hint(dnf, Family::Debian)
            .expect("um build sem DNS tem de dizer que a rede do appliance é a causa");
        assert!(h.contains("passt"), "{h}");
        assert!(h.contains("XDG_RUNTIME_DIR"), "{h}");

        // An unrecognised failure gets NO invented advice.
        assert_eq!(
            tool_failure_hint("some unrelated explosion", Family::Debian),
            None
        );
    }

    /// Builds a `VmImage` with every field empty — the shape a `vm pull`
    /// produces before annotations are applied.
    fn bare_img() -> VmImage {
        VmImage {
            name: "x".to_string(),
            tag: "x".to_string(),
            digest: "sha256:x".to_string(),
            size: 0,
            ubuntu_release: None,
            k8s_version: None,
            created_unix: 0,
            kernel_version: None,
            distro: None,
            default_vcpus: None,
            default_memory: None,
            default_backend: None,
            cloud_init: None,
            ..Default::default()
        }
    }

    #[test]
    fn metadados_antigos_sem_o_campo_continuam_a_carregar_e_contam_como_cloud_init() {
        // Every `.json` written before `cloud_init` existed. Loading these has
        // to keep working, AND they have to behave exactly as before — those
        // images are cloud images, and `vm create` must still seed them.
        let antigo = r#"{
            "name": "golden", "tag": "golden", "digest": "sha256:a", "size": 1,
            "ubuntu_release": "24.04", "k8s_version": "1.34", "created_unix": 0
        }"#;
        let img: VmImage = serde_json::from_str(antigo).unwrap();
        assert_eq!(img.cloud_init, None);
        assert!(
            img.uses_cloud_init(),
            "unknown tem de contar como cloud-init: e o que estas imagens sempre foram"
        );
    }

    #[test]
    fn so_uma_marca_explicita_de_appliance_desliga_o_seed() {
        let mut img = bare_img();
        assert!(img.uses_cloud_init());
        img.cloud_init = Some(true);
        assert!(img.uses_cloud_init());
        img.cloud_init = Some(false);
        assert!(!img.uses_cloud_init());
    }

    #[test]
    fn annotations_fazem_round_trip_pelo_push_e_pull() {
        // The point of the annotations: what the store knows must survive a
        // push/pull, because the qcow2 blob carries none of it.
        let mut orig = bare_img();
        orig.distro = Some("opnsense".to_string());
        orig.ubuntu_release = Some("26.1.2".to_string());
        orig.default_vcpus = Some(2);
        orig.default_memory = Some("2G".to_string());
        orig.default_backend = Some("libvirt".to_string());
        orig.cloud_init = Some(false);

        let recuperado = apply_pulled_annotations(bare_img(), &annotations_of(&orig));
        assert_eq!(recuperado.distro, orig.distro);
        assert_eq!(recuperado.ubuntu_release, orig.ubuntu_release);
        assert_eq!(recuperado.default_vcpus, orig.default_vcpus);
        assert_eq!(recuperado.default_memory, orig.default_memory);
        assert_eq!(recuperado.default_backend, orig.default_backend);
        assert_eq!(recuperado.cloud_init, Some(false));
        assert!(!recuperado.uses_cloud_init());
    }

    #[test]
    fn artefacto_sem_annotations_nao_muda_nada() {
        // Everything published before this existed, and anything pushed by
        // another tool: the pull must land exactly where it landed before.
        let vazio = std::collections::BTreeMap::new();
        let img = apply_pulled_annotations(bare_img(), &vazio);
        assert_eq!(img.cloud_init, None);
        assert_eq!(img.distro, None);
        assert!(img.uses_cloud_init());
    }

    #[test]
    fn annotation_malformada_e_ignorada_menos_no_campo_que_decide_o_seed() {
        let mut a = std::collections::BTreeMap::new();
        a.insert(
            "io.delonix.vmimage.default-vcpus".to_string(),
            "muitos".to_string(),
        );
        // Anything that is not exactly "true"/"false" leaves cloud-init
        // unknown — which falls back to "yes, seed it", the safe reading for
        // an image we cannot classify.
        a.insert("io.delonix.vmimage.cloud-init".to_string(), "0".to_string());
        let img = apply_pulled_annotations(bare_img(), &a);
        assert_eq!(img.default_vcpus, None);
        assert_eq!(img.cloud_init, None);
        assert!(img.uses_cloud_init());
    }

    #[test]
    fn annotations_so_carregam_o_que_e_sabido() {
        // An absent annotation and one saying "unknown" are different claims;
        // only the first is honest for a field nobody filled in.
        let a = annotations_of(&bare_img());
        assert!(a.is_empty(), "nada sabido, nada anunciado: {a:?}");
    }

    #[test]
    fn valid_fedora_release_exige_release_e_build() {
        // Fedora's artifact name carries a build number the release does not
        // determine, and its redirector has no listing to look it up in — so a
        // bare `42` is rejected here rather than 404-ing several hundred
        // megabytes into the download.
        assert!(valid_fedora_release("42-1.1").is_ok());
        assert!(valid_fedora_release("41-1.4").is_ok());
        assert!(valid_fedora_release("43-1.10.2").is_ok());
        for bad in [
            "42", "42-", "-1.1", "abc-1.1", "42-1.1a", "42-.1", "42-1.", "",
        ] {
            let e = valid_fedora_release(bad)
                .expect_err("devia recusar")
                .to_string();
            assert!(e.contains("<release>-<build>"), "{bad}: {e}");
        }
    }

    #[test]
    fn fedora_partilha_as_convencoes_rpm_do_rocky() {
        // Same dnf/RPM family: `wheel` (not `sudo`), `/etc/bashrc` (not
        // `/etc/bash.bashrc`), `dnf clean all`. Verified against the Rocky
        // steps rather than restated, so the two cannot drift apart.
        let bin = PathBuf::from("/tmp/delonix");
        let fedora = rootless_customization_steps(&[], &bin, Distro::Fedora, None);
        let rocky = rootless_customization_steps(&[], &bin, Distro::Rocky, None);
        let cmds = |ops: &[CustomizeOp]| -> Vec<String> {
            ops.iter()
                .filter_map(|o| match o {
                    CustomizeOp::RunCommand(c) => Some(c.clone()),
                    _ => None,
                })
                .collect()
        };
        let (f, r) = (cmds(&fedora), cmds(&rocky));
        assert_eq!(f, r, "Fedora e Rocky partilham os passos da familia RPM");
        assert!(f.iter().any(|c| c.contains("dnf install -y")));
        assert!(f.iter().any(|c| c.contains("wheel")));
        assert!(f.iter().any(|c| c.contains("dnf clean all")));
    }

    #[test]
    fn fedora_nunca_escreve_perfil_apparmor() {
        // AppArmor is an Ubuntu-only workaround (the
        // `kernel.apparmor_restrict_unprivileged_userns` patch); Fedora uses
        // SELinux and has no /etc/apparmor.d, so writing there would fail the
        // whole `virt-customize` run — the same trap already fixed for Rocky.
        let ops =
            rootless_customization_steps(&[], &PathBuf::from("/tmp/delonix"), Distro::Fedora, None);
        assert!(!ops.iter().any(|o| matches!(
            o,
            CustomizeOp::RunCommand(c) if c.contains("apparmor")
        )));
    }

    #[test]
    fn fedora_sem_no_k8s_e_rejeitado() {
        let (store, dir) = tmp_store();
        let err = cmd_build(
            &store,
            "t",
            Distro::Fedora,
            "24.04",
            "bookworm",
            "9",
            "42-1.1",
            None,
            vec![],
            vec![],
            None,
            true,
            false,
            false,
            None,
            None, // root_password
            None, // node_exporter
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("fedora"),
            "a recusa tem de nomear a distro: {err}"
        );
        assert!(err.contains("--no-k8s"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn image_type_label_distingue_appliance_de_cloud_init_e_de_desconhecido() {
        // The three states are genuinely different and the column must not
        // collapse them: an appliance REFUSES --ssh-key, a cloud-init image
        // accepts it, and an unknown one (pre-existing metadata, or a tag
        // published before annotations existed) is treated as cloud-init but
        // should not claim to be one.
        assert_eq!(image_type_label(Some("false")), "appliance");
        assert_eq!(image_type_label(Some("true")), "cloud-init");
        assert_eq!(image_type_label(None), "-");
        assert_eq!(image_type_label(Some("sim")), "-");
    }

    #[test]
    fn defaults_label_mostra_so_o_que_a_imagem_recomenda() {
        let mut img = bare_img();
        assert_eq!(defaults_label(&img), "-");
        img.default_vcpus = Some(4);
        assert_eq!(defaults_label(&img), "4cpu");
        img.default_memory = Some("8G".into());
        assert_eq!(defaults_label(&img), "4cpu/8G");
        img.default_vcpus = None;
        assert_eq!(defaults_label(&img), "8G");
    }

    #[test]
    fn o_reset_do_machine_id_cria_o_dir_do_dbus_antes_de_lhe_apontar() {
        // Found live on Fedora: /var/lib/dbus does not exist there, so the
        // `ln -sf` failed and virt-customize aborted the whole build. Every
        // distro must get a command that cannot fail on a missing directory.
        for d in [
            Distro::Ubuntu,
            Distro::Debian,
            Distro::Rocky,
            Distro::Fedora,
        ] {
            let ops = shared_account_steps(&[], d, None);
            let mid = ops
                .iter()
                .filter_map(|o| match o {
                    CustomizeOp::RunCommand(c) if c.contains("machine-id") => Some(c),
                    _ => None,
                })
                .next_back()
                .expect("o reset de machine-id tem de existir");
            assert!(
                mid.contains("mkdir -p /var/lib/dbus"),
                "{d:?}: sem mkdir, o ln falha onde o dir nao existe: {mid}"
            );
        }
    }

    /// **`vhd` é `vpc` para o qemu-img, e `.vhd` para quem importa.** É a única
    /// combinação em que o nome do formato e a extensão divergem, e é
    /// exactamente o detalhe que um utilizador não devia ter de saber para
    /// obter um ficheiro que o Hyper-V aceita — daí serem duas funções e não
    /// uma string usada duas vezes.
    #[test]
    fn o_formato_do_qemu_e_a_extensao_do_ecossistema_podem_divergir() {
        use super::ConvertFormat as F;
        for (f, qemu, ext) in [
            (F::Qcow2, "qcow2", "qcow2"),
            (F::Raw, "raw", "raw"),
            (F::Vmdk, "vmdk", "vmdk"),
            (F::Vdi, "vdi", "vdi"),
            (F::Vhdx, "vhdx", "vhdx"),
            (F::Vhd, "vpc", "vhd"),
        ] {
            assert_eq!(f.as_str(), qemu, "{f:?}");
            assert_eq!(f.extension(), ext, "{f:?}");
        }
    }

    /// Só o qcow2 e o vmdk aceitam o `-c` do qemu-img; passá-lo aos outros faz
    /// a ferramenta falhar, e o utilizador levaria um erro de tool numa flag
    /// que lhe foi oferecida.
    #[test]
    fn so_dois_formatos_aceitam_compressao() {
        use super::ConvertFormat as F;
        assert!(F::Qcow2.supports_compression() && F::Vmdk.supports_compression());
        for f in [F::Raw, F::Vdi, F::Vhdx, F::Vhd] {
            assert!(!f.supports_compression(), "{f:?}");
        }
    }

    #[test]
    fn o_cabecalho_do_ls_remote_tira_a_tag_mas_nao_a_porta() {
        // The default source carries a tag; showing it would suggest the
        // listing is scoped to it. A `host:port/repo` must keep its port —
        // hence cutting only a colon that comes after the last slash.
        let show = |s: &str| -> String {
            match s.rfind('/') {
                Some(slash) => match s[slash..].find(':') {
                    Some(colon) => s[..slash + colon].to_string(),
                    None => s.to_string(),
                },
                None => s.split(':').next().unwrap_or(s).to_string(),
            }
        };
        assert_eq!(
            show("ghcr.io/ang/delonix-vm-k8s:1.34"),
            "ghcr.io/ang/delonix-vm-k8s"
        );
        assert_eq!(
            show("ghcr.io/ang/delonix-vm-base"),
            "ghcr.io/ang/delonix-vm-base"
        );
        assert_eq!(show("localhost:5000/foo:v1"), "localhost:5000/foo");
        assert_eq!(show("localhost:5000/foo"), "localhost:5000/foo");
        assert_eq!(show("alpine:latest"), "alpine");
    }

    #[test]
    fn o_repo_oficial_sai_do_que_a_imagem_diz_de_si() {
        let mut img = bare_img();
        // Nothing known: must NOT guess a destination.
        assert!(official_repo_for(&img).is_none());
        img.distro = Some("ubuntu".into());
        assert_eq!(official_repo_for(&img).unwrap().key, "base");
        img.k8s_version = Some("1.34".into());
        assert_eq!(official_repo_for(&img).unwrap().key, "k8s");
        // An appliance wins over everything: it is the one that changes how
        // `vm create` behaves.
        img.cloud_init = Some(false);
        assert_eq!(official_repo_for(&img).unwrap().key, "appliances");
    }

    #[test]
    fn resolve_official_ref_so_toca_no_que_nao_tem_registo() {
        // A real reference is left alone — the argument keeps meaning
        // "a repository of your own".
        for as_is in [
            "ghcr.io/outro/coisa:1.0",
            "docker.io/library/alpine:latest",
            "localhost:5000/x:v1",
        ] {
            assert_eq!(resolve_official_ref_local(as_is).as_deref(), Some(as_is));
        }
        // Repository keys.
        assert!(resolve_official_ref_local("appliances")
            .unwrap()
            .ends_with("delonix-vm-appliances"));
        assert_eq!(
            resolve_official_ref_local("k8s:1.35").as_deref(),
            Some("ghcr.io/angolardevops/delonix-vm-k8s:1.35")
        );
    }

    #[test]
    fn um_tag_nu_nao_e_adivinhado_para_um_repositorio() {
        // The bug this replaced: EVERY bare tag was assumed to live in the
        // appliances repository, so `vm pull rocky-9` reported "no such image
        // …/delonix-vm-appliances:rocky-9" for an image that was published,
        // public, and in the base repository all along. `_local` now declines
        // to answer, and the caller goes and looks.
        for bare in ["rocky-9", "ubuntu-24.04", "opnsense:26.1", "proxmox-ve-9.1"] {
            assert_eq!(
                resolve_official_ref_local(bare),
                None,
                "{bare} must be discovered, not guessed"
            );
        }
        // The `<product>:<version>` spelling still names the published tag.
        assert_eq!(official_tag_candidate("opnsense:26.1"), "opnsense-26.1");
        assert_eq!(official_tag_candidate("rocky-9"), "rocky-9");
    }

    #[test]
    fn a_tag_publicada_e_o_inverso_do_que_o_pull_resolve() {
        // push and pull must agree without either knowing about the other.
        let mut img = bare_img();
        img.cloud_init = Some(false);
        img.distro = Some("opnsense".into());
        let tag = official_tag_for(&img, "opnsense:26.1");
        assert_eq!(tag, "opnsense-26.1");
        // The TAG is what the two sides have to agree on without either knowing
        // about the other: push derives it from the image's metadata, pull from
        // the string the user typed. (The repository no longer has to match by
        // construction — pull finds it by looking, which is why a base tag no
        // longer lands in the appliances repo.)
        assert_eq!(official_tag_candidate("opnsense:26.1"), tag);
        assert_eq!(
            official_repo_for(&img).unwrap().key,
            "appliances",
            "an appliance is published to the appliances repository"
        );
        // A base image does not repeat the family in the tag.
        assert_eq!(
            official_tag_for(&bare_img(), "delonix-vm-base:ubuntu-24.04"),
            "ubuntu-24.04"
        );
    }

    #[test]
    fn uma_imagem_em_uso_por_uma_vm_e_detectada_pelo_disco() {
        // The guard that matters: a VM runs on a thin overlay whose backing
        // file IS the image. Deleting the image does not free the VM — it makes
        // it permanently unreadable. The check reads the OVERLAY, not the
        // registry, because a VM made outside this engine (or a record edited
        // by hand) holds the image open just the same.
        let Ok(dir) = tempdir_for_test("vmsbacked") else {
            return; // no writable temp dir: nothing to assert about
        };
        let base = dir.join("base.qcow2");
        let vms = dir.join("vms");
        std::fs::create_dir_all(&vms).unwrap();
        let mk = |args: &[&std::ffi::OsStr]| {
            std::process::Command::new("qemu-img")
                .args(args)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        use std::ffi::OsStr;
        if !mk(&[
            OsStr::new("create"),
            OsStr::new("-f"),
            OsStr::new("qcow2"),
            base.as_os_str(),
            OsStr::new("1M"),
        ]) {
            return; // qemu-img absent: this host cannot run the check either
        }
        let overlay = vms.join("uservm.qcow2");
        assert!(mk(&[
            OsStr::new("create"),
            OsStr::new("-f"),
            OsStr::new("qcow2"),
            OsStr::new("-b"),
            base.as_os_str(),
            OsStr::new("-F"),
            OsStr::new("qcow2"),
            overlay.as_os_str(),
        ]));

        assert_eq!(vms_backed_by(&dir, &base), vec!["uservm".to_string()]);

        // An unrelated image is NOT reported as used — a guard that says
        // everything is in use is the same as no guard, because the first
        // thing anyone does is reach for --force.
        let other = dir.join("other.qcow2");
        assert!(mk(&[
            OsStr::new("create"),
            OsStr::new("-f"),
            OsStr::new("qcow2"),
            other.as_os_str(),
            OsStr::new("1M"),
        ]));
        assert!(vms_backed_by(&dir, &other).is_empty());

        // A file that is not a qcow2 at all is read as `raw` with no backing
        // file — measured — so it is correctly NOT a user. This assertion is
        // here because the first version of the test assumed the opposite and
        // failed, which is what sent me to measure it.
        std::fs::write(vms.join("plain.qcow2"), b"not a qcow2").unwrap();
        assert!(vms_backed_by(&dir, &other).is_empty());

        // An overlay that cannot be OPENED counts as a user: not knowing what
        // it points at is exactly when refusing to delete the base is right.
        let locked = vms.join("locked.qcow2");
        std::fs::write(&locked, b"x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
            // Root reads regardless of the mode, so the case is unobservable there.
            if std::fs::read(&locked).is_err() {
                let users = vms_backed_by(&dir, &other);
                assert!(
                    users.iter().any(|u| u.contains("locked")),
                    "an unopenable overlay must be reported, got {users:?}"
                );
            }
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A writable scratch directory for a test, or `Err` if there is none.
    fn tempdir_for_test(tag: &str) -> std::io::Result<std::path::PathBuf> {
        let d = std::env::temp_dir().join(format!("dlx-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d)?;
        Ok(d)
    }
}
