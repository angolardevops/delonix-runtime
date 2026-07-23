//! `delonix vm` — declarative microVMs (create/ls/stop/rm/status).

use std::path::PathBuf;
use std::process::Command;

use clap::Subcommand;
use clap_complete::engine::ArgValueCandidates;
use delonix_runtime_core::{Error, Result};
use delonix_vm::VmConfig;
use delonix_volume::VolumeStore;
use serde::{Deserialize, Serialize};

use super::manifest::{self, ManifestDoc};
use super::output;
use super::util::state_root;

/// `spec` for `kind: Vm` — mirrors `delonix_vm::VmConfig` (minus `name`, which
/// comes from `metadata.name`).
#[derive(Debug, Deserialize, Serialize)]
struct VmSpec {
    disk: String,
    #[serde(default = "default_vcpus")]
    vcpus: u32,
    #[serde(default = "default_memory")]
    memory: String,
    #[serde(default = "default_network")]
    network: String,
    kernel: Option<String>,
    initrd: Option<String>,
    firmware: Option<String>,
    cmdline: Option<String>,
    seed: Option<String>,
    /// cloud-init: hostname applied on first boot (CLI `--hostname`). Without an
    /// explicit `seed`, a NoCloud ISO is generated from these fields — full
    /// parity with `vm create` in the declarative path.
    hostname: Option<String>,
    /// cloud-init: authorized public SSH keys (CLI `--ssh-key`, repeatable).
    /// Each is `ssh-ed25519 AAAA…` or `@/path` to read from a file.
    #[serde(default, rename = "sshKeys", alias = "ssh_keys")]
    ssh_keys: Vec<String>,
    /// cloud-init: your own `user-data` (replaces the generated one) — a path or
    /// `@/path` (CLI `--user-data`). Full control for whoever needs it.
    #[serde(default, rename = "userData", alias = "user_data")]
    user_data: Option<String>,
    /// Canonical `restartPolicy` (uniform with `Container`); `restart_policy`
    /// stays accepted so earlier manifests don't break.
    #[serde(rename = "restartPolicy", alias = "restart_policy")]
    restart_policy: Option<String>,
    #[serde(default)]
    hugepages: bool,
    /// Canonical `cpuAffinity`; `cpu_affinity` stays accepted (back-compat).
    #[serde(rename = "cpuAffinity", alias = "cpu_affinity")]
    cpu_affinity: Option<String>,
    #[serde(default)]
    devices: Vec<String>,
    backend: Option<String>,
    /// Canonical `netMode`; `net_mode` stays accepted (back-compat).
    #[serde(rename = "netMode", alias = "net_mode")]
    net_mode: Option<String>,
    bridge: Option<String>,
    /// Volumes/Storage to mount inside the VM (virtio-9p) — closes the gap of
    /// giving storage to a VM without writing cloud-init/XML. See `VmVolumeSpec`.
    #[serde(default)]
    volumes: Vec<VmVolumeSpec>,
    #[serde(default)]
    vnc: bool,
    /// Static IP (libvirt `nat` mode): DHCP reservation on the libvirt network.
    #[serde(default)]
    ip: Option<String>,

    // --- Advanced libvirt knobs (libvirt backend) — full XML parity ---------
    /// Machine type (default `q35`).
    machine: Option<String>,
    /// CPU mode/model: `host-passthrough` (default), `host-model`, or a named model.
    #[serde(rename = "cpuModel", alias = "cpu_model")]
    cpu_model: Option<String>,
    /// CPU topology (sockets/cores/threads).
    #[serde(rename = "cpuTopology", alias = "cpu_topology")]
    cpu_topology: Option<CpuTopologySpec>,
    /// Emulated TPM 2.0.
    #[serde(default)]
    tpm: bool,
    /// Video model (`virtio`|`qxl`|`vga`|`none`).
    video: Option<String>,
    /// OS boot device order, e.g. `[cdrom, hd]`.
    #[serde(default, rename = "bootOrder", alias = "boot_order")]
    boot_order: Vec<String>,
    /// Extra disks beyond the main overlay + seed.
    #[serde(default, rename = "extraDisks", alias = "extra_disks")]
    extra_disks: Vec<ExtraDiskSpec>,
    /// Extra network interfaces beyond the primary one.
    #[serde(default, rename = "extraNics", alias = "extra_nics")]
    extra_nics: Vec<ExtraNicSpec>,
    /// Raw libvirt XML fragments injected before `</devices>` (trusted manifests).
    #[serde(default, rename = "libvirtXmlOverlay", alias = "libvirt_xml_overlay")]
    libvirt_xml_overlay: Vec<String>,
    /// Full `<domain>` XML used verbatim (ultimate escape hatch; trusted only).
    #[serde(rename = "libvirtXml", alias = "libvirt_xml")]
    libvirt_xml: Option<String>,
}

/// `spec.cpuTopology` of a `kind: Vm`.
#[derive(Debug, Deserialize, Serialize)]
struct CpuTopologySpec {
    #[serde(default)]
    sockets: u32,
    #[serde(default)]
    cores: u32,
    #[serde(default)]
    threads: u32,
}

/// One entry of `spec.extraDisks`.
#[derive(Debug, Deserialize, Serialize)]
struct ExtraDiskSpec {
    /// Host path of the disk image.
    source: String,
    /// `disk` (default) or `cdrom`.
    device: Option<String>,
    /// Bus: `virtio` (default), `sata`, `scsi`, `ide`.
    bus: Option<String>,
    /// Format: `qcow2` (default) or `raw`.
    format: Option<String>,
    /// Mount read-only.
    #[serde(default, rename = "readOnly", alias = "read_only")]
    read_only: bool,
    /// Explicit target dev (auto-assigned when omitted).
    target: Option<String>,
}

/// One entry of `spec.extraNics`.
#[derive(Debug, Deserialize, Serialize)]
struct ExtraNicSpec {
    /// `network` (libvirt network), `bridge` (host bridge) or `user`.
    #[serde(rename = "type", alias = "kind")]
    kind: String,
    /// Network/bridge name.
    source: Option<String>,
    /// Model: `virtio` (default), `e1000`, …
    model: Option<String>,
    /// Fixed MAC (random when omitted).
    mac: Option<String>,
}

/// One entry of a VM's `spec.volumes`: refers to a `Volume`/`Storage` by
/// name and says where to mount it in the guest.
#[derive(Debug, Deserialize, Serialize)]
struct VmVolumeSpec {
    /// Name of a `kind: Volume` or `kind: Storage` (resolved at apply time).
    name: String,
    /// Mount point in the guest (e.g. `/mnt/dados`).
    #[serde(rename = "mountPath")]
    mount_path: String,
    /// Mount read-only.
    #[serde(default, rename = "readOnly")]
    read_only: bool,
}

/// Field names accepted in the `spec` of `kind: Vm` (canonical + legacy aliases),
/// for the unknown-field warning. Kept aligned with `VmSpec` by the
/// test `manifest::tests::examples_nao_tem_campos_desconhecidos`.
pub(crate) const VM_SPEC_FIELDS: &[&str] = &[
    "disk",
    "vcpus",
    "memory",
    "network",
    "kernel",
    "initrd",
    "firmware",
    "cmdline",
    "seed",
    "hostname",
    "sshKeys",
    "ssh_keys",
    "userData",
    "user_data",
    "restartPolicy",
    "restart_policy",
    "hugepages",
    "cpuAffinity",
    "cpu_affinity",
    "devices",
    "backend",
    "netMode",
    "net_mode",
    "bridge",
    "volumes",
    "vnc",
    "ip",
    "machine",
    "cpuModel",
    "cpu_model",
    "cpuTopology",
    "cpu_topology",
    "tpm",
    "video",
    "bootOrder",
    "boot_order",
    "extraDisks",
    "extra_disks",
    "extraNics",
    "extra_nics",
    "libvirtXmlOverlay",
    "libvirt_xml_overlay",
    "libvirtXml",
    "libvirt_xml",
];

