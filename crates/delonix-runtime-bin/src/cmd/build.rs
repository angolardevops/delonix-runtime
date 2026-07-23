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
//! **Multi-stage** (`FROM ... AS <name>` + `COPY --from=<stage>`): each stage
//! (including the final one) is built the same way — a working container, its
//! own rootfs, its own `RUN`/`COPY` steps — but an intermediate stage's rootfs
//! is kept on disk (not unmounted/removed) until the WHOLE build finishes, so a
//! later stage's `COPY --from=<name-or-index>` can read straight out of it via
//! [`copy_into_rootfs`] (which already takes an arbitrary "source root", not
//! just the build context). `FROM <earlier-stage>` (a stage built FROM another
//! stage, not an image) is supported by cloning that stage's rootfs with
//! `cp -a --reflink=auto` (preserves symlinks/perms exactly — a naive
//! walk-and-copy would dereference `/bin -> usr/bin`-style symlinks, which is
//! wrong for a rootfs). The one gap: committing the FINAL image in **root**
//! (overlay) mode needs a real OCI base `Image` for lineage — if the final
//! stage's `FROM` names an earlier stage rather than a real image, that path
//! errors out early with a clear message; rootless has no such restriction
//! (it packs a flat squash layer, no lineage to carry).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use clap::Args;
use delonix_image::build::{parse_dockerfile_with_args, Step};
use delonix_image::{Image, ImageStore};
use delonix_runtime::{self as runtime, RunSpec};
use delonix_runtime_core::{generate_id, Container, Error, Result, Store};

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
    /// Build-time variable (`KEY=VALUE`), repeatable — only takes effect for a
    /// name the Dockerfile actually declares with `ARG KEY[=default]` (an
    /// override with no matching `ARG` is silently ignored, same as Docker).
    #[arg(long = "build-arg")]
    build_arg: Vec<String>,
}

/// Parses `KEY=VALUE` build-arg flags into pairs, dropping anything malformed
/// (missing `=`) with a warning rather than failing the whole build over a typo.
pub(crate) fn parse_build_args(raw: &[String]) -> Vec<(String, String)> {
    raw.iter()
        .filter_map(|kv| match kv.split_once('=') {
            Some((k, v)) => Some((k.to_string(), v.to_string())),
            None => {
                eprintln!("aviso: --build-arg '{kv}' ignorado — esperava KEY=VALUE");
                None
            }
        })
        .collect()
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
    let build_args = parse_build_args(&args.build_arg);
    let img = build_from_spec(&args.context, &file, &args.tag, &build_args)?;
    println!("{}", img.short_id());
    Ok(())
}

/// The accumulated state of one finished stage — what a LATER stage needs from
/// an EARLIER one, whether referencing it via `FROM <stage>` (a fresh clone of
/// its rootfs) or `COPY --from=<stage>` (a read of its rootfs, untouched).
#[derive(Clone)]
struct StageResult {
    rootfs: String,
    cmd: Vec<String>,
    entrypoint: Vec<String>,
    env: Vec<String>,
    workdir: String,
    user: String,
    /// `Some` only when this stage's `FROM` resolved to a real pulled image —
    /// needed for the root-mode OCI commit if this ends up being the FINAL
    /// stage's base (lineage/diff_ids come from a real `Image`, not a clone).
    image: Option<Image>,
}

/// Resolves a stage's `FROM <token>`: either an EARLIER stage's name/index
/// (clones its rootfs — Docker's "build on top of another stage" form) or a
/// real image reference (pull/resolve as usual).
fn resolve_stage_base(
    images: &ImageStore,
    from: &str,
    stages: &HashMap<String, StageResult>,
    new_id: &str,
) -> Result<StageResult> {
    if let Some(prior) = stages.get(from) {
        let rootfs = clone_rootfs(images, &prior.rootfs, new_id)?;
        Ok(StageResult {
            rootfs,
            cmd: prior.cmd.clone(),
            entrypoint: prior.entrypoint.clone(),
            env: prior.env.clone(),
            workdir: prior.workdir.clone(),
            user: prior.user.clone(),
            image: None,
        })
    } else {
        let img = resolve_or_pull(images, from)?;
        let rootfs = prepare_rootfs(images, &img, new_id)?;
        let workdir = if img.config.working_dir.is_empty() {
            "/".to_string()
        } else {
            img.config.working_dir.clone()
        };
        Ok(StageResult {
            rootfs,
            cmd: img.config.cmd.clone(),
            entrypoint: img.config.entrypoint.clone(),
            env: img.config.env.clone(),
            workdir,
            user: img.config.user.clone(),
            image: Some(img),
        })
    }
}

