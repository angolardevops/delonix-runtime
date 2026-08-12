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
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub(crate) struct VmSpec {
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
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct CpuTopologySpec {
    #[serde(default)]
    sockets: u32,
    #[serde(default)]
    cores: u32,
    #[serde(default)]
    threads: u32,
}

/// One entry of `spec.extraDisks`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
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
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
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
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
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
    // Grouped-form-only keys (see `normalize_vm_spec`) — `network` needs no
    // entry of its own: it's ALREADY above, reused for both shapes (a plain
    // string in the old flat form, a mapping in the new grouped one).
    "resources",
    "boot",
    "cloudInit",
    "libvirt",
];

/// Re-deserializes a `kind: Vm` document's spec, accepting BOTH the historic
/// flat shape (every field at the top level — still fully supported, never
/// breaks an existing manifest) and a newer GROUPED one (`resources:`/
/// `network:`/`boot:`/`cloudInit:`/`libvirt:`) that reads better for a spec
/// this size. The grouped form is hoisted to the flat shape on the raw YAML
/// `Value` — see `normalize_vm_spec` — BEFORE the strongly-typed `VmSpec`
/// (unchanged) ever sees it, so every existing field/alias/default keeps
/// working exactly as before for both shapes.
fn vm_spec_of(doc: &ManifestDoc) -> Result<VmSpec> {
    let normalized = normalize_vm_spec(doc.spec.clone());
    serde_yaml::from_value(normalized).map_err(|e| {
        Error::Invalid(format!(
            "{}: {e}",
            super::po::tf(
                "{kind} '{name}': invalid spec",
                &[("kind", &doc.kind), ("name", &doc.metadata.name)],
            )
        ))
    })
}

/// The grouped form's tables: group name -> (sub-key, flat field it becomes).
/// ONE source for both the hoist and the unknown-sub-key check below —
/// two copies would drift, and the drift would be silent in exactly the way
/// this check exists to stop.
const VM_GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "resources",
        &[
            ("vcpus", "vcpus"),
            ("memory", "memory"),
            ("hugepages", "hugepages"),
            ("cpuAffinity", "cpuAffinity"),
        ],
    ),
    (
        "boot",
        &[
            ("kernel", "kernel"),
            ("initrd", "initrd"),
            ("firmware", "firmware"),
            ("cmdline", "cmdline"),
        ],
    ),
    (
        "cloudInit",
        &[
            ("seed", "seed"),
            ("hostname", "hostname"),
            ("sshKeys", "sshKeys"),
            ("userData", "userData"),
        ],
    ),
    (
        "libvirt",
        &[
            ("backend", "backend"),
            ("machine", "machine"),
            ("cpuModel", "cpuModel"),
            ("cpuTopology", "cpuTopology"),
            ("tpm", "tpm"),
            ("video", "video"),
            ("bootOrder", "bootOrder"),
            ("extraDisks", "extraDisks"),
            ("extraNics", "extraNics"),
            ("xmlOverlay", "libvirtXmlOverlay"),
            ("xml", "libvirtXml"),
        ],
    ),
];

/// Sub-keys accepted inside the grouped `network:` mapping.
const VM_NETWORK_KEYS: &[&str] = &["name", "mode", "bridge", "staticIp"];

/// Sub-keys inside a grouped spec that the hoist does not know — and therefore
/// throws away.
///
/// Pure, and reading the spec BEFORE `normalize_vm_spec` touches it: after the
/// hoist the group is gone (`m.remove`), which is why the top-level
/// unknown-field guard never saw these. Returns dotted paths
/// (`resources.memoria`) so the message points at the exact line to fix.
///
/// `network` is only a group when it is a MAPPING — in the flat form it is a
/// plain string (the network's name) and has no sub-keys to be wrong about.
pub(crate) fn unknown_group_keys(spec: &serde_yaml::Value) -> Vec<String> {
    use serde_yaml::Value;
    let Value::Mapping(m) = spec else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut scan = |group: &str, known: &dyn Fn(&str) -> bool| {
        if let Some(Value::Mapping(g)) = m.get(group) {
            for k in g.keys().filter_map(|k| k.as_str()) {
                if !known(k) {
                    out.push(format!("{group}.{k}"));
                }
            }
        }
    };
    scan("network", &|k| VM_NETWORK_KEYS.contains(&k));
    for (group, pairs) in VM_GROUPS {
        scan(group, &|k| pairs.iter().any(|(from, _)| *from == k));
    }
    out
}

/// Hoists each recognized group's sub-fields to their flat top-level name.
/// Pure (no I/O) and independent of serde/`VmSpec` — testable against raw
/// YAML shapes directly. An explicit flat key always wins over a grouped one
/// of the same target name (defensive; the two shapes are not meant to be
/// mixed for the same field, but this makes the precedence unambiguous
/// rather than "whichever the map iterates last").
///
/// `network` is the one special case: the OLD flat form is a plain SCALAR
/// (`network: node1-net`), the NEW grouped form is a MAPPING (`network:
/// {name: ..., mode: ...}`) — same key, disambiguated by the YAML node's own
/// type, because `network` already existed as a flat field and reusing the
/// name (rather than inventing e.g. `net:`) is what reads naturally.
fn normalize_vm_spec(mut v: serde_yaml::Value) -> serde_yaml::Value {
    use serde_yaml::Value;
    let Value::Mapping(m) = &mut v else {
        return v;
    };

    if let Some(Value::Mapping(net)) = m.get("network").cloned() {
        m.remove("network");
        if let Some(name) = net.get("name") {
            m.insert(Value::from("network"), name.clone());
        }
        hoist(m, &net, "mode", "netMode");
        hoist(m, &net, "bridge", "bridge");
        hoist(m, &net, "staticIp", "ip");
    }
    for (group, pairs) in VM_GROUPS {
        if let Some(Value::Mapping(g)) = m.get(*group).cloned() {
            for (from, to) in pairs.iter() {
                hoist(m, &g, from, to);
            }
            m.remove(*group);
        }
    }
    v
}

