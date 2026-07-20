//! `delonix image --vm` — imagens VM douradas (Ubuntu + kubeadm/kubelet/
//! kubectl + `delonix-cri`), geridas à parte das imagens de container (essas
//! ficam em `cmd::image`/`ImageStore`). Um `.qcow2` solto por imagem (sem
//! CAS/layers — só há um blob por imagem, nada a deduplicar) + um `.json` de
//! metadados, ambos em `<root>/vm-images/`.
//!
//! `build` produz a imagem de raiz (download da cloud image Ubuntu + `virt-
//! customize`); `push`/`pull` publicam/obtêm-na de um registo OCI (artefacto
//! de blob único, ver `delonix_image::registry::{push_oci_artifact,
//! pull_oci_artifact}`) — o mesmo protocolo das imagens de container, só sem
//! o modelo de layers/config Docker.

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
        // `sanitize` (não aplicado aqui antes — achado de auditoria de segurança,
        // ver CLAUDE.md) elimina `/` de `ubuntu_release`, impedindo que
        // `--ubuntu-release '../../../etc/cron.d/x'` escreva fora de `_base/`.
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
    /// Lista as imagens VM locais.
    Ls,
    /// Detalhe legível de uma ou mais imagens VM, ao estilo `kubectl describe`.
    Describe { names: Vec<String> },
    /// Publica uma imagem VM local num registo OCI (artefacto de blob único).
    Push { name: String, target: String },
    /// Puxa uma imagem VM de um registo OCI.
    Pull {
        source: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// Constrói a imagem dourada: Ubuntu cloud image + kubeadm/kubelet/kubectl
    /// + `delonix-cri` (endpoint CRI para o kubelet), via `virt-customize`.
    Build {
        #[arg(short = 't', long = "tag")]
        tag: String,
        #[arg(long, default_value = "26.04")]
        ubuntu_release: String,
        /// Versão do Kubernetes (ex.: `1.31`) — omitir usa a última estável.
        #[arg(long)]
        k8s_version: Option<String>,
        /// Pacote apt adicional, repetível — extensibilidade sem tocar no código.
        #[arg(long = "extra-package")]
        extra_packages: Vec<String>,
        /// Comando adicional a correr dentro do guest durante o build, repetível.
        #[arg(long = "extra-run")]
        extra_run: Vec<String>,
        /// Caminho explícito do binário `delonix-cri` a instalar (senão:
        /// procura ao lado do `delonix` actual, depois tenta compilar do
        /// workspace se um `Cargo.toml` for detectado a partir do cwd).
        #[arg(long)]
        cri_bin: Option<PathBuf>,
        /// Não comprimir o qcow2 final (fica maior, mas sem custo de
        /// descompressão nas leituras do backing file em runtime).
        #[arg(long)]
        no_compress: bool,
        /// Obter os .deb do k8s no HOST (verificados: assinatura do InRelease +
        /// SHA256) e instalá-los com `dpkg` — o appliance corre sem rede
        /// (`--no-network`). Dispensa DHCP/DNS no guest, logo dispensa os
        /// workarounds de host (passt/dhclient) que o modo online exige.
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
        VmImageCmd::Pull { source, name } => cmd_pull(&store, &source, name),
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
    let mut t = output::Table::new(&["NAME", "UBUNTU", "K8S", "CREATED", "SIZE"]).right_align(4);
    for img in store.list()? {
        t.row(vec![
            img.name,
            img.ubuntu_release.as_deref().unwrap_or("-").to_string(),
            img.k8s_version.as_deref().unwrap_or("-").to_string(),
            fmt_local(img.created_unix),
            fmt_size(img.size),
        ]);
    }
    t.print();
    Ok(())
}

/// `image --vm describe` — detalhe legível ao estilo `kubectl describe`.
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
    // `pull` NÃO recupera estes metadados (o artefacto OCI só carrega o blob
    // qcow2) — numa imagem puxada ficam `None`. Ver o gap conhecido no CLAUDE.md.
    d.field(
        "Ubuntu",
        img.ubuntu_release.as_deref().unwrap_or("<unknown>"),
    );
    d.field("K8s", img.k8s_version.as_deref().unwrap_or("<unknown>"));
    let qcow2 = store.qcow2_path(&img.name);
    d.field("Path", qcow2.to_string_lossy());
    // O `size` acima é o do build/pull; este é o que ESTÁ em disco agora. Se
    // divergirem, o artefacto foi mexido por fora — vale a pena poder ver.
    d.field_opt(
        "On disk",
        std::fs::metadata(&qcow2).ok().map(|m| fmt_size(m.len())),
    );
    d.print();
}

