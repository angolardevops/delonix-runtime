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

/// What the person generating may settle on the command line instead of at
/// the prompt: the ports the project publishes and the names its certificate
/// covers. One struct, flattened into every `init` that takes a template, so
/// the three entry points cannot grow different flags.
#[derive(clap::Args, Clone, Debug, Default)]
pub(crate) struct EdgeArgs {
    /// Host and container port of the service, instead of the template's own.
    #[arg(long)]
    pub port: Option<u16>,
    /// HTTPS port, on a template that serves TLS (nginx, httpd, haproxy).
    #[arg(long = "tls-port")]
    pub tls_port: Option<u16>,
    /// Extra host name or address for the generated TLS certificate (repeatable; localhost is always included).
    #[arg(long)]
    pub hostname: Vec<String>,
}

impl EdgeArgs {
    fn is_empty(&self) -> bool {
        self.port.is_none() && self.tls_port.is_none() && self.hostname.is_empty()
    }
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
    /// `--port`/`--tls-port`/`--hostname`.
    pub edge: EdgeArgs,
}

/// Names of the available templates, for `--help`/errors.
pub(crate) fn template_names() -> Vec<&'static str> {
    TEMPLATES.iter().map(|(n, _)| *n).collect()
}

/// Whether a template supports TLS (has `tls=` in its `template.meta`).
/// Used by autocomplete to filter unsupported `--tls-port` / `--hostname` flags.
pub(crate) fn template_supports_tls(template_name: &str) -> bool {
    !template_meta(canonical_template(template_name))
        .tls_port
        .is_empty()
}

/// Whether a template has a version parameter (`version=` in its `template.meta`).
/// Used by autocomplete to filter unsupported `-v` / `--template-version` flags.
pub(crate) fn template_has_version(template_name: &str) -> bool {
    template_meta(canonical_template(template_name)).has_version
}

/// Whether this names a real template — an alias counts, since it resolves to
/// one. Completion asks BEFORE filtering flags: `template_meta` answers for an
/// unknown name with its own defaults (no version, no TLS), so without this a
/// typo — or a half-typed name, which is the common case under TAB — would
/// hide every flag the template "does not support".
pub(crate) fn template_is_known(template_name: &str) -> bool {
    let name = canonical_template(template_name);
    TEMPLATES.iter().any(|(n, _)| *n == name)
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
    /// Seconds `--up` waits for the health path to answer 200.
    wait_secs: u64,
    /// TLS port (`""` when the template does not serve TLS).
    tls_port: &'static str,
    /// Whether the template has a version parameter.
    has_version: bool,
}

/// Default `8000` + the apps' health + no version if the template does not
/// declare one.
fn template_meta(name: &str) -> TemplateMeta {
    TEMPLATE_META
        .iter()
        .find(|m| m.0 == name)
        .map(
            |&(_, port, health, version, versions, lock, wait_secs, tls_port)| TemplateMeta {
                port,
                health,
                version,
                versions,
                lock,
                wait_secs,
                tls_port,
                has_version: !version.is_empty(),
            },
        )
        .unwrap_or(TemplateMeta {
            port: "8000",
            health: "/api/v1/health/live",
            version: "",
            versions: "",
            lock: "",
            wait_secs: 120,
            tls_port: "",
            has_version: false,
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

/// The project name of a container/stack scaffold: `--name` exactly as given
/// (and refused later when it is not usable), otherwise the DIRECTORY name.
///
/// A directory is named for people (`My App`, `Shop_API`), so a derived name
/// that is not usable is turned into the nearest valid one and SAID, instead
/// of refusing a directory the operator did not name for this tool. An
/// explicit `--name` is never rewritten: a value someone typed is either used
/// or refused.
///
/// `canonicalize` cannot be used: the directory may not exist yet (it is
/// `init` that creates it). `.`/empty resolve to the cwd; a new path uses its
/// basename.
/// The line `init` prints when every file it would write is already there.
///
/// `{dir}` is `.` on the commonest invocation of all — `delonix init` with no
/// argument — and the sentence puts a full stop right after it, so the line
/// came out as `already exist in ..`: a path and a punctuation mark that read
/// as a typo, or as a parent directory. Canonicalizing names the directory the
/// message is actually talking about, which is the useful thing to print
/// anyway; `.` told the operator nothing they did not type.
///
/// Unlike [`project_name`] above, canonicalize is safe here: this line is only
/// reached once the files have been found ON DISK, so the directory exists. It
/// still falls back to the path as given rather than refusing to print.
fn already_complete_line(dir: &Path) -> String {
    let shown = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    super::po::tf(
        "nothing to do — all files already exist in {dir}. To update them, use --force:\n  delonix init --force",
        &[("dir", &shown.display().to_string())],
    )
}

pub(crate) fn project_name(explicit: Option<String>, dir: &Path) -> String {
    if let Some(n) = explicit {
        return n;
    }
    let p = if dir.as_os_str().is_empty() || dir == Path::new(".") {
        std::env::current_dir().ok()
    } else {
        Some(dir.to_path_buf())
    };
    let raw = p
        .as_deref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "app".to_string());
    if check_project_name(&raw).is_ok() {
        return raw;
    }
    let derived = slug(&raw);
    super::output::info(&super::po::tf(
        "directory '{dir}' is not usable as a project name; using '{name}' (choose another with --name)",
        &[("dir", &raw), ("name", &derived)],
    ));
    derived
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

/// Substitutes the `__NAME__`/`__MODULE__`/`__PORT__`/`__TLS_PORT__`/
/// `__TEMPLATE_VERSION__` tokens (in contents AND paths).
fn subst(s: &str, o: &InitOpts, module: &str, plan: &Plan, version: &str) -> String {
    s.replace("__NAME__", &o.name)
        .replace("__MODULE__", module)
        .replace("__TLS_PORT__", plan.tls_port.as_deref().unwrap_or(""))
        .replace("__PORT__", &plan.port)
        .replace("__TEMPLATE_VERSION__", version)
}

/// An optional key of a template's `template.meta` (`tls=`, `open=`, `login=`,
/// `password=`, `wait=`). `None` when the template does not declare it.
fn meta_kv(tname: &str, key: &str) -> Option<&'static str> {
    TEMPLATE_KV
        .iter()
        .find(|(n, _)| *n == tname)
        .and_then(|(_, kv)| kv.iter().find(|(k, _)| *k == key))
        .map(|(_, v)| *v)
}

/// What the generator decided for this project beyond the template's own
/// defaults: the ports it will publish and the names its certificate covers.
/// The template's `template.meta` gives the defaults; an interactive run may
/// change them (a busy port, a real host name).
pub(crate) struct Plan {
    pub port: String,
    /// The HTTPS port, for a template that declares `tls=<port>`.
    pub tls_port: Option<String>,
    /// Names and addresses the generated certificate is valid for.
    pub hosts: Vec<String>,
}

/// The names a locally generated certificate always covers.
const LOCAL_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

impl Plan {
    /// The template's own defaults, with nothing asked.
    fn defaults(tname: &str) -> Self {
        Plan {
            port: template_meta(tname).port.to_string(),
            tls_port: meta_kv(tname, "tls").map(String::from),
            hosts: LOCAL_HOSTS.iter().map(|h| h.to_string()).collect(),
        }
    }
}

/// The first port at or after `from` that nothing on this host holds.
fn next_free_port(from: u16) -> u16 {
    (from..=u16::MAX)
        .find(|p| !delonix_sdn::host_port_busy("127.0.0.1", *p, delonix_sdn::Proto::Tcp))
        .unwrap_or(from)
}

/// Asks one question with a default; Enter (or a closed stdin) keeps it.
fn prompt_line(question: &str, default: &str) -> String {
    use std::io::Write;
    eprint!("{question} [{default}] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        return default.to_string();
    }
    match line.trim() {
        "" => default.to_string(),
        s => s.to_string(),
    }
}

/// A port the project wants that is already held: on a terminal the user
/// picks another (the next free one is offered); otherwise it is kept and
/// said — `--up` refuses later with the owner's name, and a project generated
/// for later use may well find the port free by then.
fn settle_port(what: &str, wanted: &str, taken: &[u16], interactive: bool) -> String {
    let Ok(n) = wanted.parse::<u16>() else {
        return wanted.to_string();
    };
    if !delonix_sdn::host_port_busy("127.0.0.1", n, delonix_sdn::Proto::Tcp) && !taken.contains(&n)
    {
        return wanted.to_string();
    }
    let mut free = next_free_port(n.saturating_add(1));
    while taken.contains(&free) {
        free = next_free_port(free.saturating_add(1));
    }
    if !interactive {
        super::output::warn(&super::po::tf(
            "{what} port {port} is already in use on this host — the project is generated with it anyway; port {free} is free",
            &[("what", what), ("port", wanted), ("free", &free.to_string())],
        ));
        return wanted.to_string();
    }
    loop {
        let answer = prompt_line(
            &super::po::tf(
                "{what} port {port} is already in use. Which port instead?",
                &[("what", what), ("port", wanted)],
            ),
            &free.to_string(),
        );
        match answer.parse::<u16>() {
            Ok(p)
                if p > 0
                    && !delonix_sdn::host_port_busy("127.0.0.1", p, delonix_sdn::Proto::Tcp)
                    && !taken.contains(&p) =>
            {
                return p.to_string()
            }
            _ => eprintln!(
                "{}",
                super::po::tf(
                    "port {port} is not usable — pick a free one",
                    &[("port", &answer)]
                )
            ),
        }
    }
}

/// A name a certificate can carry: a DNS name or an IP literal. Refuses
/// anything else, because the list ends up on a `mkcert` command line.
fn valid_cert_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && !h.starts_with('-')
        && h.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '*'))
}

