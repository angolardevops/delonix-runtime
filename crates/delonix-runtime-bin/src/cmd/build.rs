//! `delonix build` — constrói uma imagem a partir de um Dockerfile.
//!
//! Único grupo com orquestração nova (as outras 5 são "ligar os fios" a APIs já
//! prontas nas crates do workspace): sobe um container "de trabalho" (placeholder
//! `sleep infinity`, mantém as namespaces vivas), corre cada `RUN` nele via
//! `runtime::exec`, aplica cada `COPY` escrevendo directamente no rootfs em
//! disco, e no fim empacota o resultado com `ImageStore::commit_flat_rootfs`
//! (rootless) ou `commit_upper`+`build_image` (root/overlay) — as mesmas duas
//! funções de "docker commit" que já existem em `delonix-image::build`.
//!
//! **Só single-stage nesta versão**: um Dockerfile com `FROM ... AS <nome>`
//! seguido doutro `FROM` (multi-stage) é recusado com um erro claro — falta
//! desenhar com cuidado a passagem de rootfs entre estágios (`COPY --from`);
//! fica para uma iteração seguinte.

use std::path::{Path, PathBuf};

use clap::Args;
use delonix_image::build::{parse_dockerfile, Step};
use delonix_image::Image;
use delonix_runtime::{self as runtime, RunSpec};
use delonix_runtime_core::{generate_id, Container, Error, Result};

use super::util::{open_stores, prepare_rootfs, resolve_or_pull};

#[derive(Args)]
pub struct BuildArgs {
    /// Contexto de build (default: `.`) — raiz para `COPY`.
    #[arg(default_value = ".")]
    context: PathBuf,
    /// Caminho do Dockerfile (default: `<contexto>/Dockerfile`).
    #[arg(short = 'f', long = "file")]
    file: Option<PathBuf>,
    /// Tag da imagem resultante (`repo:tag`).
    #[arg(short = 't', long = "tag")]
    tag: String,
}

/// Resolve o ficheiro de build por omissão: `Delonixfile` se existir no
/// contexto, senão `Dockerfile` (mesma gramática — `parse_dockerfile` já
/// suporta as extensões Delonix `SCAN`/`CPUS`/`MEMORY`/`SECURITY`/
/// `HEALTHCHECK` independentemente do nome do ficheiro; `Delonixfile` é só o
/// nome canónico, descoberto por omissão, para quem não quer reaproveitar o
/// nome `Dockerfile`).
pub(crate) fn default_build_file(context: &Path) -> PathBuf {
    let delonixfile = context.join("Delonixfile");
    if delonixfile.exists() {
        delonixfile
    } else {
        context.join("Dockerfile")
    }
}

pub fn run(args: BuildArgs) -> Result<()> {
    let file = args.file.clone().unwrap_or_else(|| default_build_file(&args.context));
    let img = build_from_spec(&args.context, &file, &args.tag)?;
    println!("{}", img.short_id());
    Ok(())
}