/// A imagem VM dourada OFICIAL do Delonix (Ubuntu 24.04 + kubeadm/kubelet/
/// kubectl + delonix-cri como serviço systemd) — publicada e validada com
/// round-trip byte-idêntico; ver CLAUDE.md, secção "Imagem VM dourada".
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
    let data = delonix_image::registry::pull_oci_artifact(&state_root(), source)?;
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
    // `k8s_version` entra num `format!` que vira comando `virt-customize --run-command`
    // (via `k8s_recipes::k8s_host_recipes`) — validar aqui fecha o mesmo achado de
    // segurança de `cmd::cluster::valid_version` (o repositório apt embutido não pode
    // conter metacaracteres de shell). Achado de auditoria, ver CLAUDE.md.
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
        // Tudo o que precisa de rede acontece AQUI, no host (verificado), para o
        // appliance poder correr com `--no-network`.
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
        // Sem isto o libguestfs arranca o passt e o appliance espera por um lease
        // DHCP que nunca chega em hosts onde o passt está partido (ver CLAUDE.md).
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

    // Encolher o artefacto. Medido numa golden 24.04 (2.38 GiB → 677 MiB, −72%):
    //  1) `virt-sparsify --in-place` — zera os blocos já libertados (a limpeza do
    //     apt acima liberta ~367 MiB que, sem isto, continuam a ocupar no qcow2).
    //  2) `qemu-img convert -c` — a cloud image da Ubuntu VEM comprimida e o
    //     `convert` inicial (acima, sem `-c`) descomprime-a; sem este passo o
    //     artefacto final fica ~4x maior que a base. `zstd` em vez do zlib por
    //     omissão: comprime 5x mais rápido (10s vs 53s), fica menor, e sobretudo
    //     DESCOMPRIME muito mais rápido — importa porque esta imagem é usada como
    //     backing file read-only das VMs (`delonix_vm::create` faz um overlay por
    //     VM), logo cada leitura do SO base passa pelo descompressor.
    // Sparsify é best-effort: se falhar, seguimos (só perde-se algum tamanho).
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
    };
    store.save(&img)?;
    println!("{tag}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Download + verificação da cloud image Ubuntu
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
// Build OFFLINE: descarrega+verifica os .deb do k8s NO HOST
// ---------------------------------------------------------------------------
// Assim o `virt-customize` corre com `--no-network` e o appliance nunca precisa
// de DHCP/DNS — o que remove os workarounds de host (passt/dhclient) que o
// caminho online exige. A cadeia de confiança é a MESMA do apt, só que feita
// aqui em vez de dentro do guest:
//   InRelease (clearsigned, verificado com a Release.key do repo)
//     → SHA256 do `Packages`  → SHA256 de cada `.deb`
// Nunca se aceita um ficheiro sem o passo anterior o ter autenticado — o mesmo
// princípio do achado CRÍTICO nº3 da auditoria (`pull_oci_artifact` sem digest).

/// Um `.deb` do repo `pkgs.k8s.io`, já resolvido a partir de um `Packages` autenticado.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct K8sDeb {
    pub name: String,
    pub version: String,
    /// Caminho relativo à raiz do repo (campo `Filename`).
    pub filename: String,
    pub sha256: String,
}

/// Parseia um índice `Packages` (Debian control, blocos separados por linha
/// vazia) e devolve, por pacote de `wanted`, a MAIOR versão disponível para
/// `arch`. Função PURA (testável sem rede).
///
/// `version_prefix` (ex.: "1.34.") só se aplica aos pacotes de `versioned` — os
/// componentes que seguem a versão do Kubernetes (kubeadm/kubelet/kubectl). Os
/// restantes do repo têm versionamento PRÓPRIO (`kubernetes-cni` é 1.7.x,
/// `cri-tools` é 1.34.x mas independente) e levam só "a mais recente": filtrá-los
/// pelo prefixo do k8s não devolvia nada.
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
        // O prefixo do k8s só vale para quem segue a versão do k8s.
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

/// Compara duas versões Debian de forma suficiente para o repo k8s
/// (`1.34.9-1.1`): compara numericamente os campos separados por `.`/`-`.
/// Não é o algoritmo completo do dpkg — o repo só usa versões desta forma, e um
/// empate/formato inesperado degrada para comparação lexicográfica.
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