/// Settles what a template leaves to the person generating the project, and
/// then says whether this host can publish what was settled.
///
/// The warning lives HERE and not only in the `--up` preflight because the
/// common flow is to generate now and `stack apply` later: there the refusal
/// lands long after the flag was given, with the image already built. A
/// warning and not a refusal, because a project may legitimately be generated
/// for a host whose threshold is lower than this one's.
fn plan_for(tname: &str, interactive: bool, edge: &EdgeArgs) -> Result<Plan> {
    let plan = settle_plan(tname, interactive, edge)?;
    for (flag, port) in std::iter::once(("--port", &plan.port))
        .chain(plan.tls_port.iter().map(|t| ("--tls-port", t)))
    {
        if let Ok(n) = port.parse::<u16>() {
            if !delonix_sdn::can_bind_host_port("127.0.0.1", n, delonix_sdn::Proto::Tcp) {
                super::output::warn(&privileged_port_message(flag, port));
            }
        }
    }
    Ok(plan)
}

/// Settles what a template leaves to the person generating the project.
/// Nothing is asked off a terminal: the defaults are the answer there.
fn settle_plan(tname: &str, interactive: bool, edge: &EdgeArgs) -> Result<Plan> {
    let mut plan = Plan::defaults(tname);
    // A flag is an answer: what it settles is neither asked nor second-guessed.
    plan.port = match edge.port {
        Some(p) => p.to_string(),
        None => settle_port("HTTP", &plan.port, &[], interactive),
    };
    let Some(tls) = plan.tls_port.clone() else {
        if edge.tls_port.is_some() || !edge.hostname.is_empty() {
            return Err(Error::Invalid(super::po::tf(
                "template '{tname}' does not serve TLS — drop --tls-port/--hostname",
                &[("tname", tname)],
            )));
        }
        return Ok(plan);
    };
    let taken: Vec<u16> = plan.port.parse().into_iter().collect();
    let tls = match edge.tls_port {
        Some(p) => p.to_string(),
        None => settle_port("HTTPS", &tls, &taken, interactive),
    };
    if tls == plan.port {
        return Err(Error::Invalid(super::po::tf(
            "the HTTP and the HTTPS port are both {port} — they must differ",
            &[("port", &tls)],
        )));
    }
    plan.tls_port = Some(tls);
    let mut add = |h: &str, strict: bool| -> Result<()> {
        if !valid_cert_host(h) {
            let msg = super::po::tf(
                "'{host}' is not a host name or an address — left out of the certificate",
                &[("host", h)],
            );
            if strict {
                return Err(Error::Invalid(super::po::tf(
                    "--hostname {host}: not a host name or an address",
                    &[("host", h)],
                )));
            }
            super::output::warn(&msg);
        } else if !plan.hosts.iter().any(|x| x == h) {
            plan.hosts.push(h.to_string());
        }
        Ok(())
    };
    if !edge.hostname.is_empty() {
        for h in &edge.hostname {
            add(h, true)?;
        }
    } else if interactive {
        let answer = prompt_line(
            super::po::t(
                "Extra host names for the TLS certificate, space-separated (localhost is always included)",
            ),
            "none",
        );
        for h in answer
            .split([' ', ','])
            .filter(|h| !h.is_empty() && *h != "none")
        {
            add(h, false)?;
        }
    }
    Ok(plan)
}

/// Where a project's certificate came from.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum CertSource {
    /// Signed by this machine's `mkcert` CA.
    Mkcert,
    /// Self-signed by this binary: no tool on the host could do better.
    SelfSigned,
    /// Already in `./tls`; left alone.
    Kept,
}

/// Writes `bytes` to `path` readable by the owner only, replacing what is there.
/// Renders a `.env.tmpl` into `<dir>/.env`: the named secrets are filled in
/// and the file is written 0600. An existing `.env` is NEVER overwritten (not
/// even with `--force`) — the secrets in it may already be in use, and silently
/// rotating them is worse than refusing. Returns whether a file was written.
fn write_env_file(dir: &Path, template: &str) -> Result<bool> {
    let dest = dir.join(".env");
    refuse_symlinks(dir, &dest)?;
    if dest.exists() {
        super::output::warn(super::po::t(
            ".env already exists — kept as is (delete it to regenerate the secrets)",
        ));
        return Ok(false);
    }
    write_private(&dest, fill_env_secrets(template)?.as_bytes())?;
    Ok(true)
}

/// Replaces every `__SECRET_<NAME>__` token with a fresh URL-safe random value;
/// the SAME `<NAME>` gets the SAME value (so a password reused in a connection
/// URL and in the service's own variable match), a DIFFERENT name an
/// independent one. URL-safe base64, so a secret is safe inside a `scheme://`.
fn fill_env_secrets(template: &str) -> Result<String> {
    let mut names: Vec<String> = Vec::new();
    let mut rest = template;
    while let Some(p) = rest.find("__SECRET_") {
        let body = &rest[p + "__SECRET_".len()..];
        let Some(end) = body.find("__") else { break };
        let name = body[..end].to_string();
        if !names.contains(&name) {
            names.push(name);
        }
        rest = &body[end + 2..];
    }
    let mut out = template.to_string();
    for name in names {
        out = out.replace(&format!("__SECRET_{name}__"), &random_secret(24)?);
    }
    Ok(out)
}

/// `n` random bytes as URL-safe base64 (no padding). From `/dev/urandom`, no
/// fallback: a secret that is not random is worse than an error.
fn random_secret(n: usize) -> Result<String> {
    use base64::Engine;
    use std::io::Read;
    let mut bytes = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::remove_file(path);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    Ok(())
}

/// Makes sure `<dir>/tls` holds a certificate and its key for `hosts`:
/// `tls.crt`, `tls.key`, and `tls.pem` (both, for servers that want one file).
///
/// The files are mounted into the container and never enter the image. An
/// existing pair is kept — a certificate somebody installed (a public one, a
/// corporate one) is not ours to replace on a re-run.
///
/// `mkcert` is preferred when the host has it: its certificates are trusted by
/// this machine's browsers once `mkcert -install` has been run. Without it the
/// certificate is self-signed here, which serves HTTPS just the same and makes
/// a browser warn.
fn ensure_tls(dir: &Path, hosts: &[String]) -> Result<CertSource> {
    // A unit test must not reach the developer's own mkcert: the first call
    // creates a CA under the home directory, outside anything a test owns.
    let mkcert = if cfg!(test) {
        "mkcert-not-in-tests"
    } else {
        "mkcert"
    };
    ensure_tls_with(dir, hosts, mkcert)
}