/// A orquestração completa de um build (parse → container de trabalho → RUN/
/// COPY → commit). Extraída de `run()` para ser reutilizada por `delonix
/// image apply` (`kind: Image`, `spec.build`) sem duplicar lógica.
pub fn build_from_spec(context: &Path, dockerfile_path: &Path, tag: &str) -> Result<Image> {
    let (images, store) = open_stores()?;
    let text = std::fs::read_to_string(dockerfile_path)
        .map_err(|e| Error::Invalid(format!("não consegui ler {}: {e}", dockerfile_path.display())))?;
    let df = parse_dockerfile(&text)?;
    if !df.stages.is_empty() {
        return Err(Error::Invalid(
            "build multi-stage (FROM ... AS <nome> seguido doutro FROM) ainda não é suportado \
             por `delonix build` — só single-stage nesta versão"
                .into(),
        ));
    }

    let base = resolve_or_pull(&images, &df.from)?;
    let id = generate_id();
    let rootless = runtime::is_rootless();
    let rootfs = prepare_rootfs(&images, &base, &id)?;

    // Container "de trabalho": `sleep infinity` mantém as namespaces vivas para
    // podermos `exec` cada RUN; COPY escreve diretamente no rootfs em disco.
    let mut c = Container::new(
        id.clone(),
        format!("dlx-build-{}", &id[..8.min(id.len())]),
        df.from.clone(),
        vec!["/bin/sh".into(), "-c".into(), "sleep infinity".into()],
        "max".into(),
    );
    c.userns = rootless;
    let spec = RunSpec { detach: true, userns: rootless, ..Default::default() };
    runtime::create_with(&store, &mut c, &rootfs, &spec)?;

    let steps_result = run_steps(&df.steps, &rootfs, context, &c, &base);

    let commit_result = steps_result.and_then(|()| {
        let _ = runtime::stop(&store, &mut c, 5);
        if rootless {
            let cmd = if df.cmd.is_empty() { base.config.cmd.clone() } else { df.cmd.clone() };
            let mut env = base.config.env.clone();
            env.extend(df.env.iter().cloned());
            let workdir = df.workdir.clone().unwrap_or_else(|| base.config.working_dir.clone());
            images.commit_flat_rootfs(Path::new(&rootfs), cmd, env, workdir, tag)
        } else {
            let layer = images.commit_upper(&c.id)?;
            images.build_image(&base, layer, &df, tag)
        }
    });

    // Limpeza best-effort do container de trabalho — nunca esconde o erro do
    // build/commit (o `?` abaixo, sobre `commit_result`, é o que decide o exit code).
    let _ = runtime::remove(&store, &c, true);
    let _ = images.unmount_rootfs(&c.id);

    commit_result
}

/// Corre os passos do Dockerfile por ordem, mantendo um acumulador local de
/// ENV/WORKDIR (o `runtime::exec` não tem noção de ambiente por-chamada — cada
/// `RUN` sintetiza `cd <workdir> && export ... ; <cmd>` num `/bin/sh -c`, tal
/// como o shell-form do Docker já implica).
fn run_steps(steps: &[Step], rootfs: &str, context: &Path, c: &Container, base: &Image) -> Result<()> {
    let mut cur_env: Vec<String> = base.config.env.clone();
    let mut cur_workdir = if base.config.working_dir.is_empty() {
        "/".to_string()
    } else {
        base.config.working_dir.clone()
    };
    for step in steps {
        match step {
            Step::Env { key, val } => {
                let prefix = format!("{key}=");
                cur_env.retain(|kv| !kv.starts_with(&prefix));
                cur_env.push(format!("{key}={val}"));
            }
            Step::Workdir(dir) => {
                cur_workdir = if dir.starts_with('/') {
                    dir.clone()
                } else {
                    format!("{cur_workdir}/{dir}")
                };
            }
            Step::Copy { src, dst, from } => {
                if from.is_some() {
                    return Err(Error::Invalid(
                        "COPY --from=<estágio> requer build multi-stage, não suportado nesta versão".into(),
                    ));
                }
                copy_into_rootfs(context, rootfs, src, dst, &cur_workdir)?;
            }
            Step::Run(cmdline) => {
                let exports: String = cur_env.iter().map(|kv| format!("export {kv}; ")).collect();
                let shell = format!("mkdir -p {cur_workdir} && cd {cur_workdir}; {exports}{cmdline}");
                let argv = vec!["/bin/sh".to_string(), "-c".to_string(), shell];
                let code = runtime::exec(c, &argv, false)?;
                if code != 0 {
                    return Err(Error::Invalid(format!("RUN falhou (exit {code}): {cmdline}")));
                }
            }
        }
    }
    Ok(())
}