fn default_vcpus() -> u32 {
    1
}
fn default_memory() -> String {
    "1G".to_string()
}
fn default_network() -> String {
    // The default ingress network (bridge delonix0/10.200, always present) — NOT
    // "bridge", which `resolve_net` would treat as a PRIVATE network to create
    // first (`vm create dev` failed with "ingress network 'bridge'" — the default
    // pointed at a network no one had created).
    "ingress".to_string()
}

// `Create` is bigger than the other variants (many optional VM flags) — it's a
// CLI enum parsed ONCE per invocation, not a hot-path; boxing each field just to
// please the lint would complicate the `clap` derive with no real benefit.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub enum VmCmd {
    /// Dashboard (KPIs + table) of the VMs — interactive TUI, or `--once` snapshot.
    Dash {
        #[arg(long)]
        once: bool,
    },
    /// Bootstrap a project with a VM manifest — files ALREADY FILLED IN (images
    /// included), ready to use without editing anything.
    Init {
        /// Project directory (default: the current one).
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Project name (default: the directory name).
        #[arg(long)]
        name: Option<String>,
        /// Image to use. Omit = fills in with the default image.
        #[arg(long)]
        image: Option<String>,
        /// Overwrite existing files.
        #[arg(long)]
        force: bool,
        /// Generate a complete PROJECT for a stack (e.g. `python`) with best
        /// practices, instead of the generic scaffold. `--template list` shows the available ones.
        #[arg(long, short = 't')]
        template: Option<String>,
        /// After generating, build the image, start it, and wait until healthy.
        #[arg(long)]
        up: bool,
    },
    /// Create (or auto-recover) a VM.
    Create {
        name: String,
        /// Base disk (qcow2/raw) — becomes a per-VM overlay. Omit to use the
        /// local golden VM image (if there is exactly one; `image --vm ls`).
        #[arg(long)]
        disk: Option<String>,
        #[arg(long, default_value_t = 1)]
        vcpus: u32,
        /// Memory (`"2G"`/`"1024M"`).
        #[arg(long, default_value = "1G")]
        memory: String,
        /// Ingress network for the tap (default: the system ingress network; a
        /// custom network must be created first with `delonix network create`).
        #[arg(long, default_value = "ingress")]
        network: String,
        /// Kernel for direct boot.
        #[arg(long)]
        kernel: Option<String>,
        #[arg(long)]
        initrd: Option<String>,
        /// Firmware, alternative to the kernel (cloud images).
        #[arg(long)]
        firmware: Option<String>,
        #[arg(long)]
        cmdline: Option<String>,
        /// Ready-made cloud-init (NoCloud) ISO — if given, takes priority over
        /// `--hostname`/`--ssh-key`/`--user-data` (those generate the ISO; this
        /// uses it directly).
        #[arg(long)]
        seed: Option<String>,
        /// Hostname to apply on first boot (generates the NoCloud ISO if no
        /// explicit `--seed` is given).
        #[arg(long)]
        hostname: Option<String>,
        /// Authorized public SSH key, `ssh-ed25519 AAAA...` or `@path`
        /// to read from a file. Repeatable.
        #[arg(long = "ssh-key")]
        ssh_keys: Vec<String>,
        /// Your own cloud-init `user-data` (fully replaces the default-generated
        /// one) — full control for whoever needs it.
        #[arg(long)]
        user_data: Option<PathBuf>,
        /// `no`|`on-failure`|`always`.
        #[arg(long)]
        restart_policy: Option<String>,
        #[arg(long)]
        hugepages: bool,
        /// Core affinity, e.g. `8-15`.
        #[arg(long)]
        cpu_affinity: Option<String>,
        /// VFIO PCI passthrough, repeatable.
        #[arg(long = "device")]
        devices: Vec<String>,
        /// `cloud-hypervisor`|`libvirt` (omit = auto-detection).
        #[arg(long)]
        backend: Option<String>,
        /// libvirt only: `user`|`nat`|`bridge`.
        #[arg(long)]
        net_mode: Option<String>,
        /// Bridge name (net-mode=bridge) or libvirt network (nat).
        #[arg(long)]
        bridge: Option<String>,
        /// Static IP (libvirt nat mode): DHCP reservation on the libvirt network.
        #[arg(long)]
        ip: Option<String>,
        /// VNC graphical console (libvirt backend only — Cloud Hypervisor has no display).
        #[arg(long)]
        vnc: bool,
        /// After starting, attach to the serial console to watch the boot live (Ctrl-] to detach).
        #[arg(long)]
        console: bool,
        /// After starting, wait (with a spinner) until the VM has an IP, up to --boot-timeout.
        #[arg(long)]
        wait: bool,
        /// Seconds to wait with --wait (default 120).
        #[arg(long = "boot-timeout", default_value_t = 120)]
        boot_timeout: u64,
    },
    /// Pull a golden VM image from an OCI registry — with no argument, the
    /// OFFICIAL Delonix image (ready for `vm create`/`cluster kubeadm`).
    Pull {
        /// OCI reference (default: the official Delonix image).
        source: Option<String>,
        /// Local name (default: derived from the reference).
        #[arg(long)]
        name: Option<String>,
    },
    /// Push a local golden VM image to an OCI registry (`vm push <name> <target>`).
    Push {
        name: String,
        target: String,
    },
    /// List the VMs.
    Ls,
    /// Attach to the VM's serial console (interactive terminal) — works with no
    /// IP (boot logs, login). Escape: Ctrl-] .
    Console {
        #[arg(add = ArgValueCandidates::new(super::complete::vms))]
        name: String,
    },
    /// Print the VNC address of a graphical VM (created with `--vnc`, libvirt).
    Vnc {
        #[arg(add = ArgValueCandidates::new(super::complete::vms))]
        name: String,
    },
    /// Current state (reconciles liveness/IP with the backend).
    Status {
        /// VM to query (omit for the state of ALL).
        #[arg(add = ArgValueCandidates::new(super::complete::vms))]
        name: Option<String>,
    },
    Reach,
    /// EXPERIMENTAL (root): give a libvirt VM DIRECT IP reachability to a
    /// container SDN network (veth from the host into the holder netns + routes).
    /// Defaults to a DRY-RUN; add `--apply` (as root) to establish it.
    Bridge {
        #[arg(add = ArgValueCandidates::new(super::complete::networks))]
        network: String,
        /// VM subnet(s) to route back (default: auto-detected `virbr*`). Repeatable.
        #[arg(long = "vm-subnet")]
        vm_subnet: Vec<String>,
        /// Actually run the privileged plan (requires root). Without it: dry-run.
        #[arg(long)]
        apply: bool,
    },
    /// Tear down a `vm bridge` (dry-run without `--apply`).
    Unbridge {
        #[arg(add = ArgValueCandidates::new(super::complete::networks))]
        network: String,
        #[arg(long)]
        apply: bool,
    },
    /// Human-readable detail of one or more VMs, `kubectl describe` style (for
    /// humans; use `status` for the usual compact view). Includes the LIVE
    /// state — `delonix_vm::status` reconciles liveness/IP with the backend.
    Describe {
        #[arg(required = true, add = ArgValueCandidates::new(super::complete::vms))]
        names: Vec<String>,
    },
    /// Stop the VM (preserves disk/record).
    #[command(alias = "down")]
    Stop {
        #[arg(add = ArgValueCandidates::new(super::complete::vms))]
        name: String,
    },
    /// Remove the VM (stops + deletes overlay/state).
    #[command(alias = "delete")]
    Rm {
        #[arg(add = ArgValueCandidates::new(super::complete::vms))]
        name: String,
        /// Remove the local state even if the libvirt cleanup fails.
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Apply the `kind: Vm` documents of a manifest (`delonix_vm::create` is
    /// already idempotent by name — creates or auto-recovers).
    Apply {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
}

/// Base 9p tag from the volume name: `[a-zA-Z0-9_]`, ≤31 chars (9p limit).
/// Since `.` and `-` both collapse to `_`, two distinct names can generate the
/// same base — uniqueness is guaranteed by `resolve_vm_volumes` (per-index
/// suffix), not here.
fn vol_tag(name: &str) -> String {
    let mut t: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    t.truncate(31);
    t
}

/// A volume `mountPath` must be an absolute path WITHOUT characters that break
/// the cloud-init YAML flow sequence (`,`/`]`/`#`/`"`) nor control chars —
/// otherwise the `mounts` entry is malformed and the volume silently fails to
/// mount after boot.
fn valid_mount_path(p: &str) -> bool {
    p.starts_with('/')
        && !p
            .chars()
            .any(|c| c.is_control() || matches!(c, ',' | ']' | '[' | '#' | '"'))
}

/// Resolve `spec.volumes` (Volume/Storage names) into `VmVolume` with the host
/// directory, ensuring a network Storage is mounted before sharing it over 9p.
/// Unique tags (`_N` suffix on collision). The `Volume`/`Storage` must already
/// exist (`stack apply` applies them before the VM; `validate_graph` already
/// confirms the reference).
fn resolve_vm_volumes(
    base: &std::path::Path,
    specs: &[VmVolumeSpec],
) -> Result<Vec<delonix_vm::VmVolume>> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let store = VolumeStore::open(base)?;
    let mut used_tags: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(specs.len());
    for v in specs {
        if !valid_mount_path(&v.mount_path) {
            return Err(Error::Invalid(format!(
                "spec.volumes: mountPath {:?} inválido (tem de ser um caminho absoluto sem , ] [ # \" nem control chars)",
                v.mount_path
            )));
        }
        let vol = store.inspect(&v.name).map_err(|_| {
            Error::Invalid(format!(
                "spec.volumes: volume/storage '{}' não existe (cria-o antes da VM)",
                v.name
            ))
        })?;
        // If it's a network Storage, ensure it's mounted on the host before sharing.
        store.ensure_mounted(&vol)?;
        // Tag uniqueness: `.` and `-` collapse to `_`, so distinct names can
        // collide — disambiguate with a `_N` suffix stable by order.
        let base_tag = vol_tag(&v.name);
        let mut tag = base_tag.clone();
        let mut n = 1;
        while used_tags.contains(&tag) {
            let suffix = format!("_{n}");
            let keep = 31usize.saturating_sub(suffix.len());
            tag = format!("{}{suffix}", &base_tag[..base_tag.len().min(keep)]);
            n += 1;
        }
        used_tags.insert(tag.clone());
        out.push(delonix_vm::VmVolume {
            tag,
            source: vol.mountpoint.clone(),
            mount_path: v.mount_path.clone(),
            read_only: v.read_only,
        });
    }
    Ok(out)
}

/// Dry-run: the spec with every `#[serde(default)]` materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: VmSpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let base = state_root();
    for doc in manifest::of_kind(docs, "Vm") {
        let name = &doc.metadata.name;
        manifest::warn_unknown_fields(doc, VM_SPEC_FIELDS);
        let spec: VmSpec = manifest::spec_of(doc)?;

        // Resolve each volume (Volume/Storage name → host directory) and
        // ensure a network Storage is mounted before sharing it.
        let vm_volumes = resolve_vm_volumes(&base, &spec.volumes)?;

        // NB: the "volumes ⇒ libvirt" rule lives in the engine (`delonix_vm::create`),
        // so any API consumer inherits it — here the backend is passed as
        // declared (with explicit CH + volumes, the engine refuses with a clear error).

        // Same rule as the CLI `vm create`: unless an explicit `seed` is given,
        // ALWAYS generate a NoCloud seed. Without a datasource the cloud image's
        // cloud-init skips the network phase and the VM boots with no IP/route —
        // so the declarative path used to leave a volume-less `kind: Vm` offline.
        // The seed also carries hostname/sshKeys/userData (CLI parity) and the
        // 9p volume mounts.
        let seed = match spec.seed {
            Some(s) => Some(s),
            None => Some(
                generate_seed_iso(
                    name,
                    spec.hostname.as_deref(),
                    &spec.ssh_keys,
                    spec.user_data.as_deref().map(std::path::Path::new),
                    &vm_volumes,
                )?
                .to_string_lossy()
                .into_owned(),
            ),
        };

        let cfg = VmConfig {
            name: name.clone(),
            disk: spec.disk,
            vcpus: spec.vcpus,
            memory: spec.memory,
            network: spec.network,
            kernel: spec.kernel,
            initrd: spec.initrd,
            firmware: spec.firmware,
            cmdline: spec.cmdline,
            seed,
            restart_policy: spec.restart_policy,
            hugepages: spec.hugepages,
            cpu_affinity: spec.cpu_affinity,
            devices: spec.devices,
            backend: spec.backend,
            net_mode: spec.net_mode,
            bridge: spec.bridge,
            volumes: vm_volumes,
            vnc: spec.vnc,
            static_ip: spec.ip,
            machine: spec.machine,
            cpu_model: spec.cpu_model,
            cpu_topology: spec.cpu_topology.map(|t| delonix_vm::CpuTopology {
                sockets: t.sockets,
                cores: t.cores,
                threads: t.threads,
            }),
            tpm: spec.tpm,
            video: spec.video,
            boot_order: spec.boot_order,
            extra_disks: spec
                .extra_disks
                .into_iter()
                .map(|d| delonix_vm::ExtraDisk {
                    source: d.source,
                    device: d.device.unwrap_or_default(),
                    bus: d.bus.unwrap_or_default(),
                    format: d.format.unwrap_or_default(),
                    read_only: d.read_only,
                    target: d.target,
                })
                .collect(),
            extra_nics: spec
                .extra_nics
                .into_iter()
                .map(|n| delonix_vm::ExtraNic {
                    kind: n.kind,
                    source: n.source,
                    model: n.model.unwrap_or_default(),
                    mac: n.mac,
                })
                .collect(),
            libvirt_xml_overlay: spec.libvirt_xml_overlay,
            libvirt_xml: spec.libvirt_xml,
        };
        delonix_vm::create(&base, &cfg)?;
        println!("vm/{name}: garantida");
    }
    Ok(())
}