/// [`ensure_tls`] with the `mkcert` program named by the caller.
fn ensure_tls_with(dir: &Path, hosts: &[String], mkcert: &str) -> Result<CertSource> {
    let tls = dir.join("tls");
    std::fs::create_dir_all(&tls)?;
    let (crt, key, pem) = (
        tls.join("tls.crt"),
        tls.join("tls.key"),
        tls.join("tls.pem"),
    );
    let source = if crt.exists() && key.exists() {
        CertSource::Kept
    } else {
        let by_mkcert = std::process::Command::new(mkcert)
            .arg("-cert-file")
            .arg(&crt)
            .arg("-key-file")
            .arg(&key)
            .arg("--")
            .args(hosts)
            .output()
            .is_ok_and(|o| o.status.success() && crt.exists() && key.exists());
        if by_mkcert {
            CertSource::Mkcert
        } else {
            let m = super::ingress_proxy::self_signed_pem(hosts)?;
            std::fs::write(&crt, m.cert_pem)?;
            write_private(&key, m.key_pem.as_bytes())?;
            CertSource::SelfSigned
        }
    };
    let mut both = std::fs::read(&crt)?;
    both.extend(std::fs::read(&key)?);
    write_private(&pem, &both)?;
    Ok(source)
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
const CI_FILES: [&str; 6] = [
    ".github/workflows/ci.yml",
    ".github/dependabot.yml",
    ".gitlab-ci.yml",
    "sonar-project.properties",
    "commitlint.config.js",
    // `.mjs` where the package is CommonJS (nextjs, nestjs): without it the
    // commitlint config was written into an adopted project whose CI was
    // deliberately skipped — a config for a gate that is not there.
    "commitlint.config.mjs",
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

const ADOPT_FILES: [&str; 10] = [
    "Delonixfile",
    "delonix-manifest.yaml",
    "delonix-tunnel.yaml",
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
    let plan = Plan::defaults(canonical_template(tname));
    render_planned(tname, o, show_next, &plan).map(|_| ())
}

/// [`render_template`] with the ports and certificate names already settled.
/// Returns where the certificate came from, for a template that serves TLS.
fn render_planned(
    tname: &str,
    o: &InitOpts,
    show_next: bool,
    plan: &Plan,
) -> Result<Option<CertSource>> {
    let tname = canonical_template(tname);
    if tname == "list" {
        println!(
            "{}: {}",
            super::po::t("available templates"),
            template_names().join(", ")
        );
        return Ok(None);
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
    let health = meta.health;
    let port = plan.port.as_str();
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
    let all_outputs: Vec<(PathBuf, String, bool)> = files
        .iter()
        .filter(|(rel, _, _)| !adopt || ADOPT_FILES.contains(rel))
        .filter(|(rel, _, _)| ci_skip.is_none() || !CI_FILES.contains(rel))
        .map(|(rel, content, exec)| {
            (
                o.dir.join(subst(rel, o, &module, plan, version)),
                subst(content, o, &module, plan, version),
                *exec,
            )
        })
        .collect();
    // `.env.tmpl` is not written as-is: it is the SOURCE of the project's
    // `.env`, rendered once with fresh random secrets (`__SECRET_<NAME>__`),
    // written 0600, and never overwritten. Everything else is a plain write.
    let is_env_tmpl = |dest: &Path| dest.file_name().and_then(|s| s.to_str()) == Some(".env.tmpl");
    let (env_tmpl, outputs): (Vec<_>, Vec<_>) = all_outputs
        .into_iter()
        .partition(|(dest, _, _)| is_env_tmpl(dest));
    // Every destination is checked before the first write, so a refusal
    // leaves the directory exactly as it was.
    for (dest, _, _) in &outputs {
        refuse_symlinks(&o.dir, dest)?;
    }
    refuse_foreign_project(&outputs, tname, &o.name, &o.dir, o.force)?;
    std::fs::create_dir_all(&o.dir)?;
    let mut n = 0;
    for (dest, content, exec) in &outputs {
        n += usize::from(write_file_mode(&o.dir, dest, content, o.force, *exec)?);
    }
    for (_, content, _) in &env_tmpl {
        n += usize::from(write_env_file(&o.dir, content)?);
    }
    if let Some(why) = &ci_skip {
        super::output::warn(&super::po::tf(
            "CI files not written: {why}",
            &[("why", why)],
        ));
    }
    // The manifest mounts `./tls`, so the certificate has to exist before the
    // first `stack apply` — with or without `--up`.
    let cert = match plan.tls_port {
        Some(_) => {
            let source = ensure_tls(&o.dir, &plan.hosts)?;
            eprintln!("  {}", cert_line(source, &plan.hosts));
            Some(source)
        }
        None => None,
    };
    if n == 0 {
        eprintln!("{}", already_complete_line(&o.dir));
        return Ok(cert);
    }
    // The project is complete here: a repository with everything in it is the
    // starting point, not a chore. Adoption is left alone — that project has
    // its own history.
    if !adopt {
        if let Some(why) = git_init(&o.dir) {
            super::output::warn(&why);
        }
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
        if has_tunnel(tname) {
            println!(
                "{}",
                super::po::tf(
                    "  {cd}delonix stack apply -f delonix-tunnel.yaml   # optional: a public https address, no public IP needed (README.md)",
                    &[("cd", &cd)],
                )
            );
        }
    }
    Ok(cert)
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

/// One line saying where the certificate came from and what that means for a
/// browser — said at generation, and again when the project is up.
fn cert_line(source: CertSource, hosts: &[String]) -> String {
    let names = hosts.join(", ");
    match source {
        CertSource::Mkcert => super::po::tf(
            "certificate: ./tls, issued by this machine's mkcert CA for {names} — browsers here trust it once `mkcert -install` has been run",
            &[("names", &names)],
        ),
        CertSource::SelfSigned => super::po::tf(
            "certificate: ./tls, self-signed for {names} — HTTPS works and browsers will warn; install mkcert and run `sh scripts/tls.sh` for one this machine trusts",
            &[("names", &names)],
        ),
        CertSource::Kept => super::po::t("certificate: ./tls already had one — kept as it is").to_string(),
    }
}

/// What `--up` prints once the project answers: where to open it, what the
/// certificate is, the credentials when the template declares any, and the
/// three commands that come next. Pure, so the promise is testable.
fn up_summary(
    tname: &str,
    name: &str,
    plan: &Plan,
    cert: Option<CertSource>,
    created_secret: Option<&str>,
) -> Vec<String> {
    let health = template_meta(tname).health;
    let port = plan.port.as_str();
    let open = meta_kv(tname, "open").unwrap_or(health);
    let mut out = vec![format!(
        "✨ {}",
        super::po::tf("{name} is UP", &[("name", name)])
    )];
    match &plan.tls_port {
        Some(tls) => {
            out.push(format!("   open:    https://localhost:{tls}{open}"));
            out.push(format!(
                "            {}",
                super::po::tf("http://localhost:{port} redirects there", &[("port", port)])
            ));
        }
        None => out.push(format!("   open:    http://localhost:{port}{open}")),
    }
    if let Some(source) = cert {
        out.push(format!("   {}", cert_line(source, &plan.hosts)));
    }
    if let (Some(login), Some(password)) = (meta_kv(tname, "login"), meta_kv(tname, "password")) {
        out.push(format!("   user:    {login}"));
        out.push(format!("   pass:    {password}"));
        out.push(format!(
            "            {}",
            super::po::t("these are the image's factory credentials — change them before anyone else can reach the port (see README.md)")
        ));
    }
    if let Some(secret) = created_secret {
        out.push(format!(
            "   {}",
            super::po::tf(
                "secret:  {secret} was created with a generated key — it lives in the secret store (`delonix secret inspect {secret}`), not in the project",
                &[("secret", secret)],
            )
        ));
    }
    out.push(format!("   health:  http://localhost:{port}{health}"));
    if has_tunnel(tname) {
        out.push(format!(
            "   {}",
            super::po::t("internet: delonix stack apply -f delonix-tunnel.yaml — a public https address with no public IP (README.md, \"On the internet without a public IP\")")
        ));
    }
    out.push(format!("   logs:    delonix container logs -f {name}"));
    out.push(format!(
        "   stop:    delonix stack destroy   {}",
        super::po::t("(tears down everything the stack owns)")
    ));
    out
}

/// Whether the template ships `delonix-tunnel.yaml`, the opt-in `kind: Gateway`
/// that puts the project on the internet through an outbound tunnel.
fn has_tunnel(tname: &str) -> bool {
    TEMPLATES
        .iter()
        .find(|(n, _)| *n == tname)
        .is_some_and(|(_, files)| {
            files
                .iter()
                .any(|(path, _, _)| *path == "delonix-tunnel.yaml")
        })
}

/// A secret the template's manifest references and that `--up` creates when
/// it does not exist yet, from `up_secret=<suffix> <KEY>=<value>` in the
/// template's meta. `{random32}` in the value is 32 random bytes in base64.
///
/// The manifest names the secret `<project>-<suffix>`; without it `stack
/// apply` stops at `no such secret`, and `--up` would end on an error for a
/// project nobody has touched yet. The key goes to the secret store only —
/// never into a file of the project — and an existing secret is left alone.
/// Returns the secret's name when this call created it.
fn ensure_up_secret(exe: &Path, name: &str, spec: &str) -> Result<Option<String>> {
    use std::io::{Read, Write};
    let invalid = || Error::Invalid(format!("template.meta: up_secret={spec}"));
    let (suffix, pair) = spec.split_once(' ').ok_or_else(invalid)?;
    let (key, value) = pair.split_once('=').ok_or_else(invalid)?;
    let secret = format!("{name}-{suffix}");
    let exists = std::process::Command::new(exe)
        .args(["secret", "inspect", &secret])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if exists {
        return Ok(None);
    }
    let value = if value.contains("{random32}") {
        // No fallback: a key that is not random is worse than no key.
        let mut bytes = [0u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        use base64::Engine;
        value.replace(
            "{random32}",
            &base64::engine::general_purpose::STANDARD.encode(bytes),
        )
    } else {
        value.to_string()
    };
    // Through stdin, so the key never appears on a command line.
    let mut child = std::process::Command::new(exe)
        .args(["secret", "create", &secret, "--from-env-file", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| Error::Invalid(e.to_string()))?;
    if let Some(mut stdin) = child.stdin.take() {
        writeln!(stdin, "{key}={value}")?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| Error::Invalid(e.to_string()))?;
    if !out.status.success() {
        return Err(Error::Invalid(super::po::tf(
            "`delonix {args}` failed:\n{stderr}",
            &[
                ("args", &format!("secret create {secret}")),
                ("stderr", String::from_utf8_lossy(&out.stderr).trim()),
            ],
        )));
    }
    Ok(Some(secret))
}

/// What the local image `tag` names IS, if there is one: its layers and its
/// configuration. Not the image id — that is the digest of a config blob that
/// carries the build time, so it changes on every build, and a rebuild of an
/// unchanged project would then recreate a container for nothing.
fn image_content(tag: &str) -> Option<String> {
    let (images, _) = super::util::open_stores().ok()?;
    let image = images.resolve(tag).ok()?;
    Some(format!(
        "{}|{}",
        image.layers.join(","),
        serde_json::to_string(&image.config).unwrap_or_default()
    ))
}

/// Removes a container and waits until it is gone. On a busy disk the exit of
/// a container can outlast one `rm -f` (it reports that the process is still
/// exiting and keeps the record); the removal is repeated for up to a few
/// minutes before that is reported as the failure it then is.
fn remove_container(exe: &Path, dir: &Path, name: &str) -> Result<()> {
    let gone = || {
        super::util::open_stores()
            .ok()
            .and_then(|(_, store)| store.list().ok())
            .is_some_and(|all| !all.iter().any(|c| c.name == name))
    };
    let mut last = Ok(());
    for _ in 0..8 {
        last = run_quiet(exe, dir, &["container", "rm", "-f", name]);
        if gone() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
    last
}

/// The containers created from `tag`, each with whether it is running.
fn containers_of_image(tag: &str) -> Vec<(String, bool)> {
    let Ok((_, store)) = super::util::open_stores() else {
        return Vec::new();
    };
    store
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.image == tag)
        .map(|c| {
            let running = matches!(
                c.status,
                delonix_model::records::Status::Running | delonix_model::records::Status::Paused
            );
            (c.name, running)
        })
        .collect()
}

/// Seconds `--up` waits for the health path: the template's `wait=`, or 120.
fn wait_secs(tname: &str) -> u64 {
    template_meta(tname).wait_secs
}

/// Makes the generated project a git repository, on `main`, with everything
/// it was just given in one commit — so the next step is `git remote add
/// <name> <url> && git push <name> main` and not four commands of setup.
///
/// Four things it refuses to do, each because doing it would be worse:
///
///  * **inside an existing work tree** (a monorepo subdirectory) it does
///    nothing: a nested repository there is a trap that only shows up when
///    someone pushes and the files are not in it;
///  * **over an existing `.git`** it does nothing — that repository is the
///    project's history, not ours to add to;
///  * **without `git` on the PATH** it says so and carries on; a scaffold is
///    not the place to fail for a tool the project does not need to build;
///  * **without an author configured** it leaves the files STAGED and prints
///    the two `git config` lines, instead of inventing a name and an address
///    that would end up in the history of every project generated here.
///
/// `--initial-branch` needs git 2.28; older git gets the same result from
/// `symbolic-ref`, which has always existed.
fn git_init(dir: &Path) -> Option<String> {
    use std::process::{Command, Stdio};
    let git = |args: &[&str]| -> Option<std::process::Output> {
        Command::new("git")
            .args(["-C", &dir.display().to_string()])
            .args(args)
            .output()
            .ok()
    };
    // A `git` that fails to START and one that starts and exits non-zero are
    // both unusable, and only the first is an `Err`: the status is read too,
    // because the probe has to answer "git works", not "git launches".
    let usable = Command::new("git")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|st| st.success());
    if !usable {
        return Some(
            super::po::t("git does not work here, so the project is not a repository yet").into(),
        );
    }
    if dir.join(".git").exists() {
        return None;
    }
    // Already inside someone else's work tree: a nested repository here is a
    // trap, and `git rev-parse` is the only honest way to ask.
    if git(&["rev-parse", "--is-inside-work-tree"]).is_some_and(|o| o.status.success()) {
        return Some(
            super::po::t(
                "this directory is already inside a git repository — not creating one inside it",
            )
            .into(),
        );
    }
    let ok = git(&["init", "--initial-branch=main"]).is_some_and(|o| o.status.success())
        || (git(&["init"]).is_some_and(|o| o.status.success())
            && git(&["symbolic-ref", "HEAD", "refs/heads/main"])
                .is_some_and(|o| o.status.success()));
    if !ok {
        return Some(
            super::po::t("`git init` failed, so the project is not a repository yet").into(),
        );
    }
    if !git(&["add", "-A"]).is_some_and(|o| o.status.success()) {
        return Some(super::po::t("the files could not be staged").into());
    }
    let Some(out) = git(&["commit", "-m", "chore: scaffold"]) else {
        return Some(super::po::t("the commit could not be run").into());
    };
    if out.status.success() {
        return None;
    }
    // A commit fails for more than a missing author: a signing key that is not
    // there, a `commit-msg` hook inherited from `init.templateDir`, a
    // read-only object store. Blaming the author names a cause nobody verified
    // and hands over two commands that fix none of those — the same family as
    // «the ENOENT of a `Command::status()` is not a missing file». So the
    // author is named only when it really is absent; otherwise git's own words
    // are quoted, because they are the ones that say what happened.
    let has =
        |k: &str| git(&["config", k]).is_some_and(|o| o.status.success() && !o.stdout.is_empty());
    if !has("user.email") || !has("user.name") {
        return Some(
            super::po::t(
                "git repository created on `main` and everything staged, but the commit needs an author:\n\
                 \x20 git config user.name  \"Your Name\"\n\
                 \x20 git config user.email you@example.com\n\
                 \x20 git commit -m \"chore: scaffold\"",
            )
            .into(),
        );
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let why = err
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    Some(super::po::tf(
        "git repository created on `main` and everything staged, but the commit failed: {why}",
        &[("why", why)],
    ))
}

/// The two files that decide what gets BUILT and STARTED here: the image
/// recipe and the declaration applied from it. Config and code are
/// deliberately not in this set — a hand-edited `nginx.conf` is the normal
/// state of a generated project and must never stop anything.
fn is_build_glue(dest: &Path) -> bool {
    matches!(
        dest.file_name().and_then(|n| n.to_str()),
        Some("Delonixfile") | Some("delonix-manifest.yaml")
    )
}

/// The `FROM`s of a Delonixfile, in order — what identifies the project a
/// recipe builds, independently of ports, names and comments.
///
/// Compared as BYTES and never as `&l[..5]`: a Delonixfile is someone else's
/// file and a line may well start with a multi-byte character (our own
/// templates open with `# Delonixfile — …`), where slicing by byte index
/// panics mid-character.
fn from_lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|l| {
            let b = l.as_bytes();
            b.len() > 5 && b[..5].eq_ignore_ascii_case(b"from ")
        })
        .collect()
}

/// Refuses when the glue already on disk is not the glue this command would
/// write. Measured in production: `delonix init -t odoo <dir>` over a
/// directory already holding an nginx project wrote nothing, printed
/// "nothing to do (everything already existed)" — and then `--up` went on to
/// build and start the NGINX project, waiting for odoo's health path on
/// odoo's port. The command reported success for something it had not done.
///
/// Scoped to [`is_build_glue`] so that editing the generated config, or the
/// adopted project's own code, never trips it; and skipped under `--force`,
/// which is the answer to this refusal. A re-run with the SAME template and
/// the SAME flags renders byte-identical glue and goes through untouched —
/// that is the idempotent second `--up`, which has to keep working.
fn refuse_foreign_project(
    outputs: &[(PathBuf, String, bool)],
    tname: &str,
    name: &str,
    dir: &Path,
    force: bool,
) -> Result<()> {
    if force {
        return Ok(());
    }
    for (dest, content, _) in outputs {
        if !is_build_glue(dest) {
            continue;
        }
        // Absent → it is about to be written; there is nothing to conflict with.
        let Ok(on_disk) = std::fs::read_to_string(dest) else {
            continue;
        };
        if on_disk == *content {
            continue;
        }
        let (have, want) = (from_lines(&on_disk), from_lines(content));
        let detail = if have.is_empty() || want.is_empty() || have == want {
            String::new()
        } else {
            super::po::tf(
                " (it builds `{have}`, the '{tname}' template builds `{want}`)",
                &[
                    ("have", &have.join(", ")),
                    ("tname", tname),
                    ("want", &want.join(", ")),
                ],
            )
        };
        return Err(Error::Conflict(super::po::tf(
            "{file} already in {dir} is not the one this command would write{detail} — \
             build what is there with `delonix build -t {name}:dev . && delonix stack apply`, \
             pass --force to overwrite it with the '{tname}' template, or pick another directory",
            &[
                (
                    "file",
                    dest.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("Delonixfile"),
                ),
                ("dir", &dir.display().to_string()),
                ("detail", &detail),
                ("name", name),
                ("tname", tname),
            ],
        )));
    }
    Ok(())
}

/// Why a port below the host's threshold cannot be published here, and the two
/// ways out. Shares its shape with the engine's own refusal in
/// `cmd::container` — the same fact, said at the moment the flag is given
/// instead of after a whole image has been built for it.
///
/// The threshold is read through `cmd::system`'s reader and not a second one
/// of our own: two readers of the same sysctl are two numbers that can
/// disagree. Unreadable → the sentence is still true, it just loses the
/// number.
fn privileged_port_message(flag: &str, port: &str) -> String {
    let n: u32 = port.parse().unwrap_or(0);
    let have = match super::system::read_sysctl("net.ipv4.ip_unprivileged_port_start") {
        Some(v) => super::po::tf(" (this host starts at {start})", &[("start", &v)]),
        None => String::new(),
    };
    super::po::tf(
        "{flag} {port} needs privilege to bind on this host — rootless cannot publish \
         ports below net.ipv4.ip_unprivileged_port_start{have}\n\
         \n\
         fix it with ONE of these:\n\
         \x20 {flag} {alt}    # generate the project on an unprivileged port\n\
         \x20 curl -fsSL {url} | bash -s -- --low-ports    # or lower the threshold for good (host-wide)",
        &[
            ("flag", flag),
            ("port", port),
            ("have", &have),
            // 80 → 8080, 443 → 8443, 943 → 8943; always inside u16.
            ("alt", &(n + 8000).to_string()),
            (
                "url",
                "https://github.com/angolardevops/delonix-runtime/\
                 releases/latest/download/install.sh",
            ),
        ],
    )
}

/// A port this project is about to publish that this host will not hand out:
/// one something else already holds, or one below the host's privileged-port
/// threshold. Said BEFORE the build, with the owner's name or the way out:
/// found only at `stack apply`, it costs the whole build first.
fn refuse_unusable_ports(plan: &Plan, name: &str) -> Result<()> {
    // A port held by THIS project's own running container is not a conflict:
    // `--up` run a second time on a project that is already up has to reach
    // `stack apply`, which converges it. Measured: it stopped here, naming the
    // project's own slirp as "another process".
    let own = |port: &str| -> Option<String> {
        let (_, store) = super::util::open_stores().ok()?;
        super::container::port_owner(&store, port).ok().flatten()
    };
    let is_ours = |owner: &str| owner == name || owner.starts_with(&format!("{name}-"));
    for (flag, port) in std::iter::once(("--port", &plan.port))
        .chain(plan.tls_port.iter().map(|t| ("--tls-port", t)))
    {
        let Ok(n) = port.parse::<u16>() else { continue };
        // Privilege first, and it is a different question from "taken": the
        // probe answers `true` for EADDRINUSE on purpose, so the two never
        // mask each other. `stack apply` refuses this one too, but only after
        // the whole image has been built for a port it was never going to
        // publish.
        if !delonix_sdn::can_bind_host_port("127.0.0.1", n, delonix_sdn::Proto::Tcp) {
            return Err(Error::Invalid(privileged_port_message(flag, port)));
        }
        if !delonix_sdn::host_port_busy("127.0.0.1", n, delonix_sdn::Proto::Tcp) {
            continue;
        }
        let owner = match own(port) {
            Some(c) if is_ours(&c) => continue,
            Some(c) => super::po::tf("the delonix container {name}", &[("name", &c)]),
            None => delonix_sdn::host_port_owner_process(n)
                .unwrap_or_else(|| super::po::t("another process").to_string()),
        };
        return Err(Error::Invalid(super::po::tf(
            "port {port} is already in use on this host (by {owner}) — free it, or change the port in the generated files and in delonix-manifest.yaml, then run `delonix stack apply`",
            &[("port", port), ("owner", &owner)],
        )));
    }
    Ok(())
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
///
/// **The build draws its own steps.** It used to run folded under a single
/// spinner, and the build is where the time goes: pulling the base image and
/// installing dependencies showed as one line that did not move for minutes.
/// It now inherits this terminal, so every instruction (`RUN apk add …`,
/// `RUN pnpm install`, the packaging) opens and closes its own animated line.
fn build_and_up(
    tname: &str,
    name: &str,
    dir: &Path,
    plan: &Plan,
    cert: Option<CertSource>,
) -> Result<()> {
    let health = template_meta(tname).health;
    let port = plan.port.as_str();
    refuse_unusable_ports(plan, name)?;
    let exe = std::env::current_exe().map_err(|e| Error::Invalid(e.to_string()))?;
    let tag = format!("{name}:dev");

    let image_before = image_content(&tag);
    eprintln!(
        "\n{}",
        super::po::tf("Building image {tag} 🔨", &[("tag", &tag)])
    );
    let built = std::process::Command::new(&exe)
        .args(["build", "-t", &tag, "."])
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .status()
        .map_err(|e| Error::Invalid(e.to_string()))?;
    if !built.success() {
        return Err(Error::Invalid(
            super::po::t("`delonix build` failed — the step that failed is shown above").into(),
        ));
    }

    let created_secret = match meta_kv(tname, "up_secret") {
        Some(spec) => ensure_up_secret(&exe, name, spec)?,
        None => None,
    };

    // A container that is already running an EARLIER build of this tag would
    // survive the apply untouched: the manifest names the tag, the tag did not
    // change, so the plan has nothing to do — and `--up` would report "is UP"
    // over the previous build (measured: an edited page, rebuilt, still served
    // the old one). Such a container is removed here and the apply recreates
    // it from the manifest; named volumes are not touched.
    //
    // A container of this tag that is NOT running goes the same way: `stack
    // apply` leaves an existing record alone, so a dead one would stay dead
    // and the health wait would run out over it.
    let rebuilt = matches!((&image_before, image_content(&tag)), (Some(b), Some(a)) if *b != a);
    for (c, running) in containers_of_image(&tag) {
        if running && !rebuilt {
            continue;
        }
        eprintln!(
            "{}",
            if running {
                super::po::tf(
                    "{name} is running the previous build of {tag} — recreating it",
                    &[("name", &c), ("tag", &tag)],
                )
            } else {
                super::po::tf(
                    "{name} exists but is not running — recreating it",
                    &[("name", &c)],
                )
            }
        );
        remove_container(&exe, dir, &c)?;
    }

    let mut p = super::output::Progress::new();
    p.step(&format!("Applying the stack ({name})"), "🚀");
    run_quiet(&exe, dir, &["stack", "apply"])?;
    p.ok();

    p.step("Waiting until healthy", "❤️ ");
    wait_health(port, health, wait_secs(tname))?;
    p.ok();
    drop(p);

    println!();
    for line in up_summary(tname, name, plan, cert, created_secret.as_deref()) {
        println!("{line}");
    }
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

/// Waits for `health` to answer 200, up to `secs`.
///
/// A ceiling, not a delay: a healthy app answers as soon as it is up. It was
/// 40s, and measured on a loaded host (load ~30) Odoo 20 took ~85s from its
/// first log line to serving HTTP — `--up` reported a failure on a stack that
/// came up on its own a minute later. A template that needs longer says so
/// with `wait=` in its `template.meta`.
fn wait_health(port: &str, health: &str, secs: u64) -> Result<()> {
    for _ in 0..secs * 2 {
        if http_ok(port, health) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err(Error::Invalid(super::po::tf(
        "the container started but did not become healthy in {secs}s — see `delonix container logs`",
        &[("secs", &secs.to_string())],
    )))
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
    if chosen.is_none() && !o.edge.is_empty() {
        return Err(Error::Invalid(
            super::po::t(
                "--port/--tls-port/--hostname need a template to apply them to — pass -t/--template",
            )
            .into(),
        ));
    }
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
        let tname = canonical_template(t);
        if !TEMPLATES.iter().any(|(n, _)| *n == tname) {
            // Unknown name: let the renderer say so, with the list.
            return render_template(t, o, false);
        }
        // What only the person generating can decide is asked here, on a
        // terminal, before anything is written: a port that is taken, the
        // names the certificate must cover, whether to start it now.
        let interactive = stdin_is_tty();
        let plan = plan_for(tname, interactive, &o.edge)?;
        let do_up = o.up || (interactive && prompt_yes("Build and start it now?", true));
        let cert = render_planned(t, o, !do_up, &plan)?;
        if do_up {
            build_and_up(tname, &o.name, &o.dir, &plan, cert)?;
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
        eprintln!("{}", already_complete_line(&o.dir));
        return Ok(());
    }
    // The generic scaffold writes the same kind of artefact as a template does
    // — a Delonixfile, a manifest, a README — so it becomes a repository for
    // the same reason. Leaving it out would be a difference nobody could
    // explain from the outside.
    if let Some(why) = git_init(&o.dir) {
        super::output::warn(&why);
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
#   delonix vm image build -t {image} --k8s-version 1.34
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
         #   delonix vm image build -t {img} --k8s-version 1.34   # uma vez\n\
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
         # default serves ONE node only here (refused above one); for workers\n  \
         # use none and apply your own CNI after the bootstrap\n  \
         cni: default\n  \
         controlPlane:\n    replicas: 1   # >1 exige controlPlaneEndpoint (LB/VIP)\n  \
         workers:\n    replicas: 0\n  \
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

    /// `delonix init` with no argument passes `.`, and the message puts a full
    /// stop straight after the directory — which printed `already exist in ..`,
    /// read as a typo and told the operator nothing. The line has to name a
    /// real directory.
    /// Passing `.` is the whole point: an absolute path never showed the bug,
    /// so a test built on a `tempdir()` path would pass against the broken
    /// version too and prove nothing. `delonix init` with no argument is what
    /// reaches this, and it passes `.`.
    #[test]
    fn the_nothing_to_do_line_names_the_directory_instead_of_a_bare_dot() {
        let cwd = std::fs::canonicalize(".").unwrap();

        let line = already_complete_line(Path::new("."));
        assert!(
            !line.contains(" in .."),
            "the path's dot collided with the sentence's: {line}"
        );
        assert!(
            line.contains(&cwd.display().to_string()),
            "the line has to name the directory: {line}"
        );
    }

    /// A path the filesystem cannot resolve still has to produce a line — the
    /// fallback prints what it was given rather than swallowing the message.
    #[test]
    fn the_nothing_to_do_line_survives_a_path_that_does_not_resolve() {
        let line = already_complete_line(Path::new("/definitely/does/not/exist"));
        assert!(
            line.contains("/definitely/does/not/exist"),
            "with no canonicalize, it shows the path as given: {line}"
        );
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
            edge: Default::default(),
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
            edge: Default::default(),
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
            edge: Default::default(),
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
            edge: Default::default(),
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
            edge: Default::default(),
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
            edge: Default::default(),
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
        // Every file of the two renders, path and content: the template may
        // change its layout, the alias may not change what it produces.
        let render = |t: &str| {
            let (_tmp, dir) = scratch();
            render_template(t, &opts(dir.clone(), "myapi", t, None), false).unwrap();
            let mut files = std::collections::BTreeMap::new();
            let mut stack = vec![dir.clone()];
            while let Some(d) = stack.pop() {
                for e in std::fs::read_dir(&d).unwrap() {
                    let p = e.unwrap().path();
                    // `.git` is metadata, and its objects differ byte for
                    // byte between two runs of the SAME template.
                    if p.file_name().and_then(|n| n.to_str()) == Some(".git") {
                        continue;
                    }
                    if p.is_dir() {
                        stack.push(p);
                    } else if p.file_name().and_then(|n| n.to_str()) == Some(".env") {
                        // `.env` holds freshly generated random secrets — different
                        // on every render by design, so it is not byte-comparable.
                        continue;
                    } else {
                        let rel = p.strip_prefix(&dir).unwrap().to_path_buf();
                        files.insert(rel, std::fs::read(&p).unwrap());
                    }
                }
            }
            files
        };
        let (fastapi, alias) = (render("fastapi"), render("python"));
        assert!(fastapi.contains_key(Path::new("pyproject.toml")));
        assert_eq!(fastapi, alias);
        assert!(template_names().contains(&"fastapi"));
        assert!(
            !template_names().contains(&"python"),
            "an alias is not listed as a template"
        );
    }

    /// The FastAPI project ships the single compose stack (`compose.yaml`),
    /// its `Dockerfile`, and a generated `.env` with INDEPENDENT random secrets
    /// written 0600 — and never leaks the `.env.tmpl` source or an unresolved
    /// `__SECRET_…__` token.
    #[test]
    fn fastapi_generates_compose_dockerfile_and_a_secret_env() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, dir) = scratch();
        render_template(
            "fastapi",
            &opts(dir.clone(), "myapi", "fastapi", None),
            false,
        )
        .unwrap();

        assert!(
            dir.join("compose.yaml").is_file(),
            "compose.yaml is written"
        );
        assert!(dir.join("Dockerfile").is_file(), "Dockerfile is written");
        assert!(
            !dir.join(".env.tmpl").exists(),
            ".env.tmpl is the source, never written into the project"
        );

        let env_path = dir.join(".env");
        let env = std::fs::read_to_string(&env_path).unwrap();
        assert!(
            !env.contains("__SECRET_"),
            "every secret token is filled:\n{env}"
        );
        let mode = std::fs::metadata(&env_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, ".env is private");

        // A name reused across variables gets the SAME value; two different
        // names get independent ones.
        let kv = |key: &str| -> String {
            env.lines()
                .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
                .unwrap_or_else(|| panic!("{key} in .env:\n{env}"))
                .to_string()
        };
        let rabbit = kv("RABBITMQ_DEFAULT_PASS");
        assert!(
            kv("RABBITMQ_URL").contains(&format!("app:{rabbit}@")),
            "the broker password is reused verbatim in the URL"
        );
        assert_eq!(
            kv("SONAR_JDBC_PASSWORD"),
            kv("POSTGRES_PASSWORD"),
            "Sonar reuses its database password"
        );
        assert_ne!(
            rabbit,
            kv("POSTGRES_PASSWORD"),
            "independent secrets differ"
        );

        // The committed .gitignore keeps the generated secrets out of git.
        let gitignore = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(
            gitignore.lines().any(|l| l.trim() == ".env"),
            ".env is ignored"
        );
    }

    /// A second render over an existing `.env` keeps it: rotating secrets a
    /// project may already be using is worse than refusing.
    #[test]
    fn a_second_render_never_overwrites_an_existing_env() {
        let (_tmp, dir) = scratch();
        render_template(
            "fastapi",
            &opts(dir.clone(), "myapi", "fastapi", None),
            false,
        )
        .unwrap();
        let first = std::fs::read_to_string(dir.join(".env")).unwrap();
        let mut again = opts(dir.clone(), "myapi", "fastapi", None);
        again.force = true;
        render_template("fastapi", &again, false).unwrap();
        assert_eq!(
            first,
            std::fs::read_to_string(dir.join(".env")).unwrap(),
            "--force does not rotate the secrets in an existing .env"
        );
    }

    /// `-v` on the pure-config templates (`nginx`/`httpd`/`haproxy`) pins the
    /// upstream image tag, the same idiom as `odoo` — never a dependency file.
    /// A directory is named for people: the derived project name is the
    /// nearest usable one. An explicit `--name` comes back untouched, so the
    /// check that follows can refuse it.
    #[test]
    fn a_directory_name_is_turned_into_a_usable_project_name() {
        for (dir, want) in [
            ("My App", "my-app"),
            ("Shop_API", "shop-api"),
            ("/srv/Weird", "weird"),
            ("fine-name", "fine-name"),
        ] {
            let got = project_name(None, Path::new(dir));
            assert_eq!(got, want, "{dir}");
            assert!(check_project_name(&got).is_ok(), "{got}");
        }
        assert_eq!(
            project_name(Some("Bad_Name".into()), Path::new("x")),
            "Bad_Name"
        );
        assert!(check_project_name("Bad_Name").is_err());
    }

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
                edge: Default::default(),
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
            edge: Default::default(),
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

    /// Every embedded template, for every `-v` it declares: renders, leaves
    /// no placeholder, every JSON and YAML file parses, and the version shows
    /// up in what was written. One test for all templates, so a new template
    /// (or a new accepted version) is covered the day it is added.
    /// Every template ships `delonix-tunnel.yaml`: one `kind: Gateway` the
    /// engine's loader accepts, opt-in (the main manifest has no Gateway, so
    /// `stack apply` never opens it), aimed at the port that serves the site —
    /// the HTTPS one on an edge template, whose plain-HTTP port redirects to a
    /// port the public address does not have.
    #[test]
    fn every_template_ships_an_opt_in_tunnel() {
        for tpl in template_names() {
            let (_tmp, dir) = scratch();
            render_template(tpl, &opts(dir.clone(), "my-svc", tpl, None), false)
                .unwrap_or_else(|e| panic!("{tpl}: {e}"));
            let docs = super::super::manifest::load(&dir.join("delonix-tunnel.yaml"))
                .unwrap_or_else(|e| panic!("{tpl}: delonix-tunnel.yaml: {e}"));
            assert_eq!(docs.len(), 1, "{tpl}");
            let doc = &docs[0];
            assert_eq!(doc.kind, super::super::kinds::GATEWAY, "{tpl}");
            assert_eq!(doc.metadata.name, "my-svc-tunnel", "{tpl}");
            let spec = &doc.spec;
            assert_eq!(spec["provider"].as_str(), Some("cloudflare"), "{tpl}");
            let edge = meta_kv(tpl, "tls");
            let want: u64 = edge.unwrap_or(template_meta(tpl).port).parse().unwrap();
            assert_eq!(spec["localPort"].as_u64(), Some(want), "{tpl}");
            assert_eq!(
                spec["insecureSkipTlsVerify"].as_bool().unwrap_or(false),
                edge.is_some(),
                "{tpl}"
            );
            let main = std::fs::read_to_string(dir.join("delonix-manifest.yaml")).unwrap();
            assert!(
                !main.contains("kind: Gateway"),
                "{tpl}: the tunnel must stay opt-in"
            );
            let readme = std::fs::read_to_string(dir.join("README.md")).unwrap();
            assert!(
                readme.contains("## On the internet without a public IP"),
                "{tpl}: README has no tunnel section"
            );
        }
    }

    #[test]
    fn every_template_renders_valid_files_for_each_declared_version() {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else {
                    out.push(p);
                }
            }
        }
        for tpl in template_names() {
            let meta = template_meta(tpl);
            let versions: Vec<&str> = if meta.versions.is_empty() {
                vec![meta.version]
            } else {
                assert!(
                    meta.versions.split(',').any(|v| v.trim() == meta.version),
                    "{tpl}: the default version {} is not in versions={}",
                    meta.version,
                    meta.versions
                );
                meta.versions.split(',').map(str::trim).collect()
            };
            for v in versions {
                let (_tmp, dir) = scratch();
                let v_opt = (!v.is_empty()).then_some(v);
                render_template(tpl, &opts(dir.clone(), "my-svc", tpl, v_opt), false)
                    .unwrap_or_else(|e| panic!("{tpl} -v {v}: {e}"));
                let mut files = Vec::new();
                walk(&dir, &mut files);
                let mut version_seen = v.is_empty();
                for f in &files {
                    let Ok(text) = std::fs::read_to_string(f) else {
                        continue;
                    };
                    for token in ["__NAME__", "__MODULE__", "__PORT__", "__TEMPLATE_VERSION__"] {
                        assert!(!text.contains(token), "{tpl} -v {v}: {token} left in {f:?}");
                    }
                    version_seen |= text.contains(v);
                    match f.extension().and_then(|e| e.to_str()) {
                        Some("json") => {
                            serde_json::from_str::<serde_json::Value>(&text)
                                .unwrap_or_else(|e| panic!("{tpl} -v {v}: {f:?}: {e}"));
                        }
                        Some("yaml" | "yml") => {
                            for doc in serde_yaml::Deserializer::from_str(&text) {
                                <serde_yaml::Value as serde::Deserialize>::deserialize(doc)
                                    .unwrap_or_else(|e| panic!("{tpl} -v {v}: {f:?}: {e}"));
                            }
                        }
                        _ => {}
                    }
                }
                assert!(version_seen, "{tpl}: -v {v} appears in no generated file");
            }
            if !meta.versions.is_empty() {
                let (_tmp, dir) = scratch();
                let err =
                    render_template(tpl, &opts(dir.clone(), "my-svc", tpl, Some("0.0.1")), false);
                assert!(
                    err.is_err() && !dir.exists(),
                    "{tpl}: -v 0.0.1 was accepted"
                );
            }
        }
    }

    /// Adoption writes the template's CI into a project that uses the
    /// template's package manager and has no CI of its own.
    #[test]
    fn adoption_writes_the_ci_when_the_project_can_run_it() {
        let (_tmp, dir) = scratch();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        std::fs::write(dir.join("pnpm-lock.yaml"), "").unwrap();
        render_template("node", &opts(dir.clone(), "app", "node", None), false).unwrap();
        assert!(dir.join(".github/workflows/ci.yml").exists());
        assert!(dir.join(".gitlab-ci.yml").exists());
        assert!(dir.join("CONTRIBUTING.md").exists());
        // The same project with npm's lock instead: glue yes, CI no.
        let (_tmp2, dir2) = scratch();
        std::fs::create_dir_all(&dir2).unwrap();
        std::fs::write(dir2.join("package-lock.json"), "{}").unwrap();
        render_template("node", &opts(dir2.clone(), "app", "node", None), false).unwrap();
        assert!(dir2.join("Delonixfile").exists());
        assert!(!dir2.join(".github/workflows/ci.yml").exists());
    }

    fn edge_opts(dir: &Path, template: &str) -> InitOpts {
        InitOpts {
            dir: dir.to_path_buf(),
            name: "edge".into(),
            image: None,
            force: false,
            template: Some(template.into()),
            template_version: None,
            up: false,
            edge: Default::default(),
        }
    }

    /// Every file the scaffold WROTE. `.git` is skipped: since a generated
    /// project is a repository, that directory is there too — and it holds
    /// binary objects whose bytes differ between two runs of the same
    /// template. It is metadata, never generated content, and every walker
    /// over a project has to agree on that.
    fn all_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.file_name().and_then(|n| n.to_str()) == Some(".git") {
                continue;
            }
            if p.is_dir() {
                all_files(&p, out);
            } else {
                out.push(p);
            }
        }
    }

    /// The three edge templates come up with HTTPS: every token is replaced
    /// (the TLS port included), the manifest publishes both ports and mounts
    /// `./tls`, and the key that was generated is private.
    /// Every edge template offers Let's Encrypt by DNS-01 (the only challenge
    /// that works when port 80 of the public address does not reach the host),
    /// and HAProxy, which has no webroot, says so instead of issuing a local
    /// certificate named after the arguments.
    #[test]
    fn the_edge_templates_offer_lets_encrypt_by_dns() {
        for tpl in ["nginx", "httpd", "haproxy"] {
            let (_tmp, dir) = scratch();
            let plan = Plan {
                port: "18080".into(),
                tls_port: Some("18443".into()),
                hosts: vec!["localhost".into()],
            };
            render_planned(tpl, &edge_opts(&dir, tpl), false, &plan).unwrap();
            let run = |args: &[&str]| {
                std::process::Command::new("sh")
                    .arg("scripts/tls.sh")
                    .args(args)
                    .current_dir(&dir)
                    .output()
                    .unwrap()
            };
            let syntax = std::process::Command::new("sh")
                .args(["-n", "scripts/tls.sh"])
                .current_dir(&dir)
                .output()
                .unwrap();
            assert!(syntax.status.success(), "{tpl}: {syntax:?}");
            // One argument short: the usage line, before certbot is looked for.
            let usage = run(&["letsencrypt-dns", "example.org"]);
            assert_eq!(usage.status.code(), Some(2), "{tpl}: {usage:?}");
            assert!(
                String::from_utf8_lossy(&usage.stderr).contains("letsencrypt-dns <domain> <email>"),
                "{tpl}: {usage:?}"
            );
            // A certificate this project already holds from ANOTHER CA (staging
            // first, then production) is replaced: certbot alone answers "not
            // yet due for renewal" and keeps it. A stand-in certbot records
            // the options it was given.
            let bin = dir.join("fakebin");
            std::fs::create_dir_all(&bin).unwrap();
            let fake = bin.join("certbot");
            std::fs::write(&fake, "#!/bin/sh\necho \"$@\" > certbot.args\nexit 1\n").unwrap();
            std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
            let path = format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            );
            let asked = |staging: bool| {
                let mut cmd = std::process::Command::new("sh");
                cmd.args(["scripts/tls.sh", "letsencrypt-dns", "example.org", "-"])
                    .current_dir(&dir)
                    .env("PATH", &path)
                    .env("DNS_AUTH_HOOK", "true")
                    .env_remove("ACME_SERVER")
                    .env_remove("LETSENCRYPT_STAGING");
                if staging {
                    cmd.env("LETSENCRYPT_STAGING", "1");
                }
                cmd.output().unwrap();
                std::fs::read_to_string(dir.join("certbot.args")).unwrap()
            };
            let first = asked(false);
            assert!(
                first.contains("--server https://acme-v02.api.letsencrypt.org/directory"),
                "{tpl}: {first}"
            );
            assert!(!first.contains("--force-renewal"), "{tpl}: {first}");
            std::fs::create_dir_all(dir.join("letsencrypt/renewal")).unwrap();
            std::fs::write(
                dir.join("letsencrypt/renewal/example.org.conf"),
                "[renewalparams]\nserver = https://acme-staging-v02.api.letsencrypt.org/directory\n",
            )
            .unwrap();
            let same_ca = asked(true);
            assert!(!same_ca.contains("--force-renewal"), "{tpl}: {same_ca}");
            let other_ca = asked(false);
            assert!(other_ca.contains("--force-renewal"), "{tpl}: {other_ca}");
            if tpl == "haproxy" {
                let before = std::fs::read(dir.join("tls/tls.crt")).unwrap();
                let http01 = run(&["letsencrypt", "example.org", "-"]);
                assert_eq!(http01.status.code(), Some(2), "{http01:?}");
                assert!(
                    String::from_utf8_lossy(&http01.stderr).contains("letsencrypt-dns"),
                    "{http01:?}"
                );
                assert_eq!(std::fs::read(dir.join("tls/tls.crt")).unwrap(), before);
            }
        }
    }

    #[test]
    fn the_edge_templates_render_with_tls_and_no_token_left() {
        use std::os::unix::fs::PermissionsExt;
        for tpl in ["nginx", "httpd", "haproxy"] {
            let (_tmp, dir) = scratch();
            let plan = Plan {
                port: "18080".into(),
                tls_port: Some("18443".into()),
                hosts: vec!["localhost".into(), "127.0.0.1".into()],
            };
            let cert = render_planned(tpl, &edge_opts(&dir, tpl), false, &plan).unwrap();
            assert_eq!(cert, Some(CertSource::SelfSigned), "{tpl}");
            let mut files = Vec::new();
            all_files(&dir, &mut files);
            for f in &files {
                let text = std::fs::read_to_string(f).unwrap();
                for token in [
                    "__PORT__",
                    "__TLS_PORT__",
                    "__NAME__",
                    "__TEMPLATE_VERSION__",
                ] {
                    assert!(
                        !text.contains(token),
                        "{tpl}: {token} left in {}",
                        f.display()
                    );
                }
            }
            let manifest = std::fs::read_to_string(dir.join("delonix-manifest.yaml")).unwrap();
            assert!(manifest.contains("\"18080:18080\""), "{tpl}:\n{manifest}");
            assert!(manifest.contains("\"18443:18443\""), "{tpl}:\n{manifest}");
            assert!(manifest.contains("\"./tls:"), "{tpl}:\n{manifest}");
            for private in ["tls/tls.key", "tls/tls.pem"] {
                let mode = std::fs::metadata(dir.join(private))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "{tpl}: {private}");
            }
            // Neither the key nor the certificate may reach the image or git.
            for ignore in [".dockerignore", ".gitignore"] {
                let text = std::fs::read_to_string(dir.join(ignore)).unwrap();
                assert!(text.lines().any(|l| l == "tls/"), "{tpl}: {ignore}");
            }
        }
    }

    /// `tls=` in a template's meta and the `__TLS_PORT__` token go together:
    /// a template that uses the token without declaring the port would render
    /// an empty port, and one that declares it without using it would get a
    /// certificate nothing mounts.
    #[test]
    fn the_tls_token_and_the_tls_meta_key_go_together() {
        for (name, files) in TEMPLATES {
            let uses = files
                .iter()
                .any(|(_, body, _)| body.contains("__TLS_PORT__"));
            assert_eq!(uses, meta_kv(name, "tls").is_some(), "{name}");
        }
    }

    /// Without mkcert the certificate is self-signed here; a second run keeps
    /// what is there instead of replacing a certificate somebody installed.
    #[test]
    fn a_certificate_is_generated_once_and_then_kept() {
        let (_tmp, dir) = scratch();
        let hosts = vec!["localhost".to_string(), "127.0.0.1".to_string()];
        let first = ensure_tls_with(&dir, &hosts, "mkcert-not-installed").unwrap();
        assert_eq!(first, CertSource::SelfSigned);
        let crt = std::fs::read(dir.join("tls/tls.crt")).unwrap();
        let key = std::fs::read(dir.join("tls/tls.key")).unwrap();
        assert!(crt.starts_with(b"-----BEGIN CERTIFICATE-----"));
        let pem = std::fs::read(dir.join("tls/tls.pem")).unwrap();
        assert_eq!(pem, [crt.clone(), key].concat());
        let second = ensure_tls_with(&dir, &hosts, "mkcert-not-installed").unwrap();
        assert_eq!(second, CertSource::Kept);
        assert_eq!(std::fs::read(dir.join("tls/tls.crt")).unwrap(), crt);
    }

    /// The names end up on a `mkcert` command line: only host names and
    /// address literals pass.
    #[test]
    fn a_certificate_name_is_a_host_or_an_address() {
        for ok in [
            "localhost",
            "shop.example.org",
            "*.example.org",
            "10.0.0.5",
            "::1",
        ] {
            assert!(valid_cert_host(ok), "{ok}");
        }
        for bad in ["", "-install", "a b", "x;y", "$(id)", "a/b"] {
            assert!(!valid_cert_host(bad), "{bad}");
        }
    }

    /// What `--up` says at the end: an HTTPS address for a template that
    /// serves TLS, and the factory credentials for one that has them.
    #[test]
    fn the_up_summary_names_the_address_and_the_credentials() {
        let nginx = up_summary(
            "nginx",
            "edge",
            &Plan::defaults("nginx"),
            Some(CertSource::SelfSigned),
            None,
        )
        .join("\n");
        assert!(nginx.contains("https://localhost:8443/"), "{nginx}");
        assert!(nginx.contains("self-signed"), "{nginx}");
        assert!(!nginx.contains("pass:"), "{nginx}");

        let odoo = up_summary("odoo", "erp", &Plan::defaults("odoo"), None, None).join("\n");
        assert!(odoo.contains("http://localhost:8069/web"), "{odoo}");
        assert!(odoo.contains("user:    admin"), "{odoo}");
        assert!(odoo.contains("pass:    admin"), "{odoo}");
        assert!(odoo.contains("change them"), "{odoo}");
    }

    /// The Laravel manifest references `<name>-app`; `--up` is told how to
    /// create it, and says so when it did.
    #[test]
    fn a_template_declares_the_secret_up_creates() {
        assert_eq!(
            meta_kv("laravel", "up_secret"),
            Some("app APP_KEY=base64:{random32}")
        );
        let manifest = TEMPLATES
            .iter()
            .find(|(n, _)| *n == "laravel")
            .and_then(|(_, f)| f.iter().find(|(rel, _, _)| *rel == "delonix-manifest.yaml"))
            .map(|(_, body, _)| *body)
            .unwrap();
        assert!(manifest.contains("- __NAME__-app"), "{manifest}");
        let said = up_summary(
            "laravel",
            "shop",
            &Plan::defaults("laravel"),
            None,
            Some("shop-app"),
        )
        .join("\n");
        assert!(said.contains("shop-app was created"), "{said}");
    }

    /// `wait=` is read: Odoo declares 300 s, a template that says nothing gets 120.
    #[test]
    fn the_health_wait_comes_from_the_template() {
        assert_eq!(wait_secs("odoo"), 300);
        assert_eq!(wait_secs("nginx"), 120);
    }

    /// A flag is an answer: it replaces the template's port without a prompt,
    /// and what it cannot mean is refused instead of being dropped.
    #[test]
    fn the_port_and_hostname_flags_settle_the_plan() {
        let edge = EdgeArgs {
            port: Some(18080),
            tls_port: Some(18443),
            hostname: vec!["shop.test".into()],
        };
        let plan = plan_for("nginx", false, &edge).unwrap();
        assert_eq!(plan.port, "18080");
        assert_eq!(plan.tls_port.as_deref(), Some("18443"));
        assert!(plan.hosts.iter().any(|h| h == "shop.test"));
        assert!(plan.hosts.iter().any(|h| h == "localhost"));

        // A template with no TLS takes --port and refuses the other two.
        let only_port = EdgeArgs {
            port: Some(18000),
            ..Default::default()
        };
        assert_eq!(plan_for("go", false, &only_port).unwrap().port, "18000");
        let tls_on_go = EdgeArgs {
            tls_port: Some(18443),
            ..Default::default()
        };
        assert!(plan_for("go", false, &tls_on_go).is_err());
        let host_on_go = EdgeArgs {
            hostname: vec!["a.test".into()],
            ..Default::default()
        };
        assert!(plan_for("go", false, &host_on_go).is_err());

        // The same port twice, and a name that is not one.
        let same = EdgeArgs {
            port: Some(18443),
            tls_port: Some(18443),
            ..Default::default()
        };
        assert!(plan_for("nginx", false, &same).is_err());
        let bad = EdgeArgs {
            hostname: vec!["a b;c".into()],
            ..Default::default()
        };
        assert!(plan_for("nginx", false, &bad).is_err());
    }

    /// What identifies the project a recipe builds. Case and the rest of the
    /// line do not matter; a `FROM` that is only part of a word does not count.
    #[test]
    fn from_lines_reads_the_base_images_and_nothing_else() {
        assert_eq!(
            from_lines("# c\nFROM nginx:1.30-alpine\nCOPY a b\n"),
            vec!["FROM nginx:1.30-alpine"]
        );
        assert_eq!(
            from_lines("FROM golang:1.25 AS build\nFROM alpine:3.22\n"),
            vec!["FROM golang:1.25 AS build", "FROM alpine:3.22"]
        );
        assert!(from_lines("FROMAGE is not a stage\nCOPY a b\n").is_empty());
        // A line whose byte 5 falls INSIDE a character: `&l[..5]` panics on
        // these two (measured — an em dash happens to land on a boundary and
        // would NOT have caught it), and a Delonixfile is someone else's file.
        assert_eq!(
            from_lines("日本語 nota\nFROM alpine:3.22\n"),
            vec!["FROM alpine:3.22"]
        );
        assert!(from_lines("ãéíóú\n").is_empty());
    }

    /// The production bug: `init -t odoo` over a directory that already holds
    /// an nginx project wrote nothing, said "nothing to do" and then `--up`
    /// built the nginx project. Refused now, naming both base images.
    #[test]
    fn a_directory_holding_another_project_is_refused_and_names_both() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        let dfile = dir.join("Delonixfile");
        std::fs::write(&dfile, "FROM nginx:1.30-alpine\nCOPY html /x\n").expect("write");
        let outputs = vec![(
            dfile.clone(),
            "FROM odoo:20.0\nCOPY addons /mnt\n".to_string(),
            false,
        )];
        let err = refuse_foreign_project(&outputs, "odoo", "demo", dir, false)
            .expect_err("a foreign Delonixfile is a conflict");
        let msg = err.to_string();
        assert!(
            msg.contains("nginx:1.30-alpine"),
            "names what is there: {msg}"
        );
        assert!(msg.contains("odoo:20.0"), "names what was asked: {msg}");
        // `--force` is the answer the refusal offers, so it must go through.
        assert!(refuse_foreign_project(&outputs, "odoo", "demo", dir, true).is_ok());
    }

    /// The idempotent second `--up` has to keep working: the same template
    /// with the same flags renders byte-identical glue. And a hand-edited
    /// config is the normal state of a generated project — it is not glue and
    /// must never stop anything.
    #[test]
    fn the_same_project_again_goes_through_and_config_is_not_glue() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        let dfile = dir.join("Delonixfile");
        let same = "FROM nginx:1.30-alpine\nCOPY html /x\n".to_string();
        std::fs::write(&dfile, &same).expect("write");
        assert!(
            refuse_foreign_project(&[(dfile, same, false)], "nginx", "demo", dir, false).is_ok()
        );

        let conf = dir.join("nginx.conf");
        std::fs::write(&conf, "worker_processes 2;\n").expect("write");
        assert!(refuse_foreign_project(
            &[(conf, "worker_processes 8;\n".to_string(), false)],
            "nginx",
            "demo",
            dir,
            false
        )
        .is_ok());
    }

    /// The remedy names the flag that was actually given — `--tls-port 943`
    /// must not be answered with advice about `--port`.
    #[test]
    fn the_privileged_port_remedy_names_the_flag_that_was_given() {
        let msg = privileged_port_message("--tls-port", "943");
        assert!(msg.contains("--tls-port 943"), "{msg}");
        assert!(msg.contains("--tls-port 8943"), "{msg}");
        assert!(!msg.contains("--port 8943"), "{msg}");
        assert!(msg.contains("--low-ports"), "{msg}");
    }

    /// The contract of ADR-0061, as a gate: a project generated from ANY
    /// template is born with the engineering pieces, not with a promise that
    /// someone will add them. Measured before this test: `nginx`, `httpd`,
    /// `haproxy` and `odoo` had no CI, no SonarQube and no CONTRIBUTING at
    /// all, and `nestjs` had lost its commitlint config while its two
    /// siblings kept theirs — nothing said so.
    #[test]
    fn every_template_is_born_with_the_engineering_contract() {
        for (name, files) in TEMPLATES {
            let has = |rel: &str| files.iter().any(|(r, _, _)| *r == rel);
            let body = |rel: &str| {
                files
                    .iter()
                    .find(|(r, _, _)| *r == rel)
                    .map(|(_, c, _)| *c)
                    .unwrap_or("")
            };
            for rel in [
                ".github/workflows/ci.yml",
                ".gitlab-ci.yml",
                "sonar-project.properties",
                "CONTRIBUTING.md",
                "README.md",
            ] {
                assert!(has(rel), "{name} ships no {rel}");
            }
            // The CI is two CIs, and they have to be the same gate: the
            // GitLab one says so in its first lines.
            assert!(
                body(".gitlab-ci.yml").contains(".github/workflows/ci.yml"),
                "{name}: .gitlab-ci.yml does not say it is the same gate as the GitHub one"
            );
            // Conventional Commits: a config where the ecosystem is Node,
            // documented everywhere — never a tool the project assumes is
            // installed.
            assert!(
                body("CONTRIBUTING.md").contains("conventionalcommits.org"),
                "{name}: CONTRIBUTING.md does not document Conventional Commits"
            );
            // The advisory architecture section: what this project is, where
            // it can go, and what NOT to do yet.
            let readme = body("README.md");
            for needle in [
                "## Architecture: where to take this",
                "### What not to do yet",
            ] {
                assert!(
                    readme.contains(needle),
                    "{name}: README.md has no {needle:?} section"
                );
            }
        }
    }

    /// A Node package gets the commitlint config, and the extension has to
    /// match the package's module system — a bare `.js` in a CommonJS package
    /// is parsed as CommonJS and the `export default` inside never loads.
    #[test]
    fn the_node_templates_carry_a_commitlint_config() {
        for name in ["node", "nestjs", "nextjs"] {
            let files = TEMPLATES
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, f)| *f)
                .unwrap_or_else(|| panic!("no template {name}"));
            assert!(
                files
                    .iter()
                    .any(|(r, _, _)| r.starts_with("commitlint.config.")),
                "{name} ships no commitlint config"
            );
            // And whatever its extension, the adopt path must treat it as a
            // CI file: otherwise it lands in a project whose CI was skipped.
            let rel = files
                .iter()
                .map(|(r, _, _)| *r)
                .find(|r| r.starts_with("commitlint.config."))
                .unwrap();
            assert!(
                CI_FILES.contains(&rel),
                "{name}: {rel} is not in CI_FILES, so adoption writes it without the CI"
            );
        }
    }

    /// Every application template (go, node, nestjs, nextjs, fastapi, django, laravel)
    /// includes VM support: a VMfile, vm.yaml, cloud-init/user-data, and .delonixignore.
    /// This allows `delonix vm image build` to work out-of-the-box.
    #[test]
    fn every_application_template_supports_vm_build() {
        let vm_templates = [
            "go", "node", "nestjs", "nextjs", "fastapi", "django", "laravel",
        ];

        for name in &vm_templates {
            let (_, files) = TEMPLATES
                .iter()
                .find(|(n, _)| *n == *name)
                .unwrap_or_else(|| panic!("no template {name}"));

            // Check for VMfile
            assert!(
                files.iter().any(|(p, _, _)| *p == "VMfile"),
                "template {name} missing VMfile"
            );

            // Check for cloud-init/user-data
            assert!(
                files.iter().any(|(p, _, _)| *p == "cloud-init/user-data"),
                "template {name} missing cloud-init/user-data"
            );

            // Check for .delonixignore
            assert!(
                files.iter().any(|(p, _, _)| *p == ".delonixignore"),
                "template {name} missing .delonixignore"
            );

            // Check template.meta has vm_support=yes
            let has_vm_support = TEMPLATE_KV
                .iter()
                .find(|(n, _)| *n == *name)
                .map(|(_, pairs)| pairs.iter().any(|(k, v)| *k == "vm_support" && *v == "yes"))
                .unwrap_or(false);
            assert!(
                has_vm_support,
                "template {name} missing vm_support=yes in template.meta"
            );
        }
    }

    /// An existing repository is the project's own history, not ours to add
    /// to: the helper leaves it alone.
    #[test]
    fn an_existing_repository_is_left_alone() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // no git here; the scaffold says so and carries on
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("app");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let ok = std::process::Command::new("git")
            .args([
                "-C",
                &dir.display().to_string(),
                "init",
                "--initial-branch=main",
            ])
            .output()
            .is_ok_and(|o| o.status.success());
        assert!(ok, "git init works here");
        assert_eq!(git_init(&dir), None, "an existing repository is left alone");
    }

    /// A nested repository inside someone else's work tree is a trap that only
    /// shows up when a push lands without the files. The helper refuses it and
    /// says why.
    #[test]
    fn a_monorepo_subdirectory_never_gets_a_nested_repository() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let ok = std::process::Command::new("git")
            .args(["-C", &root.display().to_string(), "init"])
            .output()
            .is_ok_and(|o| o.status.success());
        assert!(ok, "git init works here");
        let sub = root.join("svc");
        std::fs::create_dir_all(&sub).expect("mkdir");
        let why = git_init(&sub).expect("a nested repository is refused");
        assert!(why.contains("already inside"), "{why}");
        assert!(!sub.join(".git").exists(), "nothing was created");
    }

    /// A generated project is a git repository from the moment `init`
    /// finishes, so what a template tells you to keep out of git has to be
    /// ignored by the template itself. Measured before this test: `odoo` was
    /// the only one of the eleven with no `.gitignore` at all, while both its
    /// README and its manifest said to move the database password into
    /// `.secrets.env`, "a file you keep OUT of git" — following that advice
    /// put the password in a file git was already watching. `.dockerignore`
    /// keeps a file out of an IMAGE; it is not the same file and not the same
    /// promise.
    ///
    /// The rule is deliberately concrete — a `.gitignore` for everyone, plus
    /// the two paths this engine KNOWS carry a secret — instead of trying to
    /// parse every promise out of prose. A template that invents a third one
    /// is not caught here, and that is written down rather than pretended.
    #[test]
    fn no_template_tells_you_to_keep_a_secret_in_a_file_git_is_watching() {
        for (name, files) in TEMPLATES {
            let body = |rel: &str| files.iter().find(|(r, _, _)| *r == rel).map(|(_, c, _)| *c);
            let ignore = body(".gitignore").unwrap_or_else(|| {
                panic!("{name} ships no .gitignore, so nothing is kept out of git")
            });
            // The private key of a template that serves TLS.
            if meta_kv(name, "tls").is_some() {
                assert!(
                    ignore.lines().any(|l| l.trim() == "tls/"),
                    "{name}: serves TLS and does not ignore tls/"
                );
            }
            // A secrets file the template's own documents point at.
            let points_at_secrets = [".dockerignore", "README.md", "delonix-manifest.yaml"]
                .iter()
                .filter_map(|r| body(r))
                .any(|c| c.contains(".secrets.env"));
            if points_at_secrets {
                assert!(
                    ignore.lines().any(|l| l.trim() == ".secrets.env"),
                    "{name}: points at .secrets.env and does not ignore it"
                );
            }
        }
    }
}