/// Clones an EARLIER stage's rootfs into a NEW stage's rootfs directory —
/// `FROM <stage>`, not `FROM <image>`. `cp -a --reflink=auto` (copy-on-write
/// where the filesystem supports it, e.g. btrfs/xfs) preserves symlinks,
/// permissions and xattrs verbatim; a rootfs is full of symlinks
/// (`/bin -> usr/bin`, …) that a naive recursive copy would wrongly dereference.
fn clone_rootfs(images: &ImageStore, src_rootfs: &str, id: &str) -> Result<String> {
    let dst = images.root().join("containers").join(id).join("rootfs");
    std::fs::create_dir_all(&dst)
        .map_err(|e| Error::Invalid(format!("mkdir {}: {e}", dst.display())))?;
    let status = std::process::Command::new("cp")
        .arg("-a")
        .arg("--reflink=auto")
        .arg(format!("{}/.", src_rootfs.trim_end_matches('/')))
        .arg(&dst)
        .status()
        .map_err(|e| Error::Invalid(format!("cp do estágio '{src_rootfs}': {e}")))?;
    if !status.success() {
        return Err(Error::Invalid(format!(
            "cp do estágio '{src_rootfs}' falhou"
        )));
    }
    if runtime::is_rootless() {
        super::util::chown_tree(&dst, runtime::USERNS_UID_BASE)?;
    }
    Ok(dst.to_string_lossy().into_owned())
}

/// Builds ONE stage end to end: resolves its base (image or earlier stage),
/// brings up a working container over its rootfs, runs its steps. Returns the
/// container (for the caller to stop/clean up) and the resulting state. Does
/// NOT unmount/remove anything — an intermediate stage's rootfs must survive
/// until every later stage that might `COPY --from`/`FROM` it has run.
/// Returns the working container WHENEVER one was actually created — regardless
/// of whether the stage's steps then succeeded — so the caller can always clean
/// it up. A `RUN`/`COPY` failing partway through must not leak the container: it
/// was already `create_with`'d and possibly `exec`'d into before the failing
/// step, so there's real state (cgroup, namespaces, rootfs) to tear down.
fn build_one_stage(
    store: &Store,
    images: &ImageStore,
    context: &Path,
    from: &str,
    steps: &[Step],
    stages: &HashMap<String, StageResult>,
) -> (Option<Container>, Result<StageResult>) {
    let id = generate_id();
    let mut base = match resolve_stage_base(images, from, stages, &id) {
        Ok(b) => b,
        Err(e) => return (None, Err(e)),
    };
    let rootless = runtime::is_rootless();

    // "Working" container: `sleep infinity` keeps the namespaces alive so we
    // can `exec` each RUN; COPY writes directly to the rootfs on disk.
    let mut c = Container::new(
        id.clone(),
        format!("dlx-build-{}", &id[..8.min(id.len())]),
        from.to_string(),
        vec!["/bin/sh".into(), "-c".into(), "sleep infinity".into()],
        "max".into(),
    );
    c.userns = rootless;
    let spec = RunSpec {
        detach: true,
        userns: rootless,
        ..Default::default()
    };
    if let Err(e) = runtime::create_with(store, &mut c, &base.rootfs, &spec) {
        return (None, Err(e));
    }

    let mut cur_env = base.env.clone();
    let mut cur_workdir = base.workdir.clone();
    let steps_result = run_steps(
        steps,
        &base.rootfs,
        context,
        &c,
        &mut cur_env,
        &mut cur_workdir,
        stages,
    );
    let _ = runtime::stop(store, &mut c, 5);
    match steps_result {
        Ok(()) => {
            base.env = cur_env;
            base.workdir = cur_workdir;
            (Some(c), Ok(base))
        }
        Err(e) => (Some(c), Err(e)),
    }
}

