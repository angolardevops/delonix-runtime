//! `delonix build` — builds an image from a Dockerfile.
//!
//! The only group with new orchestration (the other 5 are "wiring up" APIs
//! already ready in the workspace crates): brings up a "working" container
//! (placeholder `sleep infinity`, keeps the namespaces alive), runs each `RUN`
//! in it via `runtime::exec`, applies each `COPY` by writing directly to the
//! rootfs on disk, and at the end packages the result with
//! `ImageStore::commit_flat_rootfs` (rootless) or `commit_upper`+`build_image`
//! (root/overlay) — the same two "docker commit" functions that already exist
//! in `delonix-image::build`.
//!
//! **Single-stage only in this version**: a Dockerfile with `FROM ... AS <name>`
//! followed by another `FROM` (multi-stage) is rejected with a clear error — the
//! rootfs handoff between stages (`COPY --from`) still needs careful design;
//! it is left for a future iteration.

use std::path::{Path, PathBuf};

use clap::Args;
use delonix_image::build::{parse_dockerfile, Step};
use delonix_image::Image;
use delonix_runtime::{self as runtime, RunSpec};
use delonix_runtime_core::{generate_id, Container, Error, Result};

use super::util::{open_stores, prepare_rootfs, resolve_or_pull};

#[derive(Args)]
pub struct BuildArgs {
    /// Build context (default: `.`) — root for `COPY`.
    #[arg(default_value = ".")]
    context: PathBuf,
    /// Path of the Dockerfile (default: `<context>/Dockerfile`).
    #[arg(short = 'f', long = "file")]
    file: Option<PathBuf>,
    /// Tag of the resulting image (`repo:tag`).
    #[arg(short = 't', long = "tag")]
    tag: String,
}

/// Resolves the default build file: `Delonixfile` if it exists in the
/// context, otherwise `Dockerfile` (same grammar — `parse_dockerfile` already
/// supports the Delonix extensions `SCAN`/`CPUS`/`MEMORY`/`SECURITY`/
/// `HEALTHCHECK` regardless of the file name; `Delonixfile` is just the
/// canonical name, discovered by default, for those who do not want to reuse
/// the `Dockerfile` name).
pub(crate) fn default_build_file(context: &Path) -> PathBuf {
    let delonixfile = context.join("Delonixfile");
    if delonixfile.exists() {
        delonixfile
    } else {
        context.join("Dockerfile")
    }
}

pub fn run(args: BuildArgs) -> Result<()> {
    let file = args
        .file
        .clone()
        .unwrap_or_else(|| default_build_file(&args.context));
    let img = build_from_spec(&args.context, &file, &args.tag)?;
    println!("{}", img.short_id());
    Ok(())
}

/// The full orchestration of a build (parse → working container → RUN/
/// COPY → commit). Extracted from `run()` to be reused by `delonix
/// image apply` (`kind: Image`, `spec.build`) without duplicating logic.
pub fn build_from_spec(context: &Path, dockerfile_path: &Path, tag: &str) -> Result<Image> {
    let (images, store) = open_stores()?;
    let text = std::fs::read_to_string(dockerfile_path).map_err(|e| {
        Error::Invalid(format!(
            "não consegui ler {}: {e}",
            dockerfile_path.display()
        ))
    })?;
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

    // "Working" container: `sleep infinity` keeps the namespaces alive so we
    // can `exec` each RUN; COPY writes directly to the rootfs on disk.
    let mut c = Container::new(
        id.clone(),
        format!("dlx-build-{}", &id[..8.min(id.len())]),
        df.from.clone(),
        vec!["/bin/sh".into(), "-c".into(), "sleep infinity".into()],
        "max".into(),
    );
    c.userns = rootless;
    let spec = RunSpec {
        detach: true,
        userns: rootless,
        ..Default::default()
    };
    runtime::create_with(&store, &mut c, &rootfs, &spec)?;

    let steps_result = run_steps(&df.steps, &rootfs, context, &c, &base);

    let commit_result = steps_result.and_then(|()| {
        let _ = runtime::stop(&store, &mut c, 5);
        if rootless {
            let cmd = if df.cmd.is_empty() {
                base.config.cmd.clone()
            } else {
                df.cmd.clone()
            };
            let mut env = base.config.env.clone();
            env.extend(df.env.iter().cloned());
            let workdir = df
                .workdir
                .clone()
                .unwrap_or_else(|| base.config.working_dir.clone());
            commit_flat_rootless(&images, &rootfs, &id, cmd, env, workdir, tag)
        } else {
            let layer = images.commit_upper(&c.id)?;
            images.build_image(&base, layer, &df, tag)
        }
    });

    // Best-effort cleanup of the working container — never hides the build/commit
    // error (the `?` below, over `commit_result`, is what decides the exit code).
    let _ = runtime::remove(&store, &c, true);
    let _ = images.unmount_rootfs(&c.id);

    commit_result
}

