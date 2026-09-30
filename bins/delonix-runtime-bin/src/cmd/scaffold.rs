//! `delonix <group> init` — bootstraps a complete, working project with **one
//! command**: the files come already filled in (images included), ready to
//! `build`/`apply`/`create` without editing anything.
//!
//! # Rule these templates follow
//!
//! **They only use fields the parser ACTUALLY reads.** A scaffold that emits
//! YAML with silently-ignored fields is worse than none: it gives the illusion
//! of configuration. Every field here is wired to code (`ContainerSpec`,
//! `VmSpec`, `KindCluster`…); what does not exist yet appears as a comment,
//! never as an active key.
//!
//! # Resilience by default
//!
//! What is generated uses `restart: always` (a detached supervisor that
//! captures the real exit code and restarts — see `container::run_supervised`),
//! named volumes for state (they survive the container's `rm`) and ports
//! published by the ingress. It is the requested "resilient and working" — not
//! a skeleton that has to be completed.

use std::path::{Path, PathBuf};

use delonix_model::{Error, Result};

// Templates embedded by `build.rs`: `TEMPLATES: &[(&str, &[(&str, &str, bool)])]`
// = [(name, [(relative-path, content, executable)])], and `TEMPLATE_META`.
include!(concat!(env!("OUT_DIR"), "/templates.rs"));

/// Default images — this is what fills the project when `--image` is NOT
/// passed. Pinned by digest where reproducibility matters.
const DEFAULT_APP_BASE: &str = "alpine:3.20";
const DEFAULT_DB_IMAGE: &str = "postgres:16-alpine";
const DEFAULT_VM_IMAGE: &str = "k8s-golden";

/// What to generate.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Target {
    Container,
    Vm,
    Cluster,
    /// Complete project: Delonixfile + manifest (all Kinds) + cluster.
    Stack,
}

pub(crate) struct InitOpts {
    pub dir: PathBuf,
    pub name: String,
    /// `None` = fill with the target's default image (the explicit request:
    /// "without passing --image it already fills in the images").
    pub image: Option<String>,
    pub force: bool,
    /// `--template <name>`: instead of the generic scaffold, generates a
    /// COMPLETE PROJECT for a language/framework (e.g. `fastapi`) with best
    /// practices — code, Delonixfile, manifest, tests and dotfiles. `list`
    /// shows the available ones.
    pub template: Option<String>,
    /// `--up`: after generating, builds the image, starts the container and
    /// waits until it is healthy — all with animated progress, in a single command.
    pub up: bool,
    /// `-v`/`--template-version`: a version parameter some templates read —
    /// an image tag (`odoo`), a framework version (`django`/`laravel`/
    /// `nextjs`/`nestjs`/`node`/`fastapi`), or a toolchain version (`go`, which
    /// has no framework to pin) — each documents the exact accepted form in
    /// its own README. Refused with a clear error on a template that has no
    /// `version=` in its `template.meta` — a version flag that is silently
    /// ignored would be worse than one that does not exist yet.
    pub template_version: Option<String>,
}

/// Names of the available templates, for `--help`/errors.
pub(crate) fn template_names() -> Vec<&'static str> {
    TEMPLATES.iter().map(|(n, _)| *n).collect()
}

/// Old template names that still answer, mapped to the directory that now
/// holds the template. A rename does not change what the project IS, so the
/// alias is silent — the same rule as a renamed Kind (`KIND_ALIASES`). `python`
/// always generated a FastAPI service; the template carries that name now, and
/// a script written against `-t python` keeps producing the same project.
const TEMPLATE_ALIASES: &[(&str, &str)] = &[("python", "fastapi")];

/// The canonical template name for what the user typed (an alias resolves to
/// its target; anything else passes through, to be refused by the caller with
/// the list of real names).
fn canonical_template(name: &str) -> &str {
    TEMPLATE_ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map(|(_, target)| *target)
        .unwrap_or(name)
}

/// What a template's `template.meta` declares (embedded by build.rs).
#[derive(Clone, Copy, Debug, PartialEq)]
struct TemplateMeta {
    port: &'static str,
    health: &'static str,
    /// Default `-v`; `""` when the template has no version parameter.
    version: &'static str,
    /// Accepted `-v` values, comma-separated (`""`: any plain version).
    versions: &'static str,
    /// Lock file of the template's package manager (`""`: none).
    lock: &'static str,
}

/// Default `8000` + the apps' health + no version if the template does not
/// declare one.
fn template_meta(name: &str) -> TemplateMeta {
    TEMPLATE_META
        .iter()
        .find(|m| m.0 == name)
        .map(|&(_, port, health, version, versions, lock)| TemplateMeta {
            port,
            health,
            version,
            versions,
            lock,
        })
        .unwrap_or(TemplateMeta {
            port: "8000",
            health: "/api/v1/health/live",
            version: "",
            versions: "",
            lock: "",
        })
}

/// Checks a project name before it is substituted anywhere. The name becomes
/// a container name, an image tag, an npm/Composer/Go module name and a YAML
/// scalar at once; the one shape all of them accept is a DNS label: lowercase
/// letters, digits and inner hyphens, at most 63 characters. Measured before
/// this check: a directory `My App"x` produced an unparsable `package.json`
/// and `Weird Go` a `module Weird Go` — both with exit 0.
pub(crate) fn check_project_name(name: &str) -> Result<()> {
    let b = name.as_bytes();
    let ok = !b.is_empty()
        && b.len() <= 63
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-';
    if ok {
        return Ok(());
    }
    Err(Error::Invalid(super::po::tf(
        "project name '{name}' is not usable: it becomes a container name, an image tag and a \
         package name, so it must be lowercase letters, digits and inner hyphens (at most 63) \
         — pass --name {suggestion}",
        &[("name", name), ("suggestion", &slug(name))],
    )))
}

/// The nearest valid project name to `name` (used in the error's suggestion).
fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.trim_matches('-').chars().take(63).collect();
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "app".to_string()
    } else {
        out
    }
}

/// A `-v` value is copied into `pyproject.toml`, `package.json`, `go.mod`,
/// `composer.json` and a `FROM` line, so it may only be a plain version —
/// `1`, `1.2` or `1.2.3`. Measured before: `-v '5.2.*", "evil==1'` added a
/// dependency to `pyproject.toml`. When the template lists the values it
/// supports, the version must be one of them or a more precise form of one
/// (`5.2.3` under `5.2`): a framework major the template was not validated
/// against is refused with the list, not discovered by the first build.
fn check_version(tname: &str, v: &str, accepted: &str) -> Result<()> {
    let plain = !v.is_empty()
        && v.split('.').count() <= 3
        && v.split('.')
            .all(|p| !p.is_empty() && p.len() <= 6 && p.bytes().all(|c| c.is_ascii_digit()));
    if !plain {
        return Err(Error::Invalid(super::po::tf(
            "-v '{v}' is not a plain version (1, 1.2 or 1.2.3)",
            &[("v", v)],
        )));
    }
    if accepted.is_empty() {
        return Ok(());
    }
    let inside = accepted
        .split(',')
        .map(str::trim)
        .any(|a| v == a || v.strip_prefix(a).is_some_and(|rest| rest.starts_with('.')));
    if inside {
        Ok(())
    } else {
        Err(Error::Invalid(super::po::tf(
            "template '{tname}' is validated with -v {accepted}; '{v}' is outside that range",
            &[("tname", tname), ("accepted", accepted), ("v", v)],
        )))
    }
}

/// Resolves `-v`/`--template-version` against a template's own default. `-v`
/// only means anything on a template that declares one (`default_version`
/// non-empty in its `template.meta`) — refusing it elsewhere beats silently
/// ignoring a flag the user explicitly passed. Pure and independent of the
/// embedded `TEMPLATES`/`TEMPLATE_META` tables, so the refusal is testable
/// without depending on a real template that happens to have no `version=`
/// (as of this writing, every shipped template does).
fn resolve_version<'a>(
    tname: &str,
    requested: Option<&'a str>,
    default_version: &'a str,
    accepted: &str,
) -> Result<&'a str> {
    match (requested, default_version) {
        (Some(_), "") => Err(Error::Invalid(super::po::tf(
            "template '{tname}' has no version parameter — drop -v/--template-version",
            &[("tname", tname)],
        ))),
        (Some(v), _) => {
            check_version(tname, v, accepted)?;
            Ok(v)
        }
        (None, d) => Ok(d),
    }
}

/// Derives a valid Python module from the project name: lowercase,
/// `[a-z0-9_]`, and never starting with a digit (otherwise it is not an identifier).
fn python_module(name: &str) -> String {
    let mut m: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if m.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(true) {
        m.insert(0, 'a');
    }
    m
}

/// Substitutes the `__NAME__`/`__MODULE__`/`__PORT__`/`__TEMPLATE_VERSION__`
/// tokens (in contents AND paths).
fn subst(s: &str, o: &InitOpts, module: &str, port: &str, version: &str) -> String {
    s.replace("__NAME__", &o.name)
        .replace("__MODULE__", module)
        .replace("__PORT__", port)
        .replace("__TEMPLATE_VERSION__", version)
}