pub fn run(action: VmCmd) -> Result<()> {
    if let VmCmd::Init {
        dir,
        name,
        image,
        force,
        template,
        up,
    } = action
    {
        return cmd_init(
            super::scaffold::Target::Vm,
            dir,
            name,
            image,
            force,
            template,
            up,
        );
    }
    if let VmCmd::Dash { once } = action {
        return super::dash::run(super::dash::DashScope::Vms, once);
    }
    let base = state_root();
    match action {
        // Handled at the top of `run` (does `return`).
        VmCmd::Init { .. } => unreachable!("tratado acima"),
        VmCmd::Dash { .. } => unreachable!("tratado acima"),
        VmCmd::Create {
            name,
            disk,
            vcpus,
            memory,
            network,
            kernel,
            initrd,
            firmware,
            cmdline,
            seed,
            hostname,
            ssh_keys,
            user_data,
            restart_policy,
            hugepages,
            cpu_affinity,
            devices,
            backend,
            net_mode,
            bridge,
            ip,
            vnc,
            console,
            wait,
            boot_timeout,
        } => {
            // No --disk: the single golden VM image (same resolution as
            // `cluster kubeadm` — 0 or several images give a clear error, never
            // a blind choice).
            let disk = match disk {
                Some(d) => d,
                None => {
                    let store = super::vmimage::VmImageStore::open(super::util::state_root())?;
                    let tag = super::cluster::resolve_vm_image(&store, None)?;
                    store.qcow2_path(&tag).to_string_lossy().into_owned()
                }
            };
            // ALWAYS a cloud-init seed (unless an explicit `--seed`). Without a
            // datasource, the cloud image's cloud-init doesn't run the network
            // phase and the VM ends up with no IP nor route ("Network is
            // unreachable" in the guest, a real case). The minimal seed
            // (network-config DHCP + hostname = VM name) makes cloud-init bring
            // up the network and apply the ssh-keys/hostname when given.
            let seed = match seed {
                Some(s) => Some(s),
                None => {
                    let iso = generate_seed_iso(
                        &name,
                        hostname.as_deref(),
                        &ssh_keys,
                        user_data.as_deref(),
                        &[],
                    )?;
                    Some(iso.to_string_lossy().into_owned())
                }
            };
            let cfg = VmConfig {
                name,
                disk,
                vcpus,
                memory,
                network,
                kernel,
                initrd,
                firmware,
                cmdline,
                seed,
                restart_policy,
                hugepages,
                cpu_affinity,
                devices,
                backend,
                net_mode,
                bridge,
                volumes: vec![],
                vnc,
                static_ip: ip,
                // Advanced libvirt knobs are declarative-only (`kind: Vm`), not CLI flags.
                ..Default::default()
            };
            // Staged progress on STDERR (human), while STDOUT stays the bare VM
            // name (scriptable — unchanged contract). Replaces the raw
            // `qemu-img`/`virsh` chatter that leaked before (now captured in the
            // engine). Each `CreateStage` renders one step as it starts.
            eprintln!(
                "{}",
                super::po::tf("Creating VM '{name}'…", &[("name", &cfg.name)])
            );
            let render = |s: delonix_vm::CreateStage| {
                use delonix_vm::CreateStage::*;
                let step = match s {
                    Disk => super::po::t("preparing the overlay disk"),
                    Network => super::po::t("configuring the network"),
                    Define => super::po::t("defining the domain"),
                    Start => super::po::t("starting the VM"),
                };
                eprintln!("  → {step}");
            };
            let vm = delonix_vm::create_with(&base, &cfg, &render)?;
            eprintln!(
                "{}",
                super::po::tf("✓ VM '{name}' is up.", &[("name", &vm.name)])
            );
            println!("{}", vm.name);
            // Honest signal instead of a silent `IP <none>`: a libvirt VM that
            // fell back to user-mode (session SLIRP) never gets a reachable IP.
            if vm.backend.contains("libvirt") && vm.tap == "user" {
                output::warn(super::po::t(
                    "user-mode network: this VM will have no reachable IP — join the `libvirt` group (nat mode then becomes the default), or pass `--net-mode nat|bridge`",
                ));
            }
            // Dynamic boot: --console attaches to the serial console (watch the
            // boot live); --wait shows a spinner until the VM gets an IP.
            if console {
                return cmd_console(&base, &vm.name);
            }
            if wait {
                wait_for_boot(
                    &base,
                    &vm.name,
                    std::time::Duration::from_secs(boot_timeout),
                );
            }
            print_vm_next_steps(&vm.name);
            Ok(())
        }
        VmCmd::Pull { source, name } => {
            let store = super::vmimage::VmImageStore::open(super::util::state_root())?;
            let src = source.unwrap_or_else(|| super::vmimage::OFFICIAL_VM_IMAGE.to_string());
            super::vmimage::cmd_pull(&store, &src, name)
        }
        VmCmd::Push { name, target } => {
            let store = super::vmimage::VmImageStore::open(super::util::state_root())?;
            super::vmimage::cmd_push(&store, &name, &target)
        }
        VmCmd::Ls => {
            let mut t = output::Table::new(&["NAME", "VCPUS", "MEMORY", "STATUS", "IP"])
                // VCPUS is a count — right-aligned like the sizes.
                .right_align(1);
            for vm in delonix_vm::list(&base)? {
                t.row(vec![
                    vm.name,
                    vm.vcpus.to_string(),
                    vm.memory,
                    fmt_vm_status(&vm.status),
                    vm.ip.unwrap_or_else(|| "<none>".into()),
                ]);
            }
            t.print();
            Ok(())
        }
        VmCmd::Describe { names } => cmd_describe(&base, &names),
        VmCmd::Console { name } => cmd_console(&base, &name),
        VmCmd::Vnc { name } => cmd_vnc(&base, &name),
        VmCmd::Status { name } => {
            // No argument: the reconciled state of ALL (consistent with
            // `ingress ls`/`egress ls` with no argument).
            let names: Vec<String> = match name {
                Some(n) => vec![n],
                None => delonix_vm::list(&base)?
                    .into_iter()
                    .map(|v| v.name)
                    .collect(),
            };
            let mut t = output::Table::new(&["NAME", "STATUS", "BACKEND", "IP"]);
            for n in names {
                let vm = delonix_vm::status(&base, &n)?;
                t.row(vec![
                    vm.name,
                    format!("{:?}", vm.status),
                    vm.backend,
                    vm.ip.unwrap_or_default(),
                ]);
            }
            t.print();
            Ok(())
        }
        VmCmd::Reach => cmd_reach(&base),
        VmCmd::Bridge {
            network,
            vm_subnet,
            apply,
        } => super::vmbridge::bridge(&network, vm_subnet, apply),
        VmCmd::Unbridge { network, apply } => super::vmbridge::unbridge(&network, apply),
        VmCmd::Stop { name } => {
            delonix_vm::stop(&base, &name)?;
            println!("{name}");
            Ok(())
        }
        VmCmd::Rm { name, force } => {
            let res = if force {
                delonix_vm::remove_force(&base, &name)
            } else {
                delonix_vm::remove(&base, &name)
            };
            if let Err(e) = res {
                // Backend cleanup refused: the local record was kept intact on
                // purpose (no orphan VMs in libvirt) — tell the user how to
                // force it, instead of leaving them in a dead end.
                if !force && !matches!(e, Error::VmNotFound(_)) {
                    output::warn(&super::po::tf(
                        "the VM record was kept; `delonix vm rm --force {name}` discards it anyway",
                        &[("name", &name)],
                    ));
                }
                return Err(e);
            }
            println!("{name}");
            Ok(())
        }
        VmCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
    }
}