/// The full orchestration of a build (parse → one working container per stage
/// → RUN/COPY → commit). Extracted from `run()` to be reused by `delonix
/// image apply` (`kind: Image`, `spec.build`) without duplicating logic.
pub fn build_from_spec(
    context: &Path,
    dockerfile_path: &Path,
    tag: &str,
    build_args: &[(String, String)],
) -> Result<Image> {
    let (images, store) = open_stores()?;
    let text = std::fs::read_to_string(dockerfile_path).map_err(|e| {
        Error::Invalid(format!(
            "não consegui ler {}: {e}",
            dockerfile_path.display()
        ))
    })?;
    let df = parse_dockerfile_with_args(&text, build_args)?;
    let rootless = runtime::is_rootless();

    // Fail fast (before building anything) in the one root-mode gap: the FINAL
    // stage's `FROM` naming an earlier stage rather than a real image — see the
    // module doc comment for why. Determined purely from `df`, no I/O needed.
    if !rootless {
        let final_from_is_stage = df
            .stages
            .iter()
            .any(|s| s.name.as_deref() == Some(df.from.as_str()))
            || df
                .from
                .parse::<usize>()
                .is_ok_and(|i| i < df.stages.len());
        if final_from_is_stage {
            return Err(Error::Invalid(format!(
                "build multi-stage em modo root (overlay): o estágio final (`FROM {}`) tem de ser \
                 uma imagem real — `FROM <estágio-anterior>` no estágio final só é suportado em \
                 rootless (sem lineage OCI a preservar)",
                df.from
            )));
        }
    }

    let mut stages: HashMap<String, StageResult> = HashMap::new();
    let mut work_containers: Vec<Container> = Vec::new();

    let build_result: Result<Image> = (|| {
        for (idx, stage) in df.stages.iter().enumerate() {
            let (c, result) =
                build_one_stage(&store, &images, context, &stage.from, &stage.steps, &stages);
            // Track the container for cleanup BEFORE propagating a step failure —
            // otherwise a `RUN`/`COPY` that fails partway through leaks it (it was
            // already created; only the caller's error handling would know to
            // tear it down).
            if let Some(c) = c {
                work_containers.push(c);
            }
            let result = result?;
            if let Some(name) = &stage.name {
                stages.insert(name.clone(), result.clone());
            }
            stages.insert(idx.to_string(), result);
        }

        let (c, final_state) =
            build_one_stage(&store, &images, context, &df.from, &df.steps, &stages);
        if let Some(c) = c {
            work_containers.push(c);
        }
        let final_state = final_state?;
        let id = work_containers.last().unwrap().id.clone();

        if rootless {
            let cmd = if df.cmd.is_empty() {
                final_state.cmd.clone()
            } else {
                df.cmd.clone()
            };
            let entrypoint = if df.entrypoint.is_empty() {
                final_state.entrypoint.clone()
            } else {
                df.entrypoint.clone()
            };
            let mut env = final_state.env.clone();
            env.extend(df.env.iter().cloned());
            let workdir = df
                .workdir
                .clone()
                .unwrap_or_else(|| final_state.workdir.clone());
            let user = if df.user.is_empty() {
                final_state.user.clone()
            } else {
                df.user.clone()
            };
            commit_flat_rootless(
                &images,
                &final_state.rootfs,
                &id,
                cmd,
                entrypoint,
                env,
                workdir,
                user,
                tag,
            )
        } else {
            let Some(base_image) = &final_state.image else {
                return Err(Error::Invalid(format!(
                    "build multi-stage em modo root (overlay): o estágio final (`FROM {}`) tem de \
                     ser uma imagem real — `FROM <estágio-anterior>` no estágio final só é \
                     suportado em rootless (sem lineage OCI a preservar)",
                    df.from
                )));
            };
            let layer = images.commit_upper(&id)?;
            images.build_image(base_image, layer, &df, tag)
        }
    })();

    // Best-effort cleanup of EVERY stage's working container/rootfs — never
    // hides the build/commit error (`build_result` alone decides the outcome).
    for c in &work_containers {
        let _ = runtime::remove(&store, c, true);
        let _ = images.unmount_rootfs(&c.id);
    }

    build_result
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
#[allow(clippy::too_many_arguments)]
fn commit_flat_rootless(
    images: &delonix_image::ImageStore,
    rootfs: &str,
    id: &str,
    cmd: Vec<String>,
    entrypoint: Vec<String>,
    env: Vec<String>,
    workdir: String,
    user: String,
    tag: &str,
) -> Result<Image> {
    let tar_path = std::env::temp_dir().join(format!("delonix-build-{id}.tar"));
    let tar_str = tar_path.to_string_lossy().to_string();
    let result = match runtime::reexec_mapped(&["__buildtar", rootfs, &tar_str]) {
        Some(true) => {
            let bytes = std::fs::read(&tar_path)
                .map_err(|e| Error::Invalid(format!("ler tar do build (userns mapeado): {e}")))?;
            images.commit_flat_rootfs_from_tar(bytes, cmd, entrypoint, env, workdir, user, tag)
        }
        Some(false) => Err(Error::Invalid(
            "empacotar rootfs dentro do userns mapeado falhou (delonix __buildtar)".into(),
        )),
        // Without subuid (rootless single-uid): the RUN files are our uid's.
        None => images.commit_flat_rootfs(Path::new(rootfs), cmd, entrypoint, env, workdir, user, tag),
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
    cur_env: &mut Vec<String>,
    cur_workdir: &mut String,
    stages: &HashMap<String, StageResult>,
) -> Result<()> {
    for step in steps {
        match step {
            Step::Env { key, val } => {
                let prefix = format!("{key}=");
                cur_env.retain(|kv| !kv.starts_with(&prefix));
                cur_env.push(format!("{key}={val}"));
            }
            Step::Workdir(dir) => {
                *cur_workdir = if dir.starts_with('/') {
                    dir.clone()
                } else {
                    format!("{cur_workdir}/{dir}")
                };
            }
            Step::Copy { src, dst, from } => {
                match from {
                    // `COPY --from=<name-or-index>`: read from an EARLIER stage's
                    // rootfs (kept alive on disk for exactly this) instead of the
                    // build context. `copy_into_rootfs` already takes an
                    // arbitrary "source root" — no change needed there.
                    Some(stage_ref) => {
                        let src_stage = stages.get(stage_ref).ok_or_else(|| {
                            Error::Invalid(format!(
                                "COPY --from={stage_ref}: estágio desconhecido (só estágios \
                                 JÁ definidos antes deste ponto do Dockerfile são visíveis)"
                            ))
                        })?;
                        // Unlike a plain `COPY` (src relative to the build context), Docker's
                        // `--from=<stage>` takes `src` rooted at that STAGE's `/` — the leading
                        // `/` here means "the stage's filesystem root", not "reject as
                        // absolute" (which is what `safe_join`/`copy_into_rootfs` do for a
                        // plain COPY's context-relative src).
                        let stage_rel = src.trim_start_matches('/');
                        copy_into_rootfs(
                            Path::new(&src_stage.rootfs),
                            rootfs,
                            stage_rel,
                            dst,
                            cur_workdir,
                        )?;
                    }
                    None => copy_into_rootfs(context, rootfs, src, dst, cur_workdir)?,
                }
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

/// Resolves `path` to its real, symlink-free location and refuses if that would
/// land outside `canon_base` (already canonicalized). SECURITY: `safe_join` only
/// rejects LEXICAL `..`/absolute components in the REQUESTED path — it says
/// nothing about a symlink already sitting on disk that the lexical path walks
/// through (build context or rootfs from an earlier layer). Two concrete escapes
/// this closes: a build-context entry `creds -> /home/u/.ssh/id_rsa` baking the
/// host's private key into an image via `COPY creds /app/creds`; and a rootfs
/// symlink `/opt/hook -> ../../../../home/u/.bashrc` shipped by a malicious `FROM`
/// image, overwritten by a later `COPY payload /opt/hook`. `path` need not exist
/// yet (the destination side of a COPY usually doesn't) — walks up to the nearest
/// EXISTING ancestor, canonicalizes THAT, and re-appends the non-existent tail
/// (which by definition can't itself be a symlink).
fn confine_to(canon_base: &Path, path: &Path) -> Result<PathBuf> {
    let mut existing: &Path = path;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(canon) => {
                let mut real = canon;
                for t in tail.into_iter().rev() {
                    real.push(t);
                }
                if !real.starts_with(canon_base) {
                    return Err(Error::Invalid(format!(
                        "'{}' sai do directório permitido através de um symlink",
                        path.display()
                    )));
                }
                return Ok(real);
            }
            Err(_) => {
                let Some(name) = existing.file_name() else {
                    return Err(Error::Invalid(format!(
                        "caminho inválido em COPY: '{}'",
                        path.display()
                    )));
                };
                tail.push(name.to_os_string());
                existing = existing.parent().ok_or_else(|| {
                    Error::Invalid(format!("caminho inválido em COPY: '{}'", path.display()))
                })?;
            }
        }
    }
}

fn canonical_base(p: &Path) -> Result<PathBuf> {
    p.canonicalize()
        .map_err(|e| Error::Invalid(format!("resolver {}: {e}", p.display())))
}

fn copy_into_rootfs(
    context: &Path,
    rootfs: &str,
    src: &str,
    dst: &str,
    workdir: &str,
) -> Result<()> {
    let canon_context = canonical_base(context)?;
    let canon_rootfs = canonical_base(Path::new(rootfs))?;

    let src_path = safe_join(context, src)?;
    let src_path = confine_to(&canon_context, &src_path)?;
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
        let dst_path = confine_to(&canon_rootfs, &dst_path)?;
        copy_dir_all(&src_path, &dst_path, &canon_context, &canon_rootfs)
    } else {
        if dir_dest || dst_path.is_dir() {
            if let Some(name) = src_path.file_name() {
                dst_path.push(name);
            }
        }
        let dst_path = confine_to(&canon_rootfs, &dst_path)?;
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

fn copy_dir_all(src: &Path, dst: &Path, canon_context: &Path, canon_rootfs: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)
        .map_err(|e| Error::Invalid(format!("mkdir {}: {e}", dst.display())))?;
    for entry in
        std::fs::read_dir(src).map_err(|e| Error::Invalid(format!("ler {}: {e}", src.display())))?
    {
        let entry = entry.map_err(|e| Error::Invalid(e.to_string()))?;
        let ty = entry
            .file_type()
            .map_err(|e| Error::Invalid(e.to_string()))?;
        // A NESTED entry can itself be a symlink escaping the tree, even though the
        // top-level src/dst of this COPY already passed `confine_to` — validate every
        // entry, not just the root.
        let entry_path = confine_to(canon_context, &entry.path())?;
        let target = confine_to(canon_rootfs, &dst.join(entry.file_name()))?;
        if ty.is_dir() {
            copy_dir_all(&entry_path, &target, canon_context, canon_rootfs)?;
        } else {
            std::fs::copy(&entry_path, &target).map_err(|e| Error::Invalid(e.to_string()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{confine_to, default_build_file, safe_join, sh_export};
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
    fn confine_to_recusa_symlink_que_escapa_da_base() {
        // Regression for the audit finding: `safe_join` only rejects lexical `..` in
        // the REQUESTED path — a symlink already on disk (planted by an earlier
        // build-context entry or a malicious `FROM` image layer) could still walk
        // out. `confine_to` must catch it even though the requested relative path
        // itself never contained `..`.
        let dir = std::env::temp_dir().join(format!("delonix-confine-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let base = dir.join("base");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"top secret").unwrap();
        std::os::unix::fs::symlink(&outside, base.join("escape")).unwrap();

        let canon_base = base.canonicalize().unwrap();
        // Existing target reached via the symlink: rejected.
        assert!(confine_to(&canon_base, &base.join("escape/secret")).is_err());
        // Non-existent target reached via the symlink (the COPY destination case,
        // where the final component doesn't exist yet): still rejected — the walk
        // up the ancestor chain hits `escape` (which DOES exist and resolves
        // outside `base`) before it ever reaches a real filesystem boundary.
        assert!(confine_to(&canon_base, &base.join("escape/not-yet-created")).is_err());
        // A normal, non-symlinked path stays accepted.
        assert!(confine_to(&canon_base, &base.join("plain/not-yet-created")).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
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