/// Resolve um componente `../`/absoluto de forma segura: junta `base` só com
/// os componentes "normais" de `rel` (rejeita `..`/raiz/prefixo — nunca deixa
/// escapar de `base`). Mesmo padrão de `safe_rel` em
/// `delonix-image/src/overlay.rs` (extracção de layers de imagem), aplicado
/// aqui ao `COPY` do Dockerfile/Delonixfile. **Achado de auditoria de
/// segurança**: sem isto, `COPY ../../../etc/passwd x` lia ficheiros
/// arbitrários do host para dentro da imagem, e um `dst` com `..` escrevia
/// fora do rootfs — ver CLAUDE.md.
fn safe_join(base: &Path, rel: &str) -> Result<PathBuf> {
    use std::path::Component;
    let mut out = base.to_path_buf();
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            _ => return Err(Error::Invalid(format!("caminho inválido em COPY: '{rel}' (sai do directório permitido)"))),
        }
    }
    Ok(out)
}

fn copy_into_rootfs(context: &Path, rootfs: &str, src: &str, dst: &str, workdir: &str) -> Result<()> {
    let src_path = safe_join(context, src)?;
    // Semântica Docker do destino: um `dst` relativo resolve-se contra o WORKDIR
    // (`COPY x ./` → WORKDIR/x, não a raiz do rootfs); um `dst` que termina em `/`
    // (ou é um directório) mantém o basename do `src` lá dentro.
    let dir_dest = dst.ends_with('/') || dst == "." || dst == "./";
    let abs_dst = if dst.starts_with('/') {
        dst.to_string()
    } else {
        format!("{}/{}", workdir.trim_end_matches('/'), dst)
    };
    let mut dst_path = safe_join(Path::new(rootfs), abs_dst.trim_start_matches('/'))?;
    if src_path.is_dir() {
        copy_dir_all(&src_path, &dst_path)
    } else {
        if dir_dest || dst_path.is_dir() {
            if let Some(name) = src_path.file_name() {
                dst_path.push(name);
            }
        }
        if let Some(parent) = dst_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Invalid(format!("mkdir {}: {e}", parent.display())))?;
        }
        std::fs::copy(&src_path, &dst_path)
            .map_err(|e| Error::Invalid(format!("COPY {} -> {}: {e}", src_path.display(), dst_path.display())))?;
        Ok(())
    }
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(|e| Error::Invalid(format!("mkdir {}: {e}", dst.display())))?;
    for entry in std::fs::read_dir(src).map_err(|e| Error::Invalid(format!("ler {}: {e}", src.display())))? {
        let entry = entry.map_err(|e| Error::Invalid(e.to_string()))?;
        let ty = entry.file_type().map_err(|e| Error::Invalid(e.to_string()))?;
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target).map_err(|e| Error::Invalid(e.to_string()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{default_build_file, safe_join};
    use std::path::Path;

    #[test]
    fn safe_join_aceita_caminho_normal() {
        let base = Path::new("/tmp/context");
        assert_eq!(safe_join(base, "src/app.txt").unwrap(), base.join("src/app.txt"));
    }

    #[test]
    fn safe_join_recusa_dot_dot() {
        let base = Path::new("/tmp/rootfs");
        assert!(safe_join(base, "../../../etc/passwd").is_err());
        assert!(safe_join(base, "a/../../b").is_err());
    }

    #[test]
    fn safe_join_recusa_absoluto() {
        // um `dst` absoluto NÃO pode substituir `base` (era o risco antes do fix:
        // `Path::join` com um componente absoluto ignora `base` por completo).
        let base = Path::new("/tmp/rootfs");
        assert!(safe_join(base, "/etc/passwd").is_err());
    }

    #[test]
    fn prefere_delonixfile_quando_existe() {
        let dir = std::env::temp_dir().join(format!("delonix-build-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        std::fs::write(dir.join("Delonixfile"), "FROM alpine\n").unwrap();
        assert_eq!(default_build_file(&dir), dir.join("Delonixfile"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recorre_a_dockerfile_sem_delonixfile() {
        let dir = std::env::temp_dir().join(format!("delonix-build-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        assert_eq!(default_build_file(&dir), dir.join("Dockerfile"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
