//! `delonix image` — pull/ls/rm/export.

use std::path::PathBuf;

use clap::Subcommand;
use delonix_image::ImageStore;
use delonix_runtime_core::{Error, Result};
use serde::Deserialize;

use super::manifest::{self, ManifestDoc};
use super::util::{effective_command, open_stores, resolve_or_pull};

/// `spec` de `kind: Image` — ou `pull: <ref>`, ou `build: {...}` (mutuamente
/// exclusivos; erro claro se faltarem os dois).
#[derive(Debug, Deserialize)]
struct ImageSpec {
    pull: Option<String>,
    build: Option<BuildSpec>,
}

#[derive(Debug, Deserialize)]
struct BuildSpec {
    #[serde(default = "default_context")]
    context: PathBuf,
    file: Option<PathBuf>,
    tag: String,
}

fn default_context() -> PathBuf {
    PathBuf::from(".")
}

#[derive(Subcommand)]
pub enum ImageCmd {
    /// Puxa uma imagem de um registo.
    Pull {
        image: String,
        /// Verifica a assinatura cosign com esta chave pública (PEM) DEPOIS do
        /// pull, e falha se não bater. Sem isto, um pull não é autenticado além
        /// do digest do próprio registo.
        #[arg(long, value_name = "PEM")]
        verify: Option<PathBuf>,
    },
    /// Lista imagens locais.
    Ls,
    /// Detalhe legível de uma ou mais imagens, ao estilo `kubectl describe`
    /// (tags/digest/tamanho/layers + o config OCI: entrypoint/cmd/env/workdir).
    /// Com `--vm`, descreve imagens VM douradas.
    Describe {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Dá outro nome/tag a uma imagem local (não copia nada — é só um nome novo
    /// para o mesmo conteúdo).
    Tag { source: String, target: String },
    /// Layers de uma imagem (digest + tamanho), da base para o topo.
    History { image: String },
    /// Verifica a assinatura cosign de uma imagem local contra uma chave pública.
    Verify {
        image: String,
        /// Chave pública em PEM.
        #[arg(value_name = "PEM")]
        key: PathBuf,
    },
    /// Remove uma imagem local.
    Rm { image: String },
    /// Exporta um bundle OCI runtime (rootfs + config.json) para `runc`/`crun`.
    Export { image: String, dir: PathBuf },
    /// Aplica os documentos `kind: Image` de um manifesto (`pull` idempotente
    /// por referência; `build` reconstrói e substitui a tag a cada apply).
    Apply {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// Autentica num registo OCI (guarda as credenciais em `<root>/auth.json`,
    /// formato docker/podman). A password vem SEMPRE do stdin — nunca de um
    /// argumento (ficaria no histórico da shell e no /proc).
    Login {
        /// Registo (ex.: `ghcr.io`, `docker.io`).
        registry: String,
        #[arg(short = 'u', long = "username")]
        username: String,
        /// Lê a password/token do stdin (única forma suportada).
        #[arg(long = "password-stdin")]
        password_stdin: bool,
    },
    /// Remove as credenciais guardadas de um registo.
    Logout { registry: String },
    /// Imagens VM douradas (`<root>/vm-images/`): ls/pull/push/build.
    /// Equivalente a `image --vm <cmd>` (forma antiga, mantida).
    Vm {
        #[command(subcommand)]
        action: VmSub,
    },
    /// Publica uma imagem local num registo OCI. Sem `target`, publica sob a
    /// própria referência da imagem. Com `--vm`, o `target` é obrigatório.
    Push { name: String, target: Option<String> },
    /// (só com `--vm`) Constrói a imagem VM dourada (Ubuntu + kubeadm/kubelet/
    /// kubectl + `delonix-cri`).
    Build {
        #[arg(short = 't', long = "tag")]
        tag: String,
        #[arg(long, default_value = "26.04")]
        ubuntu_release: String,
        #[arg(long)]
        k8s_version: Option<String>,
        #[arg(long = "extra-package")]
        extra_packages: Vec<String>,
        #[arg(long = "extra-run")]
        extra_run: Vec<String>,
        #[arg(long)]
        cri_bin: Option<PathBuf>,
        /// Não comprimir o qcow2 final (maior, mas sem custo de descompressão
        /// nas leituras do backing file em runtime).
        #[arg(long)]
        no_compress: bool,
        /// Obter os .deb do k8s no HOST (verificados) e instalá-los com `dpkg` —
        /// o appliance corre sem rede. Dispensa DHCP/DNS no guest.
        #[arg(long)]
        offline: bool,
    },
}

/// Subcomandos de `image vm` — espelham 1:1 o `cmd::vmimage::VmImageCmd`.
#[derive(Subcommand)]
pub enum VmSub {
    /// Lista as imagens VM locais.
    Ls,
    /// Detalhe legível de uma ou mais imagens VM, ao estilo `kubectl describe`.
    Describe {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Obtém uma imagem VM de um registo OCI (artefacto de blob único).
    Pull {
        source: String,
        /// Nome local (default: derivado da referência).
        #[arg(long)]
        name: Option<String>,
    },
    /// Publica uma imagem VM local num registo OCI.
    Push { name: String, target: String },
    /// Constrói a imagem VM dourada (Ubuntu + kubeadm/kubelet/kubectl + `delonix-cri`).
    Build {
        #[arg(short = 't', long = "tag")]
        tag: String,
        #[arg(long, default_value = "26.04")]
        ubuntu_release: String,
        #[arg(long)]
        k8s_version: Option<String>,
        #[arg(long = "extra-package")]
        extra_packages: Vec<String>,
        #[arg(long = "extra-run")]
        extra_run: Vec<String>,
        #[arg(long)]
        cri_bin: Option<PathBuf>,
        /// Não comprimir o qcow2 final (maior, mas sem custo de descompressão
        /// nas leituras do backing file em runtime).
        #[arg(long)]
        no_compress: bool,
        /// Obter os .deb do k8s no HOST (verificados) e instalá-los com `dpkg` —
        /// o appliance corre sem rede. Dispensa DHCP/DNS no guest.
        #[arg(long)]
        offline: bool,
    },
}

/// `vm`: activa `--vm` no grupo `image` — despacha `ls`/`pull`/`push`/`build`
/// para `cmd::vmimage` (imagens VM douradas) em vez de `ImageStore` (imagens
/// de container). `rm`/`export`/`apply` não fazem sentido para imagens VM
/// nesta fase — erro claro em vez de um comportamento silenciosamente errado.
pub fn run(vm: bool, action: ImageCmd) -> Result<()> {
    // login/logout são agnósticos a container-vs-VM (mesmo auth.json).
    match &action {
        ImageCmd::Login { registry, username, password_stdin } => {
            return cmd_login(registry, username, *password_stdin);
        }
        ImageCmd::Logout { registry } => {
            delonix_image::auth::logout(&super::util::state_root(), registry)?;
            println!("credenciais de {registry} removidas");
            return Ok(());
        }
        _ => {}
    }
    if let ImageCmd::Vm { action } = action {
        use super::vmimage::{self, VmImageCmd};
        return vmimage::run(match action {
            VmSub::Ls => VmImageCmd::Ls,
            VmSub::Describe { names } => VmImageCmd::Describe { names },
            VmSub::Pull { source, name } => VmImageCmd::Pull { source, name },
            VmSub::Push { name, target } => VmImageCmd::Push { name, target },
            VmSub::Build { tag, ubuntu_release, k8s_version, extra_packages, extra_run, cri_bin, no_compress, offline } => {
                VmImageCmd::Build { tag, ubuntu_release, k8s_version, extra_packages, extra_run, cri_bin, no_compress, offline }
            }
        });
    }
    if vm {
        return run_vm(action);
    }
    let (images, _store) = open_stores()?;
    match action {
        ImageCmd::Pull { image, verify } => cmd_pull(&images, &image, verify.as_deref()),
        ImageCmd::Ls => cmd_ls(&images),
        ImageCmd::Describe { names } => cmd_describe(&images, &names),
        ImageCmd::Tag { source, target } => cmd_tag(&images, &source, &target),
        ImageCmd::History { image } => cmd_history(&images, &image),
        ImageCmd::Verify { image, key } => cmd_verify(&images, &image, &key),
        ImageCmd::Rm { image } => cmd_rm(&images, &image),
        ImageCmd::Export { image, dir } => cmd_export(&images, &image, &dir),
        ImageCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
        ImageCmd::Push { name, target } => cmd_push(&images, &name, target.as_deref()),
        ImageCmd::Build { .. } => Err(Error::Invalid(
            "`build` neste grupo é só para imagens VM — usa `delonix image --vm build`, ou `delonix build` para imagens de container".into(),
        )),
        ImageCmd::Login { .. } | ImageCmd::Logout { .. } | ImageCmd::Vm { .. } => unreachable!("tratados acima"),
    }
}

/// `image login` — lê a password do stdin (obrigatório: um argumento ficaria no
/// histórico da shell e visível em /proc) e delega no `delonix_image::auth`.
fn cmd_login(registry: &str, username: &str, password_stdin: bool) -> Result<()> {
    if !password_stdin {
        return Err(Error::Invalid(
            "usa --password-stdin (ex.: `gh auth token | delonix image login ghcr.io -u USER --password-stdin`)".into(),
        ));
    }
    let mut pw = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut pw)
        .map_err(|e| Error::Invalid(format!("a ler a password do stdin: {e}")))?;
    let pw = pw.trim();
    if pw.is_empty() {
        return Err(Error::Invalid("password vazia no stdin".into()));
    }
    delonix_image::auth::login(&super::util::state_root(), registry, username, pw)?;
    println!("login em {registry} guardado (auth.json)");
    Ok(())
}

fn run_vm(action: ImageCmd) -> Result<()> {
    use super::vmimage::{self, VmImageCmd};
    let mapped = match action {
        ImageCmd::Ls => VmImageCmd::Ls,
        ImageCmd::Describe { names } => VmImageCmd::Describe { names },
        ImageCmd::Pull { image, verify: _ } => VmImageCmd::Pull { source: image, name: None },
        ImageCmd::Push { name, target } => VmImageCmd::Push {
            name,
            // Uma imagem VM não tem repo_tags de onde inferir o destino.
            target: target.ok_or_else(|| Error::Invalid("`image --vm push <nome> <destino>`: o destino é obrigatório".into()))?,
        },
        ImageCmd::Build { tag, ubuntu_release, k8s_version, extra_packages, extra_run, cri_bin, no_compress, offline } => {
            VmImageCmd::Build { tag, ubuntu_release, k8s_version, extra_packages, extra_run, cri_bin, no_compress, offline }
        }
        ImageCmd::Tag { .. } | ImageCmd::History { .. } | ImageCmd::Verify { .. } => {
            return Err(Error::Invalid(
                "tag/history/verify são de imagens de container — não se aplicam a imagens VM (--vm)".into(),
            ))
        }
        ImageCmd::Rm { .. } | ImageCmd::Export { .. } | ImageCmd::Apply { .. } => {
            return Err(Error::Invalid(
                "comando não disponível para imagens VM (--vm) — usa ls/pull/push/build".into(),
            ))
        }
        ImageCmd::Login { .. } | ImageCmd::Logout { .. } | ImageCmd::Vm { .. } => unreachable!("tratados em run()"),
    };
    vmimage::run(mapped)
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let (images, _store) = open_stores()?;
    for doc in manifest::of_kind(docs, "Image") {
        let name = &doc.metadata.name;
        let spec: ImageSpec = manifest::spec_of(doc)?;
        match (spec.pull, spec.build) {
            (Some(reference), None) => {
                resolve_or_pull(&images, &reference)?;
                println!("image/{name}: garantida ({reference})");
            }
            (None, Some(b)) => {
                let file = b.file.unwrap_or_else(|| super::build::default_build_file(&b.context));
                let img = super::build::build_from_spec(&b.context, &file, &b.tag)?;
                println!("image/{name}: construída ({})", img.short_id());
            }
            (Some(_), Some(_)) => {
                return Err(Error::Invalid(format!("image/{name}: spec tem `pull` E `build` — só um dos dois")))
            }
            (None, None) => return Err(Error::Invalid(format!("image/{name}: spec sem `pull` nem `build`"))),
        }
    }
    Ok(())
}

fn cmd_pull(images: &ImageStore, reference: &str, verify: Option<&std::path::Path>) -> Result<()> {
    let img = delonix_image::pull_from_registry(images, reference)?;
    // Verifica DEPOIS do pull (a assinatura cosign vive num tag ao lado da
    // imagem no registo, logo é preciso tê-la cá). Se falhar, o comando falha —
    // a imagem fica local, mas quem pediu `--verify` sabe que não é de confiança.
    if let Some(key) = verify {
        let pem = std::fs::read_to_string(key)?;
        let digest = delonix_image::verify_signature(images, reference, &pem)?;
        println!("assinatura válida para {reference} ({digest})");
    }
    println!("{}", img.short_id());
    Ok(())
}

/// `image tag` — outro nome para o mesmo conteúdo (não copia layers).
fn cmd_tag(images: &ImageStore, source: &str, target: &str) -> Result<()> {
    images.tag(source, target)?;
    println!("{source} -> {target}");
    Ok(())
}

/// `image history` — os layers da imagem, da base para o topo.
///
/// O `#` é a posição no stack (0 = base), como no `docker history`. O tamanho é
/// o do blob COMPRIMIDO no CAS — ver a nota em `image_size`.
fn cmd_history(images: &ImageStore, image: &str) -> Result<()> {
    let img = images.resolve(image)?;
    let mut t = super::output::Table::new(&["#", "LAYER", "SIZE"]).right_align(2);
    for (i, dg) in img.layers.iter().enumerate() {
        let size = std::fs::metadata(images.cas().path(dg)).map(|m| m.len()).unwrap_or(0);
        t.row(vec![i.to_string(), super::output::truncate(dg, 23), super::output::fmt_size(size)]);
    }
    t.print();
    Ok(())
}

/// `image verify` — assinatura cosign contra uma chave pública.
fn cmd_verify(images: &ImageStore, image: &str, key: &std::path::Path) -> Result<()> {
    let pem = std::fs::read_to_string(key)?;
    let digest = delonix_image::verify_signature(images, image, &pem)?;
    println!("OK: assinatura válida para {image} ({digest})");
    Ok(())
}

/// `image push` — publica uma imagem de container num registo OCI.
fn cmd_push(images: &ImageStore, image: &str, destination: Option<&str>) -> Result<()> {
    // Sem destino, publica sob a própria referência (o caso comum: a imagem já
    // foi construída com a tag do registo de destino).
    let dest = destination.unwrap_or(image);
    let digest = delonix_image::push_to_registry(images, image, dest)?;
    println!("{dest}  {digest}");
    Ok(())
}

/// Tamanho de uma imagem = soma dos blobs dos seus layers no CAS.
///
/// **Não é o "SIZE" do `docker images`**, que é o rootfs DESCOMPACTADO; aqui é
/// o que a imagem ocupa mesmo no disco (layers comprimidos, partilhados entre
/// imagens que os reusem). É a única medida que se obtém sem descompactar tudo,
/// e é a que responde à pergunta que se faz a um `ls` ("quanto espaço isto
/// gasta?"). Um layer que falte no CAS não conta — daí `Option` só quando NADA
/// é legível, para não passar por "0 B" uma imagem cujos blobs desapareceram.
fn image_size(images: &ImageStore, img: &delonix_image::Image) -> Option<u64> {
    if img.layers.is_empty() {
        return None;
    }
    let mut total = 0u64;
    let mut seen_any = false;
    for l in &img.layers {
        if let Ok(m) = std::fs::metadata(images.cas().path(l)) {
            total += m.len();
            seen_any = true;
        }
    }
    seen_any.then_some(total)
}

fn cmd_ls(images: &ImageStore) -> Result<()> {
    let mut imgs = images.list()?;
    // O mais recente primeiro, como no `docker images`.
    imgs.sort_by(|a, b| b.created_unix.cmp(&a.created_unix));
    let mut t = super::output::Table::new(&["REPOSITORY:TAG", "IMAGE ID", "CREATED", "SIZE"]).right_align(3);
    for img in imgs {
        let tag = img.repo_tags.first().cloned().unwrap_or_else(|| "<none>".into());
        t.row(vec![
            // Truncado: uma referência com digest (`kindest/node:v1.34.0@sha256:7416a…`,
            // 84 chars) esticava a coluna e empurrava todas as outras para fora
            // do terminal — a tabela mede pelo conteúdo, logo UMA linha destas
            // estragava a leitura de todas as restantes.
            super::output::truncate(&tag, 44),
            img.short_id(),
            // Era o epoch cru (`CRIADA(unix)`) — ilegível numa tabela.
            super::output::fmt_age(img.created_unix),
            image_size(images, &img).map(super::output::fmt_size).unwrap_or_else(|| "-".into()),
        ]);
    }
    t.print();
    Ok(())
}

/// `image describe` — detalhe legível ao estilo `kubectl describe`.
fn cmd_describe(images: &ImageStore, names: &[String]) -> Result<()> {
    for (i, name) in names.iter().enumerate() {
        // `resolve` (não `resolve_or_pull`): descrever não é obter — um
        // `describe` de uma imagem que não existe deve dizer isso, não passar
        // minutos a puxar do registo por engano.
        let img = images.resolve(name)?;
        if i > 0 {
            println!();
        }
        describe_one(images, &img);
    }
    Ok(())
}

fn describe_one(images: &ImageStore, img: &delonix_image::Image) {
    let mut d = super::output::Describe::new();
    d.field("ID", &img.id);
    d.field("Short ID", img.short_id());
    d.list("Tags", &img.repo_tags);
    d.field("Created", super::output::fmt_local(img.created_unix));
    d.field("Age", super::output::fmt_age(img.created_unix));
    d.field(
        "Size",
        image_size(images, img).map(super::output::fmt_size).unwrap_or_else(|| "<unknown>".into()),
    );

    // Layers com o tamanho de cada blob — é o que mostra ONDE está o peso.
    if img.layers.is_empty() {
        d.field("Layers", "<none>");
    } else {
        d.section("Layers");
        for l in &img.layers {
            let sz = std::fs::metadata(images.cas().path(l))
                .map(|m| super::output::fmt_size(m.len()))
                .unwrap_or_else(|_| "<missing>".into());
            d.item(format!("{l}  {sz}"));
        }
    }

    let c = &img.config;
    d.section("Config");
    d.sub("Entrypoint", if c.entrypoint.is_empty() { "<none>".to_string() } else { c.entrypoint.join(" ") });
    d.sub("Cmd", if c.cmd.is_empty() { "<none>".to_string() } else { c.cmd.join(" ") });
    d.sub("Workdir", if c.working_dir.is_empty() { "/" } else { &c.working_dir });
    d.sub("User", if c.user.is_empty() { "root" } else { &c.user });
    // Extensões Delonix do Dockerfile/Delonixfile (`CPUS`/`MEMORY`/`SECURITY`/
    // `HEALTHCHECK`) — omitidas por inteiro nas imagens que não as tenham.
    d.sub_opt("CPUs", c.cpus.as_deref());
    d.sub_opt("Memory", c.memory.as_deref());
    d.sub_opt("Healthcheck", c.healthcheck.as_deref());
    if !c.security.is_empty() {
        d.sub("Security", c.security.join(", "));
    }
    d.list("Env", &c.env);
    d.print();
}

fn cmd_rm(images: &ImageStore, reference: &str) -> Result<()> {
    let removed = images.remove(reference)?;
    println!("{removed}");
    Ok(())
}

/// Escreve um bundle OCI runtime mínimo (rootfs + config.json) para `runc`/`crun`.
fn cmd_export(images: &ImageStore, reference: &str, dir: &std::path::Path) -> Result<()> {
    let img = resolve_or_pull(images, reference)?;
    std::fs::create_dir_all(dir).map_err(|e| Error::Invalid(format!("mkdir {}: {e}", dir.display())))?;
    let rootfs = dir.join("rootfs");
    images.export_rootfs(&img, &rootfs)?;
    let args = effective_command(&img, &[]);
    let args = if args.is_empty() { vec!["/bin/sh".to_string()] } else { args };
    let cwd = if img.config.working_dir.is_empty() {
        "/".to_string()
    } else {
        img.config.working_dir.clone()
    };
    let spec = serde_json::json!({
        "ociVersion": "1.0.2",
        "process": {
            "terminal": false,
            "user": { "uid": 0, "gid": 0 },
            "args": args,
            "env": img.config.env,
            "cwd": cwd,
            "capabilities": {
                "bounding": ["CAP_CHOWN","CAP_DAC_OVERRIDE","CAP_FOWNER","CAP_SETGID","CAP_SETUID","CAP_NET_BIND_SERVICE"]
            },
            "noNewPrivileges": true
        },
        "root": { "path": "rootfs", "readonly": false },
        "hostname": "delonix",
        "linux": {
            "namespaces": [
                {"type": "pid"}, {"type": "ipc"}, {"type": "uts"}, {"type": "mount"}
            ]
        }
    });
    let cfg = dir.join("config.json");
    std::fs::write(&cfg, serde_json::to_vec_pretty(&spec).unwrap_or_default())
        .map_err(|e| Error::Invalid(format!("escrever {}: {e}", cfg.display())))?;
    println!("bundle OCI em {}", dir.display());
    println!("corre com:  runc run -b {} delonix-oci", dir.display());
    Ok(())
}