/// IPv4 gateways of the host's libvirt VM networks (the `virbr*` bridge
/// addresses) — what a `nat` VM uses to reach a host-published service.
/// Best-effort: no `ip` tool → empty, and `vm reach` still shows the port binds.
fn libvirt_gateways() -> Vec<String> {
    match Command::new("ip")
        .args(["-br", "-4", "addr", "show"])
        .output()
    {
        Ok(o) if o.status.success() => parse_ip_gateways(&String::from_utf8_lossy(&o.stdout)),
        _ => Vec::new(),
    }
}

/// Parses `ip -br -4 addr show` output → the IPv4 addresses of `virbr*`
/// bridges. Pure — tested without the `ip` tool.
fn parse_ip_gateways(out: &str) -> Vec<String> {
    let mut gws = Vec::new();
    for line in out.lines() {
        let mut it = line.split_whitespace();
        let iface = it.next().unwrap_or("");
        if !iface.starts_with("virbr") {
            continue;
        }
        if let Some(cidr) = it.find(|s| s.contains('.')) {
            if let Some((ip, _)) = cidr.split_once('/') {
                gws.push(ip.to_string());
            }
        }
    }
    gws
}

/// Map `host_port -> bind address` for every listening TCP socket (via `ss`).
/// The LIVE truth of where a published port is bound — the bind address is not
/// kept in the container record (it came from `DELONIX_PUBLISH_ADDR` at publish
/// time), so `vm reach` reads it from the actual listeners. Prefers a
/// non-loopback bind when a port has more than one.
fn listening_binds() -> std::collections::HashMap<String, String> {
    match Command::new("ss").args(["-tlnH"]).output() {
        Ok(o) if o.status.success() => parse_ss_binds(&String::from_utf8_lossy(&o.stdout)),
        _ => std::collections::HashMap::new(),
    }
}

/// Parses `ss -tlnH` output → `host_port -> bind address`. Prefers a
/// non-loopback bind when a port has more than one listener. Pure — tested
/// without `ss`.
fn parse_ss_binds(out: &str) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    for line in out.lines() {
        // columns: State Recv-Q Send-Q Local-Address:Port Peer ...
        let cols: Vec<&str> = line.split_whitespace().collect();
        let Some(local) = cols.get(3) else { continue };
        let Some(idx) = local.rfind(':') else {
            continue;
        };
        let (addr, port) = (local[..idx].to_string(), local[idx + 1..].to_string());
        m.entry(port)
            .and_modify(|cur: &mut String| {
                if cur == "127.0.0.1" && addr != "127.0.0.1" {
                    *cur = addr.clone();
                }
            })
            .or_insert(addr);
    }
    m
}