/// The only files a template writes when ADOPTING an existing project — never
/// its demo source (`src/`, `test/`, `README.md`, `package.json`/`go.mod`/...):
/// those are the template author's own example code, and dropping them next
/// to a real project would either collide with what is already there or sit
/// unused.
///
/// `Delonixfile`/`delonix-manifest.yaml`/`.dockerignore` carry the one
/// assumption this whole mode has to make (the demo's file layout) — flagged
/// by the warning after generation, not hidden. The rest — CI/CD pipelines,
/// SonarQube config, git-flow/Conventional-Commits docs, commitlint config —
/// are generic: they describe the LANGUAGE's own build/test commands (already
/// read from the template's real `package.json`/`pyproject.toml`/etc., not
/// invented), never a source path specific to the demo app, so they carry no
/// such risk and are exactly as safe to hand to a real project as to a fresh one.
/// The adopted files that encode a CI pipeline, a package manager or a commit
/// tool. Written into an adopted project only when it has no CI of its own
/// AND uses the template's package manager (its lock file is present):
/// measured before, an npm project with its own workflow received a pnpm
/// `ci.yml` and `.gitlab-ci.yml` that could not run.
const CI_FILES: [&str; 5] = [
    ".github/workflows/ci.yml",
    ".github/dependabot.yml",
    ".gitlab-ci.yml",
    "sonar-project.properties",
    "commitlint.config.js",
];

/// Why the CI files must not be written into the adopted project at `dir`,
/// or `None` when they fit it.
fn adopt_ci_skip_reason(dir: &Path, lock: &str) -> Option<String> {
    let workflows = dir.join(".github/workflows");
    let has_workflow = std::fs::read_dir(&workflows)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false);
    if has_workflow || dir.join(".gitlab-ci.yml").exists() {
        return Some(super::po::t("the project already has its own CI").to_string());
    }
    if !lock.is_empty() && !dir.join(lock).exists() {
        return Some(super::po::tf(
            "the project has no {lock}, so it does not use the package manager the template's CI runs",
            &[("lock", lock)],
        ));
    }
    None
}

const ADOPT_FILES: [&str; 9] = [
    "Delonixfile",
    "delonix-manifest.yaml",
    ".dockerignore",
    ".github/workflows/ci.yml",
    ".gitlab-ci.yml",
    "sonar-project.properties",
    "CONTRIBUTING.md",
    "commitlint.config.js",
    ".github/dependabot.yml",
];

/// True when `dir` already has something in it. The signal for "this is a
/// real, already-existing project" is deliberately this generic (not
/// per-template evidence like `package.json`): it also covers `delonix init`
/// re-run a second time on its own output, which must not splat the demo
/// files back over edits the user has since made.
fn dir_has_content(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

/// Generates a project from an embedded template. `show_next` prints the next
/// steps (build/apply/curl) — suppressed when `--up` will do them.
///
/// Switches to ADOPT mode when `o.dir` already has content: writes only
/// [`ADOPT_FILES`] instead of the full demo project, and warns that the
/// `Delonixfile`'s `COPY`/`CMD` assume the demo's own file layout and need a
/// look before `build`/`--up` — never presented as "ready", since the shape
/// of the real project might not match what got written.
fn render_template(tname: &str, o: &InitOpts, show_next: bool) -> Result<()> {
    let tname = canonical_template(tname);
    if tname == "list" {
        println!(
            "{}: {}",
            super::po::t("available templates"),
            template_names().join(", ")
        );
        return Ok(());
    }
    let files = TEMPLATES
        .iter()
        .find(|(n, _)| *n == tname)
        .map(|(_, f)| *f)
        .ok_or_else(|| {
            Error::Invalid(super::po::tf(
                "template '{tname}' does not exist — available: {available}",
                &[
                    ("tname", tname),
                    ("available", &template_names().join(", ")),
                ],
            ))
        })?;
    // Everything substituted is checked BEFORE the first file is written:
    // a refusal after half the project exists would be the misleading partial
    // output the checks are here to prevent.
    check_project_name(&o.name)?;
    let module = python_module(&o.name);
    let meta = template_meta(tname);
    let (port, health) = (meta.port, meta.health);
    let version = resolve_version(
        tname,
        o.template_version.as_deref(),
        meta.version,
        meta.versions,
    )?;
    let adopt = dir_has_content(&o.dir);
    let ci_skip = if adopt {
        adopt_ci_skip_reason(&o.dir, meta.lock)
    } else {
        None
    };
    let plan: Vec<(PathBuf, String, bool)> = files
        .iter()
        .filter(|(rel, _, _)| !adopt || ADOPT_FILES.contains(rel))
        .filter(|(rel, _, _)| ci_skip.is_none() || !CI_FILES.contains(rel))
        .map(|(rel, content, exec)| {
            (
                o.dir.join(subst(rel, o, &module, port, version)),
                subst(content, o, &module, port, version),
                *exec,
            )
        })
        .collect();
    // Every destination is checked before the first write, so a refusal
    // leaves the directory exactly as it was.
    for (dest, _, _) in &plan {
        refuse_symlinks(&o.dir, dest)?;
    }
    std::fs::create_dir_all(&o.dir)?;
    let mut n = 0;
    for (dest, content, exec) in &plan {
        n += usize::from(write_file_mode(&o.dir, dest, content, o.force, *exec)?);
    }
    if let Some(why) = &ci_skip {
        super::output::warn(&super::po::tf(
            "CI files not written: {why}",
            &[("why", why)],
        ));
    }
    if n == 0 {
        eprintln!(
            "{}",
            super::po::t("nothing to do (everything already existed)")
        );
        return Ok(());
    }
    if adopt {
        println!(
            "{}",
            super::po::tf(
                "adopted '{name}' ({tname}) in {dir} — only Delonix/CI glue was written \
                 (Delonixfile, manifest, CI/CD, SonarQube, CONTRIBUTING.md), the project's \
                 own code was left untouched.",
                &[
                    ("name", &o.name),
                    ("tname", tname),
                    ("dir", &o.dir.display().to_string())
                ],
            )
        );
        super::output::warn(super::po::t(
            "the Delonixfile's COPY/CMD assume the demo project's own file layout — \
             review them against this project's actual structure before `build`",
        ));
    } else {
        println!(
            "{}",
            super::po::tf(
                "done. Project '{name}' ({tname}) in {dir}.",
                &[
                    ("name", &o.name),
                    ("tname", tname),
                    ("dir", &o.dir.display().to_string())
                ],
            )
        );
    }
    if show_next {
        let cd = if o.dir == Path::new(".") {
            String::new()
        } else {
            format!("cd {} && ", o.dir.display())
        };
        println!(
            "{}",
            super::po::tf(
                "  {cd}delonix build -t {name}:dev .        # builds the image (Delonixfile)\n  \
                 {cd}delonix stack apply              # brings the app up\n  \
                 {cd}curl localhost:{port}{health}",
                &[
                    ("cd", &cd),
                    ("name", &o.name),
                    ("port", port),
                    ("health", health),
                ],
            )
        );
    }
    Ok(())
}

/// Writes a file, refusing to destroy work without `--force`.
fn write_file(path: &Path, content: &str, force: bool) -> Result<bool> {
    let root = path.parent().unwrap_or(Path::new("."));
    write_file_mode(root, path, content, force, false)
}

/// Refuses a destination reached through a symlink: in an adopted directory a
/// `.github` or `Delonixfile` that is a link would otherwise send the write
/// somewhere outside the project — `Path::exists` follows links, and a
/// dangling one even reads as "absent, go ahead".
fn refuse_symlinks(root: &Path, path: &Path) -> Result<()> {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let mut cur = root.to_path_buf();
    for comp in rel.components() {
        cur.push(comp);
        if std::fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(Error::Invalid(super::po::tf(
                "{path} is a symbolic link — refusing to write through it",
                &[("path", &cur.display().to_string())],
            )));
        }
    }
    Ok(())
}