/// Extrai de um `Release` autenticado o SHA256 esperado de um ficheiro
/// (ex.: "Packages"). Os índices vêm na secção `SHA256:` como
/// `<sha>  <tamanho>  <caminho>`. Função PURA.
pub(crate) fn release_sha256_of(release: &str, want_path: &str) -> Option<String> {
    let mut in_sha = false;
    for line in release.lines() {
        if line.starts_with("SHA256:") {
            in_sha = true;
            continue;
        }
        // outra secção de topo (não indentada) termina o bloco SHA256.
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

/// Verifica o `InRelease` (clearsigned) com a `Release.key` do repo e devolve o
/// corpo JÁ AUTENTICADO. Usa `gpgv` com um keyring temporário — nunca toca no
/// keyring do utilizador. Falha fechado: sem assinatura válida, não há build.
fn verify_inrelease(work: &Path, repo_base: &str) -> Result<String> {
    let key_armored = http_get_text(&format!("{repo_base}/Release.key"))?;
    let key_asc = work.join("k8s-release.asc");
    let keyring = work.join("k8s-release.gpg");
    std::fs::write(&key_asc, &key_armored)?;
    // ASCII-armored → keyring binário que o gpgv entende.
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

/// Descarrega para `dest_dir` os `.deb` do k8s (fecho do repo: kubeadm/kubelet/
/// kubectl + `kubernetes-cni`), com a cadeia apt completa verificada no host.
/// Devolve os caminhos locais. `arch` é a arquitectura Debian (ex.: "amd64").
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

    // `Packages` autenticado pelo SHA256 que consta do InRelease assinado.
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

    // Fecho: os 3 pedidos + `kubernetes-cni` (dep do kubelet dentro do repo).
    // As restantes deps do kubelet (iptables/mount/util-linux/libc6) já vêm na
    // cloud image da Ubuntu — se alguma faltar, o `dpkg -i` falha ALTO no guest,
    // que é o que queremos (nunca instalar meio-instalado em silêncio).
    // `versioned` seguem a versão do k8s (`--k8s-version 1.34` → `1.34.*`);
    // `kubernetes-cni` tem versionamento próprio → só "a mais recente".
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
// Resolução do binário `delonix-cri` a instalar no guest
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
    // Ao lado do `delonix` actual (instalação normal, release).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("delonix-cri");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    // Conveniência de dev: workspace do código-fonte a partir do cwd.
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
// Passos de customização (função pura — testável sem VM/virt-customize real)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CustomizeOp {
    RunCommand(String),
    CopyIn(PathBuf, String),
    Password { user: String, password: String },
    RootPassword(String),
}

/// Constrói a lista de passos de customização a aplicar à imagem base — a
/// parte "100% parametrizada": `extra_packages`/`extra_run` estendem sem
/// tocar nesta função. Pura (sem I/O), testável isoladamente. As receitas
/// tecnicamente sensíveis (repo/pacotes/swap/módulos/sysctls) vêm de
/// `k8s_recipes::k8s_host_recipes` — o MESMO catálogo que `cmd::cluster`
/// usa via SSH, para a imagem dourada e um host preparado por `cluster
/// apply` ficarem exactamente iguais.
/// Como [`k8s_customization_steps`], mas SEM rede no guest: em vez do
/// repositório apt + `apt-get install`, injecta os `.deb` já descarregados e
/// verificados no HOST (`download_k8s_debs`) e instala-os com `dpkg -i`. As
/// restantes receitas (swap/módulos/sysctls) são as MESMAS do caminho online
/// (`k8s_recipes::k8s_config_recipes`) — não divergem.
///
/// `dpkg -i` em vez de `apt-get install ./*.deb`: o apt precisaria de contactar
/// as listas para resolver deps; as deps do kubelet fora do repo k8s
/// (iptables/mount/util-linux/libc6) já vêm na cloud image. Se alguma faltar, o
/// `dpkg` falha ALTO e o build pára — nunca deixa um guest meio-instalado.
pub(crate) fn k8s_customization_steps_offline(
    debs: &[PathBuf],
    extra_run: &[String],
    cri_bin: &Path,
    cri_service: &Path,
) -> Vec<CustomizeOp> {
    let mut ops: Vec<CustomizeOp> = Vec::new();
    // `--copy-in` exige que o directório-alvo JÁ exista no guest.
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

/// A cauda comum aos dois modos (online/offline): `delonix-cri` + contas +
/// `--extra-run` do utilizador + limpeza do apt. Partilhada para os dois
/// caminhos nunca divergirem no que produzem.
fn common_customization_steps(
    extra_run: &[String],
    cri_bin: &Path,
    cri_service: &Path,
) -> Vec<CustomizeOp> {
    let mut ops: Vec<CustomizeOp> = Vec::new();
    ops.extend([
        // `delonix-cri` — endpoint CRI para o kubelet (substitui containerd).
        CustomizeOp::CopyIn(cri_bin.to_path_buf(), "/usr/local/bin".to_string()),
        CustomizeOp::RunCommand("chmod +x /usr/local/bin/delonix-cri".into()),
        CustomizeOp::CopyIn(cri_service.to_path_buf(), "/etc/systemd/system".to_string()),
        CustomizeOp::RunCommand("systemctl enable delonix-cri.service".into()),
        // Conta padrão: root/delonix e delonix:delonix em sudoers (pedido explícito).
        CustomizeOp::RootPassword("delonix".to_string()),
        CustomizeOp::RunCommand("useradd -m -s /bin/bash -G sudo delonix || true".into()),
        CustomizeOp::Password { user: "delonix".to_string(), password: "delonix".to_string() },
        CustomizeOp::RunCommand(
            "echo 'delonix ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/90-delonix && chmod 440 /etc/sudoers.d/90-delonix"
                .into(),
        ),
    ]);
    ops.extend(extra_run.iter().cloned().map(CustomizeOp::RunCommand));
    // Limpeza do apt — SEMPRE no fim (depois do `--extra-run` do utilizador, que
    // pode instalar mais pacotes). Medido numa golden 24.04: `/var/cache/apt`
    // (~181 MiB de .deb já instalados) + `/var/lib/apt/lists` (~186 MiB de
    // índices) = ~367 MiB de puro lixo, que enchiam a raiz a 92%. Um `apt-get
    // update` regenera os índices se o nó precisar.
    //
    // DELIBERADAMENTE aqui e não em `k8s_recipes`: aquele catálogo é PARTILHADO
    // com `cluster apply`, que prepara hosts VIVOS — limpar a cache apt é uma
    // preocupação do ARTEFACTO (encolher uma imagem distribuível), não da
    // preparação de um host.
    ops.push(CustomizeOp::RunCommand(
        "apt-get clean && rm -rf /var/lib/apt/lists/*".into(),
    ));
    ops
}

/// Traduz os `CustomizeOp` para os argumentos reais do `virt-customize`.
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
        // 1784216635 → uma data/hora local; validamos a FORMA (o fuso é do host).
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
        // `--extra-run` corre depois de todos os passos base; só a limpeza do apt
        // vem a seguir (tem de ser a última — o extra-run pode instalar pacotes).
        let idx_extra = ops
            .iter()
            .position(|op| matches!(op, CustomizeOp::RunCommand(c) if c == "echo oi"))
            .expect("o --extra-run devia estar na lista");
        assert_eq!(
            idx_extra,
            ops.len() - 2,
            "o --extra-run devia vir logo antes da limpeza"
        );
        assert!(
            matches!(ops.last(), Some(CustomizeOp::RunCommand(c)) if c.contains("apt-get clean"))
        );
    }

    /// Um `Packages` reduzido, com a mesma forma do real (várias arquitecturas e
    /// versões por pacote) — inclui o caso que partiu o 1.º build offline.
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
        // REGRESSÃO: o `kubernetes-cni` é 1.7.x — filtrá-lo por "1.34." não
        // devolvia nada e o build offline abortava com "não tem kubernetes-cni".
        let got = parse_packages_index(
            PACKAGES_FIXTURE,
            "amd64",
            "1.34.",
            &["kubeadm", "kubernetes-cni"],
            &["kubeadm"], // só o kubeadm segue a versão do k8s
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
        // A garantia central do modo offline: nada contacta a rede no guest.
        for c in &cmds {
            assert!(
                !c.contains("curl") && !c.contains("apt-get update") && !c.contains("https://"),
                "passo offline com rede: {c}"
            );
        }
        // E o .deb é injectado.
        assert!(ops
            .iter()
            .any(|o| matches!(o, CustomizeOp::CopyIn(_, d) if d == "/tmp/k8s-debs")));
    }

    #[test]
    fn customization_steps_limpam_a_cache_apt_no_fim() {
        let cri = PathBuf::from("/tmp/delonix-cri");
        let svc = PathBuf::from("/tmp/delonix-cri.service");
        let ops = k8s_customization_steps(None, &[], &[], &cri, &svc);
        // ~367 MiB de .deb + índices que, sem isto, enchiam a raiz da golden a 92%.
        let last = ops.last().expect("devia haver passos");
        assert!(
            matches!(last, CustomizeOp::RunCommand(c) if c.contains("apt-get clean") && c.contains("/var/lib/apt/lists")),
            "o último passo devia limpar a cache apt, obtido: {last:?}"
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