/// `delonix vm reach` — how VMs reach container services. A published port is
/// reachable from a libvirt VM only if bound to an address the VM routes to
/// (the VM network gateway, e.g. `192.168.122.1`), not the safe-by-default
/// loopback. Surfaces the gap AND the exact fix, instead of leaving the user
/// with a silent "connection refused" from inside the VM.
fn cmd_reach(_base: &std::path::Path) -> Result<()> {
    let gateways = libvirt_gateways();
    let binds = listening_binds();
    let gw = gateways
        .first()
        .cloned()
        .unwrap_or_else(|| "<vm-gateway>".into());

    let (_images, store) = super::util::open_stores()?;
    let mut reachable = output::Table::new(&["CONTAINER", "SERVICE", "ADDRESS (from a VM)"]);
    let mut hostonly = output::Table::new(&["CONTAINER", "HOST PORT", "BOUND TO"]);
    let (mut n_reach, mut n_host) = (0usize, 0usize);
    for c in store.list()? {
        for p in &c.ports {
            let hp = p.split(':').next().unwrap_or(p).to_string();
            match binds.get(&hp).map(String::as_str) {
                // loopback only → not reachable from VMs
                Some("127.0.0.1") | Some("127.0.0.0") => {
                    n_host += 1;
                    hostonly.row(vec![c.name.clone(), hp, "127.0.0.1 (host only)".into()]);
                }
                // bound to a routable address (gateway or 0.0.0.0) → reachable
                Some(addr) => {
                    n_reach += 1;
                    let shown = if addr == "0.0.0.0" || addr == "*" {
                        gateways
                            .first()
                            .cloned()
                            .unwrap_or_else(|| addr.to_string())
                    } else {
                        addr.to_string()
                    };
                    reachable.row(vec![c.name.clone(), p.clone(), format!("{shown}:{hp}")]);
                }
                // in the record but no live listener (container stopped) → skip
                None => {}
            }
        }
    }
    if !gateways.is_empty() {
        println!(
            "{}",
            super::po::tf(
                "VM network gateway(s): {gws}",
                &[("gws", &gateways.join(", "))]
            )
        );
    }
    if n_reach > 0 {
        println!();
        println!("{}", super::po::t("Reachable from VMs:"));
        reachable.print();
    }
    if n_host > 0 {
        println!();
        output::warn(super::po::t(
            "Published on loopback only — NOT reachable from VMs:",
        ));
        hostonly.print();
        println!(
            "{}",
            super::po::tf(
                "  fix: re-publish bound to the VM gateway — `delonix ingress unpublish <c> <port>`, then `DELONIX_PUBLISH_ADDR={gw} delonix ingress publish <c> <port>` (reachable from VMs on that network, not the external LAN)",
                &[("gw", &gw)],
            )
        );
    }
    if n_reach == 0 && n_host == 0 {
        println!(
            "{}",
            super::po::t("no running container publishes a port — nothing for a VM to reach yet")
        );
    }
    Ok(())
}

/// A VM's state as text, without the raw enum `{:?}`: `Failed(137)` from
/// `Debug` would become "Failed(137)" — readable, but `Exited (137)` is the
/// vocabulary the rest of the CLI already uses (`container ps`). Pure.
fn fmt_vm_status(status: &delonix_runtime_core::Status) -> String {
    use delonix_runtime_core::Status as S;
    match status {
        S::Created => "Created".to_string(),
        S::Running => "Running".to_string(),
        S::Paused => "Paused".to_string(),
        S::Stopped => "Stopped".to_string(),
        S::Failed(code) => format!("Exited ({code})"),
        S::Crashed => "Dead".to_string(),
    }
}