/// Packages and commits the FLAT rootfs of a **rootless** build, packaging it
/// INSIDE the mapped userns when there is subuid.
///
/// Reason: a `RUN` with `apt`/`dpkg` leaves subuid files with restricted modes
/// (`aux-cache` 0600, `partial` dirs 0700) that the REAL user cannot read — the
/// in-process tar of `commit_flat_rootfs` gave `Permission denied` at the end of a
/// build that had already passed all the RUNs. Here we re-exec `delonix __buildtar`
/// as root in a userns with the subuids mapped (`reexec_mapped`, the same
/// mechanism as volume snapshots): inside it we own everything, the tar comes out
/// complete and readable, and the parent reads it back (it becomes 0644) to store it in the CAS.
///
/// `reexec_mapped` returns `None` when it does not apply — rootless **single-uid**
/// (without `newuidmap`): there the RUN files belong to OUR uid and the
/// in-process path reads them just the same, so we fall back to `commit_flat_rootfs`.
fn commit_flat_rootless(
    images: &delonix_image::ImageStore,
    rootfs: &str,
    id: &str,
    cmd: Vec<String>,
    env: Vec<String>,
    workdir: String,
    tag: &str,
) -> Result<Image> {
    let tar_path = std::env::temp_dir().join(format!("delonix-build-{id}.tar"));
    let tar_str = tar_path.to_string_lossy().to_string();
    let result = match runtime::reexec_mapped(&["__buildtar", rootfs, &tar_str]) {
        Some(true) => {
            let bytes = std::fs::read(&tar_path)
                .map_err(|e| Error::Invalid(format!("ler tar do build (userns mapeado): {e}")))?;
            images.commit_flat_rootfs_from_tar(bytes, cmd, env, workdir, tag)
        }
        Some(false) => Err(Error::Invalid(
            "empacotar rootfs dentro do userns mapeado falhou (delonix __buildtar)".into(),
        )),
        // Without subuid (rootless single-uid): the RUN files are our uid's.
        None => images.commit_flat_rootfs(Path::new(rootfs), cmd, env, workdir, tag),
    };
    let _ = std::fs::remove_file(&tar_path); // best-effort, never hides the result
    result
}

/// Synthesizes a safe `export KEY='VALUE'; ` for the `/bin/sh -c` of each `RUN`.
/// The **value** goes in single-quotes because base images ship
/// ENV with spaces — the classic is `PHPIZE_DEPS="autoconf dpkg-dev file g++ …"`
/// from the whole `php`/`frankenphp` image. Without the quotes, `export PHPIZE_DEPS=autoconf
/// dpkg-dev …` makes the shell treat `dpkg-dev` as a second name to export →
/// `export: dpkg-dev: bad variable name`, and the **entire** `RUN` of that image fails.
/// Single-quotes inside the value become `'\''` (close, escape a literal quote, reopen).
fn sh_export(kv: &str) -> String {
    match kv.split_once('=') {
        Some((key, val)) => format!("export {key}='{}'; ", val.replace('\'', "'\\''")),
        // Without `=` it is not an assignment — leave it as is (degenerate case).
        None => format!("export {kv}; "),
    }
}

