//! `vm.yaml` — the compose-style file of `vm build`.
//!
//! The pair it completes: a `VMfile` is to a VM image what a `Dockerfile` is
//! to a container image (one recipe, imperative steps), and a `vm.yaml` is what
//! a `compose.yaml` is to it — it names the images of a folder, says how each
//! is built, and carries the parameters that make the qcow2 complete (size,
//! packages, users, services, cloud-init, what to REMOVE, cleanup).
//!
//! It is a front end and nothing else. Every image compiles to one of the two
//! builders that already exist, so there is no second build engine to drift:
//!
//! * a **custom** image becomes a [`VmFile`] and goes through
//!   `vmfile::build_parsed`;
//! * a **`profile: rootless|k8s`** image is the golden recipe, and goes
//!   through `vmimage::cmd_build` with the same arguments the flags would
//!   have produced;
//! * an image with `build.file` points at a `VMfile` on disk and nothing else.
//!
//! **Fail-closed, like the `VMfile` parser.** An unknown key is an error
//! (`deny_unknown_fields`), and so is a field the chosen route cannot honour:
//! accepting a `hostname:` that the golden recipe then ignores is the failure
//! this repo names as its worst.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use delonix_model::{Error, Result};
use serde::Deserialize;

use super::vmfile::{Stage, Step, VmFile};