/// `vm describe` — human-readable detail, `kubectl describe` style.
///
/// Uses `delonix_vm::status` (not the raw record): reconciles liveness/IP with
/// the backend, so what you read is the LIVE state and not the last one that
/// got saved. It's the difference between "says it's Running" and "is Running".
/// Waits (with a spinner) for the VM to get an IP — the sign the network came
/// up and the boot advanced. Only makes sense in modes with a visible IP (CH,
/// or libvirt nat/bridge); in user-mode (libvirt session, SLIRP) there's never
/// an IP, so it warns and points to the console instead of waiting in vain.
fn wait_for_boot(base: &std::path::Path, name: &str, timeout: std::time::Duration) {
    let start = std::time::Instant::now();
    let deadline = start + timeout;
    let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let tty = super::output::color_enabled();
    let mut i = 0usize;
    loop {
        if let Ok(vm) = delonix_vm::status(base, name) {
            if let Some(ip) = vm.ip.clone().filter(|s| !s.is_empty()) {
                if tty {
                    eprint!("\r\x1b[K");
                }
                super::output::info(&super::po::tf(
                    "vm '{name}' is up — ip {ip}",
                    &[("name", name), ("ip", &ip)],
                ));
                return;
            }
            // libvirt user-mode never gives an IP: after a short start, steer
            // toward the console instead of waiting the whole timeout in vain.
            // `vm.tap` records the EFFECTIVE mode (the engine may default to
            // nat) — a nat/bridge VM legitimately takes tens of seconds to get
            // its DHCP lease, so only user-mode short-circuits here.
            if vm.backend.contains("libvirt")
                && vm.tap == "user"
                && vm.ip.is_none()
                && start.elapsed() >= std::time::Duration::from_secs(3)
            {
                if tty {
                    eprint!("\r\x1b[K");
                }
                super::output::info(&super::po::tf(
                    "vm '{name}' started (user-mode network, no reachable IP) — `delonix vm console {name}` to log in",
                    &[("name", name)],
                ));
                return;
            }
        }
        if std::time::Instant::now() >= deadline {
            if tty {
                eprint!("\r\x1b[K");
            }
            super::output::warn(&super::po::tf(
                "vm '{name}' still booting after the timeout — `delonix vm console {name}` to watch",
                &[("name", name)],
            ));
            return;
        }
        if tty {
            eprint!(
                "\r\x1b[K{} {}",
                frames[i % 10],
                super::po::tf("booting '{name}'...", &[("name", name)])
            );
            use std::io::Write;
            let _ = std::io::stderr().flush();
        }
        i += 1;
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
}

/// `delonix vm vnc <name>` — the VNC address of a graphical VM (created with
/// `--vnc`, libvirt backend). Cloud Hypervisor has no display — in that case
/// it points to `vm console` (serial). Opens no client; prints the address
/// for the user to connect with their own (`vncviewer`, Remmina, ...).
fn cmd_vnc(base: &std::path::Path, name: &str) -> Result<()> {
    let vm = delonix_vm::status(base, name)?;
    let backend = vm.backend.as_str();
    if !(backend.contains("libvirt") || backend.contains("qemu") || backend.contains("kvm")) {
        return Err(Error::Invalid(super::po::tf(
            "VM '{name}' uses Cloud Hypervisor, which has no VNC — use `delonix vm console {name}` (serial), or recreate with `--backend libvirt --vnc`",
            &[("name", name)],
        )));
    }
    // `virsh vncdisplay` returns `:N` (port = 5900 + N) or `127.0.0.1:N`.
    let uri = delonix_vm::libvirt_uri(name);
    let out = std::process::Command::new("virsh")
        .args(["-c", &uri, "vncdisplay", "--", name])
        .output()
        .map_err(|e| Error::Runtime {
            context: "virsh vncdisplay",
            message: e.to_string(),
        })?;
    let disp = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || disp.is_empty() {
        return Err(Error::Invalid(super::po::tf(
            "no VNC display for '{name}' — was it created with `--vnc`?",
            &[("name", name)],
        )));
    }
    // Normalize ":N" -> "127.0.0.1:590N" (N is the display index).
    let addr = if let Some(rest) = disp.strip_prefix(':') {
        match rest.parse::<u32>() {
            Ok(n) => format!("127.0.0.1:{}", 5900 + n),
            Err(_) => disp.clone(),
        }
    } else {
        disp.clone()
    };
    println!("{addr}");
    super::output::info(&super::po::tf(
        "connect with a VNC client, e.g. `vncviewer {addr}`",
        &[("addr", &addr)],
    ));
    Ok(())
}

/// `delonix vm console <name>` — the VM's interactive serial terminal. Needs no
/// IP (like a serial cable): to watch the boot and log in even without network.
/// Cloud Hypervisor: connects to the serial UNIX socket and bridges it with the
/// Prints the "what now?" block after a successful `vm create` — on STDERR so
/// STDOUT stays the bare VM name for scripts. The console hint spells out the
/// escape key because with serial autologin `exit`/`logout` just loop.
fn print_vm_next_steps(name: &str) {
    let rows = [
        (
            format!("delonix vm console {name}"),
            super::po::t("open the serial console (back to host: Ctrl+])"),
        ),
        (
            format!("delonix vm status {name}"),
            super::po::t("state, backend and IP"),
        ),
        (
            format!("delonix vm describe {name}"),
            super::po::t("full details"),
        ),
        (
            format!("delonix vm stop {name}"),
            super::po::t("stop it (keeps the disk)"),
        ),
    ];
    eprintln!("\n{}", super::po::t("Next steps:"));
    for (cmd, desc) in rows {
        eprintln!("  {cmd:<30} # {desc}");
    }
}

/// local tty (raw mode); libvirt: delegates to `virsh console` (which does it).
fn cmd_console(base: &std::path::Path, name: &str) -> Result<()> {
    let vm = delonix_vm::status(base, name)?;
    if !matches!(vm.status, delonix_runtime_core::Status::Running) {
        return Err(Error::Invalid(super::po::tf(
            "VM '{name}' is not running — start it first",
            &[("name", name)],
        )));
    }
    // The golden image auto-logs-in on ttyS0, so inside the console `exit`/`logout`
    // just re-trigger the getty and loop forever — the ONLY way back to the host
    // is the escape key. Spelling it out (in the user's language) fixes the
    // recurring "I can't get out of the VM console" report.
    eprintln!(
        "{}",
        super::po::tf(
            "Console of '{name}'. To return to the host: press Ctrl+]  (exit/logout only restarts the session — autologin re-enters).",
            &[("name", name)],
        )
    );
    let backend = vm.backend.as_str();
    if backend.contains("libvirt") || backend.contains("qemu") || backend.contains("kvm") {
        // Spawn `virsh console` as a CHILD (not exec/replace) so that when the
        // user presses Ctrl+] we regain control and can confirm the return —
        // virsh handles the raw tty and the escape key itself.
        let uri = delonix_vm::libvirt_uri(name);
        let status = std::process::Command::new("virsh")
            .args(["-c", &uri, "console", "--", name])
            .status()
            .map_err(|e| Error::Runtime {
                context: "virsh console",
                message: e.to_string(),
            })?;
        // virsh returns non-zero on some disconnects; that is not an error to
        // surface — the user asked to leave.
        let _ = status;
        eprintln!("{}", super::po::t("Back to the host shell."));
        return Ok(());
    }
    // Cloud Hypervisor: ponte tty<->socket.
    let sock = delonix_vm::console_socket(base, name);
    if !sock.exists() {
        // The VM is alive but was started by an old binary (serial to a file,
        // not a socket). An idempotent `create` won't restart it; you have to
        // stop it and let `create` restart it with the socket.
        return Err(Error::Invalid(super::po::tf(
            "no console socket for VM '{name}' — it was started by an older delonix; run `delonix vm stop {name} && delonix vm create {name}` to restart it with a console",
            &[("name", name)],
        )));
    }
    let r = console_bridge(&sock);
    eprintln!("{}", super::po::t("Back to the host shell."));
    r
}

/// Saves stdin's tty mode and restores it on `Drop` (even on Ctrl-C, panic,
/// or VM exit) — without this the terminal would stay in raw after exiting.
struct RawTty(libc::termios);
impl RawTty {
    fn enable() -> Option<Self> {
        // SAFETY: tcgetattr/tcsetattr on fd 0 (stdin); no preconditions.
        unsafe {
            if libc::isatty(0) != 1 {
                return None;
            }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return None;
            }
            let orig = t;
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(0, libc::TCSANOW, &t);
            Some(RawTty(orig))
        }
    }
}
impl Drop for RawTty {
    fn drop(&mut self) {
        // SAFETY: restores the saved original termios.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.0);
        }
    }
}

/// Connects stdin/stdout to the console socket, byte by byte, until `Ctrl-]`
/// (0x1d) on stdin — the same escape key as `telnet`.
fn console_bridge(sock: &std::path::Path) -> Result<()> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    let stream = UnixStream::connect(sock).map_err(|e| Error::Runtime {
        context: "vm console",
        message: e.to_string(),
    })?;
    use std::os::unix::io::AsRawFd;
    let _raw = RawTty::enable();
    eprintln!(
        "[connected — detach with Ctrl-]; the console returns here when the VM powers off]\r"
    );

    // Bidirectional bridge with `poll()` on a single thread: reacts to stdin AND
    // to the socket, and — the point of the fix — RETURNS to the host when the
    // socket closes (the VM powered off/shut down), without getting stuck in a
    // stdin `read`. Ctrl-] (0x1d) detaches; `exit`/Ctrl-D inside the VM go to the
    // getty (autologin), not here — the only manual exit is Ctrl-], so it's announced.
    let mut wr = stream.try_clone().map_err(|e| Error::Runtime {
        context: "vm console",
        message: e.to_string(),
    })?;
    let mut rd = stream;
    let (in_fd, sock_fd) = (std::io::stdin().as_raw_fd(), rd.as_raw_fd());
    let mut buf = [0u8; 4096];
    'bridge: loop {
        let mut fds = [
            libc::pollfd {
                fd: in_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: sock_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: poll over 2 valid pollfds; -1 = blocks until an event.
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
            break;
        }
        // stdin -> socket (Ctrl-] detaches; host EOF exits).
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            match std::io::stdin().read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if buf[..n].contains(&0x1d) {
                        break;
                    }
                    if wr.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        // socket -> stdout; EOF = the VM closed → returns to the host.
        if fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            match rd.read(&mut buf) {
                Ok(0) | Err(_) => break 'bridge,
                Ok(n) => {
                    let mut out = std::io::stdout();
                    if out.write_all(&buf[..n]).is_err() || out.flush().is_err() {
                        break;
                    }
                }
            }
        }
    }
    let _ = wr.shutdown(std::net::Shutdown::Both);
    eprintln!("\r\n[console closed]\r");
    Ok(())
}

fn cmd_describe(base: &std::path::Path, names: &[String]) -> Result<()> {
    for (i, name) in names.iter().enumerate() {
        let vm = delonix_vm::status(base, name)?;
        if i > 0 {
            println!();
        }
        describe_one(&vm);
    }
    Ok(())
}

/// Size of a file on disk, if readable. An overlay/disk that disappeared
/// (deleted by hand) gives `None` and the field omits the size — better than
/// printing `0 B`, which would read as "empty" instead of "doesn't exist".
fn file_size(path: &str) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

fn describe_one(vm: &delonix_runtime_core::Vm) {
    let mut d = output::Describe::new();
    d.field("Name", &vm.name);
    d.field("Status", fmt_vm_status(&vm.status));
    d.field("Backend", &vm.backend);
    d.field("Created", output::fmt_local(vm.created_unix));
    d.field("Age", output::fmt_age(vm.created_unix));
    d.field(
        "PID",
        vm.pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "<none>".into()),
    );
    d.field(
        "Restart policy",
        vm.restart_policy.as_deref().unwrap_or("no"),
    );

    d.section("Resources");
    d.sub("vCPUs", vm.vcpus.to_string());
    d.sub("Memory", &vm.memory);

    d.section("Disk");
    d.sub("Base", &vm.disk);
    d.sub("Overlay", &vm.overlay);
    // REAL on-disk size of the overlay (what the VM wrote on top of the base).
    d.sub_opt("Overlay size", file_size(&vm.overlay).map(output::fmt_size));

    d.section("Network");
    d.sub("Network", &vm.network);
    d.sub("IP", vm.ip.as_deref().unwrap_or("<none>"));
    d.sub("TAP", if vm.tap.is_empty() { "<none>" } else { &vm.tap });
    d.sub("MAC", &vm.mac);

    d.field("API socket", &vm.api_socket);
    d.print();
}