/// [`write_file`] inside the project root `root`, never through a symlink,
/// with the executable bit when the template file had it.
fn write_file_mode(
    root: &Path,
    path: &Path,
    content: &str,
    force: bool,
    exec: bool,
) -> Result<bool> {
    refuse_symlinks(root, path)?;
    if path.exists() && !force {
        eprintln!(
            "{}",
            super::po::tf(
                "  already exists, skipped: {path}  (use --force to overwrite)",
                &[("path", &path.display().to_string())],
            )
        );
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, content).map_err(|e| {
        Error::Invalid(format!(
            "{}: {e}",
            super::po::tf("writing {path}", &[("path", &path.display().to_string())])
        ))
    })?;
    if exec {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    eprintln!(
        "{}",
        super::po::tf(
            "  created: {path}",
            &[("path", &path.display().to_string())]
        )
    );
    Ok(true)
}

/// Is stdin an interactive terminal? (menu/questions only make sense on a TTY).
fn stdin_is_tty() -> bool {
    // SAFETY: `isatty` takes an integer fd and has no preconditions.
    unsafe { libc::isatty(0) == 1 }
}

/// Numbered menu of the templates + the "generic" option. `Some(name)` =
/// template; `None` = generic scaffold. Reprompts on invalid input; accepts a
/// number or a name.
fn choose_template_interactive() -> Result<Option<String>> {
    use std::io::Write;
    let names = template_names();
    eprintln!("\nChoose a template (Enter for the generic scaffold):");
    for (i, n) in names.iter().enumerate() {
        eprintln!("  {}) {}", i + 1, n);
    }
    eprintln!(
        "  {}) generic — Delonixfile + manifest, no app code",
        names.len() + 1
    );
    loop {
        eprint!("> ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            return Ok(None);
        }
        let s = line.trim();
        if s.is_empty() {
            return Ok(None);
        }
        if let Ok(k) = s.parse::<usize>() {
            if (1..=names.len()).contains(&k) {
                return Ok(Some(names[k - 1].to_string()));
            }
            if k == names.len() + 1 {
                return Ok(None);
            }
        } else if names.contains(&canonical_template(s)) {
            return Ok(Some(canonical_template(s).to_string()));
        }
        eprintln!("invalid — pick 1..{}", names.len() + 1);
    }
}

/// Yes/no question. On non-TTY it returns the default (without blocking scripts).
fn prompt_yes(question: &str, default_yes: bool) -> bool {
    use std::io::Write;
    if !stdin_is_tty() {
        return default_yes;
    }
    eprint!(
        "{question} {} ",
        if default_yes { "[Y/n]" } else { "[y/N]" }
    );
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        return default_yes;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" | "s" | "sim" => true,
        _ => false,
    }
}

/// Builds the image, applies the generated manifest and waits until healthy —
/// each step with animated progress (like `cluster create`). A single command
/// until it is UP.
///
/// Goes through `delonix stack apply` and NOT a hand-rolled `container run`:
/// the manifest is the actual declaration (network, volumes, extra
/// containers, `restart`/`memory`/`cpus`/`env`/`readOnly`/`tmpfs`), and a
/// parallel fast path that only started the ONE main container booted
/// something that did not match what `stack apply` — the command every
/// generated README tells the user to run — would bring up. The `odoo`
/// template is the sharpest example: its own manifest comment says "Odoo
/// will not boot without the database, so `stack apply` (not a lone
/// `container run`) is the way in" — the old fast path violated that on the
/// very first `--up`.
fn build_and_up(name: &str, dir: &Path, port: &str, health: &str) -> Result<()> {
    let exe = std::env::current_exe().map_err(|e| Error::Invalid(e.to_string()))?;
    let tag = format!("{name}:dev");
    let mut p = super::output::Progress::new();

    p.step(&format!("Building image {tag}"), "🔨");
    run_quiet(&exe, dir, &["build", "-t", &tag, "."])?;
    p.ok();

    p.step(&format!("Applying the stack ({name})"), "🚀");
    run_quiet(&exe, dir, &["stack", "apply"])?;
    p.ok();

    p.step("Waiting until healthy", "❤️ ");
    wait_health(port, health)?;
    p.ok();

    println!("\n✨ {name} is UP → http://localhost:{port}{health}");
    println!("   logs:  delonix container logs -f {name}");
    println!("   stop:  delonix stack destroy   (tears down everything the stack owns)");
    Ok(())
}

/// Runs `delonix <args>` in `dir`, capturing the output (the spinner does the
/// talking). The error carries the captured output for diagnostics.
fn run_quiet(exe: &Path, dir: &Path, args: &[&str]) -> Result<()> {
    let out = std::process::Command::new(exe)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| Error::Invalid(e.to_string()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(Error::Invalid(super::po::tf(
            "`delonix {args}` failed:\n{stderr}",
            &[
                ("args", &args.join(" ")),
                ("stderr", String::from_utf8_lossy(&out.stderr).trim()),
            ],
        )))
    }
}