fn hoist(m: &mut serde_yaml::Mapping, group: &serde_yaml::Mapping, from: &str, to: &str) {
    if m.contains_key(to) {
        return;
    }
    if let Some(val) = group.get(from) {
        m.insert(serde_yaml::Value::from(to), val.clone());
    }
}

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
        #[arg(long)]
        json: bool,
    },
    /// Bootstrap a project with a VM manifest.
    ///
    /// Files ALREADY FILLED IN (images included), ready to use without editing
    /// anything.
    Init {
        /// Project directory (default: the current one).
        #[arg(value_hint = clap::ValueHint::DirPath, default_value = ".")]
        dir: PathBuf,
        /// Project name (default: the directory name).
        #[arg(long)]
        name: Option<String>,
        /// Image to use. Omit = fills in with the default image.
        #[arg(long, add = ArgValueCandidates::new(super::complete::images))]
        image: Option<String>,
        /// Overwrite existing files.
        #[arg(long)]
        force: bool,
        /// Scaffold a `VMfile` for BUILDING your own qcow2 image, instead of a
        /// manifest for RUNNING an existing one. The two are different jobs and
        /// this is the same verb for both: `init` starts a project either way.
        #[arg(long)]
        vmfile: bool,
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
        /// Absolute URL of a qcow2 cloud image to boot this VM from.
        ///
        /// Downloaded once and cached, so a second `create` from the same URL
        /// costs nothing. Verified against a sibling `<url>.sha256` when the
        /// publisher offers one; without it, the download is trusted on TLS
        /// alone and SAYS SO — someone pointing this at their own bucket
        /// deserves to know which of the two they got.
        #[arg(long = "url-img", conflicts_with_all = ["disk"])]
        url_img: Option<String>,
        /// Base disk (qcow2/raw) — becomes a per-VM overlay. Omit to use the
        /// local golden VM image (if there is exactly one; `image --vm ls`).
        #[arg(long, add = ArgValueCandidates::new(super::complete::vm_images))]
        disk: Option<String>,
        /// vCPUs (default: 1, or the image's `VCPUS` — see `HYPERVISOR`/`VCPUS`
        /// in `delonix vm init --vmfile` — when `--disk` names a local image).
        #[arg(long)]
        vcpus: Option<u32>,
        /// Memory (`"2G"`/`"1024M"`; default: `1G`, or the image's `MEMORY`
        /// when `--disk` names a local image).
        #[arg(long)]
        memory: Option<String>,
        /// Ingress network for the tap (default: the system ingress network; a
        /// custom network must be created first with `delonix network create`).
        #[arg(long, default_value = "ingress", add = ArgValueCandidates::new(super::complete::networks))]
        network: String,
        /// Isolation namespace (default `default`): VMs of different namespaces do not reach each other. Requires `--backend cloud-hypervisor`
        //
        // Deliberately ONE line, like every other flag in this group: clap's
        // derive turns a multi-paragraph doc comment into a `long_help`, and the
        // help translation looks the rendered string up verbatim — so a second
        // paragraph would silently come out untranslated under `--l18n=pt`.
        // The nuance (why libvirt is refused) is in the error the user actually
        // hits, in `vm_namespace_supported`, and in the release notes.
        #[arg(long, add = ArgValueCandidates::new(super::complete::namespaces))]
        namespace: Option<String>,
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
        #[arg(value_hint = clap::ValueHint::FilePath, long)]
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
        /// `cloud-hypervisor`|`libvirt` (omit: the image's `HYPERVISOR` if
        /// `--disk` names a local one, else `DELONIX_VM_BACKEND`/`vm
        /// default-backend`/auto-detection, in that order).
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
    /// Build a qcow2 VM image from a `VMfile`.
    Build {
        #[arg(short = 't', long = "tag")]
        tag: String,
        /// The `VMfile` (default: `<context>/VMfile`).
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
        /// Build context — the directory `COPY` reads from.
        #[arg(value_hint = clap::ValueHint::DirPath, default_value = ".")]
        context: PathBuf,
        /// Do not compress the final qcow2.
        #[arg(long)]
        no_compress: bool,
        /// Give the guest network access during `RUN` (for `apt-get install`
        /// and friends). Off by default: a build that reaches the internet
        /// produces a different image depending on when it ran.
        #[arg(long)]
        network: bool,
        /// Show each step's own output instead of folding it behind the step line (a failed step unfolds either way).
        #[arg(short = 'v', long)]
        verbose: bool,
    },
    /// Pull a golden VM image from an OCI registry.
    ///
    /// With no argument, the OFFICIAL Delonix image (ready for
    /// `vm create`/`cluster kubeadm`).
    Pull {
        /// OCI reference (default: the official Delonix image).
        source: Option<String>,
        /// Local name (default: derived from the reference).
        #[arg(long)]
        name: Option<String>,
        /// With no `source`, pull the official NO-Kubernetes golden (just
        /// the `delonix` engine, rootless-ready) instead of the Kubernetes
        /// one.
        #[arg(long)]
        no_k8s: bool,
    },
    /// List the tags available in a remote OCI repository.
    ///
    /// With no argument, the OFFICIAL Delonix golden image repo (discover
    /// which k8s versions are published before `pull`).
    LsRemote {
        source: Option<String>,
        /// With no `source`, list the official NO-Kubernetes golden's repo
        /// instead of the Kubernetes one.
        #[arg(long)]
        no_k8s: bool,
    },
    /// Push a local golden VM image to an OCI registry (`vm push <name> <target>`).
    Push {
        #[arg(add = ArgValueCandidates::new(super::complete::vm_images))]
        name: String,
        /// Destination. Omit it to publish to the OFFICIAL repository this
        /// image belongs in (decided from the image's own metadata).
        target: Option<String>,
    },
    /// Convert a VM disk to the format another ecosystem imports.
    ///
    /// `qcow2`, `raw`, `vmdk` (VMware), `vdi` (VirtualBox), `vhdx`/`vhd`
    /// (Hyper-V, Azure). Flattened either way, so the result is a standalone
    /// file with no backing chain. This engine's own two backends already
    /// share `qcow2`/`raw`; the rest exist so an image built here is
    /// importable elsewhere without a backend per product.
    Convert {
        /// A local VM image name (`vm ls`) or a literal `.qcow2`/`.raw` path.
        #[arg(add = ArgValueCandidates::new(super::complete::vm_images))]
        source: String,
        #[arg(long = "to", value_enum)]
        to: super::vmimage::ConvertFormat,
        /// Destination file (default: alongside the source, with the new extension).
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'o', long = "output")]
        output: Option<PathBuf>,
        /// Compress the output. Only `qcow2` and `vmdk` can — refused for the
        /// others rather than handed to `qemu-img` to fail on.
        #[arg(long)]
        compress: bool,
    },
    /// Get or set the default VM backend.
    ///
    /// Used by `vm create` when neither `--backend` nor `DELONIX_VM_BACKEND`
    /// is given — above the engine's own auto-detection heuristic. With no
    /// flag, prints the current default (`none` if auto-detection decides).
    DefaultBackend {
        /// Set the persisted default (`cloud-hypervisor` or `libvirt`).
        #[arg(long)]
        set: Option<String>,
        /// Clear the persisted default (fall back to auto-detection).
        #[arg(long)]
        clear: bool,
    },
    /// List the VMs.
    Ls {
        /// Also probe a short list of well-known ports (22, 6443, 10250, 80,
        /// 443) on each VM's IP and show which respond — a real TCP connect
        /// per port, short timeout, run concurrently. Off by default: unlike
        /// the rest of `ls` (local state only), this does live network I/O
        /// and can add latency, especially for an unreachable/booting VM.
        #[arg(long)]
        ports: bool,
        /// Output format: `table` (default) or `json` (ADR-0005). The
        /// `ports_open` field is included only with `--ports` (same as the table
        /// column — the probe does live network I/O, off by default).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: super::output::OutputFormat,
    },
    /// Attach to the VM's serial console (interactive terminal).
    ///
    /// Works with no IP (boot logs, login). Escape: Ctrl-] .
    Console {
        #[arg(add = ArgValueCandidates::new(super::complete::vms))]
        name: String,
        /// Key that detaches the console, as `^X` (default `^]`). Also settable via `$DELONIX_CONSOLE_ESCAPE`.
        #[arg(short = 'e', long = "escape")]
        escape: Option<String>,
    },
    /// SSH into a VM by NAME, or straight to an address.
    ///
    /// The name is enough — its IP comes from the record. With a trailing
    /// command, runs it and returns instead of opening a shell.
    ///
    /// `delonix vm ssh dev` · `delonix vm ssh dev -- systemctl status` ·
    /// `delonix vm ssh 192.168.122.50 -l root`
    Ssh {
        /// VM name (`vm ls`) or an IP/hostname to go to directly.
        #[arg(add = ArgValueCandidates::new(super::complete::vms))]
        target: String,
        /// Login user. Default: `delonix` on cloud-init images, `root` on appliances (which have no `delonix` account).
        #[arg(short = 'l', long = "user")]
        user: Option<String>,
        /// Private key to authenticate with (`ssh -i`).
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'i', long = "identity")]
        identity: Option<PathBuf>,
        /// Command to run instead of an interactive shell.
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
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
    /// Which published ports a VM can actually reach, and how to fix the rest.
    ///
    /// A port published to the default `127.0.0.1` is invisible to a VM —
    /// this lists the libvirt gateways, reads each port's LIVE bind, and for
    /// every loopback-only one prints the exact republish command.
    Reach,
    /// EXPERIMENTAL (root): give a libvirt VM DIRECT IP reachability to a
    /// container SDN network.
    ///
    /// A veth from the host into the holder netns, plus routes. Defaults to a
    /// DRY-RUN; add `--apply` (as root) to establish it.
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
    /// Human-readable detail of one or more VMs, `kubectl describe` style.
    ///
    /// For humans; use `status` for the usual compact view. Includes the LIVE
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
    /// Start an existing, stopped VM — idempotent (already running = no-op).
    ///
    /// Reboots with the base disk/vcpus/memory/network/backend recorded at
    /// its last `create`/`start`, reusing the same overlay (disk state
    /// preserved). Does NOT restore anything that only ever existed as a
    /// `vm create` flag (custom kernel/seed/volumes/static IP/VNC/advanced
    /// libvirt knobs) — a VM using those needs the original `vm create`
    /// invocation instead (also idempotent/auto-heal).
    Start {
        #[arg(add = ArgValueCandidates::new(super::complete::vms))]
        name: String,
    },
    /// Stop (if running) then start — always a real reboot, unlike `start`.
    /// Same recovered-fields limits as `start`.
    Restart {
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
    /// Take a named snapshot of a VM.
    ///
    /// libvirt: a running VM's snapshot is a system checkpoint — memory +
    /// disk; `restore` reverts to it. Not yet supported on the
    /// cloud-hypervisor backend.
    Snapshot {
        #[arg(add = ArgValueCandidates::new(super::complete::vms))]
        name: String,
        /// Snapshot name.
        snapshot: String,
    },
    /// Revert the VM to a named snapshot.
    Restore {
        #[arg(add = ArgValueCandidates::new(super::complete::vms))]
        name: String,
        /// Snapshot name to revert to.
        snapshot: String,
    },
    /// List the VM's snapshots.
    Snapshots {
        #[arg(add = ArgValueCandidates::new(super::complete::vms))]
        name: String,
    },
    /// Apply the `kind: Vm` documents of a manifest.
    ///
    /// `delonix_vm::create` is already idempotent by name — creates or
    /// auto-recovers.
    Apply {
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
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
            return Err(Error::Invalid(super::po::tf(
                "spec.volumes: mountPath {mount_path} invalid (must be an absolute path without , ] [ # \" nor control chars)",
                &[("mount_path", &format!("{:?}", v.mount_path))],
            )));
        }
        let vol = store.inspect(&v.name).map_err(|_| {
            Error::Invalid(super::po::tf(
                "spec.volumes: volume/storage '{name}' does not exist (create it before the VM)",
                &[("name", &v.name)],
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
    let spec: VmSpec = vm_spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

/// Fields the reconciler compares for a `kind: Vm`.
///
/// **None of them converge hot, and that is not a gap.** This engine does not
/// hotplug: changing a VM's vCPUs, memory or disk means booting a different
/// machine, so every one of these is a `Replace` that `apply` refuses without
/// `--replace`. Refusing is the point — recreating a VM throws away its overlay
/// disk, which is everything the guest wrote since it was created.
pub(crate) const RECONCILED_VM_FIELDS: &[&str] = &["disk", "vcpus", "memory", "network", "backend"];

fn desired_vm_fields(spec: &VmSpec) -> std::collections::BTreeMap<String, String> {
    let mut f = std::collections::BTreeMap::new();
    f.insert("disk".into(), spec.disk.clone());
    f.insert("vcpus".into(), spec.vcpus.to_string());
    f.insert("memory".into(), spec.memory.clone());
    f.insert("network".into(), spec.network.clone());
    if let Some(b) = &spec.backend {
        f.insert("backend".into(), b.clone());
    }
    f
}

/// What the manifest declares, for the reconciler.
pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec: VmSpec = vm_spec_of(doc)?;
    Ok(super::reconcile::Desired {
        kind: "Vm".into(),
        name: doc.metadata.name.clone(),
        fields: desired_vm_fields(&spec),
        converges: true,
        ownable: true,
    })
}

/// What is on the machine, for the reconciler.
///
/// Reads the STORE and not `delonix_vm::status`: `status` asks the backend
/// whether the domain is running, which is a shell-out per VM. A plan compares
/// declared configuration, and none of the compared fields change because a VM
/// happens to be off — paying a `virsh` round-trip per VM to learn something the
/// plan does not use would make planning slow for nothing.
pub(crate) fn actual() -> Result<Vec<super::reconcile::Actual>> {
    let base = state_root();
    Ok(delonix_vm::list(&base)
        .unwrap_or_default()
        .into_iter()
        .map(|vm| {
            let mut f = std::collections::BTreeMap::new();
            f.insert("disk".into(), vm.disk.clone());
            f.insert("vcpus".into(), vm.vcpus.to_string());
            f.insert("memory".into(), vm.memory.clone());
            f.insert("network".into(), vm.network.clone());
            f.insert("backend".into(), vm.backend.clone());
            super::reconcile::Actual {
                kind: "Vm".into(),
                name: vm.name.clone(),
                fields: f,
                owner: vm.labels.get(super::reconcile::STACK_LABEL).cloned(),
                last_applied: vm
                    .annotations
                    .get(super::reconcile::LAST_APPLIED)
                    .and_then(|raw| super::reconcile::decode_last_applied(raw)),
            }
        })
        .collect())
}

/// Records that this stack owns the VM, and what it last applied.
pub(crate) fn stamp(
    name: &str,
    stack: &str,
    fields: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let st: delonix_runtime_core::JsonStore<delonix_runtime_core::Vm> =
        delonix_runtime_core::JsonStore::open(state_root().join("vms"))?;
    let encoded = super::reconcile::encode_last_applied(fields);
    st.update(name, |vm| {
        vm.labels
            .insert(super::reconcile::STACK_LABEL.into(), stack.to_string());
        vm.labels
            .insert(super::reconcile::MANAGED_BY.into(), "delonix".into());
        vm.annotations
            .insert(super::reconcile::LAST_APPLIED.into(), encoded.clone());
        true
    })?;
    Ok(())
}

/// Destroys a VM so the normal creation path can rebuild it. **The overlay disk
/// goes with it** — everything the guest wrote. That is the whole reason a VM
/// change is refused without `--replace`.
pub(crate) fn remove_for_replace(name: &str) -> Result<()> {
    delonix_vm::remove_force(&state_root(), name)
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let base = state_root();
    for doc in manifest::of_kind(docs, "Vm") {
        let name = &doc.metadata.name;
        let spec: VmSpec = vm_spec_of(doc)?;

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
            // `metadata.namespace`, the same source every other Kind reads it
            // from — a VM does not get a namespace field of its own in `spec`.
            namespace: doc.metadata.namespace.clone(),
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
        println!("{}", super::po::tf("vm/{name}: ensured", &[("name", name)]));
    }
    Ok(())
}

/// Applies `--vcpus`/`--memory`/`--backend` precedence over an image's
/// recorded `VCPUS`/`MEMORY`/`HYPERVISOR` (a `VMfile` build — see
/// `cmd::vmfile`): an explicit CLI value always wins; the image's value is
/// the fallback; the final vcpus/memory default (1 / `"1G"`) only applies
/// when NEITHER said anything. Pure — no `VmImageStore` I/O — so the
/// precedence itself is testable without a real disk on the file system.
fn resolve_vm_defaults(
    vcpus: Option<u32>,
    memory: Option<String>,
    backend: Option<String>,
    image_meta: Option<&super::vmimage::VmImage>,
) -> (u32, String, Option<String>) {
    let vcpus = vcpus
        .or_else(|| image_meta.and_then(|m| m.default_vcpus))
        .unwrap_or(1);
    let memory = memory
        .or_else(|| image_meta.and_then(|m| m.default_memory.clone()))
        .unwrap_or_else(|| "1G".to_string());
    let backend = backend.or_else(|| image_meta.and_then(|m| m.default_backend.clone()));
    (vcpus, memory, backend)
}

pub fn run(action: VmCmd) -> Result<()> {
    if let VmCmd::Init {
        dir,
        name,
        image,
        force,
        vmfile,
        template,
        up,
    } = action
    {
        if vmfile {
            // Building an image and running one are different jobs; `init`
            // starts a project for either. The name defaults to the directory,
            // like the manifest scaffold already does.
            let project = name.unwrap_or_else(|| {
                dir.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .filter(|n| !n.is_empty() && n != ".")
                    .unwrap_or_else(|| "myimage".to_string())
            });
            return super::vmimage::run(super::vmimage::VmImageCmd::Init {
                name: project,
                dir: Some(dir),
                force,
            });
        }
        return init_for(
            super::scaffold::Target::Vm,
            dir,
            name,
            image,
            force,
            template,
            up,
        );
    }
    if let VmCmd::Dash { once, json } = action {
        return super::dash::run(super::dash::DashScope::Vms, once, json);
    }
    let base = state_root();
    match action {
        // Handled at the top of `run` (does `return`).
        VmCmd::Init { .. } => unreachable!("tratado acima"),
        VmCmd::Dash { .. } => unreachable!("tratado acima"),
        VmCmd::Create {
            name,
            url_img,
            disk,
            vcpus,
            memory,
            network,
            namespace,
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
            // a blind choice). When `--disk` names a KNOWN local image (or
            // none is given at all, which always resolves to one), its
            // `VCPUS`/`MEMORY`/`HYPERVISOR` (recorded by a `VMfile` build —
            // see `cmd::vmfile`) become defaults for `--vcpus`/`--memory`/
            // `--backend` below, applied only where the caller left the flag
            // unset. A `--disk` that names an ordinary path (no store entry)
            // behaves exactly as before this change.
            let mut image_meta: Option<super::vmimage::VmImage> = None;
            let disk = match (url_img, disk) {
                // An explicit URL is the most specific thing the caller can
                // say, so nothing else gets consulted — not the local store,
                // not the official image.
                (Some(url), _) => {
                    let store = super::vmimage::VmImageStore::open(super::util::state_root())?;
                    super::vmimage::download_url_base(&store, &url)?
                        .to_string_lossy()
                        .into_owned()
                }
                (None, Some(d)) => {
                    let store = super::vmimage::VmImageStore::open(super::util::state_root())?;
                    let known = store.get(&d).ok().map(|meta| (store.qcow2_path(&d), meta));
                    match known {
                        Some((path, meta)) => {
                            image_meta = Some(meta);
                            path.to_string_lossy().into_owned()
                        }
                        None => d,
                    }
                }
                (None, None) => {
                    let store = super::vmimage::VmImageStore::open(super::util::state_root())?;
                    // Downloads the official golden when nothing is local — the
                    // same helper `cluster kubeadm` uses. This path used to
                    // fail with "run `image --vm build` (or `pull`) first",
                    // which is a research task for an image the project
                    // publishes so it never has to be built by hand.
                    let tag = super::cluster::resolve_or_pull_vm_image(&store, None, None)?;
                    image_meta = store.get(&tag).ok();
                    store.qcow2_path(&tag).to_string_lossy().into_owned()
                }
            };
            let (vcpus, memory, backend) =
                resolve_vm_defaults(vcpus, memory, backend, image_meta.as_ref());
            // ALWAYS a cloud-init seed (unless an explicit `--seed`). Without a
            // datasource, the cloud image's cloud-init doesn't run the network
            // phase and the VM ends up with no IP nor route ("Network is
            // unreachable" in the guest, a real case). The minimal seed
            // (network-config DHCP + hostname = VM name) makes cloud-init bring
            // up the network and apply the ssh-keys/hostname when given.
            // Recorded before `ssh_keys` is moved into the seed, and only for
            // the generated-seed path: a caller who brought their OWN `--seed`
            // decided the accounts themselves, and we would be guessing.
            //
            // EXCEPT for an appliance (`cloud_init: false` in the image's
            // metadata — OPNsense, Proxmox, TrueNAS). Those install and
            // configure themselves; the seed would be an ISO nothing reads, on
            // a CD-ROM that changes the guest's device list for no reason.
            let appliance = image_meta.as_ref().is_some_and(|m| !m.uses_cloud_init());
            if appliance && seed.is_none() {
                // Refuse rather than drop: silently ignoring an option the
                // caller passed is the failure this repo names as its worst.
                let asked: Vec<&str> = [
                    (hostname.is_some(), "--hostname"),
                    (!ssh_keys.is_empty(), "--ssh-key"),
                    (user_data.is_some(), "--user-data"),
                ]
                .iter()
                .filter(|(given, _)| *given)
                .map(|(_, flag)| *flag)
                .collect();
                if !asked.is_empty() {
                    return Err(Error::Invalid(super::po::tf(
                        "{flags}: this image is an appliance and does not run cloud-init, so \
                         these would be silently ignored — configure it on first boot (console \
                         or web UI), or pass your own `--seed` if you know the guest reads one",
                        &[("flags", &asked.join(", "))],
                    )));
                }
            }
            let injected_key = seed.is_none() && !ssh_keys.is_empty();
            // Did the seed directory exist BEFORE this invocation? `vm create` is
            // idempotent/auto-heal, so a re-create over a live VM must never have
            // its seed cleaned up underneath it — only a directory this call
            // brought into being is ours to remove. See `seed_to_clean` below.
            let vmdir = state_root().join("vms").join(&name);
            let vmdir_existed = vmdir.exists();
            let seed = match seed {
                Some(s) => Some(s),
                None if appliance => None,
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
            // The seed is written BEFORE any backend is chosen, and everything
            // between here and `create_with` can fail: an unknown/unavailable
            // backend, `vm_admission_check` (no RAM on the host), the `--namespace`
            // refusal on libvirt, `prepare_local_overlay`. Every one of those used
            // to leave `<root>/vms/<name>/` behind — with `seed.iso` and a
            // `user-data` holding the injected SSH public key — invisible to
            // `vm ls` (no record) and unremovable by `vm rm` (which answers «no
            // such VM»). One directory per attempt, and the most likely trigger is
            // retrying the command that just failed. This repo has already had a
            // 45 GiB disk-pressure incident from the same class of unreaped orphan.
            let seed_to_clean = (!vmdir_existed).then(|| vmdir.clone());
            let cfg = VmConfig {
                name,
                disk,
                vcpus,
                memory,
                network,
                namespace,
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
            // Same live display as `vm build`: a spinner while a stage runs, a
            // green tick and how long it took when it ends. The engine reports
            // only that a stage STARTED, so each report closes the previous one
            // — correct here because a stage that failed never reaches the next
            // (and the one left open closes with ✗ on the way out, from `Drop`).
            let prog = std::cell::RefCell::new(super::output::Progress::new());
            let render = |s: delonix_vm::CreateStage| {
                use delonix_vm::CreateStage::*;
                let (step, icon) = match s {
                    Disk => (super::po::t("preparing the overlay disk"), "💽"),
                    Network => (super::po::t("configuring the network"), "🌐"),
                    Define => (super::po::t("defining the domain"), "📋"),
                    Start => (super::po::t("starting the VM"), "▶"),
                };
                let mut p = prog.borrow_mut();
                p.ok();
                p.step(step, icon);
            };
            let created = delonix_vm::create_with(&base, &cfg, &render);
            // The last stage has no successor to close it, so it is closed here
            // — before anything else prints, or the tick lands after the line
            // that says the VM is up.
            if created.is_ok() {
                prog.borrow_mut().ok();
            }
            drop(prog);
            let vm = match created {
                Ok(vm) => vm,
                Err(e) => {
                    // Best-effort, and it must never mask the real error: the
                    // operator needs to read why the create failed, not why the
                    // cleanup did.
                    if let Some(dir) = &seed_to_clean {
                        let _ = std::fs::remove_dir_all(dir);
                    }
                    return Err(e);
                }
            };
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
                return cmd_console(&base, &vm.name, None);
            }
            if wait {
                wait_for_boot(
                    &base,
                    &vm.name,
                    std::time::Duration::from_secs(boot_timeout),
                );
            }
            let fresh = delonix_vm::status(&base, &vm.name).ok();
            let ip = fresh.as_ref().and_then(|v| v.ip.clone());
            let ssh_user = fresh.as_ref().map(|v| default_ssh_user(v));
            print_vm_next_steps(&vm.name, ip.as_deref(), injected_key, ssh_user);
            Ok(())
        }
        VmCmd::Build {
            tag,
            file,
            context,
            no_compress,
            network,
            verbose,
        } => {
            // The VMfile path only — this group has no golden-recipe flags, so
            // there is nothing to disambiguate. `-f` absent means
            // `<context>/VMfile`, and its absence is an error rather than a
            // silent fallback to the golden recipe: `delonix vm build` in a
            // directory with no VMfile is a mistake, not a request for
            // Kubernetes.
            let store = super::vmimage::VmImageStore::open(super::util::state_root())?;
            let path = file.unwrap_or_else(|| context.join("VMfile"));
            if !path.exists() {
                return Err(Error::Invalid(super::po::tf(
                    "no VMfile at {path} — run `delonix vm init` to scaffold one",
                    &[("path", &path.display().to_string())],
                )));
            }
            super::vmfile::build(
                &store,
                &path,
                &context,
                &tag,
                !no_compress,
                network,
                verbose,
            )
        }
        VmCmd::Pull {
            source,
            name,
            no_k8s,
        } => {
            let store = super::vmimage::VmImageStore::open(super::util::state_root())?;
            // Same rule as `image vm pull`: a reference with no registry is
            // resolved against the official catalogue, one with a `/` is used
            // as given.
            let src = match source {
                Some(s) => super::vmimage::resolve_official_ref(&s)?,
                None => super::vmimage::default_pull_source(no_k8s).to_string(),
            };
            super::vmimage::cmd_pull(&store, &src, name)
        }
        VmCmd::LsRemote { source, no_k8s } => match source {
            Some(s) => super::vmimage::cmd_ls_remote(&super::vmimage::resolve_official_ref(&s)?),
            None if no_k8s => super::vmimage::cmd_ls_remote(
                super::vmimage::default_pull_source(true)
                    .rsplit_once(':')
                    .map_or(super::vmimage::default_pull_source(true), |(r, _)| r),
            ),
            None => super::vmimage::cmd_ls_remote_official(),
        },
        VmCmd::Push { name, target } => {
            let store = super::vmimage::VmImageStore::open(super::util::state_root())?;
            super::vmimage::cmd_push(&store, &name, target.as_deref())
        }
        VmCmd::Convert {
            source,
            to,
            output,
            compress,
        } => {
            let store = super::vmimage::VmImageStore::open(super::util::state_root())?;
            super::vmimage::cmd_convert(&store, &source, to, output, compress)
        }
        VmCmd::DefaultBackend { set, clear } => {
            if clear {
                delonix_vm::clear_default_backend(&base)?;
                println!(
                    "{}",
                    super::po::t("default backend cleared (falls back to auto-detection)")
                );
            } else if let Some(backend) = set {
                delonix_vm::set_default_backend(&base, &backend)?;
                let canon = delonix_vm::get_default_backend(&base).unwrap_or(backend);
                println!(
                    "{}",
                    super::po::tf("default backend set to {backend}", &[("backend", &canon)])
                );
            } else {
                match delonix_vm::get_default_backend(&base) {
                    Some(b) => println!("{b}"),
                    None => println!(
                        "{}",
                        super::po::t(
                            "none (auto-detection: cloud-hypervisor if installed, else libvirt)"
                        )
                    ),
                }
            }
            Ok(())
        }
        VmCmd::Ls { ports, output } => {
            if output == super::output::OutputFormat::Json {
                let rows: Vec<VmLsRow> = delonix_vm::list(&base)?
                    .into_iter()
                    .map(|vm| VmLsRow {
                        name: vm.name.clone(),
                        vcpus: vm.vcpus,
                        memory: vm.memory,
                        status: fmt_vm_status(&vm.status),
                        ip: vm.ip.clone(),
                        started_unix: vm.started_unix,
                        role: vm_role(&vm.name).to_string(),
                        gpu: fmt_vm_gpu(&vm.devices),
                        // The probe does live network I/O — only when --ports (like the column).
                        ports_open: ports.then(|| fmt_open_ports(vm.ip.as_deref())),
                    })
                    .collect();
                return output::print_json(&rows);
            }
            let mut cols = vec![
                "NAME", "VCPUS", "MEMORY", "STATUS", "IP", "UPTIME", "ROLE", "GPU",
            ];
            if ports {
                cols.push("PORTS OPEN");
            }
            let mut t = output::Table::new(&cols)
                // VCPUS is a count — right-aligned like the sizes.
                .right_align(1);
            for vm in delonix_vm::list(&base)? {
                let mut row = vec![
                    vm.name.clone(),
                    vm.vcpus.to_string(),
                    vm.memory,
                    fmt_vm_status(&vm.status),
                    vm.ip.clone().unwrap_or_else(|| "<none>".into()),
                    fmt_vm_uptime(vm.started_unix),
                    vm_role(&vm.name).to_string(),
                    fmt_vm_gpu(&vm.devices),
                ];
                if ports {
                    row.push(fmt_open_ports(vm.ip.as_deref()));
                }
                t.row(row);
            }
            t.print();
            Ok(())
        }
        VmCmd::Describe { names } => cmd_describe(&base, &names),
        VmCmd::Console { name, escape } => cmd_console(&base, &name, escape.as_deref()),
        VmCmd::Ssh {
            target,
            user,
            identity,
            command,
        } => cmd_ssh(
            &base,
            &target,
            user.as_deref(),
            identity.as_deref(),
            &command,
        ),
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
        VmCmd::Start { name } => {
            delonix_vm::start(&base, &name)?;
            println!("{name}");
            Ok(())
        }
        VmCmd::Restart { name } => {
            delonix_vm::restart(&base, &name)?;
            println!("{name}");
            Ok(())
        }
        VmCmd::Snapshot { name, snapshot } => {
            delonix_vm::snapshot(&base, &name, &snapshot)?;
            println!("{snapshot}");
            Ok(())
        }
        VmCmd::Restore { name, snapshot } => {
            delonix_vm::restore(&base, &name, &snapshot)?;
            println!("{name}");
            Ok(())
        }
        VmCmd::Snapshots { name } => {
            for s in delonix_vm::snapshots(&base, &name)? {
                println!("{s}");
            }
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
                "  fix: re-publish bound to the VM gateway — `delonix net ingress unpublish <c> <port>`, then `DELONIX_PUBLISH_ADDR={gw} delonix net ingress publish <c> <port>` (reachable from VMs on that network, not the external LAN)",
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
/// `vm ls -o json` row (ADR-0005): stable keys mirroring the table columns, with
/// machine-friendly values (`ip`/`started_unix` as nullable raw, not `<none>`/a
/// human duration). `ports_open` present only with `--ports`.
#[derive(serde::Serialize)]
struct VmLsRow {
    name: String,
    vcpus: u32,
    memory: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_unix: Option<u64>,
    role: String,
    gpu: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ports_open: Option<String>,
}

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

// ---- Adapters for the unified `delonix workload` layer (ADR-0002, Phase 2a) ----
// Thin wrappers over the existing vm list/stop/rm APIs (mirror `vm ls`/`vm stop`/
// `vm rm`) so `cmd/workload.rs` can drive VMs uniformly. `vm ls` uses the stored
// records (not a per-VM `status()` reconcile), so this matches it exactly.

pub(crate) fn workload_rows() -> Result<Vec<super::workload::WorkloadRow>> {
    let base = state_root();
    let mut rows = Vec::new();
    for vm in delonix_vm::list(&base)? {
        rows.push(super::workload::WorkloadRow {
            kind: "vm",
            name: vm.name.clone(),
            status: fmt_vm_status(&vm.status),
            info: format!("{} vCPU, {}", vm.vcpus, vm.memory),
        });
    }
    Ok(rows)
}

/// `true` if a VM with this exact NAME exists.
pub(crate) fn workload_owns(name: &str) -> Result<bool> {
    Ok(delonix_vm::list(&state_root())?
        .iter()
        .any(|v| v.name == name))
}

pub(crate) fn workload_stop(name: &str) -> Result<()> {
    delonix_vm::stop(&state_root(), name)?;
    // Echo the name on success, mirroring the native `vm stop` (whose CLI arm
    // prints it — `delonix_vm::stop` itself is silent).
    println!("{name}");
    Ok(())
}

pub(crate) fn workload_remove(name: &str, force: bool) -> Result<()> {
    let base = state_root();
    if force {
        delonix_vm::remove_force(&base, name)?;
    } else {
        delonix_vm::remove(&base, name)?;
    }
    println!("{name}");
    Ok(())
}

pub(crate) fn workload_describe(name: &str) -> Result<()> {
    cmd_describe(&state_root(), &[name.to_string()])
}

/// UPTIME column: "Up X" since the CURRENT boot (`started_unix`, distinct
/// from `created_unix` — see the field doc in `delonix-runtime-core`), or
/// "-" for a stopped VM / an old record predating this field.
fn fmt_vm_uptime(started_unix: Option<u64>) -> String {
    match started_unix {
        Some(t) => format!(
            "Up {}",
            output::fmt_duration_secs(output::now_unix().saturating_sub(t))
        ),
        None => "-".to_string(),
    }
}

/// ROLE column: derived from the deterministic naming `cluster kubeadm` gives
/// its nodes (`vm_names` in `cmd/cluster.rs`: `<cluster>-cp<N>`/`<cluster>-w<N>`)
/// — no new state to keep in sync, just reading a convention the codebase
/// already committed to everywhere else (`cluster ls` derives similarly from
/// labels rather than its own store). A VM outside that convention (manifest/
/// `vm create` standalone) has no role to report — "-", not a guess.
fn vm_role(vm_name: &str) -> &'static str {
    let suffix = vm_name.rsplit('-').next().unwrap_or("");
    if let Some(n) = suffix.strip_prefix("cp") {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return "control-plane";
        }
    }
    if let Some(n) = suffix.strip_prefix('w') {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return "worker";
        }
    }
    "-"
}

/// GPU column: count of PCI passthrough devices attached at boot (SR-IOV VFs
/// — see `VmConfig.devices`, overwhelmingly used for GPU passthrough in
/// practice; we don't claim to identify the device CLASS, just that
/// passthrough hardware is attached). "-" when none, so an all-dash column
/// (the common case, no GPU VMs) reads as "nothing to see" at a glance.
fn fmt_vm_gpu(devices: &[String]) -> String {
    match devices.len() {
        0 => "-".to_string(),
        n => format!("{n} dev"),
    }
}

/// Well-known ports worth a quick reachability check from `vm ls --ports`:
/// SSH (every VM), and the Kubernetes control-plane/kubelet/HTTP(S) ports a
/// cluster node commonly exposes. Deliberately small — this is a glance, not
/// a port scanner.
const PROBE_PORTS: &[u16] = &[22, 6443, 10250, 80, 443];

/// Probes [`PROBE_PORTS`] on `ip` concurrently (one thread per port, short
/// connect timeout) and returns the ones that accepted a TCP connection.
/// Zero new dependencies: `TcpStream::connect_timeout` is std. A VM with no
/// IP yet (still booting) skips the network I/O entirely.
fn fmt_open_ports(ip: Option<&str>) -> String {
    let Some(ip) = ip else {
        return "-".to_string();
    };
    use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
    use std::time::Duration;
    let handles: Vec<_> = PROBE_PORTS
        .iter()
        .filter_map(|&port| {
            let addr: SocketAddr = (ip, port).to_socket_addrs().ok()?.next()?;
            Some((
                port,
                std::thread::spawn(move || {
                    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
                }),
            ))
        })
        .collect();
    let mut open: Vec<u16> = handles
        .into_iter()
        .filter_map(|(port, h)| h.join().ok().filter(|&ok| ok).map(|_| port))
        .collect();
    open.sort_unstable();
    if open.is_empty() {
        "-".to_string()
    } else {
        open.iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",")
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
/// Is this a literal address rather than a VM name? Deliberately narrow: an
/// IPv4 literal, or anything with a dot or colon in it (a hostname, an IPv6).
/// `valid_vm_name` forbids `/` and control characters but ALLOWS dots, so a VM
/// could in principle be called `a.b` — that is why the store is consulted
/// FIRST and this only decides what to do when the store has nothing.
fn looks_like_address(s: &str) -> bool {
    s.contains(':') || s.split('.').count() > 1
}

/// `vm ssh` — go to a VM by name (the record holds its IP) or straight to an
/// address.
///
/// Why the engine and not "just type ssh": the IP is the thing the user does not
/// have. It lives in the record, it is only learnt well after `create` (a nat VM
/// gets its DHCP lease late), and the default account is `delonix` — not the
/// distro's own (`ubuntu`, `rocky`, `debian`), which EXISTS and does not carry
/// the key, so guessing it answers `Permission denied (publickey)` and reads
/// like a broken key instead of a wrong name.
///
/// `exec`s: this is a shortcut for a shell, so `ssh` inherits the terminal
/// whole. Nothing to do after it returns.
fn cmd_ssh(
    base: &std::path::Path,
    target: &str,
    user: Option<&str>,
    identity: Option<&std::path::Path>,
    command: &[String],
) -> Result<()> {
    // The store decides, and the address heuristic only breaks the tie when it
    // has nothing — same order as `vm convert`, and for the same reason: a name
    // the user has is worth more than a shape that looks like an address.
    let (host, vm) = match delonix_vm::status(base, target) {
        Ok(vm) => match vm.ip.as_deref().filter(|s| !s.is_empty()) {
            Some(ip) => (ip.to_string(), Some(vm)),
            None => {
                return Err(Error::Invalid(super::po::tf(
                    "VM '{name}' has no IP yet — it is '{status}'. A VM only gets one once it has \
                     booted AND its network came up; watch it with `delonix vm console {name}`, or \
                     check `delonix vm ls` again in a moment",
                    &[("name", target), ("status", &format!("{:?}", vm.status))],
                )));
            }
        },
        Err(_) if looks_like_address(target) => (target.to_string(), None),
        Err(e) => return Err(e),
    };
    // An explicit `-l` always wins; otherwise the IMAGE decides, because the
    // answer is a property of the guest and not of this command.
    let user = match (user, vm.as_ref()) {
        (Some(u), _) => u.to_string(),
        (None, Some(vm)) => default_ssh_user(vm).to_string(),
        (None, None) => GUEST_SSH_USER.to_string(),
    };
    // An appliance authenticates with a PASSWORD set when the image was built,
    // and nothing on this host knows it — so say where it came from instead of
    // leaving a bare prompt. Reported live: three `Permission denied` for
    // `delonix@…` against a Proxmox guest that has no `delonix` account at all.
    if user == "root" && command.is_empty() {
        eprintln!(
            "{}",
            super::po::t(
                "appliance image: logging in as root with the password set when the image was built (`root-password` in scripts/appliances/answer-*.toml; the published images use `delonix-admin`)"
            )
        );
    }

    let mut cmd = std::process::Command::new("ssh");
    if let Some(key) = identity {
        cmd.arg("-i").arg(key);
    }
    // A VM is recreated at the same address all the time; a changed host key is
    // the NORM here, not an attack, and refusing to connect over it would make
    // this shortcut useless. Said out loud rather than hidden: this is a lab
    // convenience, and `delonix vm ssh` is not the tool for a host you do not
    // own.
    cmd.args([
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "LogLevel=ERROR",
    ]);
    // `--` before the destination: a name starting with `-` would otherwise be
    // read as an option (the same defence the `ssh`/`scp` of `cluster apply`
    // got in the first security audit).
    cmd.arg("--").arg(format!("{user}@{host}"));
    if !command.is_empty() {
        cmd.args(command);
    }
    use std::os::unix::process::CommandExt;
    // `exec` and not `spawn`+`wait`: this hands the terminal to ssh whole
    // (interactive shell, pty, escape handling) and there is nothing to do
    // afterwards. `exec` only RETURNS on failure — most often ssh not installed.
    let err = cmd.exec();
    Err(Error::Runtime {
        context: "ssh",
        message: format!("could not run ssh: {err}"),
    })
}

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
/// The account the injected key lands in.
///
/// Named here rather than inlined because the next-steps block has to agree
/// with `build_user_data`, and a reader who guesses is a reader who is wrong:
/// on an Ubuntu cloud image the obvious guess is `ubuntu`, and that account
/// exists — it just does not have the key, so it answers
/// `Permission denied (publickey)`, which reads as a broken key rather than a
/// wrong username. Hit while validating `vm create --url-img`.
const GUEST_SSH_USER: &str = "delonix";

/// The account to log into a VM as, when the caller did not say.
///
/// An appliance is somebody else's operating system: it has `root` and whatever
/// its installer created, and no `delonix` — nothing here ever ran cloud-init
/// on it. Defaulting to `delonix` there produces a password prompt for an
/// account that does not exist, which is indistinguishable from a wrong
/// password. Reported exactly that way against a Proxmox VE guest: three
/// `Permission denied` in a row, for a user the image never had.
fn default_ssh_user(vm: &delonix_runtime_core::Vm) -> &'static str {
    match super::vmimage::image_of_disk(&vm.disk).and_then(|i| i.cloud_init) {
        Some(false) => "root",
        _ => GUEST_SSH_USER,
    }
}

fn print_vm_next_steps(name: &str, ip: Option<&str>, has_key: bool, ssh_user: Option<&str>) {
    let mut rows = vec![
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
    // Second row, not last: it is what most people want first, and it is the
    // one piece of the block they cannot derive themselves.
    if has_key {
        rows.insert(
            1,
            (
                format!(
                    "ssh {}@{}",
                    ssh_user.unwrap_or(GUEST_SSH_USER),
                    ip.unwrap_or("<ip>")
                ),
                super::po::t("log in with the key you injected"),
            ),
        );
    } else if ssh_user == Some("root") {
        // An appliance gets no injected key, and the row was therefore omitted
        // entirely — leaving the reader to guess a username (`delonix`, which
        // does not exist there) and a password. Naming the account is half the
        // answer; `vm ssh` prints where the password comes from.
        rows.insert(
            1,
            (
                format!("delonix vm ssh {name} -l root"),
                super::po::t("log in (appliance: root + the password from the image build)"),
            ),
        );
    }
    eprintln!("\n{}", super::po::t("Next steps:"));
    for (cmd, desc) in rows {
        eprintln!("  {cmd:<30} # {desc}");
    }
}

/// local tty (raw mode); libvirt: delegates to `virsh console` (which does it).
fn cmd_console(base: &std::path::Path, name: &str, escape: Option<&str>) -> Result<()> {
    let vm = delonix_vm::status(base, name)?;
    if !matches!(vm.status, delonix_runtime_core::Status::Running) {
        return Err(Error::Invalid(super::po::tf(
            "VM '{name}' is not running — start it first",
            &[("name", name)],
        )));
    }
    let esc = resolve_escape(escape)?;
    // The golden image auto-logs-in on ttyS0, so inside the console `exit`/`logout`
    // just re-trigger the getty and loop forever — the ONLY way back to the host
    // is the escape key. Spelling it out (in the user's language) fixes the
    // recurring "I can't get out of the VM console" report.
    eprintln!(
        "{}",
        super::po::tf(
            "Console of '{name}'. To return to the host: press Ctrl+{key}  (exit/logout only restarts the session — autologin re-enters).",
            &[("name", name), ("key", &esc.letter.to_string())],
        )
    );
    let backend = vm.backend.as_str();
    if backend.contains("libvirt") || backend.contains("qemu") || backend.contains("kvm") {
        // Spawn `virsh console` as a CHILD (not exec/replace) so that when the
        // user presses Ctrl+] we regain control and can confirm the return —
        // virsh handles the raw tty and the escape key itself.
        //
        // BUG FIXED HERE, found live on a real host: without `--force`, a console
        // left behind by a crashed/killed `delonix vm console` (SSH drop, Ctrl-C
        // hitting virsh's foreground process, terminal closed) makes libvirt
        // believe a session is still attached, and every subsequent `vm console`
        // fails with "Active console session exists for this domain" — no way
        // out short of a host-level libvirtd restart. `delonix vm console <name>`
        // is a single-operator "get me back into this VM" command, so a stale
        // lock on your own VM is the overwhelmingly common case, not a real
        // concurrent viewer to protect; `--force` (built for exactly this) takes
        // over the stale session instead of refusing forever.
        let uri = delonix_vm::libvirt_uri(name);
        // A serial console is a pty, and a pty keeps NO history: everything the
        // guest printed before you attached — the boot log, the `login:` prompt —
        // is already gone. Attaching to a VM that finished booting therefore
        // lands on a BLANK screen, and a blank screen reads as "the console is
        // broken".
        //
        // BUG FIXED HERE, reproduced on a Proxmox VE guest: `vm console pve`
        // showed nothing but the two virsh banners, while a VNC screenshot of
        // the same VM showed a healthy `pve login:`. Nothing was broken — the
        // getty had said its piece a minute earlier and had no reason to speak
        // again. One newline makes it repaint the prompt, which is the whole
        // difference between "doesn't enter the VM" and a login. It is sent
        // slightly LATE on purpose: virsh has to own the pty first, or the
        // answer is written into a pty nobody is reading yet.
        if let Some(pty) = console_pty(&uri, name) {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(600));
                nudge_serial(&pty);
            });
        }
        let status = std::process::Command::new("virsh")
            // `-e` is a GLOBAL virsh option, not a `console` one — it has to come
            // before the subcommand or virsh takes it as the domain's.
            .args([
                "-c", &uri, "-e", &esc.spec, "console", "--force", "--", name,
            ])
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
    let r = console_bridge(&sock, esc.byte);
    eprintln!("{}", super::po::t("Back to the host shell."));
    r
}

/// The key that detaches a console: the byte a terminal really sends, the
/// spelling `virsh -e` wants, and the letter to print to a human.
pub(crate) struct Escape {
    pub byte: u8,
    pub spec: String,
    pub letter: char,
}

/// Which key detaches the console: `--escape`, then `$DELONIX_CONSOLE_ESCAPE`,
/// then `^]`.
///
/// The default is `^]` because that is what telnet, virsh and every serial
/// console before them used — but it is NOT typeable on every keyboard, and
/// that is the reason this is configurable at all. On a Portuguese layout `]`
/// is `AltGr+9`, and `Ctrl+AltGr+9` does not produce 0x1d: the console opens,
/// works, and cannot be left except by killing the terminal. Reported exactly
/// that way. `delonix vm console x -e ^X` (or the env var, once, in a profile)
/// gives back a key the keyboard can actually press.
fn resolve_escape(flag: Option<&str>) -> Result<Escape> {
    let raw = flag
        .map(str::to_string)
        .or_else(|| std::env::var("DELONIX_CONSOLE_ESCAPE").ok())
        .unwrap_or_else(|| "^]".to_string());
    let byte = escape_byte(&raw).ok_or_else(|| {
        Error::Invalid(super::po::tf(
            "invalid console escape '{raw}' — give ONE control key, as `^X` (or `X`)",
            &[("raw", &raw)],
        ))
    })?;
    // Back to the printable letter from the control byte, so what is announced
    // is what the user must press whichever of the two forms they wrote.
    let letter = (byte | 0x40) as char;
    Ok(Escape {
        byte,
        spec: format!("^{letter}"),
        letter,
    })
}

/// `^X`/`X` -> the control byte (`Ctrl` clears the top three bits). `None` for
/// anything that is not one control key, `^@` included: that byte is NUL, which
/// no terminal sends for a keypress, and accepting it would silently install an
/// escape that can never fire.
pub(crate) fn escape_byte(spec: &str) -> Option<u8> {
    let s = spec.trim();
    let c = match (s.len(), s.starts_with('^')) {
        (1, false) => s.chars().next()?,
        (2, true) => s.chars().nth(1)?,
        _ => return None,
    };
    if !c.is_ascii() {
        return None;
    }
    let b = (c.to_ascii_uppercase() as u8) & 0x1f;
    (b != 0).then_some(b)
}

/// The host pty libvirt wired the domain's serial port to (`/dev/pts/N`).
/// `virsh ttyconsole` answers this directly, so there is no XML to parse.
/// `None` when libvirt cannot say — the console still opens, it just does not
/// get the nudge below.
fn console_pty(uri: &str, name: &str) -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("virsh")
        .args(["-c", uri, "ttyconsole", "--", name])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!p.is_empty()).then(|| std::path::PathBuf::from(p))
}

/// Sends one carriage return to a serial console, to make whatever is listening
/// on the other end (a getty, a shell) repaint its prompt for a viewer who
/// arrived after it was printed. Harmless in both cases: at a `login:` it
/// re-prompts, at a shell it runs an empty command.
///
/// Best-effort by design — a console that cannot be nudged is still a console,
/// so every failure here is silent rather than turned into an error the user
/// can do nothing about.
fn nudge_serial(pty: &std::path::Path) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(pty) {
        let _ = f.write_all(b"\r");
        let _ = f.flush();
    }
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
fn console_bridge(sock: &std::path::Path, escape: u8) -> Result<()> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    let stream = UnixStream::connect(sock).map_err(|e| Error::Runtime {
        context: "vm console",
        message: e.to_string(),
    })?;
    use std::os::unix::io::AsRawFd;
    let _raw = RawTty::enable();
    eprintln!("[connected — the console returns here when you detach or the VM powers off]\r");

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
    // Same reason as the libvirt path (see `nudge_serial`): the pty/socket has
    // no history, so a viewer who attaches after the boot finished sees nothing
    // until the getty is given a reason to reprint its prompt.
    let _ = wr.write_all(b"\r");
    let _ = wr.flush();
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
                    if buf[..n].contains(&escape) {
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
    // An isolation boundary that is invisible is an isolation boundary nobody
    // audits — shown always, `default` included, so "which namespace is this VM
    // in" never needs a guess or a look at the JSON.
    d.sub("Namespace", &vm.namespace);
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
        // BUG FIXED HERE: a bare top-level `ssh_authorized_keys:` only reaches
        // cloud-init's DEFAULT distro user (`ubuntu` on this Ubuntu-based golden
        // image) — NOT the `delonix` user the golden image itself creates at
        // build time (`vmimage.rs`, `sudo` NOPASSWD) and that everything else
        // here assumes is the login target: the autologin config right below
        // (`agetty --autologin delonix`), and `cluster kubeadm`'s SSH user,
        // hardcoded to `delonix` (the account "the golden image already
        // creates"). Found live: `delonix cluster kubeadm` consistently failed
        // "SSH did not respond within --boot-timeout" — the VM WAS reachable
        // and the key WAS installed, just onto `ubuntu`, not `delonix`.
        // Scoping the key under `users:` (keeping `- default` so the `ubuntu`
        // account nothing else here relies on still gets created too) targets
        // the EXISTING `delonix` account directly — cloud-init adds keys to an
        // already-existing user without trying to recreate it.
        out.push_str("users:\n");
        out.push_str("  - default\n");
        out.push_str("  - name: delonix\n");
        out.push_str("    ssh_authorized_keys:\n");
        for k in ssh_keys {
            out.push_str(&format!("      - {k}\n"));
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
                    "{}: {e}",
                    super::po::tf(
                        "could not copy --user-data '{path}'",
                        &[("path", &p.display().to_string())],
                    )
                ))
            })?;
            // The user's own user-data replaces EVERYTHING — there's nowhere to
            // inject the volume mounts without merging them. Warn instead of
            // losing them silently (the `<filesystem>` stays in the XML, but the
            // guest won't mount them by itself without a `mounts:` entry).
            if !volumes.is_empty() {
                eprintln!(
                    "{}",
                    super::po::tf(
                        "WARNING: VM '{vm_name}': custom --user-data/seed does not include the 9p volume mounts — add them manually (tags: {tags})",
                        &[
                            ("vm_name", vm_name),
                            (
                                "tags",
                                &volumes.iter().map(|v| v.tag.as_str()).collect::<Vec<_>>().join(", "),
                            ),
                        ],
                    )
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
        .map_err(|e| Error::Invalid(format!("{}: {e}", super::po::t("running cloud-localds"))))?;
    if !status.success() {
        return Err(Error::Invalid(super::po::tf(
            "cloud-localds failed (exit {code})",
            &[("code", &format!("{:?}", status.code()))],
        )));
    }
    Ok(iso_path)
}

/// Handles the `init` of this group (see `cmd::scaffold`).
/// The generator behind `vm init`, exposed so `delonix init` can dispatch here once it has
/// DETECTED a VM project (a `VMfile`) instead of duplicating it.
pub(crate) fn init_for(
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
        build_meta_data, build_user_data, fmt_vm_gpu, fmt_vm_status, fmt_vm_uptime,
        looks_like_address, normalize_vm_spec, parse_ip_gateways, parse_ss_binds,
        resolve_vm_defaults, vm_role, VmSpec,
    };
    use delonix_runtime_core::Status;

    fn image_with_defaults(
        vcpus: Option<u32>,
        memory: Option<String>,
        backend: Option<String>,
    ) -> super::super::vmimage::VmImage {
        super::super::vmimage::VmImage {
            name: "img".to_string(),
            tag: "img".to_string(),
            digest: "sha256:x".to_string(),
            size: 0,
            ubuntu_release: None,
            k8s_version: None,
            created_unix: 0,
            kernel_version: None,
            distro: None,
            default_vcpus: vcpus,
            default_memory: memory,
            default_backend: backend,
            cloud_init: None,
        }
    }

    /// A VM name and an address are told apart ONLY when the store has nothing —
    /// `valid_vm_name` allows dots, so a VM could be called `a.b` and the store
    /// has to win. This guards the tie-breaker, not the lookup order.
    #[test]
    fn looks_like_address_so_decide_o_desempate() {
        for a in [
            "192.168.122.50",
            "10.0.0.1",
            "nas.local",
            "fe80::1",
            "host:2222",
        ] {
            assert!(looks_like_address(a), "'{a}' parece um endereço");
        }
        for n in ["dev", "demovm", "vm1", "kaeso-odoo18"] {
            assert!(!looks_like_address(n), "'{n}' é um nome de VM");
        }
    }
    #[test]
    fn resolve_vm_defaults_cli_explicito_ganha_a_imagem() {
        let img = image_with_defaults(Some(4), Some("4G".into()), Some("cloud-hypervisor".into()));
        // O CLI diz tudo — a imagem é ignorada por inteiro.
        assert_eq!(
            resolve_vm_defaults(
                Some(2),
                Some("2G".into()),
                Some("libvirt".into()),
                Some(&img)
            ),
            (2, "2G".to_string(), Some("libvirt".to_string()))
        );
    }

    #[test]
    fn resolve_vm_defaults_cai_para_a_imagem_quando_o_cli_nao_diz_nada() {
        let img = image_with_defaults(Some(4), Some("4G".into()), Some("libvirt".into()));
        assert_eq!(
            resolve_vm_defaults(None, None, None, Some(&img)),
            (4, "4G".to_string(), Some("libvirt".to_string()))
        );
    }

    #[test]
    fn resolve_vm_defaults_sem_cli_nem_imagem_usa_1_vcpu_1g_e_backend_none() {
        assert_eq!(
            resolve_vm_defaults(None, None, None, None),
            (1, "1G".to_string(), None)
        );
    }

    #[test]
    fn resolve_vm_defaults_campo_a_campo_da_imagem_so_preenche_o_que_falta_no_cli() {
        // A imagem só tem VCPUS — MEMORY/HYPERVISOR ficam nos defaults finais.
        let img = image_with_defaults(Some(8), None, None);
        assert_eq!(
            resolve_vm_defaults(None, None, None, Some(&img)),
            (8, "1G".to_string(), None)
        );
    }

    #[test]
    fn vm_role_le_o_sufixo_determinístico_do_cluster_kubeadm() {
        assert_eq!(vm_role("lab-cp1"), "control-plane");
        assert_eq!(vm_role("lab-cp12"), "control-plane");
        assert_eq!(vm_role("lab-w1"), "worker");
        assert_eq!(vm_role("prod-w3"), "worker");
        // Nada disto é um nó de cluster kubeadm — sem papel a reportar.
        assert_eq!(vm_role("dev"), "-");
        assert_eq!(vm_role("my-custom-vm"), "-");
        assert_eq!(vm_role("lab-cp"), "-"); // sem número, não bate no padrão
        assert_eq!(vm_role("lab-cpx"), "-"); // sufixo não-numérico
    }

    #[test]
    fn fmt_vm_gpu_conta_dispositivos_passthrough() {
        assert_eq!(fmt_vm_gpu(&[]), "-");
        assert_eq!(fmt_vm_gpu(&["0000:65:00.1".to_string()]), "1 dev");
        assert_eq!(
            fmt_vm_gpu(&["0000:65:00.1".to_string(), "0000:65:00.2".to_string()]),
            "2 dev"
        );
    }

    #[test]
    fn fmt_vm_uptime_distingue_parado_de_a_correr() {
        assert_eq!(fmt_vm_uptime(None), "-");
        let five_min_ago = super::output::now_unix().saturating_sub(300);
        assert_eq!(fmt_vm_uptime(Some(five_min_ago)), "Up 5 minutes");
    }

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
    fn normalize_vm_spec_deixa_a_forma_plana_intacta() {
        // The historic flat shape must pass through byte-for-byte-equivalent
        // (no group keys present — the whole function is a no-op on it).
        let flat: serde_yaml::Value = serde_yaml::from_str(
            "disk: d\nvcpus: 4\nmemory: 4G\nnetwork: node1-net\nhostname: h\n",
        )
        .unwrap();
        let normalized = normalize_vm_spec(flat.clone());
        assert_eq!(flat, normalized);
    }

    #[test]
    fn normalize_vm_spec_hoisteia_todos_os_grupos_para_a_forma_plana() {
        let grouped: serde_yaml::Value = serde_yaml::from_str(
            "disk: k8s-golden\n\
             resources:\n\
             \x20 vcpus: 4\n\
             \x20 memory: 4G\n\
             \x20 hugepages: true\n\
             \x20 cpuAffinity: 8-15\n\
             network:\n\
             \x20 name: node1-net\n\
             \x20 mode: nat\n\
             \x20 bridge: br0\n\
             \x20 staticIp: 192.168.122.50\n\
             boot:\n\
             \x20 kernel: /boot/vmlinuz\n\
             \x20 cmdline: console=ttyS0\n\
             cloudInit:\n\
             \x20 hostname: node1\n\
             \x20 sshKeys: [ssh-ed25519 AAAA foo]\n\
             libvirt:\n\
             \x20 backend: libvirt\n\
             \x20 tpm: true\n\
             \x20 xmlOverlay: [\"<serial/>\"]\n\
             \x20 xml: null\n",
        )
        .unwrap();
        let spec: VmSpec = serde_yaml::from_value(normalize_vm_spec(grouped)).unwrap();
        assert_eq!(spec.disk, "k8s-golden");
        assert_eq!(spec.vcpus, 4);
        assert_eq!(spec.memory, "4G");
        assert!(spec.hugepages);
        assert_eq!(spec.cpu_affinity.as_deref(), Some("8-15"));
        assert_eq!(spec.network, "node1-net");
        assert_eq!(spec.net_mode.as_deref(), Some("nat"));
        assert_eq!(spec.bridge.as_deref(), Some("br0"));
        assert_eq!(spec.ip.as_deref(), Some("192.168.122.50"));
        assert_eq!(spec.kernel.as_deref(), Some("/boot/vmlinuz"));
        assert_eq!(spec.cmdline.as_deref(), Some("console=ttyS0"));
        assert_eq!(spec.hostname.as_deref(), Some("node1"));
        assert_eq!(spec.ssh_keys, vec!["ssh-ed25519 AAAA foo".to_string()]);
        assert_eq!(spec.backend.as_deref(), Some("libvirt"));
        assert!(spec.tpm);
        assert_eq!(spec.libvirt_xml_overlay, vec!["<serial/>".to_string()]);
    }

    /// O buraco que isto fecha: o hoist copia as sub-chaves que conhece e
    /// depois APAGA o grupo, por isso uma mal escrita desaparecia sem aviso —
    /// medido num container, onde `resources: {memoria: 128M}` deu um container
    /// sem limite de memória nenhum e exit 0.
    #[test]
    fn sub_chave_desconhecida_no_grupo_e_reportada() {
        use super::unknown_group_keys;
        let v: serde_yaml::Value = serde_yaml::from_str(
            "disk: d\nresources:\n  vcpus: 2\n  memoria: 1G\nnetwork:\n  name: n\n  modo: nat\n",
        )
        .unwrap();
        let mut got = unknown_group_keys(&v);
        got.sort();
        assert_eq!(got, vec!["network.modo", "resources.memoria"]);
    }

    /// A forma plana não tem grupos — e `network` plano é uma STRING, não um
    /// mapa: nada para reportar, ou o aviso disparava em cada manifesto antigo.
    #[test]
    fn forma_plana_nao_gera_avisos_de_grupo() {
        use super::unknown_group_keys;
        let v: serde_yaml::Value =
            serde_yaml::from_str("disk: d\nvcpus: 2\nnetwork: minha-rede\n").unwrap();
        assert!(unknown_group_keys(&v).is_empty());
    }

    #[test]
    fn normalize_vm_spec_forma_plana_explicita_ganha_ao_grupo() {
        // A field set BOTH at the flat top level and inside a group — the
        // explicit flat value wins (unambiguous precedence, not "whichever
        // the map happens to iterate last").
        let mixed: serde_yaml::Value =
            serde_yaml::from_str("disk: d\nvcpus: 8\nresources:\n  vcpus: 2\n  memory: 1G\n")
                .unwrap();
        let spec: VmSpec = serde_yaml::from_value(normalize_vm_spec(mixed)).unwrap();
        assert_eq!(
            spec.vcpus, 8,
            "o vcpus plano explícito devia ganhar ao do grupo"
        );
        assert_eq!(
            spec.memory, "1G",
            "sem colisão, o do grupo aplica-se na mesma"
        );
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
        assert!(ud.contains("package_update: false\n"));
        // Regression: a bare top-level `ssh_authorized_keys:` only reaches
        // cloud-init's DEFAULT user (`ubuntu`), never the `delonix` account the
        // golden image actually creates and that `cluster kubeadm`'s SSH login
        // (and the autologin config below) hardcode. Found live: `delonix
        // cluster kubeadm` consistently failed "SSH did not respond" against a
        // fully-booted, reachable VM, because the key landed on the wrong user.
        // Must be scoped under `users: - name: delonix`, alongside `- default`
        // so the `ubuntu` account still gets created too (unrelated code paths
        // may depend on it existing).
        assert!(ud.contains("users:\n  - default\n  - name: delonix\n"));
        assert!(ud.contains("ssh_authorized_keys:\n      - ssh-ed25519 AAAA foo\n"));
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