/// The names `vm build` looks for in a context, in order.
pub(crate) const SPEC_FILES: &[&str] = &["vm.yaml", "vm.yml"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Spec {
    /// Schema version. Only `1` exists; the field is there so a future `2`
    /// can refuse an old engine instead of half-reading it.
    #[serde(default)]
    pub(crate) version: Option<u32>,
    pub(crate) images: BTreeMap<String, Image>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Image {
    /// Tag of the result. `-t` on the command line wins.
    pub(crate) tag: Option<String>,
    /// `ubuntu` | `debian` | `rocky` | `fedora`.
    pub(crate) distro: Option<String>,
    /// Release of that distro (`26.04`, `bookworm`, `9`, `42-1.1`).
    pub(crate) release: Option<String>,
    /// A base that is not a distro release: an absolute URL to a qcow2, or an
    /// image already in the local store.
    pub(crate) from: Option<String>,
    /// `apt` | `dnf`. Only needed when it cannot be derived from `distro`.
    pub(crate) package_manager: Option<String>,
    /// `custom` (default) | `rootless` | `k8s` — the last two are the golden
    /// recipes.
    pub(crate) profile: Option<String>,
    /// Point at a `VMfile` instead of describing the image here.
    pub(crate) build: Option<BuildRef>,
    pub(crate) size: Option<String>,
    pub(crate) hostname: Option<String>,
    pub(crate) vcpus: Option<u32>,
    pub(crate) memory: Option<String>,
    pub(crate) hypervisor: Option<String>,
    pub(crate) labels: BTreeMap<String, String>,
    /// Network inside the guest DURING the build. Off by default: a build that
    /// reaches the internet gives a different image depending on the day.
    pub(crate) network: bool,
    pub(crate) compress: Option<bool>,
    pub(crate) packages: Packages,
    pub(crate) remove: Remove,
    pub(crate) services: Services,
    pub(crate) users: Vec<User>,
    pub(crate) root_password: Option<String>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) files: Vec<FileCopy>,
    /// Path of a cloud-init user-data file baked in as the image's default.
    pub(crate) cloud_init: Option<String>,
    /// Extra commands, run inside the guest after packages and before removal.
    pub(crate) run: Vec<String>,
    pub(crate) cleanup: Cleanup,
    pub(crate) k8s: Option<K8s>,
    /// Bare `true` means `0.0.0.0:9100`; a string is the listen address.
    pub(crate) node_exporter: Option<NodeExporter>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BuildRef {
    pub(crate) file: Option<String>,
    pub(crate) context: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Packages {
    pub(crate) install: Vec<String>,
    /// Bring the base up to date before installing.
    pub(crate) upgrade: bool,
}

/// What the image must NOT contain — removed after everything else ran, so it
/// can also prune what a `run:` step or a package pulled in.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Remove {
    pub(crate) packages: Vec<String>,
    pub(crate) paths: Vec<String>,
    pub(crate) users: Vec<String>,
    /// Disabled AND masked, so nothing pulls them back in.
    pub(crate) services: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Services {
    pub(crate) enable: Vec<String>,
    pub(crate) disable: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct User {
    pub(crate) name: String,
    pub(crate) groups: Vec<String>,
    /// Passwordless sudo.
    pub(crate) sudo: bool,
    pub(crate) shell: Option<String>,
    pub(crate) password: Option<String>,
    /// Each entry is a path to a public key file or the key itself.
    pub(crate) ssh_keys: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct FileCopy {
    pub(crate) src: String,
    pub(crate) dst: String,
    /// Octal, e.g. `0644`.
    pub(crate) mode: Option<String>,
}

/// Housekeeping that makes the qcow2 reusable. Each one defaults to what a
/// published image wants; say `false` to keep it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Cleanup {
    pub(crate) package_cache: bool,
    pub(crate) logs: bool,
    pub(crate) history: bool,
    pub(crate) tmp: bool,
    /// Empty `/etc/machine-id`, so every VM cloned from the image gets its own.
    pub(crate) machine_id: bool,
    /// Off by default: cloud-init regenerates them per instance already.
    pub(crate) ssh_host_keys: bool,
}

impl Default for Cleanup {
    fn default() -> Self {
        Cleanup {
            package_cache: true,
            logs: true,
            history: true,
            tmp: true,
            machine_id: true,
            ssh_host_keys: false,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct K8s {
    pub(crate) version: Option<String>,
    #[serde(default)]
    pub(crate) offline: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum NodeExporter {
    Enabled(bool),
    Addr(String),
}

/// How the image is produced, after validation.
pub(crate) enum Route {
    /// A `VMfile` synthesised from the fields.
    Custom(Box<VmFile>),
    /// The golden recipe.
    Golden(Box<Golden>),
    /// A `VMfile` that already exists on disk.
    File { file: PathBuf, context: PathBuf },
}

/// The arguments `vmimage::cmd_build` takes, resolved from the fields.
pub(crate) struct Golden {
    pub(crate) distro: String,
    pub(crate) ubuntu_release: String,
    pub(crate) debian_release: String,
    pub(crate) rocky_release: String,
    pub(crate) fedora_release: String,
    pub(crate) k8s_version: Option<String>,
    pub(crate) offline: bool,
    pub(crate) no_k8s: bool,
    pub(crate) extra_packages: Vec<String>,
    pub(crate) extra_run: Vec<String>,
    pub(crate) root_password: Option<String>,
    pub(crate) node_exporter: Option<String>,
}

/// One image, ready to build.
pub(crate) struct Plan {
    pub(crate) name: String,
    pub(crate) tag: Option<String>,
    pub(crate) route: Route,
    pub(crate) network: bool,
    pub(crate) compress: bool,
}

// ─── templating ────────────────────────────────────────────────────────────

/// Expands `${NAME}` and `${NAME:-default}` from `vars`, then the process
/// environment. An unset name with no default is an ERROR: leaving it literal
/// would put the text `${TAG}` inside an image tag.
pub(crate) fn expand(text: &str, vars: &BTreeMap<String, String>) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let end = after.find('}').ok_or_else(|| {
            Error::Invalid("vm.yaml: unterminated `${` — close it with `}`".into())
        })?;
        let body = &after[..end];
        let (name, default) = match body.split_once(":-") {
            Some((n, d)) => (n, Some(d)),
            None => (body, None),
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(Error::Invalid(format!(
                "vm.yaml: `${{{body}}}` is not a variable name (letters, digits and `_` only)"
            )));
        }
        let val = vars
            .get(name)
            .cloned()
            .or_else(|| std::env::var(name).ok().filter(|v| !v.is_empty()))
            .or_else(|| default.map(str::to_string));
        match val {
            Some(v) => out.push_str(&v),
            None => {
                return Err(Error::Invalid(format!(
                    "vm.yaml: `${{{name}}}` is not set — pass `-t`, export it, or write `${{{name}:-default}}`"
                )))
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn expand_value(v: &mut serde_yaml::Value, vars: &BTreeMap<String, String>) -> Result<()> {
    use serde_yaml::Value;
    match v {
        Value::String(t) => *t = expand(t, vars)?,
        Value::Sequence(seq) => {
            for x in seq {
                expand_value(x, vars)?;
            }
        }
        Value::Mapping(m) => {
            for (_, x) in m.iter_mut() {
                expand_value(x, vars)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Parses a `vm.yaml`. `tag` (from `-t`) becomes `${TAG}`.
pub(crate) fn parse(text: &str, tag: Option<&str>) -> Result<Spec> {
    let mut vars = BTreeMap::new();
    if let Some(t) = tag {
        vars.insert("TAG".to_string(), t.to_string());
    }
    // Expanded on the PARSED values, not on the raw text: a `${TAG}` in a
    // comment (the natural place to document it) must not be evaluated.
    let mut value: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|e| Error::Invalid(format!("vm.yaml: {e}")))?;
    expand_value(&mut value, &vars)?;
    let spec: Spec =
        serde_yaml::from_value(value).map_err(|e| Error::Invalid(format!("vm.yaml: {e}")))?;
    match spec.version {
        None | Some(1) => {}
        Some(v) => {
            return Err(Error::Invalid(format!(
                "vm.yaml: version {v} is newer than this engine understands (1)"
            )))
        }
    }
    if spec.images.is_empty() {
        return Err(Error::Invalid(
            "vm.yaml: `images:` is empty — nothing to build".into(),
        ));
    }
    Ok(spec)
}

// ─── validation of values that end up inside a shell ────────────────────────

fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn bad(what: &str, v: &str) -> Error {
    Error::Invalid(format!("vm.yaml: {what} '{v}' is not valid"))
}

fn valid_pkg(p: &str) -> bool {
    !p.is_empty()
        && p.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || "+._:=~-".contains(c))
}

fn valid_unit(u: &str) -> bool {
    !u.is_empty()
        && !u.starts_with('-')
        && u.chars()
            .all(|c| c.is_ascii_alphanumeric() || "@._:-".contains(c))
}

fn valid_account(u: &str) -> bool {
    !u.is_empty()
        && u.len() <= 32
        && u.chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && u.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Directories whose removal would leave a guest that cannot boot. Refusing
/// them by NAME is a floor, not a guarantee — a `run:` can still do anything —
/// but a typo in `remove.paths` should not be able to empty `/etc`.
const PROTECTED: &[&str] = &[
    "/",
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/lib",
    "/lib64",
    "/proc",
    "/root",
    "/run",
    "/sbin",
    "/sys",
    "/usr",
    "/var",
    "/home",
    "/usr/bin",
    "/usr/lib",
    "/usr/sbin",
    "/etc/systemd",
    "/var/lib",
    "/var/log",
];

fn valid_remove_path_like(p: &str) -> std::result::Result<(), String> {
    valid_remove_path(p)
}

fn valid_remove_path(p: &str) -> std::result::Result<(), String> {
    if !p.starts_with('/') {
        return Err("must be absolute".into());
    }
    if p.split('/').any(|c| c == "..") {
        return Err("must not contain `..`".into());
    }
    if p.chars().any(|c| c.is_control() || c == '\'' || c == '"') {
        return Err("must not contain quotes or control characters".into());
    }
    let trimmed = p.trim_end_matches('/');
    let norm = if trimmed.is_empty() { "/" } else { trimmed };
    if PROTECTED.contains(&norm) {
        return Err(format!(
            "`{norm}` is protected — remove something inside it instead"
        ));
    }
    Ok(())
}

// ─── planning ───────────────────────────────────────────────────────────────

fn pkg_manager(img: &Image) -> Option<&'static str> {
    match img.package_manager.as_deref() {
        Some("apt") => Some("apt"),
        Some("dnf") => Some("dnf"),
        Some(_) => None,
        None => match img.distro.as_deref() {
            Some("ubuntu") | Some("debian") => Some("apt"),
            Some("rocky") | Some("fedora") => Some("dnf"),
            _ => None,
        },
    }
}

fn used_content_fields(img: &Image) -> Vec<&'static str> {
    let mut v = Vec::new();
    let mut add = |on: bool, n: &'static str| {
        if on {
            v.push(n)
        }
    };
    add(img.distro.is_some(), "distro");
    add(img.release.is_some(), "release");
    add(img.from.is_some(), "from");
    add(img.size.is_some(), "size");
    add(img.hostname.is_some(), "hostname");
    add(img.vcpus.is_some(), "vcpus");
    add(img.memory.is_some(), "memory");
    add(img.hypervisor.is_some(), "hypervisor");
    add(!img.labels.is_empty(), "labels");
    add(
        !img.packages.install.is_empty() || img.packages.upgrade,
        "packages",
    );
    add(
        !img.remove.packages.is_empty()
            || !img.remove.paths.is_empty()
            || !img.remove.users.is_empty()
            || !img.remove.services.is_empty(),
        "remove",
    );
    add(
        !img.services.enable.is_empty() || !img.services.disable.is_empty(),
        "services",
    );
    add(!img.users.is_empty(), "users");
    add(img.root_password.is_some(), "root_password");
    add(!img.env.is_empty(), "env");
    add(!img.files.is_empty(), "files");
    add(img.cloud_init.is_some(), "cloud_init");
    add(!img.run.is_empty(), "run");
    add(img.k8s.is_some(), "k8s");
    add(img.node_exporter.is_some(), "node_exporter");
    v
}

/// Validates one image and decides how it is built. `dir` is the folder of the
/// `vm.yaml`: every relative path in the file is relative to it, exactly as in
/// a `compose.yaml`.
pub(crate) fn plan(name: &str, img: &Image, dir: &Path, cli_network: bool) -> Result<Plan> {
    let ctx = |m: String| Error::Invalid(format!("vm.yaml, image '{name}': {m}"));
    let compress = img.compress.unwrap_or(true);
    let base = |route| Plan {
        name: name.to_string(),
        tag: img.tag.clone(),
        route,
        network: img.network,
        compress,
    };

    // ── build.file: a VMfile that already exists ───────────────────────────
    if let Some(b) = &img.build {
        let used = used_content_fields(img);
        if !used.is_empty() || img.profile.is_some() {
            let mut all: Vec<&str> = used;
            if img.profile.is_some() {
                all.push("profile");
            }
            return Err(ctx(format!(
                "`build:` points at a VMfile, which describes the image itself — remove {}",
                all.join(", ")
            )));
        }
        let ctxdir = dir.join(b.context.as_deref().unwrap_or("."));
        let file = dir.join(b.file.as_deref().unwrap_or("VMfile"));
        return Ok(base(Route::File {
            file,
            context: ctxdir,
        }));
    }

    if let Some(v) = &img.vcpus {
        if *v == 0 {
            return Err(ctx("`vcpus` must be at least 1".into()));
        }
    }
    if let Some(h) = &img.hypervisor {
        delonix_vm::valid_backend_name(h).map_err(|e| ctx(e.to_string()))?;
    }

    // ── golden profiles ────────────────────────────────────────────────────
    let profile = img.profile.as_deref().unwrap_or("custom");
    match profile {
        "custom" => {}
        "rootless" | "k8s" => {
            return plan_golden(
                name,
                img,
                profile,
                base(Route::File {
                    file: PathBuf::new(),
                    context: PathBuf::new(),
                }),
            )
        }
        other => {
            return Err(ctx(format!(
                "profile '{other}' unknown — custom, rootless or k8s"
            )))
        }
    }
    if img.k8s.is_some() || img.node_exporter.is_some() {
        return Err(ctx(
            "`k8s:` and `node_exporter:` belong to `profile: rootless|k8s`".into(),
        ));
    }

    // ── custom: compile to a VMfile ────────────────────────────────────────
    let from = match (&img.from, &img.distro, &img.release) {
        (Some(f), None, None) => f.clone(),
        (None, Some(d), Some(r)) => {
            if !matches!(d.as_str(), "ubuntu" | "debian" | "rocky" | "fedora") {
                return Err(ctx(format!(
                    "distro '{d}' unknown — ubuntu, debian, rocky or fedora"
                )));
            }
            format!("{d}:{r}")
        }
        (None, Some(_), None) => return Err(ctx("`distro` needs a `release`".into())),
        (None, None, _) => {
            return Err(ctx(
                "needs a base: `distro` + `release`, or `from`, or `build`".into(),
            ))
        }
        (Some(_), _, _) => {
            return Err(ctx(
                "`from` and `distro`/`release` are two ways to say the same thing — use one".into(),
            ))
        }
    };

    let pm = pkg_manager(img);
    let installs = !img.packages.install.is_empty() || img.packages.upgrade;
    if installs && !(img.network || cli_network) {
        return Err(ctx(
            "`packages` needs the network inside the guest — set `network: true` \
             (a build that reaches the internet is not reproducible; that is why it is opt-in)"
                .into(),
        ));
    }
    let touches_packages = installs || !img.remove.packages.is_empty();
    if touches_packages && pm.is_none() {
        return Err(ctx(
            "cannot tell the package manager — set `distro`, or `package_manager: apt|dnf`".into(),
        ));
    }
    for p in img.packages.install.iter().chain(&img.remove.packages) {
        if !valid_pkg(p) {
            return Err(bad("package name", p));
        }
    }
    for u in img
        .services
        .enable
        .iter()
        .chain(&img.services.disable)
        .chain(&img.remove.services)
    {
        if !valid_unit(u) {
            return Err(bad("service name", u));
        }
    }
    for p in &img.remove.paths {
        valid_remove_path(p).map_err(|m| ctx(format!("remove.paths '{p}': {m}")))?;
    }
    for u in &img.remove.users {
        if !valid_account(u) || u == "root" {
            return Err(bad("remove.users entry", u));
        }
    }
    for k in img.env.keys() {
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(bad("env name", k));
        }
    }
    if let Some(s) = &img.size {
        if s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') {
            return Err(bad("size", s));
        }
    }
    if let Some(h) = &img.hostname {
        if h.is_empty()
            || h.starts_with('-')
            || !h
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        {
            return Err(bad("hostname", h));
        }
    }

    let mut st = Stage {
        name: None,
        from,
        steps: Vec::new(),
        size: img.size.clone(),
        hostname: img.hostname.clone(),
    };
    let run = |st: &mut Stage, s: String| st.steps.push(Step::Run(s));

    for (k, v) in &img.env {
        st.steps.push(Step::Env {
            key: k.clone(),
            value: v.clone(),
        });
    }

    for u in &img.users {
        if !valid_account(&u.name) {
            return Err(bad("user name", &u.name));
        }
        st.steps.push(Step::User(u.name.clone()));
        for g in &u.groups {
            if !valid_account(g) {
                return Err(bad("group name", g));
            }
        }
        if !u.groups.is_empty() {
            run(
                &mut st,
                format!("usermod -aG {} {}", u.groups.join(","), u.name),
            );
        }
        if let Some(sh) = &u.shell {
            if !sh.starts_with('/') || sh.chars().any(|c| c.is_whitespace() || c == '\'') {
                return Err(bad("shell", sh));
            }
            run(&mut st, format!("usermod -s {} {}", sq(sh), u.name));
        }
        if u.sudo {
            run(
                &mut st,
                format!(
                    "printf '%s\\n' '{n} ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/90-{n} && chmod 440 /etc/sudoers.d/90-{n}",
                    n = u.name
                ),
            );
        }
        if let Some(pw) = &u.password {
            st.steps.push(Step::Password {
                user: u.name.clone(),
                password: pw.clone(),
            });
        }
        for k in &u.ssh_keys {
            st.steps.push(Step::SshKey {
                user: u.name.clone(),
                key: k.clone(),
            });
        }
    }
    if let Some(pw) = &img.root_password {
        st.steps.push(Step::RootPassword(pw.clone()));
    }

    for (i, f) in img.files.iter().enumerate() {
        if f.src.is_empty() || f.dst.is_empty() {
            return Err(ctx("each `files` entry needs `src` and `dst`".into()));
        }
        // `dst` is the full path of the file inside the image, as with `cp`.
        // The engine underneath (`virt-customize --copy-in`) only copies INTO
        // a directory and keeps the name, so the file goes through a staging
        // directory and is moved to where the recipe said. Found by building a
        // real image: a recipe with `dst: /etc/motd` failed with "target is not
        // a directory".
        valid_remove_path_like(&f.dst).map_err(|m| ctx(format!("files.dst '{}': {m}", f.dst)))?;
        let base = Path::new(&f.src)
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| {
                !n.is_empty() && !n.chars().any(|c| c == '\'' || c == '"' || c.is_control())
            })
            .ok_or_else(|| bad("files.src", &f.src))?
            .to_string();
        let stage = format!("/tmp/.delonix-files-{i}");
        run(&mut st, format!("mkdir -p {stage}"));
        st.steps.push(Step::Copy {
            src: f.src.clone(),
            dst: stage.clone(),
        });
        run(
            &mut st,
            format!(
                "mkdir -p -- \"$(dirname -- {d})\" && mv -f -- {stage}/{b} {d} && rm -rf {stage}",
                d = sq(&f.dst),
                b = sq(&base),
            ),
        );
        if let Some(m) = &f.mode {
            if m.is_empty() || m.len() > 4 || !m.chars().all(|c| ('0'..='7').contains(&c)) {
                return Err(bad("file mode", m));
            }
            run(&mut st, format!("chmod {m} {}", sq(&f.dst)));
        }
    }

    if let Some(pm) = pm {
        if img.packages.upgrade {
            run(
                &mut st,
                match pm {
                    "apt" => "export DEBIAN_FRONTEND=noninteractive; apt-get update && apt-get -y upgrade".into(),
                    _ => "dnf -y upgrade".into(),
                },
            );
        }
        if !img.packages.install.is_empty() {
            let list = img.packages.install.join(" ");
            run(
                &mut st,
                match pm {
                    "apt" => format!(
                        "export DEBIAN_FRONTEND=noninteractive; apt-get update && apt-get install -y {list}"
                    ),
                    _ => format!("dnf install -y {list}"),
                },
            );
        }
    }

    for c in &img.run {
        run(&mut st, c.clone());
    }

    // ── removal: after everything that could have added it ──────────────────
    if let (Some(pm), false) = (pm, img.remove.packages.is_empty()) {
        let list = img.remove.packages.join(" ");
        run(
            &mut st,
            match pm {
                "apt" => format!(
                    "export DEBIAN_FRONTEND=noninteractive; apt-get purge -y {list} && apt-get autoremove -y --purge"
                ),
                _ => format!("dnf remove -y {list}"),
            },
        );
    }
    for u in &img.remove.users {
        run(&mut st, format!("userdel -r {u} || true"));
    }
    for unit in &img.remove.services {
        run(
            &mut st,
            format!("systemctl disable {unit} 2>/dev/null || true; systemctl mask {unit}"),
        );
    }
    for p in &img.remove.paths {
        run(&mut st, format!("rm -rf -- {}", sq(p)));
    }

    for unit in &img.services.enable {
        run(&mut st, format!("systemctl enable {unit}"));
    }
    for unit in &img.services.disable {
        run(&mut st, format!("systemctl disable {unit}"));
    }

    // ── cleanup, last of the shell steps ────────────────────────────────────
    let c = &img.cleanup;
    if c.package_cache {
        match pm {
            Some("apt") => run(
                &mut st,
                "apt-get clean && rm -rf /var/lib/apt/lists/*".into(),
            ),
            Some(_) => run(&mut st, "dnf clean all".into()),
            None => {}
        }
    }
    if c.history {
        run(
            &mut st,
            "rm -f /root/.bash_history /home/*/.bash_history".into(),
        );
    }
    if c.tmp {
        run(&mut st, "rm -rf /tmp/* /var/tmp/*".into());
    }
    if c.logs {
        run(
            &mut st,
            "find /var/log -type f -exec truncate -s 0 {} +".into(),
        );
    }
    if c.ssh_host_keys {
        run(&mut st, "rm -f /etc/ssh/ssh_host_*".into());
    }
    if c.machine_id {
        run(
            &mut st,
            "truncate -s 0 /etc/machine-id; mkdir -p /var/lib/dbus; ln -sf /etc/machine-id /var/lib/dbus/machine-id"
                .into(),
        );
    }

    if let Some(ci) = &img.cloud_init {
        st.steps.push(Step::CloudInit(ci.clone()));
    }

    let vf = VmFile {
        stages: vec![st],
        vcpus: img.vcpus,
        memory: img.memory.clone(),
        backend: img
            .hypervisor
            .as_deref()
            .map(|h| delonix_vm::valid_backend_name(h).map(str::to_string))
            .transpose()
            .map_err(|e| ctx(e.to_string()))?,
        labels: img
            .labels
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    };
    Ok(base(Route::Custom(Box::new(vf))))
}

fn plan_golden(name: &str, img: &Image, profile: &str, mut p: Plan) -> Result<Plan> {
    let ctx = |m: String| Error::Invalid(format!("vm.yaml, image '{name}': {m}"));
    // What the golden recipe cannot honour is refused BY NAME.
    let mut refused: Vec<&str> = Vec::new();
    let mut chk = |on: bool, n: &'static str| {
        if on {
            refused.push(n)
        }
    };
    chk(img.from.is_some(), "from");
    chk(img.size.is_some(), "size");
    chk(img.hostname.is_some(), "hostname");
    chk(img.vcpus.is_some(), "vcpus");
    chk(img.memory.is_some(), "memory");
    chk(img.hypervisor.is_some(), "hypervisor");
    chk(!img.labels.is_empty(), "labels");
    chk(img.packages.upgrade, "packages.upgrade");
    chk(
        !img.remove.packages.is_empty()
            || !img.remove.paths.is_empty()
            || !img.remove.users.is_empty()
            || !img.remove.services.is_empty(),
        "remove",
    );
    chk(
        !img.services.enable.is_empty() || !img.services.disable.is_empty(),
        "services",
    );
    chk(!img.users.is_empty(), "users");
    chk(!img.env.is_empty(), "env");
    chk(!img.files.is_empty(), "files");
    chk(img.cloud_init.is_some(), "cloud_init");
    chk(img.network, "network");
    if !refused.is_empty() {
        return Err(ctx(format!(
            "`profile: {profile}` is the built-in golden recipe and does not honour {} — \
             use `profile: custom` to describe the image yourself",
            refused.join(", ")
        )));
    }
    let distro = img.distro.clone().unwrap_or_else(|| "ubuntu".into());
    if !matches!(distro.as_str(), "ubuntu" | "debian" | "rocky" | "fedora") {
        return Err(ctx(format!("distro '{distro}' unknown")));
    }
    let mut g = Golden {
        distro: distro.clone(),
        ubuntu_release: "26.04".into(),
        debian_release: "bookworm".into(),
        rocky_release: "9".into(),
        fedora_release: "42-1.1".into(),
        k8s_version: None,
        offline: false,
        no_k8s: profile == "rootless",
        extra_packages: img.packages.install.clone(),
        extra_run: img.run.clone(),
        root_password: img.root_password.clone(),
        node_exporter: match &img.node_exporter {
            None | Some(NodeExporter::Enabled(false)) => None,
            Some(NodeExporter::Enabled(true)) => Some("0.0.0.0:9100".into()),
            Some(NodeExporter::Addr(a)) => Some(a.clone()),
        },
    };
    if let Some(r) = &img.release {
        match distro.as_str() {
            "ubuntu" => g.ubuntu_release = r.clone(),
            "debian" => g.debian_release = r.clone(),
            "rocky" => g.rocky_release = r.clone(),
            _ => g.fedora_release = r.clone(),
        }
    }
    if let Some(k) = &img.k8s {
        if profile == "rootless" {
            return Err(ctx("`k8s:` needs `profile: k8s`".into()));
        }
        g.k8s_version = k.version.clone();
        g.offline = k.offline;
    }
    p.route = Route::Golden(Box::new(g));
    Ok(p)
}

// ─── entry point used by `vm build` ─────────────────────────────────────────

/// Finds the `vm.yaml` a build should use: an explicit `-f` that names a YAML,
/// or — with no `-f` — one in the context. `None` means "not a vm.yaml build"
/// and leaves the `VMfile`/golden decision to the caller.
pub(crate) fn locate(file: Option<&Path>, context: &Path) -> Option<PathBuf> {
    match file {
        Some(f) => {
            let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("");
            matches!(ext, "yaml" | "yml").then(|| f.to_path_buf())
        }
        None => SPEC_FILES
            .iter()
            .map(|n| context.join(n))
            .find(|p| p.is_file()),
    }
}

/// Picks which images to build, in a stable order.
pub(crate) fn select<'a>(
    spec: &'a Spec,
    target: Option<&str>,
) -> Result<Vec<(&'a String, &'a Image)>> {
    match target {
        Some(t) => spec
            .images
            .get_key_value(t)
            .map(|kv| vec![kv])
            .ok_or_else(|| {
                let known: Vec<&str> = spec.images.keys().map(String::as_str).collect();
                Error::Invalid(format!(
                    "vm.yaml has no image '{t}' — it declares: {}",
                    known.join(", ")
                ))
            }),
        None => Ok(spec.images.iter().collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(y: &str) -> Result<Plan> {
        let spec = parse(y, None)?;
        let (n, i) = spec.images.iter().next().unwrap();
        plan(n, i, Path::new("."), false)
    }

    fn vmfile(y: &str) -> VmFile {
        match one(y).unwrap().route {
            Route::Custom(v) => *v,
            _ => panic!("expected a custom route"),
        }
    }

    fn runs(vf: &VmFile) -> Vec<String> {
        vf.stages[0]
            .steps
            .iter()
            .filter_map(|s| match s {
                Step::Run(c) => Some(c.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn expands_variables_and_defaults() {
        let mut v = BTreeMap::new();
        v.insert("TAG".into(), "26.04".into());
        assert_eq!(expand("a-${TAG}", &v).unwrap(), "a-26.04");
        assert_eq!(expand("${NOPE_XYZ:-d}", &v).unwrap(), "d");
        assert!(
            expand("${NOPE_XYZ}", &v).is_err(),
            "unset without default must fail"
        );
        assert!(expand("${", &v).is_err());
        assert!(expand("${a b}", &v).is_err());
    }

    #[test]
    fn a_variable_in_a_comment_is_not_evaluated() {
        let y = "# uses ${TAG_NOT_SET_ANYWHERE}\nimages:\n  a:\n    distro: ubuntu\n    release: '${R_NOT_SET:-26.04}'\n";
        let s = parse(y, None).unwrap();
        assert_eq!(s.images["a"].release.as_deref(), Some("26.04"));
        let s = parse(&y.replace("R_NOT_SET:-26.04", "TAG"), Some("24.04")).unwrap();
        assert_eq!(s.images["a"].release.as_deref(), Some("24.04"));
    }

    #[test]
    fn unknown_key_is_refused() {
        let e = parse("images:\n  a:\n    distro: ubuntu\n    hostnam: x\n", None).unwrap_err();
        assert!(e.to_string().contains("hostnam"), "{e}");
        assert!(parse("images: {}\n", None).is_err());
        assert!(parse("version: 2\nimages:\n  a: {}\n", None).is_err());
    }

    #[test]
    fn custom_compiles_to_from_distro_release() {
        let vf = vmfile("images:\n  u:\n    distro: ubuntu\n    release: '26.04'\n    size: 10G\n    hostname: h\n    vcpus: 2\n    memory: 2G\n");
        assert_eq!(vf.stages[0].from, "ubuntu:26.04");
        assert_eq!(vf.stages[0].size.as_deref(), Some("10G"));
        assert_eq!(vf.stages[0].hostname.as_deref(), Some("h"));
        assert_eq!(vf.vcpus, Some(2));
        assert_eq!(vf.memory.as_deref(), Some("2G"));
    }

    #[test]
    fn packages_need_network_and_a_known_manager() {
        let e = one("images:\n  u:\n    distro: ubuntu\n    release: '26.04'\n    packages: {install: [curl]}\n").err().unwrap();
        assert!(e.to_string().contains("network: true"), "{e}");
        let e = one("images:\n  u:\n    from: https://x/y.qcow2\n    network: true\n    packages: {install: [curl]}\n").err().unwrap();
        assert!(e.to_string().contains("package manager"), "{e}");
        let vf = vmfile("images:\n  u:\n    from: https://x/y.qcow2\n    package_manager: dnf\n    network: true\n    packages: {install: [curl]}\n");
        assert!(runs(&vf).iter().any(|c| c == "dnf install -y curl"));
    }

    #[test]
    fn a_file_goes_to_its_full_destination_through_a_staging_dir() {
        let vf = vmfile("images/u:\n".replace("images/u:\n","images:\n  u:\n    distro: ubuntu\n    release: '26.04'\n    files: [{src: artifacts/motd, dst: /etc/motd, mode: '0644'}]\n").as_str());
        let steps = &vf.stages[0].steps;
        let copy = steps
            .iter()
            .position(
                |s| matches!(s, Step::Copy { dst, .. } if dst.starts_with("/tmp/.delonix-files-")),
            )
            .expect("copies into a staging dir, not onto the final name");
        let mv = steps.iter().position(|s| matches!(s, Step::Run(c) if c.contains("mv -f --") && c.contains("'/etc/motd'"))).expect("moves to the final path");
        assert!(copy < mv);
        assert!(runs(&vf).iter().any(|c| c == "chmod 0644 '/etc/motd'"));
        let bad = "images:\n  u:\n    distro: ubuntu\n    release: '26.04'\n    files: [{src: a, dst: /etc}]\n";
        assert!(
            one(bad).is_err(),
            "a protected directory is not a file destination"
        );
    }

    /// The recipes shipped under `images/` are documentation people copy, so
    /// they must never rot: every one parses under the strict schema, plans
    /// with its defaults, and every file it points at exists.
    #[test]
    fn every_shipped_recipe_is_valid_and_complete() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../images");
        let mut seen = 0;
        for entry in std::fs::read_dir(&root).expect("images/ exists") {
            let dir = entry.unwrap().path();
            let file = dir.join("vm.yaml");
            if !file.is_file() {
                continue;
            }
            seen += 1;
            let text = std::fs::read_to_string(&file).unwrap();
            let spec = parse(&text, None).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            for (name, img) in &spec.images {
                let p = plan(name, img, &dir, false)
                    .unwrap_or_else(|e| panic!("{} / {name}: {e}", file.display()));
                assert!(
                    p.tag.is_some(),
                    "{}: image '{name}' has no tag",
                    file.display()
                );
                assert!(
                    dir.join("README.md").is_file(),
                    "{}: no README.md",
                    dir.display()
                );
                for f in &img.files {
                    assert!(
                        dir.join(&f.src).exists(),
                        "{}: files.src {} missing",
                        dir.display(),
                        f.src
                    );
                }
                if let Some(ci) = &img.cloud_init {
                    assert!(
                        dir.join(ci).is_file(),
                        "{}: cloud_init {ci} missing",
                        dir.display()
                    );
                }
            }
        }
        assert!(
            seen >= 4,
            "expected ubuntu, debian, rocky and fedora, found {seen}"
        );
    }

    #[test]
    fn the_network_flag_satisfies_packages() {
        let y = "images:\n  u:\n    distro: ubuntu\n    release: '26.04'\n    packages: {install: [curl]}\n";
        let spec = parse(y, None).unwrap();
        let (n, i) = spec.images.iter().next().unwrap();
        assert!(plan(n, i, Path::new("."), false).is_err());
        assert!(plan(n, i, Path::new("."), true).is_ok());
    }

    #[test]
    fn removal_runs_after_install_and_run() {
        let vf = vmfile("images:\n  u:\n    distro: debian\n    release: bookworm\n    network: true\n    packages: {install: [nginx]}\n    run: ['echo hi']\n    remove: {packages: [snapd], paths: [/usr/share/doc], services: [cups]}\n");
        let r = runs(&vf);
        let pos = |needle: &str| r.iter().position(|c| c.contains(needle)).unwrap();
        assert!(pos("install -y nginx") < pos("echo hi"));
        assert!(pos("echo hi") < pos("purge -y snapd"));
        assert!(pos("purge -y snapd") < pos("rm -rf -- '/usr/share/doc'"));
        assert!(r.iter().any(|c| c.contains("systemctl mask cups")));
    }

    #[test]
    fn protected_paths_and_traversal_are_refused() {
        for p in [
            "/",
            "/etc",
            "/etc/",
            "/usr",
            "relative",
            "/a/../etc",
            "/x'y",
        ] {
            let y = format!("images:\n  u:\n    distro: ubuntu\n    release: '26.04'\n    remove: {{paths: ['{p}']}}\n");
            assert!(one(&y).is_err(), "{p} must be refused");
        }
        assert!(one("images:\n  u:\n    distro: ubuntu\n    release: '26.04'\n    remove: {paths: [/usr/share/doc]}\n").is_ok());
    }

    #[test]
    fn shell_metacharacters_never_reach_a_command() {
        for bad in ["a;b", "$(x)", "a b", "-rf", ""] {
            let y = format!("images:\n  u:\n    distro: ubuntu\n    release: '26.04'\n    network: true\n    packages: {{install: ['{bad}']}}\n");
            assert!(one(&y).is_err(), "package '{bad}'");
        }
        let y = "images:\n  u:\n    distro: ubuntu\n    release: '26.04'\n    services: {enable: ['a;b']}\n";
        assert!(one(y).is_err());
    }

    #[test]
    fn cleanup_defaults_make_the_image_reusable() {
        let vf = vmfile("images:\n  u:\n    distro: ubuntu\n    release: '26.04'\n");
        let r = runs(&vf);
        assert!(r.iter().any(|c| c.contains("/etc/machine-id")));
        assert!(r.iter().any(|c| c.contains("apt-get clean")));
        let off = vmfile("images:\n  u:\n    distro: ubuntu\n    release: '26.04'\n    cleanup: {machine_id: false, package_cache: false}\n");
        let r = runs(&off);
        assert!(!r.iter().any(|c| c.contains("/etc/machine-id")));
        assert!(!r.iter().any(|c| c.contains("apt-get clean")));
    }

    #[test]
    fn golden_profile_maps_and_refuses_what_it_ignores() {
        let p = one("images:\n  g:\n    profile: rootless\n    distro: debian\n    release: trixie\n    node_exporter: true\n    packages: {install: [htop]}\n").unwrap();
        match p.route {
            Route::Golden(g) => {
                assert!(g.no_k8s);
                assert_eq!(g.debian_release, "trixie");
                assert_eq!(g.node_exporter.as_deref(), Some("0.0.0.0:9100"));
                assert_eq!(g.extra_packages, vec!["htop"]);
            }
            _ => panic!(),
        }
        let e = one("images:\n  g:\n    profile: k8s\n    hostname: x\n    users: [{name: a}]\n")
            .err()
            .unwrap();
        assert!(
            e.to_string().contains("hostname") && e.to_string().contains("users"),
            "{e}"
        );
        assert!(one("images:\n  g:\n    profile: rootless\n    k8s: {version: '1.36'}\n").is_err());
    }

    #[test]
    fn build_file_is_exclusive_with_content() {
        let p = one("images:\n  b:\n    build: {file: VMfile}\n    tag: t\n").unwrap();
        assert!(matches!(p.route, Route::File { .. }));
        assert!(
            one("images:\n  b:\n    build: {file: VMfile}\n    packages: {upgrade: true}\n")
                .is_err()
        );
    }

    #[test]
    fn a_base_is_required_and_unambiguous() {
        assert!(one("images:\n  a: {}\n").is_err());
        assert!(one("images:\n  a:\n    distro: ubuntu\n").is_err());
        assert!(
            one("images:\n  a:\n    from: x:1\n    distro: ubuntu\n    release: '1'\n").is_err()
        );
        assert!(one("images:\n  a:\n    distro: gentoo\n    release: '1'\n").is_err());
    }

    #[test]
    fn locate_prefers_yaml_extension_and_context_file() {
        assert!(locate(Some(Path::new("VMfile")), Path::new(".")).is_none());
        assert!(locate(Some(Path::new("x/ubuntu.yaml")), Path::new(".")).is_some());
        let d = std::env::temp_dir().join(format!("vmspec-locate-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert!(locate(None, &d).is_none());
        std::fs::write(d.join("vm.yaml"), "images: {}").unwrap();
        assert_eq!(locate(None, &d), Some(d.join("vm.yaml")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn select_target_names_the_known_images() {
        let s = parse("images:\n  a:\n    distro: ubuntu\n    release: '1'\n  b:\n    distro: ubuntu\n    release: '2'\n", None).unwrap();
        assert_eq!(select(&s, None).unwrap().len(), 2);
        assert_eq!(select(&s, Some("b")).unwrap().len(), 1);
        let e = select(&s, Some("z")).err().unwrap();
        assert!(e.to_string().contains("a, b"), "{e}");
    }
}