/// Runs the Dockerfile steps in order, keeping a local accumulator of
/// ENV/WORKDIR (`runtime::exec` has no notion of per-call environment — each
/// `RUN` synthesizes `cd <workdir> && export ... ; <cmd>` in a `/bin/sh -c`, just
/// as Docker's shell-form already implies).
fn run_steps(
    steps: &[Step],
    rootfs: &str,
    context: &Path,
    c: &Container,
    base: &Image,
) -> Result<()> {
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
                let exports: String = cur_env.iter().map(|kv| sh_export(kv)).collect();
                let shell =
                    format!("mkdir -p {cur_workdir} && cd {cur_workdir}; {exports}{cmdline}");
                let argv = vec!["/bin/sh".to_string(), "-c".to_string(), shell];
                let code = runtime::exec(c, &argv, false)?;
                if code != 0 {
                    return Err(Error::Invalid(format!(
                        "RUN falhou (exit {code}): {cmdline}"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Resolves a `../`/absolute component safely: joins `base` only with
/// the "normal" components of `rel` (rejects `..`/root/prefix — never lets it
/// escape from `base`). Same pattern as `safe_rel` in
/// `delonix-image/src/overlay.rs` (image-layer extraction), applied
/// here to the `COPY` of the Dockerfile/Delonixfile. **Security-audit
/// finding**: without this, `COPY ../../../etc/passwd x` read
/// arbitrary host files into the image, and a `dst` with `..` wrote
/// outside the rootfs — see CLAUDE.md.
fn safe_join(base: &Path, rel: &str) -> Result<PathBuf> {
    use std::path::Component;
    let mut out = base.to_path_buf();
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            _ => {
                return Err(Error::Invalid(format!(
                    "caminho inválido em COPY: '{rel}' (sai do directório permitido)"
                )))
            }
        }
    }
    Ok(out)
}

fn copy_into_rootfs(
    context: &Path,
    rootfs: &str,
    src: &str,
    dst: &str,
    workdir: &str,
) -> Result<()> {
    let src_path = safe_join(context, src)?;
    // Docker semantics of the destination: a relative `dst` resolves against the WORKDIR
    // (`COPY x ./` → WORKDIR/x, not the rootfs root); a `dst` ending in `/`
    // (or being a directory) keeps the basename of `src` inside it.
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
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Invalid(format!("mkdir {}: {e}", parent.display())))?;
        }
        std::fs::copy(&src_path, &dst_path).map_err(|e| {
            Error::Invalid(format!(
                "COPY {} -> {}: {e}",
                src_path.display(),
                dst_path.display()
            ))
        })?;
        Ok(())
    }
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)
        .map_err(|e| Error::Invalid(format!("mkdir {}: {e}", dst.display())))?;
    for entry in
        std::fs::read_dir(src).map_err(|e| Error::Invalid(format!("ler {}: {e}", src.display())))?
    {
        let entry = entry.map_err(|e| Error::Invalid(e.to_string()))?;
        let ty = entry
            .file_type()
            .map_err(|e| Error::Invalid(e.to_string()))?;
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
    use super::{default_build_file, safe_join, sh_export};
    use std::path::Path;

    #[test]
    fn sh_export_cita_valor_com_espacos() {
        // Regression: `PHPIZE_DEPS` (php/frankenphp image) has spaces — without quotes
        // the shell treated `dpkg-dev` as a name to export and the WHOLE RUN failed
        // ("export: dpkg-dev: bad variable name").
        assert_eq!(
            sh_export("PHPIZE_DEPS=autoconf dpkg-dev file g++"),
            "export PHPIZE_DEPS='autoconf dpkg-dev file g++'; "
        );
        // Without spaces it stays correct.
        assert_eq!(sh_export("PATH=/usr/bin"), "export PATH='/usr/bin'; ");
        // A literal single-quote in the value is escaped (close/escape/reopen).
        assert_eq!(sh_export("MSG=it's ok"), "export MSG='it'\\''s ok'; ");
        // `=` in the value (e.g. base64) only splits on the first one.
        assert_eq!(sh_export("K=a=b=c"), "export K='a=b=c'; ");
    }

    #[test]
    fn safe_join_aceita_caminho_normal() {
        let base = Path::new("/tmp/context");
        assert_eq!(
            safe_join(base, "src/app.txt").unwrap(),
            base.join("src/app.txt")
        );
    }

    #[test]
    fn safe_join_recusa_dot_dot() {
        let base = Path::new("/tmp/rootfs");
        assert!(safe_join(base, "../../../etc/passwd").is_err());
        assert!(safe_join(base, "a/../../b").is_err());
    }

    #[test]
    fn safe_join_recusa_absoluto() {
        // an absolute `dst` must NOT replace `base` (that was the risk before the fix:
        // `Path::join` with an absolute component ignores `base` entirely).
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