// ---------------------------------------------------------------------------
// Per-instance NoCloud cloud-init ISO generation (not to be confused with the
// golden image build, in `cmd::vmimage` — this runs once per VM, at startup;
// that one runs once per image, at build time).
// ---------------------------------------------------------------------------

/// Resolve a `--ssh-key` entry: literal, or `@path` to read from a file.
fn resolve_ssh_key(spec: &str) -> Result<String> {
    match spec.strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path)
            .map(|s| s.trim().to_string())
            .map_err(|e| {
                Error::Invalid(format!(
                    "{} '{path}': {e}",
                    super::po::t("could not read the SSH key from")
                ))
            }),
        None => Ok(spec.trim().to_string()),
    }
}

/// Minimal NoCloud `user-data` — pure, testable without a real `cloud-localds`.
/// `package_update: false`/`package_upgrade: false` because the golden image
/// already comes ready (see `cmd::vmimage`); no point spending the first boot
/// on `apt update`.
fn build_user_data(
    hostname: &str,
    ssh_keys: &[String],
    volumes: &[delonix_vm::VmVolume],
) -> String {
    let mut out = String::from("#cloud-config\n");
    out.push_str(&format!("hostname: {hostname}\n"));
    out.push_str("package_update: false\n");
    out.push_str("package_upgrade: false\n");
    if !ssh_keys.is_empty() {
        out.push_str("ssh_authorized_keys:\n");
        for k in ssh_keys {
            out.push_str(&format!("  - {k}\n"));
        }
    }
    // Auto-login on the serial console (ttyS0) as the golden's `delonix` user:
    // `vm console` enters directly, without asking for a password (user's choice
    // — a dev VM's serial console is local access, like in multipass/kind).
    // Without this, cloud-init reconfigures the getty and the console asks for login.
    out.push_str("write_files:\n");
    out.push_str("  - path: /etc/systemd/system/serial-getty@ttyS0.service.d/autologin.conf\n");
    out.push_str("    content: |\n");
    out.push_str("      [Service]\n");
    out.push_str("      ExecStart=\n");
    out.push_str(
        "      ExecStart=-/sbin/agetty --autologin delonix --keep-baud 115200,57600,38400,9600 - $TERM\n",
    );
    out.push_str("runcmd:\n");
    out.push_str("  - [ systemctl, daemon-reload ]\n");
    out.push_str("  - [ systemctl, restart, serial-getty@ttyS0 ]\n");
    // Mount each 9p volume shared by the domain's `<filesystem>`. The `_netdev`
    // avoids blocking the boot if the share isn't ready; `trans=virtio`
    // + `9p2000.L` is the dialect that libvirt/QEMU expose. This way the guest
    // mounts the NAS/volume WITHOUT the user writing fstab or cloud-init by hand.
    if !volumes.is_empty() {
        out.push_str("mounts:\n");
        for v in volumes {
            let mode = if v.read_only { "ro" } else { "rw" };
            // `mount_path` quoted (validated without `"` in `valid_mount_path`) and
            // `tag` sanitized (`vol_tag`) — the YAML flow sequence doesn't break.
            out.push_str(&format!(
                "  - [ \"{}\", \"{}\", 9p, \"trans=virtio,version=9p2000.L,{mode},_netdev\", \"0\", \"0\" ]\n",
                v.tag, v.mount_path
            ));
        }
    }
    out
}

fn build_meta_data(instance_id: &str, hostname: &str) -> String {
    format!("instance-id: {instance_id}\nlocal-hostname: {hostname}\n")
}

/// Generates (or reuses, via `user_data_override`) the `user-data`/`meta-data`
/// and packages them into a NoCloud ISO with `cloud-localds`. Returns the ISO
/// path. `pub(crate)`: reused by `cmd::cluster::provision_and_apply` (each VM
/// provisioned by `delonix cluster kubeadm` needs the same seed).
pub(crate) fn generate_seed_iso(
    vm_name: &str,
    hostname: Option<&str>,
    ssh_keys: &[String],
    user_data_override: Option<&std::path::Path>,
    volumes: &[delonix_vm::VmVolume],
) -> Result<PathBuf> {
    // SECURITY: this runs BEFORE `delonix_vm::create()` — which is where
    // `valid_vm_name` is enforced — so a `../../../home/<u>/.ssh` name reached
    // `create_dir_all`/`fs::write` here (seed.iso with fully attacker-controlled
    // content via `--user-data`) before ever hitting that check. Enforce it here
    // too: this function is a path-writing boundary of its own, not just an API
    // consumer of `create()`.
    if !delonix_vm::valid_vm_name(vm_name) {
        return Err(Error::Invalid(format!("invalid VM name: {vm_name:?}")));
    }
    let hostname = hostname.unwrap_or(vm_name).to_string();
    let work_dir = state_root().join("vms").join(vm_name);
    std::fs::create_dir_all(&work_dir)?;

    let user_data_path = work_dir.join("user-data");
    match user_data_override {
        Some(p) => {
            std::fs::copy(p, &user_data_path).map_err(|e| {
                Error::Invalid(format!(
                    "não consegui copiar --user-data '{}': {e}",
                    p.display()
                ))
            })?;
            // The user's own user-data replaces EVERYTHING — there's nowhere to
            // inject the volume mounts without merging them. Warn instead of
            // losing them silently (the `<filesystem>` stays in the XML, but the
            // guest won't mount them by itself without a `mounts:` entry).
            if !volumes.is_empty() {
                eprintln!(
                    "AVISO: VM '{vm_name}': --user-data/seed próprio não inclui os mounts dos volumes 9p — acrescenta-os manualmente (tags: {})",
                    volumes.iter().map(|v| v.tag.as_str()).collect::<Vec<_>>().join(", ")
                );
            }
        }
        None => {
            let resolved_keys: Result<Vec<String>> =
                ssh_keys.iter().map(|s| resolve_ssh_key(s)).collect();
            let content = build_user_data(&hostname, &resolved_keys?, volumes);
            std::fs::write(&user_data_path, content)?;
        }
    }
    let meta_data_path = work_dir.join("meta-data");
    std::fs::write(&meta_data_path, build_meta_data(vm_name, &hostname))?;

    // network-config (NoCloud v2): DHCP on any ethernet interface — without this
    // the cloud image may not configure the network and the VM ends up with no
    // IP. `match name: "e*"` covers eth0/ens2/enp0s2/... (predictable or not).
    let net_cfg_path = work_dir.join("network-config");
    std::fs::write(
        &net_cfg_path,
        "version: 2\nethernets:\n  eth-all:\n    match:\n      name: \"e*\"\n    dhcp4: true\n",
    )?;

    let iso_path = work_dir.join("seed.iso");
    let status = Command::new("cloud-localds")
        .arg(format!("--network-config={}", net_cfg_path.display()))
        .arg(&iso_path)
        .arg(&user_data_path)
        .arg(&meta_data_path)
        .status()
        .map_err(|e| Error::Invalid(format!("a correr cloud-localds: {e}")))?;
    if !status.success() {
        return Err(Error::Invalid(format!(
            "cloud-localds falhou (exit {:?})",
            status.code()
        )));
    }
    Ok(iso_path)
}

