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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

    pub fn base_cache_path(&self, ubuntu_release: &str) -> PathBuf {
        // `sanitize` (not applied here before — security-audit finding, see
        // CLAUDE.md) strips `/` from `ubuntu_release`, preventing
        // `--ubuntu-release '../../../etc/cron.d/x'` from writing outside `_base/`.
        self.root.join("_base").join(format!(
            "ubuntu-{}-server-cloudimg-amd64.img",
            Self::sanitize(ubuntu_release)
        ))
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

    pub fn get(&self, name: &str) -> Result<VmImage> {
        let bytes = std::fs::read(self.meta_path(name))
            .map_err(|_| Error::NotFound(format!("imagem VM '{name}'")))?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

#[derive(Subcommand)]
pub enum VmImageCmd {
    /// List the local VM images.
    Ls,
    /// Human-readable detail of one or more VM images, `kubectl describe` style.
    Describe { names: Vec<String> },
    /// Publish a local VM image to an OCI registry (single-blob artifact).
    Push { name: String, target: String },
    /// Pull a VM image from an OCI registry — with no argument, the OFFICIAL
    /// Delonix image (ready for `vm create`/`cluster kubeadm`).
    Pull {
        source: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    /// Build the golden image: Ubuntu cloud image + kubeadm/kubelet/kubectl
    /// + `delonix-cri` (CRI endpoint for the kubelet), via `virt-customize`.
    Build {
        #[arg(short = 't', long = "tag")]
        tag: String,
        #[arg(long, default_value = "26.04")]
        ubuntu_release: String,
        /// Kubernetes version (e.g. `1.31`) — omit to use the latest stable.
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
        #[arg(long)]
        cri_bin: Option<PathBuf>,
        /// Do not compress the final qcow2 (larger, but no decompression cost
        /// on backing-file reads at runtime).
        #[arg(long)]
        no_compress: bool,
        /// Fetch the k8s .deb files on the HOST (verified: InRelease signature +
        /// SHA256) and install them with `dpkg` — the appliance runs without
        /// network (`--no-network`). Dispenses with DHCP/DNS in the guest, so it
        /// dispenses with the host workarounds (passt/dhclient) the online mode requires.
        #[arg(long)]
        offline: bool,
    },
}

pub fn run(action: VmImageCmd) -> Result<()> {
    let store = VmImageStore::open(state_root())?;
    match action {
        VmImageCmd::Ls => cmd_ls(&store),
        VmImageCmd::Describe { names } => cmd_describe(&store, &names),
        VmImageCmd::Push { name, target } => cmd_push(&store, &name, &target),
        VmImageCmd::Pull { source, name } => {
            // BUG FIXED HERE, found live: this is the shared engine command
            // behind BOTH `image --vm pull` AND `image vm pull` — it never
            // got the "no argument = official image" default that `delonix
            // vm pull` (a separate, sibling CLI definition in `cmd/vm.rs`)
            // already has, despite this exact struct's own doc comment
            // claiming it. A user on a real host hit this: `delonix image vm
            // pull --name delonix-vm-k8s:1.34` (no source) errored "required
            // arguments were not provided: <SOURCE>".
            let src = source.unwrap_or_else(|| OFFICIAL_VM_IMAGE.to_string());
            cmd_pull(&store, &src, name)
        }
        VmImageCmd::Build {
            tag,
            ubuntu_release,
            k8s_version,
            extra_packages,
            extra_run,
            cri_bin,
            no_compress,
            offline,
        } => cmd_build(
            &store,
            &tag,
            &ubuntu_release,
            k8s_version,
            extra_packages,
            extra_run,
            cri_bin,
            !no_compress,
            offline,
        ),
    }
}

fn cmd_ls(store: &VmImageStore) -> Result<()> {
    let mut t =
        output::Table::new(&["NAME", "DISTRO", "KERNEL", "K8S", "CREATED", "SIZE"]).right_align(5);
    for img in store.list()? {
        t.row(vec![
            img.name,
            img.ubuntu_release.as_deref().unwrap_or("-").to_string(),
            img.kernel_version.as_deref().unwrap_or("-").to_string(),
            img.k8s_version.as_deref().unwrap_or("-").to_string(),
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
    // qcow2 blob) — on a pulled image they stay `None`. See the known gap in CLAUDE.md.
    d.field(
        "Distro",
        img.ubuntu_release.as_deref().unwrap_or("<unknown>"),
    );
    d.field(
        "Kernel",
        img.kernel_version.as_deref().unwrap_or("<unknown>"),
    );
    d.field("K8s", img.k8s_version.as_deref().unwrap_or("<unknown>"));
    let qcow2 = store.qcow2_path(&img.name);
    d.field("Path", qcow2.to_string_lossy());
    // The `size` above is the build/pull one; this is what IS on disk now. If
    // they diverge, the artifact was tampered with out-of-band — worth being able to see.
    d.field_opt(
        "On disk",
        std::fs::metadata(&qcow2).ok().map(|m| fmt_size(m.len())),
    );
    d.print();
}

/// Delonix's OFFICIAL golden VM image (Ubuntu 24.04 + kubeadm/kubelet/
/// kubectl + delonix-cri as a systemd service) — published and validated with
/// a byte-identical round-trip; see CLAUDE.md, section "Golden VM image".
pub(crate) const OFFICIAL_VM_IMAGE: &str = "ghcr.io/angolardevops/delonix-vm-k8s:1.34";

pub(crate) fn cmd_push(store: &VmImageStore, name: &str, target: &str) -> Result<()> {
    let img = store.get(name)?;
    let data = std::fs::read(store.qcow2_path(name)).map_err(|e| {
        Error::Invalid(format!(
            "{} '{name}': {e}",
            super::po::t("could not read the qcow2 of")
        ))
    })?;
    let digest = delonix_image::registry::push_oci_artifact(
        &state_root(),
        target,
        VM_IMAGE_MEDIA_TYPE,
        &data,
    )?;
    println!("{digest}");
    let _ = img;
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
    let data = delonix_image::registry::pull_oci_artifact_with_progress(
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
    };
    store.save(&img)?;
    println!("{name}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_build(
    store: &VmImageStore,
    tag: &str,
    ubuntu_release: &str,
    k8s_version: Option<String>,
    extra_packages: Vec<String>,
    extra_run: Vec<String>,
    cri_bin: Option<PathBuf>,
    compress: bool,
    offline: bool,
) -> Result<()> {
    // `k8s_version` goes into a `format!` that becomes a `virt-customize --run-command`
    // (via `k8s_recipes::k8s_host_recipes`) — validating here closes the same security
    // finding as `cmd::cluster::valid_version` (the embedded apt repository must not
    // contain shell metacharacters). Audit finding, see CLAUDE.md.
    if let Some(v) = &k8s_version {
        if !super::cluster::valid_version(v) {
            return Err(Error::Invalid(format!(
                "--k8s-version '{v}' inválido (só dígitos e pontos, ex.: '1.31')"
            )));
        }
    }
    let base = download_ubuntu_base(store, ubuntu_release)?;
    let cri = resolve_cri_bin(cri_bin)?;

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

    let service_unit = workspace_dist_file("delonix-cri.service")?;
    let ops = if offline {
        // Everything that needs network happens HERE, on the host (verified), so the
        // appliance can run with `--no-network`.
        eprintln!("modo offline: a obter os .deb do k8s no host...");
        let debs = download_k8s_debs(
            &work_dir,
            &work_dir.join("debs"),
            k8s_version.as_deref(),
            "amd64",
            &extra_packages,
        )?;
        k8s_customization_steps_offline(&debs, &extra_run, &cri, &service_unit)
    } else {
        k8s_customization_steps(
            k8s_version.as_deref(),
            &extra_packages,
            &extra_run,
            &cri,
            &service_unit,
        )
    };
    let mut args = customize_args(&work_qcow2, &ops);
    if offline {
        // Without this, libguestfs starts passt and the appliance waits for a DHCP
        // lease that never arrives on hosts where passt is broken (see CLAUDE.md).
        args.insert(0, "--no-network".to_string());
    }

    eprintln!(
        "a correr virt-customize ({} passos{})...",
        ops.len(),
        if offline { ", sem rede" } else { "" }
    );
    run_tool(
        "virt-customize",
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
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
        ubuntu_release: Some(ubuntu_release.to_string()),
        k8s_version,
        created_unix: now_unix(),
        kernel_version,
    };
    store.save(&img)?;
    println!("{tag}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Download + verification of the Ubuntu cloud image
// ---------------------------------------------------------------------------

fn download_ubuntu_base(store: &VmImageStore, release: &str) -> Result<PathBuf> {
    let cached = store.base_cache_path(release);
    if cached.exists() {
        return Ok(cached);
    }
    let base_url = format!("https://cloud-images.ubuntu.com/releases/{release}/release");
    let img_name = format!("ubuntu-{release}-server-cloudimg-amd64.img");
    let img_url = format!("{base_url}/{img_name}");
    let sums_url = format!("{base_url}/SHA256SUMS");

    eprintln!("a descarregar {img_url}...");
    let tmp = cached.with_extension("download");
    stream_download(&img_url, &tmp)?;

    eprintln!("a verificar SHA256SUMS...");
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
        return Err(Error::Invalid(format!(
            "checksum inválido para {img_name}: esperado {expected}, obtido {got} — download descartado"
        )));
    }
    std::fs::rename(&tmp, &cached)?;
    Ok(cached)
}

fn stream_download(url: &str, dest: &Path) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(std::time::Duration::from_secs(3600))
        .build()
        .map_err(|e| Error::Invalid(format!("cliente HTTP: {e}")))?;
    let mut resp = client
        .get(url)
        .send()
        .map_err(|e| Error::Invalid(format!("GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Invalid(format!("GET {url}: HTTP {}", resp.status())));
    }
    let mut file = std::fs::File::create(dest)?;
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = resp
            .read(&mut buf)
            .map_err(|e| Error::Invalid(format!("a ler resposta: {e}")))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
    }
    Ok(())
}

fn http_get_text(url: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("delonix/0.1")
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| Error::Invalid(format!("cliente HTTP: {e}")))?;
    let resp = client
        .get(url)
        .send()
        .map_err(|e| Error::Invalid(format!("GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Invalid(format!("GET {url}: HTTP {}", resp.status())));
    }
    resp.text()
        .map_err(|e| Error::Invalid(format!("corpo de {url}: {e}")))
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
            "assinatura do InRelease do repo k8s NÃO confere com a Release.key — a abortar \
             (possível repo comprometido ou MITM)"
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

    eprintln!("a verificar a assinatura do repo k8s ({repo})...");
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
        return Err(Error::Invalid(format!(
            "SHA256 do índice Packages não confere (esperado {}, obtido {}) — a abortar",
            &want_sha[..16.min(want_sha.len())],
            &got[..16.min(got.len())]
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
            return Err(Error::Invalid(format!(
                "o repo k8s ({repo}) não tem '{base}' para {arch} — versão inexistente?"
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
            return Err(Error::Invalid(format!(
                "SHA256 de {file_name} não confere (esperado {}, obtido {}) — a abortar",
                &d.sha256[..16.min(d.sha256.len())],
                &got[..16.min(got.len())]
            )));
        }
        out.push(dest);
    }
    Ok(out)
}

fn hex_sha256(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex(&h.finalize())
}

fn hex_sha256_file(path: &Path) -> Result<String> {
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_unix() -> u64 {
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
            return Err(Error::Invalid(format!(
                "--cri-bin '{}' não existe",
                p.display()
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
            "a compilar delonix-cri (release) a partir de {}...",
            workspace_root.display()
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
            .map_err(|e| Error::Invalid(format!("a correr cargo build: {e}")))?;
        if !status.success() {
            return Err(Error::Invalid("cargo build do delonix-cri falhou".into()));
        }
        let built = workspace_root.join("target/release/delonix-cri");
        if built.exists() {
            return Ok(built);
        }
    }
    Err(Error::Invalid(
        "não encontrei o binário delonix-cri: usa --cri-bin <caminho>, instala-o ao lado do \
         delonix, ou corre a partir do checkout do código-fonte"
            .into(),
    ))
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

pub(crate) fn workspace_dist_file(name: &str) -> Result<PathBuf> {
    if let Some(root) = find_workspace_root() {
        let p = root.join("dist").join(name);
        if p.exists() {
            return Ok(p);
        }
    }
    Err(Error::Invalid(format!(
        "não encontrei dist/{name} — corre a partir do checkout do código-fonte ou fornece via --extra-run"
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
pub(crate) fn k8s_customization_steps_offline(
    debs: &[PathBuf],
    extra_run: &[String],
    cri_bin: &Path,
    cri_service: &Path,
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
    ops.extend(common_customization_steps(extra_run, cri_bin, cri_service));
    ops
}

pub(crate) fn k8s_customization_steps(
    k8s_version: Option<&str>,
    extra_packages: &[String],
    extra_run: &[String],
    cri_bin: &Path,
    cri_service: &Path,
) -> Vec<CustomizeOp> {
    let mut ops: Vec<CustomizeOp> =
        super::k8s_recipes::k8s_host_recipes(k8s_version, extra_packages)
            .into_iter()
            .map(|r| CustomizeOp::RunCommand(r.apply_offline().to_string()))
            .collect();
    ops.extend(common_customization_steps(extra_run, cri_bin, cri_service));
    ops
}

/// The tail common to both modes (online/offline): `delonix-cri` + accounts +
/// the user's `--extra-run` + apt cleanup. Shared so the two paths
/// never diverge in what they produce.
fn common_customization_steps(
    extra_run: &[String],
    cri_bin: &Path,
    cri_service: &Path,
) -> Vec<CustomizeOp> {
    let mut ops: Vec<CustomizeOp> = Vec::new();
    ops.extend([
        // `delonix-cri` — CRI endpoint for the kubelet (replaces containerd).
        CustomizeOp::CopyIn(cri_bin.to_path_buf(), "/usr/local/bin".to_string()),
        CustomizeOp::RunCommand("chmod +x /usr/local/bin/delonix-cri".into()),
        CustomizeOp::CopyIn(cri_service.to_path_buf(), "/etc/systemd/system".to_string()),
        CustomizeOp::RunCommand("systemctl enable delonix-cri.service".into()),
        // Default account: root/delonix and delonix:delonix in sudoers (explicit request).
        CustomizeOp::RootPassword("delonix".to_string()),
        CustomizeOp::RunCommand("useradd -m -s /bin/bash -G sudo delonix || true".into()),
        CustomizeOp::Password { user: "delonix".to_string(), password: "delonix".to_string() },
        CustomizeOp::RunCommand(
            "echo 'delonix ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/90-delonix && chmod 440 /etc/sudoers.d/90-delonix"
                .into(),
        ),
        // Shell UX the Kubernetes docs recommend: kubectl/kubeadm bash completion
        // + the `k` alias (with completion wired to it). Written to
        // `/etc/bash.bashrc` (Ubuntu sources it for every interactive bash — login
        // AND non-login — so both the serial console and SSH get it), NOT to
        // `/etc/profile.d` (those are sourced by `sh` too, which chokes on the
        // `<(...)` process substitution). Each block is guarded by `command -v`
        // so it is inert if a tool is missing, and evaluated at shell-start (not
        // build time) — order relative to the package install does not matter.
        CustomizeOp::RunCommand(
            "cat >> /etc/bash.bashrc <<'DELONIX_KUBECTL_EOF'\n\
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
                .into(),
        ),
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
    // apt cleanup — ALWAYS at the end (after the user's `--extra-run`, which
    // may install more packages). Measured on a 24.04 golden: `/var/cache/apt`
    // (~181 MiB of already-installed .deb) + `/var/lib/apt/lists` (~186 MiB of
    // indexes) = ~367 MiB of pure garbage, which filled the root to 92%. An `apt-get
    // update` regenerates the indexes if the node needs them.
    //
    // DELIBERATELY here and not in `k8s_recipes`: that catalog is SHARED
    // with `cluster apply`, which prepares LIVE hosts — cleaning the apt cache is a
    // concern of the ARTIFACT (shrinking a distributable image), not of
    // host preparation.
    ops.push(CustomizeOp::RunCommand(
        "apt-get clean && rm -rf /var/lib/apt/lists/*".into(),
    ));
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
    ops.push(CustomizeOp::RunCommand(
        "truncate -s 0 /etc/machine-id && rm -f /var/lib/dbus/machine-id && ln -sf /etc/machine-id /var/lib/dbus/machine-id"
            .into(),
    ));
    ops
}

/// Translates the `CustomizeOp`s into the actual `virt-customize` arguments.
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
    args
}

fn run_tool(bin: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(bin)
        .args(args)
        .status()
        .map_err(|e| Error::Invalid(format!("a correr {bin}: {e}")))?;
    if !status.success() {
        return Err(Error::Invalid(format!(
            "{bin} falhou (exit {:?})",
            status.code()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn customization_steps_incluem_pacotes_extra() {
        let cri = PathBuf::from("/tmp/delonix-cri");
        let svc = PathBuf::from("/tmp/delonix-cri.service");
        let ops = k8s_customization_steps(None, &["htop".to_string()], &[], &cri, &svc);
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
        let svc = PathBuf::from("/tmp/delonix-cri.service");
        let ops = k8s_customization_steps(None, &[], &["echo oi".to_string()], &cri, &svc);
        // `--extra-run` runs after all base steps; only the apt cleanup
        // comes after it (it must be last — the extra-run may install packages).
        let idx_extra = ops
            .iter()
            .position(|op| matches!(op, CustomizeOp::RunCommand(c) if c == "echo oi"))
            .expect("o --extra-run devia estar na lista");
        assert_eq!(
            idx_extra,
            ops.len() - 4,
            "o --extra-run devia vir logo antes da leitura do kernel + limpeza"
        );
        assert!(
            matches!(&ops[ops.len() - 3], CustomizeOp::RunCommand(c) if c.contains("/etc/delonix-kernel-version"))
        );
        assert!(
            matches!(&ops[ops.len() - 2], CustomizeOp::RunCommand(c) if c.contains("apt-get clean"))
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
        let svc = PathBuf::from("/tmp/delonix-cri.service");
        // Both build paths (online + offline) share `common_customization_steps`,
        // so the kubectl UX must be present in both.
        for ops in [
            k8s_customization_steps(None, &[], &[], &cri, &svc),
            k8s_customization_steps_offline(
                &[PathBuf::from("/tmp/x/kubeadm_1.34.9-1.1_amd64.deb")],
                &[],
                &cri,
                &svc,
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

    #[test]
    fn steps_offline_instalam_por_dpkg_e_nao_tocam_a_rede() {
        let debs = vec![PathBuf::from("/tmp/x/kubeadm_1.34.9-1.1_amd64.deb")];
        let ops = k8s_customization_steps_offline(
            &debs,
            &[],
            &PathBuf::from("/tmp/delonix-cri"),
            &PathBuf::from("/tmp/delonix-cri.service"),
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
    fn customization_steps_limpam_a_cache_apt_no_fim() {
        let cri = PathBuf::from("/tmp/delonix-cri");
        let svc = PathBuf::from("/tmp/delonix-cri.service");
        let ops = k8s_customization_steps(None, &[], &[], &cri, &svc);
        // ~367 MiB of .deb + indexes that, without this, filled the golden's root to 92%.
        // Second-to-last: the machine-id reset (below) must run AFTER it.
        let clean = &ops[ops.len() - 2];
        assert!(
            matches!(clean, CustomizeOp::RunCommand(c) if c.contains("apt-get clean") && c.contains("/var/lib/apt/lists")),
            "o penúltimo passo devia limpar a cache apt, obtido: {clean:?}"
        );
    }

    #[test]
    fn customization_steps_configuram_delonix_user_e_root_password() {
        let cri = PathBuf::from("/tmp/delonix-cri");
        let svc = PathBuf::from("/tmp/delonix-cri.service");
        let ops = k8s_customization_steps(None, &[], &[], &cri, &svc);
        assert!(ops
            .iter()
            .any(|op| matches!(op, CustomizeOp::RootPassword(p) if p == "delonix")));
        assert!(ops.iter().any(|op| matches!(op, CustomizeOp::Password{user,password} if user=="delonix" && password=="delonix")));
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
}