/// Waits for `health` to answer 200 (up to ~120s).
///
/// A ceiling, not a delay: a healthy app answers as soon as it is up. It was
/// 40s, and measured on a loaded host (load ~30) Odoo 20 took ~85s from its
/// first log line to serving HTTP — `--up` reported a failure on a stack that
/// came up on its own a minute later.
fn wait_health(port: &str, health: &str) -> Result<()> {
    for _ in 0..240 {
        if http_ok(port, health) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err(Error::Invalid(
        super::po::t(
            "the container started but did not become healthy in 120s — see `delonix container logs`",
        )
        .into(),
    ))
}

/// Minimal GET to `127.0.0.1:port` (HTTP/1.0); `true` if the response is `200`.
fn http_ok(port: &str, path: &str) -> bool {
    use std::io::{Read, Write};
    let Ok(mut s) = std::net::TcpStream::connect(format!("127.0.0.1:{port}")) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let req = format!("GET {path} HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 128];
    matches!(s.read(&mut buf), Ok(n) if String::from_utf8_lossy(&buf[..n]).contains(" 200 "))
}

pub(crate) fn init(target: Target, o: &InitOpts) -> Result<()> {
    // `--template list` only lists.
    if o.template.as_deref() == Some("list") {
        return render_template("list", o, false);
    }
    // No `--template` on an interactive TTY: open the menu. On non-TTY
    // (scripts/CI) it falls back to the generic scaffold, as always.
    // The template menu/flag is for CONTAINERIZED APPS (django/nginx/...): it
    // only makes sense in `container init`/`stack init`. In `vm init`/`cluster
    // init` the user wants the TARGET's scaffold — before this, picking a
    // template in `vm init` built and started a CONTAINER (seen live: gunicorn
    // running where a VM was expected).
    let templates_apply = matches!(target, Target::Container | Target::Stack);
    if !templates_apply && o.template.is_some() && o.template.as_deref() != Some("list") {
        return Err(delonix_model::Error::Invalid(
            super::po::t(
                "app templates scaffold containerized apps — use `delonix stack init --template <t>`; this command scaffolds a VM/cluster project",
            )
            .into(),
        ));
    }
    let chosen: Option<String> = match &o.template {
        Some(t) => Some(t.clone()),
        None if templates_apply && stdin_is_tty() => choose_template_interactive()?,
        None => None,
    };
    // Same non-negotiable as inside `render_template`: `-v` without a
    // template to apply it to (no `-t`, and none picked from the interactive
    // menu either) would otherwise be silently swallowed right here, never
    // reaching `resolve_version` at all.
    if chosen.is_none() && o.template_version.is_some() {
        return Err(Error::Invalid(
            super::po::t(
                "-v/--template-version needs a template to apply it to — pass -t/--template",
            )
            .into(),
        ));
    }
    // The generic container/stack scaffold writes the name into a manifest and
    // an image tag too; a VM/cluster scaffold keeps its own naming rules.
    if templates_apply {
        check_project_name(&o.name)?;
    }
    // A template (via flag or menu) → complete project; optionally an animated
    // build+run (`--up`, or the interactive question).
    if let Some(t) = &chosen {
        let do_up = o.up || (stdin_is_tty() && prompt_yes("Build and start it now?", true));
        render_template(t, o, !do_up)?;
        if do_up {
            let meta = template_meta(canonical_template(t));
            build_and_up(&o.name, &o.dir, meta.port, meta.health)?;
        }
        return Ok(());
    }
    std::fs::create_dir_all(&o.dir)?;
    let mut n = 0;
    match target {
        Target::Container => {
            n += usize::from(write_file(
                &o.dir.join("Delonixfile"),
                &delonixfile(o),
                o.force,
            )?);
            n += usize::from(write_file(
                &o.dir.join("delonix-manifest.yaml"),
                &manifest_container(o),
                o.force,
            )?);
        }
        Target::Vm => {
            n += usize::from(write_file(
                &o.dir.join("delonix-manifest.yaml"),
                &manifest_vm(o),
                o.force,
            )?);
        }
        Target::Cluster => {
            n += usize::from(write_file(
                &o.dir.join("cluster-kind.yaml"),
                &cluster_kind(o),
                o.force,
            )?);
            n += usize::from(write_file(
                &o.dir.join("cluster-vm.yaml"),
                &cluster_vm(o),
                o.force,
            )?);
            n += usize::from(write_file(
                &o.dir.join("cluster-ssh.yaml"),
                &cluster_ssh(o),
                o.force,
            )?);
        }
        Target::Stack => {
            n += usize::from(write_file(
                &o.dir.join("Delonixfile"),
                &delonixfile(o),
                o.force,
            )?);
            n += usize::from(write_file(
                &o.dir.join("delonix-manifest.yaml"),
                &manifest_stack(o),
                o.force,
            )?);
            n += usize::from(write_file(
                &o.dir.join("cluster-kind.yaml"),
                &cluster_kind(o),
                o.force,
            )?);
            n += usize::from(write_file(
                &o.dir.join(".dockerignore"),
                DOCKERIGNORE,
                o.force,
            )?);
            n += usize::from(write_file(&o.dir.join("README.md"), &readme(o), o.force)?);
        }
    }
    if n == 0 {
        eprintln!(
            "{}",
            super::po::t("nothing to do (everything already existed)")
        );
        return Ok(());
    }
    println!("{}", next_steps(target, o));
    Ok(())
}

fn next_steps(target: Target, o: &InitOpts) -> String {
    let cd = if o.dir == Path::new(".") {
        String::new()
    } else {
        format!("cd {} && ", o.dir.display())
    };
    match target {
        Target::Container => super::po::tf(
            "done. Now:\n  {cd}delonix build -t {name}:dev .\n  {cd}delonix container apply",
            &[("cd", &cd), ("name", &o.name)],
        ),
        Target::Vm => super::po::tf("done. Now:\n  {cd}delonix vm apply", &[("cd", &cd)]),
        Target::Cluster => super::po::tf(
            "done. Now:\n  {cd}delonix cluster create --name {name}    # local, no manifest\n  (or edit cluster-vm.yaml / cluster-ssh.yaml for VMs / remote hosts)",
            &[("cd", &cd), ("name", &o.name)],
        ),
        Target::Stack => super::po::tf(
            "done. Full project in {dir}. Now:\n  \
             {cd}delonix build -t {name}:dev .        # builds the app's image\n  \
             {cd}delonix stack apply              # network + volume + DB + app, all up\n  \
             {cd}delonix container ls",
            &[
                ("dir", &o.dir.display().to_string()),
                ("cd", &cd),
                ("name", &o.name),
            ],
        ),
    }
}

// ---------------------------------------------------------------- templates

fn delonixfile(o: &InitOpts) -> String {
    let base = o
        .image
        .clone()
        .unwrap_or_else(|| DEFAULT_APP_BASE.to_string());
    format!(
        "# Delonixfile — mesma gramática do Dockerfile, com extensões Delonix.\n\
         # Constrói com:  delonix build -t {name}:dev .\n\
         #\n\
         # NOTA: o build é single-stage. Um `FROM ... AS x` seguido doutro `FROM`\n\
         # é recusado com erro claro (multi-stage ainda não está implementado).\n\
         FROM {base}\n\
         \n\
         WORKDIR /app\n\
         \n\
         # As dependências primeiro: esta camada só é reconstruída quando elas mudam.\n\
         # RUN apk add --no-cache python3\n\
         \n\
         COPY . /app\n\
         \n\
         # Extensões Delonix (aceites pelo parser; ver AGENTS.md):\n\
         #   CPUS 2\n\
         #   MEMORY 512M\n\
         #   HEALTHCHECK CMD wget -qO- http://localhost:8080/health || exit 1\n\
         \n\
         EXPOSE 8080\n\
         CMD [\"sh\", \"-c\", \"echo 'a servir na 8080'; while true; do sleep 3600; done\"]\n",
        name = o.name,
        base = base
    )
}

fn manifest_container(o: &InitOpts) -> String {
    let img = o.image.clone().unwrap_or_else(|| format!("{}:dev", o.name));
    // COMPLETE template: the full `kind: Container` spec, with defaults and one
    // comment per field. Delete what you do not need — the parser accepts any
    // subset (only `image` is mandatory).
    CONTAINER_REFERENCE
        .replace("{name}", &o.name)
        .replace("{image}", &img)
}

/// Complete reference for `kind: Container` (all the fields that `apply` reads).
const CONTAINER_REFERENCE: &str = r#"# Apply with:  delonix container apply
apiVersion: delonix.io/v1
kind: Container
metadata:
  name: {name}
spec:
  image: {image}              # required — the only mandatory field
  # command + args override the image ENTRYPOINT/CMD (docker semantics):
  command: []                 # e.g. ["nginx", "-g", "daemon off;"]
  entrypoint: null            # override the image ENTRYPOINT ("" clears it)
  detach: true                # run in the background (a manifest is declarative)
  restart: always             # no | on-failure[:max] | always | unless-stopped
  # --- network ---
  network: host               # host | none | <a network created by `network create`>
  ports: []                   # ["8080:80"] — gives the container its own netns + slirp NAT
  networkAlias: []            # DNS aliases on the network
  knows: []                   # restrict name resolution to these containers (isolation)
  netBps: null                # egress rate limit (e.g. "10mbit"), only with a custom network
  netBurst: null              # burst for netBps
  # --- storage ---
  volumes: []                 # ["data:/var/lib", "/host/path:/inside:ro"]
  tmpfs: []                   # ["/scratch"]
  # --- resources (cgroup v2) ---
  memory: max                 # 64M | 2G | max (no cap)
  cpus: "1.0"                 # CPU cores
  cpuWeight: null             # relative CPU weight under contention (1-10000)
  cpuset: null                # pin to CPUs, e.g. "0-3"
  ioWeight: null              # relative I/O weight
  # --- env & secrets ---
  env: []                     # ["KEY=value"]
  envFile: []                 # ["./.env"]
  secret: []                  # vault secret names (see `delonix secret`); injected as env
  secretFiles: false          # true → secrets go to /run/secrets/<name> instead of env
  # --- security ---
  privileged: false           # all caps, seccomp off — trusted workloads only
  readOnly: false             # read-only rootfs (writes go to tmpfs/volumes)
  capAdd: []                  # ["NET_ADMIN"]
  capDrop: []                 # ["MKNOD"]
  securityOpt: []             # ["seccomp=unconfined", "apparmor=<profile>"]
  apparmor: null              # AppArmor profile
  selinux: null               # SELinux context
  userns: false               # force the subuid user namespace (default-on in rootless)
  hostPid: false              # share the host PID namespace
  hostIpc: false              # share the host IPC namespace
  detect: false               # seccomp in log mode — discover the syscalls a workload uses
  # --- devices & limits ---
  devices: []                 # ["/dev/fuse"]
  gpus: null                  # all | nvidia | dri
  ulimit: []                  # ["nofile=1024:2048"]
  sysctl: []                  # ["net.core.somaxconn=1024"]
  # --- misc ---
  labels: []                  # ["tier=frontend"]
  logDriver: null             # json | cri
"#;

fn manifest_vm(o: &InitOpts) -> String {
    let img = o
        .image
        .clone()
        .unwrap_or_else(|| DEFAULT_VM_IMAGE.to_string());
    VM_REFERENCE
        .replace("{name}", &o.name)
        .replace("{image}", &img)
}

/// Complete reference for `kind: VirtualMachine` (mirrors `delonix_vm::VmConfig`), plus the
/// two resources it needs around it.
///
/// **The Network is here because the VM already referenced it and nothing
/// created it.** Measured on a fresh `vm init`: the generated project failed its
/// own `delonix stack validate` with «Vm 'x' → network 'x-net' is not declared
/// nor does it exist». A scaffold whose first act is to produce something that
/// does not apply teaches the wrong thing about the tool.
///
/// **The Volume is declared and deliberately NOT attached**, and the comment on
/// it says why rather than leaving the reader to find out: `spec.volumes` needs
/// virtio-9p, which is libvirt only (the engine auto-selects it and refuses an
/// explicit `backend: cloud-hypervisor` beside volumes), while `network:` is the
/// rootless SDN, which only a Cloud Hypervisor VM joins — a libvirt domain lives
/// on the host's `virbr0`. Attaching both would generate a manifest that looks
/// right and silently drops one of them, which is the shape this engine spends
/// its time removing.
const VM_REFERENCE: &str = r#"# Apply with:  delonix stack apply   (or `delonix vm apply` for the Vm alone)
# The golden VM image is built once with:
#   delonix image vm build -t {image} --k8s-version 1.34
---
apiVersion: delonix.io/v1
kind: Network
metadata:
  name: {name}-net
spec:
  driver: bridge              # bridge | macvlan | ipvlan | overlay
  # subnet: 10.89.0.0/16      # optional — picked automatically when omitted
---
apiVersion: delonix.io/v1
kind: Volume
metadata:
  name: {name}-data
spec:
  driver: local               # local | nfs (for a NAS, use an `nfs:` block)
  # quota: 2g                 # hard cap as root, monitored in rootless
---
apiVersion: delonix.io/v1
kind: VirtualMachine
metadata:
  name: {name}
spec:
  disk: {image}               # required — base qcow2/raw (an overlay is made per VM)
  vcpus: 2
  memory: 2G                  # "2G" | "1024M" (a "…i" suffix is accepted, k8s-style)
  network: {name}-net         # the kind: Network above — the VM's tap joins it
  restart_policy: null        # no | on-failure | always
  # volumes:                  # NOT set together with `network:` above, and that
  #   - name: {name}-data     # is a real constraint, not caution: volumes need
  #     mountPath: /mnt/data  # virtio-9p (libvirt only), and a libvirt domain
  #     readOnly: false       # lives on virbr0, not on the rootless SDN that a
  #                           # kind: Network is. Pick one: keep `network:` for
  #                           # the SDN, or drop it and uncomment these for
  #                           # shared storage. `{name}-data` is declared above
  #                           # either way, and containers of this stack can use
  #                           # it with no such trade-off.
  # --- boot: firmware (cloud images) OR direct kernel ---
  firmware: null              # UEFI firmware path (typical for cloud images)
  kernel: null                # direct kernel boot (alternative to firmware)
  initrd: null                # initramfs for direct kernel boot
  cmdline: null               # kernel cmdline for direct boot
  # --- cloud-init ---
  seed: null                  # path to a prebuilt NoCloud ISO (hostname/ssh come from here)
                              # via manifest only a ready seed is accepted; use
                              # `delonix vm create --user-data …` to generate one.
  # --- performance / passthrough ---
  hugepages: false
  cpu_affinity: null          # pin vCPUs, e.g. "8-15"
  devices: []                 # VFIO PCI passthrough (sysfs paths)
  # --- backend selection ---
  backend: null               # cloud-hypervisor | libvirt | null (auto)
  net_mode: null              # libvirt only: user | nat | bridge
  bridge: null                # host bridge / libvirt network
"#;

fn manifest_stack(o: &InitOpts) -> String {
    let app = o.image.clone().unwrap_or_else(|| format!("{}:dev", o.name));
    format!(
        "# Projecto completo: volume + BD + app. Aplica TUDO com:\n\
         #   delonix stack apply\n\
         #\n\
         # Ordem de aplicação (por dependência): Network -> Volume -> Image -> Vm -> Container.\n\
         # Semântica: \"garante presente\", idempotente por nome. Fail-fast SEM rollback:\n\
         # o que já foi aplicado antes de um erro FICA aplicado.\n\
         #\n\
         # PORQUÊ SEM `network:` E SEM `ports:` — não é esquecimento, é a opção\n\
         # mais simples para um scaffold que tem de funcionar à primeira:\n\
         #   Estes containers ficam em `--net host` (o default) e falam entre si por\n\
         #   `127.0.0.1` — testado e funcional em rootless, sem nenhum passo extra\n\
         #   (`network create`).\n\
         #     * `network: <rede>` (isolamento + DNS por nome) TAMBÉM funciona em\n\
         #       rootless (re-exec `nsenter ... ip netns exec` para dentro da netns\n\
         #       do holder — ver AGENTS.md/`reexec_into_netns`). Troca para esta forma\n\
         #       quando precisares de isolamento real ou de vários stacks a partilhar\n\
         #       portas iguais; usa o NOME do container (não `127.0.0.1`) no\n\
         #       DATABASE_URL nesse caso.\n\
         #     * `ports:` dá ao container uma netns PRÓPRIA com slirp — óptimo para\n\
         #       expor ao mundo, mas o slirp corre com `--disable-host-loopback`, por\n\
         #       isso a app deixaria de alcançar a BD em `127.0.0.1` (usa `network:`\n\
         #       + o nome do container nesse caso também).\n\
         apiVersion: delonix.io/v1\n\
         kind: Volume\n\
         metadata:\n  name: {name}-data\n\
         spec: {{}}\n\
         ---\n\
         apiVersion: delonix.io/v1\n\
         kind: Container\n\
         metadata:\n  name: {name}-db\n\
         spec:\n  \
         image: {db}\n  \
         # Supervisor destacado: captura o exit code real e reinicia. Um `stop`\n  \
         # explícito é respeitado (não ressuscita).\n  \
         restart: always\n  \
         # Volume NOMEADO: os dados sobrevivem ao `rm` do container.\n  \
         volumes:\n    - \"{name}-data:/var/lib/postgresql/data\"\n  \
         env:\n    - \"POSTGRES_PASSWORD=troca-me\"\n    - \"POSTGRES_DB={name}\"\n\
         ---\n\
         apiVersion: delonix.io/v1\n\
         kind: Container\n\
         metadata:\n  name: {name}-app\n\
         spec:\n  \
         # Constrói primeiro:  delonix build -t {app} .\n  \
         image: {app}\n  \
         restart: always\n  \
         env:\n    - \"APP_ENV=dev\"\n    \
         - \"DATABASE_URL=postgres://postgres:troca-me@127.0.0.1:5432/{name}\"\n",
        name = o.name,
        db = DEFAULT_DB_IMAGE,
        app = app
    )
}

fn cluster_kind(o: &InitOpts) -> String {
    let img = o
        .image
        .clone()
        .unwrap_or_else(|| super::kindmode::DEFAULT_NODE_IMAGE.to_string());
    format!(
        "# Cluster Kubernetes LOCAL, em containers — sem Docker e sem o binário `kind`.\n\
         #   delonix cluster create --name {name}       # não precisa deste ficheiro\n\
         #   delonix cluster apply -f cluster-kind.yaml # versiona a config no git\n\
         apiVersion: delonix.io/v1\n\
         kind: KubernetesCluster\n\
         metadata:\n  name: {name}\n\
         spec:\n  \
         # kind = containers aqui | vm = VMs douradas | ssh = hosts remotos\n  \
         mode: kind\n  \
         k8sVersion: \"1.34\"\n  \
         podSubnet: 10.244.0.0/16\n  \
         serviceSubnet: 10.96.0.0/12\n  \
         # default = a CNI da imagem (kindnet); none = aplicas a tua depois\n  \
         cni: default\n  \
         controlPlane:\n    replicas: 1\n  \
         workers:\n    replicas: 0\n  \
         kind:\n    \
         # Fixada por digest: uma tag móvel tornaria o cluster irreprodutível.\n    \
         image: {img}\n    \
         apiServerPort: 6443\n",
        name = o.name,
        img = img
    )
}

fn cluster_vm(o: &InitOpts) -> String {
    format!(
        "# Cluster Kubernetes em microVMs (kernel próprio por nó — isolamento de\n\
         # hipervisor). A imagem dourada já traz kubeadm/kubelet/kubectl + delonix-cri:\n\
         # arrancar um nó NÃO instala nada.\n\
         #   delonix image vm build -t {img} --k8s-version 1.34   # uma vez\n\
         #   delonix network create {name}-net\n\
         #   delonix cluster apply -f cluster-vm.yaml\n\
         apiVersion: delonix.io/v1\n\
         kind: KubernetesCluster\n\
         metadata:\n  name: {name}\n\
         spec:\n  \
         mode: vm\n  \
         k8sVersion: \"1.34\"\n  \
         podSubnet: 10.244.0.0/16\n  \
         serviceSubnet: 10.96.0.0/12\n  \
         cni: default\n  \
         controlPlane:\n    replicas: 1   # >1 exige controlPlaneEndpoint (LB/VIP)\n  \
         workers:\n    replicas: 2\n  \
         vm:\n    \
         image: {img}\n    \
         network: {name}-net\n    \
         vcpus: 2\n    \
         memory: 2G\n    \
         bootTimeout: 300s\n",
        name = o.name,
        img = DEFAULT_VM_IMAGE
    )
}

fn cluster_ssh(o: &InitOpts) -> String {
    format!(
        "# Cluster Kubernetes em hosts remotos JÁ EXISTENTES (datacenter/bare-metal).\n\
         # O delonix NÃO cria estas máquinas: têm de estar vivas, alcançáveis por SSH,\n\
         # e o utilizador tem de ter `sudo` NOPASSWD.\n\
         #   delonix cluster apply -f cluster-ssh.yaml\n\
         #\n\
         # Idempotente SEM ficheiro de estado (\"Terraform sem .tfstate\"): cada passo\n\
         # tem um `check` e um `apply`. Correr duas vezes não faz nada de novo.\n\
         apiVersion: delonix.io/v1\n\
         kind: KubernetesCluster\n\
         metadata:\n  name: {name}\n\
         spec:\n  \
         mode: ssh\n  \
         k8sVersion: \"1.34\"\n  \
         podSubnet: 10.244.0.0/16\n  \
         serviceSubnet: 10.96.0.0/12\n  \
         # Em produção instalas normalmente a TUA CNI (Cilium, Calico...).\n  \
         cni: none\n  \
         # HA: com >1 control-plane isto é OBRIGATÓRIO (o kubeadm precisa de um\n  \
         # endereço estável à frente deles). O delonix não provisiona o LB.\n  \
         controlPlaneEndpoint: \"k8s-api.exemplo.ao:6443\"\n  \
         controlPlane:\n    hosts:\n      - address: 10.0.0.11\n      - address: 10.0.0.12\n      - address: 10.0.0.13\n  \
         workers:\n    hosts:\n      - address: 10.0.0.21\n      - address: 10.0.0.22\n  \
         ssh:\n    user: delonix\n    keyPath: ~/.ssh/id_ed25519\n    port: 22\n  \
         # Só `stacked` é suportado; `external` é recusado com erro claro.\n  \
         etcd:\n    mode: stacked\n",
        name = o.name
    )
}

const DOCKERIGNORE: &str =
    "# Fora do contexto de build (menos bytes a copiar, imagem mais pequena).\n\
                            .git\n\
                            target/\n\
                            node_modules/\n\
                            *.qcow2\n\
                            delonix-manifest.yaml\n\
                            cluster-*.yaml\n";

fn readme(o: &InitOpts) -> String {
    format!(
        "# {name}\n\n\
         Projecto Delonix — containers e Kubernetes **sem daemon e sem root**.\n\n\
         ## Arrancar\n\n\
         ```bash\n\
         delonix build -t {name}:dev .   # constrói a imagem da app (Delonixfile)\n\
         delonix stack apply             # rede + volume + BD + app\n\
         delonix container ls\n\
         delonix container logs -f {name}-app\n\
         ```\n\n\
         ## Kubernetes local\n\n\
         ```bash\n\
         delonix cluster create --name {name}    # nós em containers, sem Docker\n\
         kubectl --kubeconfig ~/.local/share/delonix/clusters/{name}-kubeconfig.yaml get nodes\n\
         delonix delete clusters {name}\n\
         ```\n\n\
         ## Ficheiros\n\n\
         | Ficheiro | O quê |\n\
         |---|---|\n\
         | `Delonixfile` | Build da imagem (gramática do Dockerfile + extensões) |\n\
         | `delonix-manifest.yaml` | Rede, volume, BD e app — `delonix stack apply` |\n\
         | `cluster-kind.yaml` | Cluster Kubernetes local |\n\n\
         ## Notas honestas\n\n\
         - O build é **single-stage** (multi-stage é recusado com erro claro).\n\
         - `stack apply` é *garante-presente*, fail-fast e **sem rollback**.\n\
         - `restart: always` é servido por um supervisor por container (não há daemon);\n\
           sem daemon, não há ressurreição no reboot do host.\n\
         - Os containers do stack ficam em `--net host` e falam por `127.0.0.1`.\n\
           Redes de utilizador (isolamento + DNS por nome) têm uma limitação\n\
           conhecida em rootless (`setns`) — só funcionam como root. Ver o\n\
           comentário no topo do `delonix-manifest.yaml`.\n",
        name = o.name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project path that does not exist yet, inside a temp dir the guard
    /// removes when the test ends.
    fn scratch() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("project");
        (tmp, dir)
    }

    #[test]
    fn dir_has_content_e_falso_para_um_caminho_que_nao_existe() {
        let (_tmp, dir) = scratch();
        assert!(
            !dir_has_content(&dir),
            "um caminho inexistente não tem conteúdo"
        );
    }

    #[test]
    fn dir_has_content_e_falso_para_um_directorio_vazio() {
        let (_tmp, dir) = scratch();
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!dir_has_content(&dir));
    }

    #[test]
    fn dir_has_content_e_verdadeiro_assim_que_ha_uma_entrada() {
        let (_tmp, dir) = scratch();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ja-tem-algo"), "x").unwrap();
        assert!(dir_has_content(&dir));
    }

    /// Um directório vazio continua a receber o scaffold INTEIRO — o caso
    /// coberto desde sempre, e o que não pode regredir com a chegada do modo
    /// de adopção.
    #[test]
    fn scaffold_num_directorio_vazio_escreve_o_projecto_completo() {
        let (_tmp, dir) = scratch();
        let o = InitOpts {
            dir: dir.clone(),
            name: "app".into(),
            image: None,
            force: false,
            template: Some("node".into()),
            template_version: None,
            up: false,
        };
        render_template("node", &o, false).unwrap();
        assert!(dir.join("Delonixfile").exists());
        assert!(dir.join("delonix-manifest.yaml").exists());
        assert!(
            dir.join("src/index.ts").exists(),
            "o scaffold completo tem de escrever o código de exemplo também"
        );
        assert!(dir.join("package.json").exists());
        assert!(
            dir.join(".github/workflows/ci.yml").exists(),
            "um scaffold novo já nasce com CI/CD pronto"
        );
    }

    /// Um directório com um projecto REAL já lá dentro só recebe o Delonixfile,
    /// o manifesto e o dockerignore — nunca o código de exemplo do template,
    /// que colidiria com (ou ficaria sem uso ao lado de) o código verdadeiro.
    #[test]
    fn adopcao_num_projecto_existente_so_escreve_o_glue() {
        let (_tmp, dir) = scratch();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        std::fs::write(dir.join("server.js"), "// o código real do utilizador").unwrap();
        let o = InitOpts {
            dir: dir.clone(),
            name: "app".into(),
            image: None,
            force: false,
            template: Some("node".into()),
            template_version: None,
            up: false,
        };
        render_template("node", &o, false).unwrap();
        assert!(dir.join("Delonixfile").exists());
        assert!(dir.join("delonix-manifest.yaml").exists());
        assert!(dir.join(".dockerignore").exists());
        assert!(
            !dir.join("src").exists(),
            "adopção nunca escreve o código de exemplo do template"
        );
        assert!(
            !dir.join("README.md").exists(),
            "adopção nunca escreve o README do template"
        );
        // O código real do utilizador sobrevive intacto.
        assert_eq!(
            std::fs::read_to_string(dir.join("server.js")).unwrap(),
            "// o código real do utilizador"
        );
    }

    /// CI/CD (workflows, SonarQube, CONTRIBUTING.md, commitlint) does not
    /// assume the demo's file layout — only the language's own commands,
    /// already read from the template's real `package.json` — so it is
    /// included in adopt mode too, unlike the Delonixfile/manifest.
    #[test]
    fn adopcao_tambem_recebe_o_ci_cd_generico() {
        let (_tmp, dir) = scratch();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        let o = InitOpts {
            dir: dir.clone(),
            name: "app".into(),
            image: None,
            force: false,
            template: Some("node".into()),
            template_version: None,
            up: false,
        };
        render_template("node", &o, false).unwrap();
        assert!(dir.join(".github/workflows/ci.yml").exists());
        assert!(dir.join(".gitlab-ci.yml").exists());
        assert!(dir.join("sonar-project.properties").exists());
        assert!(dir.join("CONTRIBUTING.md").exists());
        assert!(dir.join("commitlint.config.js").exists());
    }

    /// Re-correr `init` sobre o SEU PRÓPRIO scaffold anterior (agora não-vazio)
    /// tem de mudar para adopção — nunca voltar a despejar o código de exemplo
    /// por cima de edições que o utilizador já tenha feito.
    #[test]
    fn re_correr_init_sobre_o_proprio_output_muda_para_adopcao() {
        let (_tmp, dir) = scratch();
        let o = InitOpts {
            dir: dir.clone(),
            name: "app".into(),
            image: None,
            force: false,
            template: Some("node".into()),
            template_version: None,
            up: false,
        };
        render_template("node", &o, false).unwrap();
        std::fs::remove_dir_all(dir.join("src")).unwrap();
        render_template("node", &o, false).unwrap();
        assert!(
            !dir.join("src").exists(),
            "a segunda passagem não pode recriar o código de exemplo"
        );
    }

    /// `-v`/`--template-version` only means anything on a template that
    /// declares its own default version (`template.meta`'s `version=`) —
    /// passing it to another one has to be a clear refusal, never a value
    /// silently ignored. Exercised against `resolve_version` directly
    /// (not through a real template) because every shipped template now
    /// declares a version — this proves the guard survives even so, for
    /// whatever future template doesn't.
    #[test]
    fn v_recusa_num_template_sem_versao() {
        let err = resolve_version("future-template", Some("1.2.3"), "", "").unwrap_err();
        assert!(
            err.to_string().contains("has no version parameter"),
            "erro inesperado: {err}"
        );
    }

    /// Without `-v`, a template with no default (`""`) just gets `""` back —
    /// no refusal, because nothing was explicitly asked for.
    #[test]
    fn sem_v_e_sem_default_nao_e_erro() {
        assert_eq!(
            resolve_version("future-template", None, "", "").unwrap(),
            ""
        );
    }

    /// `-v` with NO `-t`/`--template` at all — no real template to apply it
    /// to, and `resolve_version` is never even reached — used to be silently
    /// swallowed by `init()` before ever getting there. Refused now, same
    /// non-negotiable as everywhere else this flag is checked. A non-TTY
    /// test process never opens the interactive menu, so `chosen` stays
    /// `None` deterministically here.
    #[test]
    fn v_sem_t_nenhum_e_recusado_antes_de_gerar_seja_o_que_for() {
        let (_tmp, dir) = scratch();
        let o = InitOpts {
            dir: dir.clone(),
            name: "app".into(),
            image: None,
            force: false,
            template: None,
            template_version: Some("1.2.3".into()),
            up: false,
        };
        let err = init(Target::Container, &o).unwrap_err();
        assert!(
            err.to_string().contains("needs a template to apply it to"),
            "erro inesperado: {err}"
        );
        assert!(!dir.exists(), "não devia ter gerado nada");
    }

    /// The `odoo` template substitutes `__TEMPLATE_VERSION__` with the `-v`
    /// value in EVERY file — the Delonixfile (`FROM odoo:<version>`), the
    /// `.pylintrc`/`.pylintrc-mandatory` (`valid-odoo-versions=<version>`)
    /// and `config/odoo.conf`, not just the README.
    #[test]
    fn odoo_com_v_substitui_a_versao_em_todos_os_ficheiros() {
        let (_tmp, dir) = scratch();
        let o = InitOpts {
            dir: dir.clone(),
            name: "myodoo".into(),
            image: None,
            force: false,
            template: Some("odoo".into()),
            template_version: Some("18.0".into()),
            up: false,
        };
        render_template("odoo", &o, false).unwrap();
        let delonixfile = std::fs::read_to_string(dir.join("Delonixfile")).unwrap();
        assert!(
            delonixfile.contains("FROM odoo:18.0"),
            "Delonixfile não referenciou a versão pedida:\n{delonixfile}"
        );
        let pylintrc = std::fs::read_to_string(dir.join(".pylintrc")).unwrap();
        assert!(pylintrc.contains("valid-odoo-versions=18.0"));
        assert!(
            !delonixfile.contains("__TEMPLATE_VERSION__"),
            "token não substituído"
        );
    }

    /// Without `-v`, the `odoo` template falls back to `template.meta`'s own
    /// default version — the token must never survive by anyone's omission.
    #[test]
    fn odoo_sem_v_usa_a_versao_por_omissao() {
        let (_tmp, dir) = scratch();
        let o = InitOpts {
            dir: dir.clone(),
            name: "myodoo".into(),
            image: None,
            force: false,
            template: Some("odoo".into()),
            template_version: None,
            up: false,
        };
        render_template("odoo", &o, false).unwrap();
        let delonixfile = std::fs::read_to_string(dir.join("Delonixfile")).unwrap();
        assert!(
            !delonixfile.contains("__TEMPLATE_VERSION__"),
            "sem -v, a versão por omissão do template.meta tem de preencher o token:\n{delonixfile}"
        );
        assert!(
            delonixfile.contains("FROM odoo:20.0"),
            "without -v the odoo template defaults to Odoo 20:\n{delonixfile}"
        );
        // Odoo 20 listens on 127.0.0.1 unless told otherwise, which inside a
        // container means the published port never answers. Both configs, the
        // baked-in one and the one the dev manifest bind-mounts.
        for conf in ["config/odoo.conf", "config/odoo-dev.conf"] {
            let text = std::fs::read_to_string(dir.join(conf)).unwrap();
            assert!(
                text.lines().any(|l| l.trim() == "http_interface = 0.0.0.0"),
                "{conf} must bind every interface:\n{text}"
            );
        }
    }

    /// `python` was the FastAPI template's old name. It keeps answering, and
    /// it must produce the SAME project as `fastapi` — an alias that rendered
    /// something different would be a second template under a misleading name.
    #[test]
    fn the_python_alias_renders_the_fastapi_template() {
        let render = |t: &str| {
            let (tmp, dir) = scratch();
            let o = InitOpts {
                dir: dir.clone(),
                name: "myapi".into(),
                image: None,
                force: false,
                template: Some(t.into()),
                template_version: None,
                up: false,
            };
            render_template(t, &o, false).unwrap();
            let py = std::fs::read_to_string(dir.join("pyproject.toml")).unwrap();
            let main = std::fs::read_to_string(dir.join("src/myapi/main.py")).unwrap();
            drop(tmp);
            (py, main)
        };
        let (fastapi, alias) = (render("fastapi"), render("python"));
        assert_eq!(fastapi, alias);
        assert!(fastapi.0.contains("\"fastapi==0.142.*\""), "{}", fastapi.0);
        assert!(template_names().contains(&"fastapi"));
        assert!(
            !template_names().contains(&"python"),
            "an alias is not listed as a template"
        );
    }

    /// `-v` on the `django` template pins `pyproject.toml`'s dependency as
    /// `django==<version>.*` — a bare major (`5`) or major.minor (`5.2`), the
    /// same token substituted into the README so the generated project
    /// documents what it was actually pinned to.
    #[test]
    fn django_com_v_fixa_a_dependencia_como_wildcard() {
        let (_tmp, dir) = scratch();
        let o = InitOpts {
            dir: dir.clone(),
            name: "myapp".into(),
            image: None,
            force: false,
            template: Some("django".into()),
            template_version: Some("5.2".into()),
            up: false,
        };
        render_template("django", &o, false).unwrap();
        let pyproject = std::fs::read_to_string(dir.join("pyproject.toml")).unwrap();
        assert!(
            pyproject.contains("\"django==5.2.*\""),
            "pyproject.toml não fixou a versão pedida:\n{pyproject}"
        );
        assert!(
            !pyproject.contains("__TEMPLATE_VERSION__"),
            "token não substituído"
        );
        let readme = std::fs::read_to_string(dir.join("README.md")).unwrap();
        assert!(readme.contains("django==5.2.*"));
    }

    /// Without `-v`, `django` falls back to `template.meta`'s own default
    /// (`5.1`) — never a literal `>=5.1` again (that let a future major in
    /// unannounced) and never the raw token surviving into the file.
    #[test]
    fn django_sem_v_usa_a_versao_por_omissao() {
        let (_tmp, dir) = scratch();
        let o = InitOpts {
            dir: dir.clone(),
            name: "myapp".into(),
            image: None,
            force: false,
            template: Some("django".into()),
            template_version: None,
            up: false,
        };
        render_template("django", &o, false).unwrap();
        let pyproject = std::fs::read_to_string(dir.join("pyproject.toml")).unwrap();
        assert!(
            pyproject.contains("\"django==5.1.*\""),
            "sem -v, devia cair no default do template.meta:\n{pyproject}"
        );
        assert!(!pyproject.contains("__TEMPLATE_VERSION__"));
    }

    /// A bare major (`-v 5`, no minor) is a valid form too — it pins
    /// `django==5.*`, i.e. "any 5.x", not a specific minor.
    #[test]
    fn django_com_v_major_nu_fixa_qualquer_5x() {
        let (_tmp, dir) = scratch();
        let o = InitOpts {
            dir: dir.clone(),
            name: "myapp".into(),
            image: None,
            force: false,
            template: Some("django".into()),
            template_version: Some("5".into()),
            up: false,
        };
        render_template("django", &o, false).unwrap();
        let pyproject = std::fs::read_to_string(dir.join("pyproject.toml")).unwrap();
        assert!(
            pyproject.contains("\"django==5.*\""),
            "um major nu devia fixar `==5.*`, não uma versão específica:\n{pyproject}"
        );
    }

    /// `-v` on `fastapi` pins `pyproject.toml`'s FastAPI dependency the same
    /// way `django` pins its own — `==X.Y.*`, not an unbounded `>=X.Y`.
    #[test]
    fn fastapi_with_v_pins_fastapi_as_a_wildcard() {
        let (_tmp, dir) = scratch();
        let o = InitOpts {
            dir: dir.clone(),
            name: "myapp".into(),
            image: None,
            force: false,
            template: Some("fastapi".into()),
            template_version: Some("0.116".into()),
            up: false,
        };
        render_template("fastapi", &o, false).unwrap();
        let pyproject = std::fs::read_to_string(dir.join("pyproject.toml")).unwrap();
        assert!(
            pyproject.contains("\"fastapi==0.116.*\""),
            "pyproject.toml não fixou o fastapi pedido:\n{pyproject}"
        );
        assert!(!pyproject.contains("__TEMPLATE_VERSION__"));
    }

    /// `-v` on `go` has no framework to pin — it fixes the TOOLCHAIN, in
    /// BOTH `go.mod` (what `go build` enforces) and the Delonixfile's `FROM`
    /// (what actually builds it). The two must never disagree.
    #[test]
    fn go_com_v_fixa_o_toolchain_no_gomod_e_no_delonixfile() {
        let (_tmp, dir) = scratch();
        let o = InitOpts {
            dir: dir.clone(),
            name: "myapp".into(),
            image: None,
            force: false,
            template: Some("go".into()),
            template_version: Some("1.22".into()),
            up: false,
        };
        render_template("go", &o, false).unwrap();
        let gomod = std::fs::read_to_string(dir.join("go.mod")).unwrap();
        assert!(
            gomod.contains("go 1.22"),
            "go.mod não fixou a versão:\n{gomod}"
        );
        let delonixfile = std::fs::read_to_string(dir.join("Delonixfile")).unwrap();
        assert!(
            delonixfile.contains("FROM golang:1.22-alpine"),
            "Delonixfile não fixou o toolchain:\n{delonixfile}"
        );
    }

    /// `-v` on `laravel` pins `composer.json`'s `laravel/framework` — the PHP
    /// version stays independent (it tracks the FrankenPHP base image, not `-v`).
    #[test]
    fn laravel_com_v_fixa_o_framework_sem_tocar_no_php() {
        let (_tmp, dir) = scratch();
        let o = InitOpts {
            dir: dir.clone(),
            name: "myapp".into(),
            image: None,
            force: false,
            template: Some("laravel".into()),
            template_version: Some("11".into()),
            up: false,
        };
        render_template("laravel", &o, false).unwrap();
        let composer = std::fs::read_to_string(dir.join("composer.json")).unwrap();
        assert!(
            composer.contains("\"laravel/framework\": \"^11\""),
            "composer.json não fixou o framework pedido:\n{composer}"
        );
        assert!(
            composer.contains("\"php\": \"^8.2\""),
            "a versão do PHP não devia mexer com -v:\n{composer}"
        );
        assert!(!composer.contains("__TEMPLATE_VERSION__"));
    }

    /// `-v` on `node` pins `package.json`'s `fastify` dependency.
    #[test]
    fn node_com_v_fixa_o_fastify() {
        let (_tmp, dir) = scratch();
        let o = InitOpts {
            dir: dir.clone(),
            name: "myapp".into(),
            image: None,
            force: false,
            template: Some("node".into()),
            template_version: Some("5.1.0".into()),
            up: false,
        };
        render_template("node", &o, false).unwrap();
        let pkg = std::fs::read_to_string(dir.join("package.json")).unwrap();
        assert!(
            pkg.contains("\"fastify\": \"^5.1.0\""),
            "package.json não fixou o fastify pedido:\n{pkg}"
        );
        assert!(!pkg.contains("__TEMPLATE_VERSION__"));
    }

    /// `-v` on `nextjs` pins `package.json`'s `next` dependency.
    #[test]
    fn nextjs_com_v_fixa_o_next() {
        let (_tmp, dir) = scratch();
        let o = InitOpts {
            dir: dir.clone(),
            name: "myapp".into(),
            image: None,
            force: false,
            template: Some("nextjs".into()),
            template_version: Some("14.2.0".into()),
            up: false,
        };
        render_template("nextjs", &o, false).unwrap();
        let pkg = std::fs::read_to_string(dir.join("package.json")).unwrap();
        assert!(
            pkg.contains("\"next\": \"^14.2.0\""),
            "package.json não fixou o next pedido:\n{pkg}"
        );
        assert!(!pkg.contains("__TEMPLATE_VERSION__"));
    }

    /// `-v` on `nestjs` pins EVERY `@nestjs/*` package to the SAME version —
    /// core, common, platform-express and the CLI move together, never a
    /// mixed-major install.
    #[test]
    fn nestjs_com_v_fixa_todos_os_pacotes_nestjs_juntos() {
        let (_tmp, dir) = scratch();
        let o = InitOpts {
            dir: dir.clone(),
            name: "myapp".into(),
            image: None,
            force: false,
            template: Some("nestjs".into()),
            template_version: Some("10.0.0".into()),
            up: false,
        };
        render_template("nestjs", &o, false).unwrap();
        let pkg = std::fs::read_to_string(dir.join("package.json")).unwrap();
        for pkg_name in [
            "@nestjs/common",
            "@nestjs/core",
            "@nestjs/platform-express",
            "@nestjs/cli",
        ] {
            assert!(
                pkg.contains(&format!("\"{pkg_name}\": \"^10.0.0\"")),
                "{pkg_name} não ficou fixado à mesma versão:\n{pkg}"
            );
        }
        assert!(!pkg.contains("__TEMPLATE_VERSION__"));
    }

    /// `-v` on the pure-config templates (`nginx`/`httpd`/`haproxy`) pins the
    /// upstream image tag, the same idiom as `odoo` — never a dependency file.
    #[test]
    fn nginx_httpd_haproxy_com_v_fixam_a_tag_da_imagem() {
        for (tpl, from_prefix) in [
            ("nginx", "FROM nginx:"),
            ("httpd", "FROM httpd:"),
            ("haproxy", "FROM haproxy:"),
        ] {
            let (_tmp, dir) = scratch();
            let o = InitOpts {
                dir: dir.clone(),
                name: "myapp".into(),
                image: None,
                force: false,
                template: Some(tpl.into()),
                template_version: Some("9.9".into()),
                up: false,
            };
            render_template(tpl, &o, false).unwrap();
            let delonixfile = std::fs::read_to_string(dir.join("Delonixfile")).unwrap();
            assert!(
                delonixfile.contains(&format!("{from_prefix}9.9-alpine")),
                "{tpl}: Delonixfile não fixou a tag pedida:\n{delonixfile}"
            );
            assert!(
                !delonixfile.contains("__TEMPLATE_VERSION__"),
                "{tpl}: token não substituído"
            );
        }
    }

    fn opts(dir: PathBuf, name: &str, template: &str, v: Option<&str>) -> InitOpts {
        InitOpts {
            dir,
            name: name.into(),
            image: None,
            force: false,
            template: Some(template.into()),
            template_version: v.map(String::from),
            up: false,
        }
    }

    /// The name lands in package.json, go.mod, composer.json, YAML and an image
    /// tag at once; before this check `My App"x` produced an unparsable
    /// package.json with exit 0.
    #[test]
    fn a_project_name_must_be_a_dns_label_and_the_error_suggests_one() {
        for ok in ["a", "my-svc", "svc2", &"x".repeat(63)] {
            assert!(check_project_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "My App",
            "my_app",
            "-a",
            "a-",
            "caf\u{e9}",
            "a\"b",
            &"x".repeat(64),
        ] {
            assert!(check_project_name(bad).is_err(), "{bad:?} accepted");
        }
        assert_eq!(slug("My App\"x"), "my-app-x");
        assert_eq!(slug("___"), "app");
        let (_tmp, dir) = scratch();
        let err = render_template("go", &opts(dir.clone(), "Weird Go", "go", None), false);
        assert!(
            err.is_err() && !dir.exists(),
            "a refused name must write nothing"
        );
    }

    /// `-v` is copied into manifests; only a plain version inside the
    /// template's validated range passes.
    #[test]
    fn a_version_must_be_plain_and_inside_the_declared_range() {
        assert!(check_version("t", "5", "").is_ok());
        assert!(check_version("t", "5.2.3", "").is_ok());
        for bad in [
            "",
            "5.",
            ".5",
            "5.2.3.4",
            "v5",
            "5.2.*\", \"evil==1",
            "latest",
            "1234567",
        ] {
            assert!(check_version("t", bad, "").is_err(), "{bad:?} accepted");
        }
        assert!(check_version("t", "5.2", "5.2,6.0").is_ok());
        assert!(
            check_version("t", "5.2.7", "5.2,6.0").is_ok(),
            "a patch of a listed minor"
        );
        assert!(
            check_version("t", "5.20", "5.2,6.0").is_err(),
            "5.20 is not 5.2"
        );
        let e = check_version("t", "4.2", "5.2,6.0")
            .unwrap_err()
            .to_string();
        assert!(e.contains("5.2,6.0"), "{e}");
    }

    /// A symlink among the destinations refuses the whole render before the
    /// first file is written.
    #[test]
    fn a_symlinked_destination_refuses_the_render_and_writes_nothing() {
        let (tmp, dir) = scratch();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        let outside = tmp.path().join("outside");
        std::os::unix::fs::symlink(&outside, dir.join("Delonixfile")).unwrap();
        let before: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        let err = render_template("go", &opts(dir.clone(), "sl", "go", None), false);
        assert!(err.unwrap_err().to_string().contains("symbolic link"));
        let after: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            before.len(),
            after.len(),
            "a file was written before the refusal"
        );
        assert!(!outside.exists(), "the write went through the link");
    }

    #[test]
    fn an_executable_template_file_stays_executable() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, dir) = scratch();
        let f = dir.join("run.sh");
        std::fs::create_dir_all(&dir).unwrap();
        write_file_mode(&dir, &f, "#!/bin/sh\n", false, true).unwrap();
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    /// Adoption writes CI only into a project that has none and uses the
    /// template's package manager.
    #[test]
    fn adopted_ci_needs_no_existing_ci_and_the_templates_lock() {
        let (_tmp, dir) = scratch();
        std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
        assert!(
            adopt_ci_skip_reason(&dir, "pnpm-lock.yaml").is_some(),
            "no lock"
        );
        std::fs::write(dir.join("pnpm-lock.yaml"), "").unwrap();
        assert!(adopt_ci_skip_reason(&dir, "pnpm-lock.yaml").is_none());
        std::fs::write(dir.join(".github/workflows/test.yml"), "").unwrap();
        assert!(
            adopt_ci_skip_reason(&dir, "pnpm-lock.yaml").is_some(),
            "own CI"
        );
        assert!(adopt_ci_skip_reason(&dir, "").is_some());
    }
}