/// Handles the `init` of this group (see `cmd::scaffold`).
fn cmd_init(
    target: super::scaffold::Target,
    dir: PathBuf,
    name: Option<String>,
    image: Option<String>,
    force: bool,
    template: Option<String>,
    up: bool,
) -> Result<()> {
    let name = name.unwrap_or_else(|| {
        // Without `--name`, use the DIRECTORY name. Can't use `canonicalize`: the
        // directory doesn't exist yet (it's `init` that creates it) and would
        // always fail, falling into the fallback — every project got named "app".
        // `.`/empty resolve to the cwd; a new path uses its basename.
        let p = if dir.as_os_str().is_empty() || dir == std::path::Path::new(".") {
            std::env::current_dir().ok()
        } else {
            Some(dir.clone())
        };
        p.as_deref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "app".to_string())
    });
    super::scaffold::init(
        target,
        &super::scaffold::InitOpts {
            dir,
            name,
            image,
            force,
            template,
            up,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{
        build_meta_data, build_user_data, fmt_vm_status, parse_ip_gateways, parse_ss_binds, VmSpec,
    };
    use delonix_runtime_core::Status;

    #[test]
    fn parse_ip_gateways_pega_so_as_virbr() {
        let out = "\
lo               UNKNOWN        127.0.0.1/8
virbr0           UP             192.168.122.1/24
br0              DOWN           10.0.0.1/24
virbr1           UP             10.10.100.1/24
delonix0         UNKNOWN        10.200.0.1/16";
        // Só as bridges libvirt (virbr*) são gateways de VM — nem o delonix0
        // (SDN, no netns do holder) nem br0 (bridge de host qualquer) entram.
        assert_eq!(parse_ip_gateways(out), vec!["192.168.122.1", "10.10.100.1"]);
    }

    #[test]
    fn parse_ss_binds_classifica_loopback_vs_gateway() {
        let out = "\
LISTEN 0      1          127.0.0.1:8069  0.0.0.0:*
LISTEN 0      1      192.168.122.1:18077 0.0.0.0:*
LISTEN 0      128          0.0.0.0:22    0.0.0.0:*
LISTEN 0      128             [::]:443   [::]:*";
        let m = parse_ss_binds(out);
        assert_eq!(m.get("8069").map(String::as_str), Some("127.0.0.1")); // loopback → host-only
        assert_eq!(m.get("18077").map(String::as_str), Some("192.168.122.1")); // gateway → VM-reachable
        assert_eq!(m.get("22").map(String::as_str), Some("0.0.0.0")); // all ifaces
        assert_eq!(m.get("443").map(String::as_str), Some("[::]")); // IPv6, parse não estoura
    }

    #[test]
    fn parse_ss_binds_prefere_nao_loopback_quando_a_porta_tem_dois() {
        // Uma porta com listener em loopback E no gateway conta como alcançável.
        let out = "\
LISTEN 0 1 127.0.0.1:9000 0.0.0.0:*
LISTEN 0 1 192.168.122.1:9000 0.0.0.0:*";
        assert_eq!(
            parse_ss_binds(out).get("9000").map(String::as_str),
            Some("192.168.122.1")
        );
    }

    #[test]
    fn vmspec_aceita_snake_case_legado_e_camel_case_canonico() {
        // Legacy (snake_case) — must not break.
        let legado: VmSpec = serde_yaml::from_str(
            "disk: d\nrestart_policy: always\ncpu_affinity: 0-3\nnet_mode: nat\n",
        )
        .unwrap();
        assert_eq!(legado.restart_policy.as_deref(), Some("always"));
        assert_eq!(legado.cpu_affinity.as_deref(), Some("0-3"));
        assert_eq!(legado.net_mode.as_deref(), Some("nat"));
        // Canonical (camelCase) — the new form in the examples.
        let canon: VmSpec = serde_yaml::from_str(
            "disk: d\nrestartPolicy: always\ncpuAffinity: 0-3\nnetMode: nat\n",
        )
        .unwrap();
        assert_eq!(canon.restart_policy.as_deref(), Some("always"));
        assert_eq!(canon.cpu_affinity.as_deref(), Some("0-3"));
        assert_eq!(canon.net_mode.as_deref(), Some("nat"));
    }

    #[test]
    fn status_de_vm_usa_o_vocabulario_da_cli() {
        assert_eq!(fmt_vm_status(&Status::Running), "Running");
        assert_eq!(fmt_vm_status(&Status::Stopped), "Stopped");
        // `{:?}` would give "Failed(137)"; the rest of the CLI says "Exited (137)".
        assert_eq!(fmt_vm_status(&Status::Failed(137)), "Exited (137)");
        assert_eq!(fmt_vm_status(&Status::Crashed), "Dead");
    }

    #[test]
    fn user_data_inclui_hostname_e_chaves() {
        let ud = build_user_data("myvm", &["ssh-ed25519 AAAA foo".to_string()], &[]);
        assert!(ud.starts_with("#cloud-config\n"));
        assert!(ud.contains("hostname: myvm\n"));
        assert!(ud.contains("ssh_authorized_keys:\n  - ssh-ed25519 AAAA foo\n"));
        assert!(ud.contains("package_update: false\n"));
    }

    #[test]
    fn user_data_sem_chaves_nao_tem_seccao_ssh() {
        let ud = build_user_data("myvm", &[], &[]);
        assert!(!ud.contains("ssh_authorized_keys"));
    }

    #[test]
    fn user_data_configura_autologin_serial() {
        // The serial console enters directly as `delonix` (`vm console` without
        // asking for a password) — cloud-init would reconfigure the getty otherwise.
        let ud = build_user_data("myvm", &[], &[]);
        assert!(ud.contains("serial-getty@ttyS0.service.d/autologin.conf"));
        assert!(ud.contains("--autologin delonix"));
        assert!(ud.contains("restart, serial-getty@ttyS0"));
    }

    #[test]
    fn user_data_com_volumes_injecta_mounts_9p() {
        let vols = vec![
            delonix_vm::VmVolume {
                tag: "dados".into(),
                source: "/srv/dados".into(),
                mount_path: "/mnt/dados".into(),
                read_only: false,
            },
            delonix_vm::VmVolume {
                tag: "ro".into(),
                source: "/srv/ro".into(),
                mount_path: "/mnt/ro".into(),
                read_only: true,
            },
        ];
        let ud = build_user_data("myvm", &[], &vols);
        assert!(ud.contains("mounts:\n"));
        assert!(ud.contains("[ \"dados\", \"/mnt/dados\", 9p, \"trans=virtio,version=9p2000.L,rw,_netdev\", \"0\", \"0\" ]"), "{ud}");
        assert!(ud.contains("[ \"ro\", \"/mnt/ro\", 9p, \"trans=virtio,version=9p2000.L,ro,_netdev\", \"0\", \"0\" ]"), "{ud}");
        // No volumes → no mounts section.
        assert!(!build_user_data("myvm", &[], &[]).contains("mounts:"));
    }

    #[test]
    fn vol_tag_saneia_e_trunca() {
        assert_eq!(super::vol_tag("nas-creds.db"), "nas_creds_db");
        assert_eq!(super::vol_tag(&"x".repeat(40)).len(), 31);
        // `.` and `-` both collapse to `_` → same base (uniqueness is in resolve).
        assert_eq!(super::vol_tag("nas.creds"), super::vol_tag("nas-creds"));
    }

    #[test]
    fn valid_mount_path_rejeita_relativos_e_chars_que_partem_o_yaml() {
        assert!(super::valid_mount_path("/mnt/dados"));
        assert!(super::valid_mount_path("/mnt/com espaco")); // space is ok (goes between quotes)
        assert!(!super::valid_mount_path("relativo/x")); // not absolute
        for bad in ["/mnt/a,b", "/mnt/a]b", "/mnt/a\"b", "/mnt/a#b", "/mnt/a\nb"] {
            assert!(!super::valid_mount_path(bad), "{bad:?} devia ser rejeitado");
        }
    }

    #[test]
    fn meta_data_tem_instance_id_e_hostname() {
        let md = build_meta_data("vm-1", "myvm");
        assert_eq!(md, "instance-id: vm-1\nlocal-hostname: myvm\n");
    }
}
