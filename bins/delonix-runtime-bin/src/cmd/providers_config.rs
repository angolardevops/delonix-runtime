//! The node's providers file (ADR-0054): which providers this node has, how the
//! engine reaches each one, and which one serves a request that names none.
//!
//! ```yaml
//! apiVersion: config.delonix.io/v1
//! defaultProvider: libvirt
//! providers:
//!   - type: libvirt
//!   - type: proxmox
//!     url: https://pve.example:8006
//!     node: pve
//!     auth: { tokenId: delonix@pve!engine, tokenSecretFile: /etc/delonix/proxmox.token }
//! ```
//!
//! **Read here, in the composition root, and nowhere else.** The engine crates
//! never open a configuration file: this module hands the default provider to
//! `delonix_vm::set_configured_default_backend` and the Proxmox target to the
//! one parser both registrations use (`vmbackends::proxmox_target`).
//! The file is translated into the SAME keys the environment carries
//! (`DELONIX_PROXMOX_*`), so a target configured either way goes through one
//! reader and cannot be read two ways.
//!
//! **Located once, first file wins, never merged** (D2):
//! `DELONIX_PROVIDERS_CONFIG`, then `$XDG_CONFIG_HOME/delonix/providers.yaml`
//! (or `~/.config/…`), then `/etc/delonix/providers.yaml`.

use super::po;
use delonix_model::{Error, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// A key → value reader over one configuration source (the environment, or
/// the providers file translated into the same keys).
pub type Lookup<'a> = Box<dyn Fn(&str) -> Option<String> + 'a>;

/// The only version of the format this build reads.
pub const API_VERSION: &str = "config.delonix.io/v1";

/// The system-wide location, written by whoever provisions the node.
pub const SYSTEM_PATH: &str = "/etc/delonix/providers.yaml";

/// The parsed file.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProviderConfig {
    /// The format version; this build reads only `config.delonix.io/v1`.
    #[schemars(extend("const" = "config.delonix.io/v1"))]
    pub api_version: String,
    /// The provider a request that names none goes to.
    #[serde(default)]
    pub default_provider: Option<String>,
    /// One entry per provider type this node has.
    #[serde(default)]
    pub providers: Vec<ProviderEntry>,
}

/// One provider. Tagged by `type`, the same name `--backend` and a VM record use.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ProviderEntry {
    Libvirt(LocalEntry),
    CloudHypervisor(LocalEntry),
    Proxmox(Box<ProxmoxEntry>),
}

impl ProviderEntry {
    fn type_name(&self) -> &'static str {
        match self {
            ProviderEntry::Libvirt(_) => "libvirt",
            ProviderEntry::CloudHypervisor(_) => "cloud-hypervisor",
            ProviderEntry::Proxmox(_) => "proxmox",
        }
    }
    fn name(&self) -> Option<&str> {
        match self {
            ProviderEntry::Libvirt(e) | ProviderEntry::CloudHypervisor(e) => e.name.as_deref(),
            ProviderEntry::Proxmox(e) => e.name.as_deref(),
        }
    }
}

/// A local provider: listing it declares it; there is nothing to configure.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocalEntry {
    /// Reserved: defaults to the type; a different name is refused until a
    /// node can carry two providers of one type.
    #[serde(default)]
    pub name: Option<String>,
}

/// A Proxmox VE node reached through its API, as the vendor documents it.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProxmoxEntry {
    #[serde(default)]
    pub name: Option<String>,
    pub url: String,
    pub node: String,
    pub auth: ProxmoxAuth,
    #[serde(default)]
    pub tls: Tls,
    #[serde(default)]
    pub network: Network,
}

/// The credential, by reference only. The two value fields exist so they are
/// refused BY NAME, with the accepted forms, instead of as an unknown field.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProxmoxAuth {
    #[serde(default)]
    pub token_id: Option<String>,
    #[serde(default)]
    pub token_secret_file: Option<String>,
    #[serde(default)]
    pub secret_ref: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password_file: Option<String>,
    // Out of the schema: they exist only to be refused by name, and the
    // schema's `additionalProperties: false` already underlines them.
    #[serde(default)]
    #[schemars(skip)]
    token_secret: Option<serde_yaml::Value>,
    #[serde(default)]
    #[schemars(skip)]
    password: Option<serde_yaml::Value>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Tls {
    #[serde(default)]
    pub ca_file: Option<String>,
    #[serde(default)]
    pub insecure_skip_verify: bool,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Network {
    #[serde(default)]
    pub bridge: Option<String>,
    #[serde(default)]
    pub vlan: Option<u16>,
}

/// Parses and validates a file's content. `origin` names it in every error.
pub fn parse(content: &str, origin: &Path) -> Result<ProviderConfig> {
    let at = origin.display().to_string();
    let cfg: ProviderConfig =
        serde_yaml::from_str(content).map_err(|e| Error::Invalid(format!("{at}: {e}")))?;
    if cfg.api_version != API_VERSION {
        return Err(Error::Invalid(po::tf(
            "{path}: apiVersion '{got}' is not one this build reads (expected '{want}')",
            &[
                ("path", &at),
                ("got", &cfg.api_version),
                ("want", API_VERSION),
            ],
        )));
    }
    let mut seen: Vec<&str> = Vec::new();
    for p in &cfg.providers {
        let t = p.type_name();
        if seen.contains(&t) {
            return Err(Error::Invalid(po::tf(
                "{path}: provider type '{type}' is listed twice — one entry per type in this \
                 version (two targets of one type are a later phase of ADR-0054)",
                &[("path", &at), ("type", t)],
            )));
        }
        seen.push(t);
        if let Some(n) = p.name().filter(|n| *n != t) {
            return Err(Error::Invalid(po::tf(
                "{path}: `name: {name}` is reserved — it must equal the type ('{type}') until a \
                 node can carry two providers of one type",
                &[("path", &at), ("name", n), ("type", t)],
            )));
        }
        if let ProviderEntry::Proxmox(px) = p {
            if px.auth.token_secret.is_some() || px.auth.password.is_some() {
                return Err(Error::Invalid(po::tf(
                    "{path}: a secret VALUE is never written in this file — use `tokenSecretFile` \
                     (a 0600 file) or `secretRef` (a `kind: Secret`), or `passwordFile` for a \
                     password",
                    &[("path", &at)],
                )));
            }
        }
    }
    Ok(cfg)
}

/// The file this process reads, by D2's order. `env` and `system` are injected
/// so a test never writes the process environment or `/etc`.
pub fn locate_with(env: &dyn Fn(&str) -> Option<String>, system: &Path) -> Result<Option<PathBuf>> {
    if let Some(p) = env("DELONIX_PROVIDERS_CONFIG") {
        let p = PathBuf::from(p);
        // Named explicitly: a missing file is a mistake to report, not a
        // reason to fall back to another file the operator did not name.
        if !p.is_file() {
            return Err(Error::Invalid(po::tf(
                "DELONIX_PROVIDERS_CONFIG names '{path}', which is not a file",
                &[("path", &p.display().to_string())],
            )));
        }
        return Ok(Some(p));
    }
    let user_dir = env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".config")));
    if let Some(p) = user_dir.map(|d| d.join("delonix/providers.yaml")) {
        if p.is_file() {
            return Ok(Some(p));
        }
    }
    Ok(system.is_file().then(|| system.to_path_buf()))
}

/// The loaded configuration: `None` when no file exists. Read once per process.
pub fn loaded() -> &'static Result<Option<(PathBuf, ProviderConfig)>> {
    static LOADED: OnceLock<Result<Option<(PathBuf, ProviderConfig)>>> = OnceLock::new();
    LOADED.get_or_init(|| {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        load_with(&env, Path::new(SYSTEM_PATH))
    })
}

/// [`loaded`] without the cache, for a test.
pub fn load_with(
    env: &dyn Fn(&str) -> Option<String>,
    system: &Path,
) -> Result<Option<(PathBuf, ProviderConfig)>> {
    let Some(path) = locate_with(env, system)? else {
        return Ok(None);
    };
    let content = std::fs::read_to_string(&path)
        .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
    let cfg = parse(&content, &path)?;
    Ok(Some((path, cfg)))
}

/// The Proxmox entry as the `DELONIX_PROXMOX_*` keys the target parser reads.
pub fn proxmox_keys(px: &ProxmoxEntry) -> HashMap<&'static str, String> {
    let mut m = HashMap::new();
    m.insert("DELONIX_PROXMOX_URL", px.url.clone());
    m.insert("DELONIX_PROXMOX_NODE", px.node.clone());
    let a = &px.auth;
    for (k, v) in [
        ("DELONIX_PROXMOX_TOKEN_ID", &a.token_id),
        ("DELONIX_PROXMOX_TOKEN_FILE", &a.token_secret_file),
        ("DELONIX_PROXMOX_SECRET", &a.secret_ref),
        ("DELONIX_PROXMOX_USER", &a.username),
        ("DELONIX_PROXMOX_PASSWORD_FILE", &a.password_file),
        ("DELONIX_PROXMOX_CA_FILE", &px.tls.ca_file),
        ("DELONIX_PROXMOX_BRIDGE", &px.network.bridge),
    ] {
        if let Some(v) = v {
            m.insert(k, v.clone());
        }
    }
    if px.tls.insecure_skip_verify {
        m.insert("DELONIX_PROXMOX_INSECURE_TLS", "1".into());
    }
    if let Some(vlan) = px.network.vlan {
        m.insert("DELONIX_PROXMOX_VLAN", vlan.to_string());
    }
    m
}

/// Where the Proxmox target comes from for this process (D4): the environment
/// as a whole when it carries `DELONIX_PROXMOX_URL`, else the file's entry.
/// The route trace is a diagnostic knob and always comes from the environment.
pub fn proxmox_lookup_with<'a>(
    env: impl Fn(&str) -> Option<String> + 'a,
    file: Option<&ProviderConfig>,
) -> Lookup<'a> {
    if env("DELONIX_PROXMOX_URL").is_some() {
        return Box::new(env);
    }
    let keys = file
        .and_then(|c| {
            c.providers.iter().find_map(|p| match p {
                ProviderEntry::Proxmox(px) => Some(proxmox_keys(px)),
                _ => None,
            })
        })
        .unwrap_or_default();
    Box::new(move |k| {
        if k == delonix_proxmox::TRACE_ROUTES_ENV {
            return env(k);
        }
        keys.get(k).cloned()
    })
}

/// Hands the file's default provider to the engine (ADR-0054 D3). A file that
/// could not be read is handed over as an error, so a VM request that would
/// have used its default fails with the reason instead of guessing one.
pub fn install_default() {
    let value = match loaded() {
        Ok(Some((_, cfg))) => Ok(cfg.default_provider.clone()),
        Ok(None) => Ok(None),
        Err(e) => Err(po::tf(
            "the node's providers file could not be read, so there is no default VM provider \
             to use: {err}",
            &[("err", &e.to_string())],
        )),
    };
    if let Err(why) = &value {
        eprintln!("{}", po::tf("warning: {why}", &[("why", why)]));
    }
    delonix_vm::set_configured_default_backend(value);
}

/// The user-scope path (`$XDG_CONFIG_HOME` or `~/.config`), whether or not it exists.
pub fn user_path_with(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|d| d.join("delonix/providers.yaml"))
}

/// Existing files D2's order skips because `chosen` wins — listed so an
/// operator who edits the wrong one is told which file is actually read.
pub fn ignored_with(
    env: &dyn Fn(&str) -> Option<String>,
    system: &Path,
    chosen: &Path,
) -> Vec<PathBuf> {
    let mut all: Vec<PathBuf> = Vec::new();
    all.extend(env("DELONIX_PROVIDERS_CONFIG").map(PathBuf::from));
    all.extend(user_path_with(env));
    all.push(system.to_path_buf());
    all.into_iter()
        .filter(|p| p.is_file() && p != chosen)
        .collect()
}

fn process_env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

/// [`ignored_with`] for this process.
pub fn ignored(chosen: &Path) -> Vec<PathBuf> {
    ignored_with(&process_env, Path::new(SYSTEM_PATH), chosen)
}

/// The file a write goes to: the one this process reads, or — when none exists
/// yet — the system file for root and the user file for everyone else, the
/// same split `install.sh` makes.
pub fn write_target() -> Result<PathBuf> {
    if let Ok(Some((p, _))) = loaded() {
        return Ok(p.clone());
    }
    if let Some(p) = process_env("DELONIX_PROVIDERS_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    if delonix_node::is_rootless() {
        user_path_with(&process_env).ok_or_else(|| {
            Error::Invalid(
                po::t("neither XDG_CONFIG_HOME nor HOME is set: no user providers file to write")
                    .into(),
            )
        })
    } else {
        Ok(PathBuf::from(SYSTEM_PATH))
    }
}

/// Everything [`parse`] checks, plus what the file POINTS AT — the token file
/// (and that only its owner reads it), the CA, the secret, the URL and node
/// syntax — through the same reader registration uses. Nothing is contacted.
pub fn validate(cfg: &ProviderConfig, origin: &Path) -> Result<()> {
    let listed: Vec<&str> = cfg.providers.iter().map(|p| p.type_name()).collect();
    if let Some(d) = &cfg.default_provider {
        let local = matches!(d.as_str(), "libvirt" | "cloud-hypervisor");
        if !local && !listed.contains(&d.as_str()) {
            return Err(Error::Invalid(po::tf(
                "{path}: defaultProvider '{name}' has no entry in this file, so no process can \
                 serve it",
                &[("path", &origin.display().to_string()), ("name", d)],
            )));
        }
    }
    for p in &cfg.providers {
        if let ProviderEntry::Proxmox(px) = p {
            let keys = proxmox_keys(px);
            let lookup = |k: &str| keys.get(k).cloned();
            let (target, opts) = super::vmbackends::proxmox_target_with(&lookup)?
                .ok_or_else(|| Error::Invalid(format!("{}: proxmox entry", origin.display())))?;
            delonix_proxmox::registration(target, opts)?;
        }
    }
    Ok(())
}

/// The content `install.sh` writes, for a file that does not exist yet.
fn fresh_content(provider: &str) -> String {
    format!(
        "# The node's VM providers (ADR-0054).\napiVersion: {API_VERSION}\ndefaultProvider: \
         {provider}\nproviders:\n  - type: {provider}\n"
    )
}

/// `content` with its top-level `defaultProvider:` set to `value` (or removed
/// for `None`). Every other line — comments included — is kept as written:
/// the operator's file is edited, never regenerated.
pub fn with_default(content: &str, value: Option<&str>) -> String {
    let mut lines: Vec<String> = content
        .lines()
        .filter(|l| !l.starts_with("defaultProvider:"))
        .map(str::to_string)
        .collect();
    if let Some(v) = value {
        let at = lines
            .iter()
            .position(|l| l.starts_with("apiVersion:"))
            .map(|i| i + 1)
            .unwrap_or(0);
        lines.insert(at, format!("defaultProvider: {v}"));
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Sets (or, with `None`, removes) the default provider in `path`. The result
/// is parsed before it is written, so a write never leaves a file this build
/// would refuse; the write is atomic and the file keeps its mode (0644 when new).
pub fn set_default_in(path: &Path, value: Option<&str>) -> Result<()> {
    let (content, mode) = match std::fs::read_to_string(path) {
        Ok(c) => {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)?.permissions().mode() & 0o7777;
            (with_default(&c, value), mode)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match value {
            None => return Ok(()),
            Some(v) => (fresh_content(v), 0o644),
        },
        Err(e) => return Err(e.into()),
    };
    parse(&content, path)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::Invalid(format!("{}: {e}", dir.display())))?;
    }
    delonix_state::write_atomic_mode(path, content.as_bytes(), Some(mode)).map_err(|e| {
        Error::Invalid(po::tf(
            "could not write {path}: {err} — a system file needs root (sudo), or point \
             DELONIX_PROVIDERS_CONFIG at a file you can write",
            &[
                ("path", &path.display().to_string()),
                ("err", &e.to_string()),
            ],
        ))
    })?;
    Ok(())
}

/// The JSON Schema of the file, generated from the types [`parse`] reads —
/// published as `docs/schema/v1/providers.json`, and a test keeps the two equal.
pub fn schema() -> serde_json::Value {
    let generator = schemars::generate::SchemaSettings::draft2020_12().into_generator();
    let root = generator.into_root_schema_for::<ProviderConfig>();
    let mut v = serde_json::to_value(root).unwrap_or(serde_json::Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert(
            "$id".into(),
            "https://angolardevops.github.io/delonix-runtime/schema/v1/providers.json".into(),
        );
        o.insert(
            "title".into(),
            "Delonix providers file (config.delonix.io/v1)".into(),
        );
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> PathBuf {
        PathBuf::from("/etc/delonix/providers.yaml")
    }

    const FULL: &str = "apiVersion: config.delonix.io/v1
defaultProvider: proxmox
providers:
  - type: libvirt
  - type: proxmox
    url: https://pve.invalid:8006
    node: pve
    auth:
      tokenId: 'delonix@pve!engine'
      tokenSecretFile: /etc/delonix/proxmox.token
    tls:
      caFile: /etc/delonix/pve-ca.pem
    network:
      bridge: vmbr1
      vlan: 20
";

    #[test]
    fn a_full_file_parses_and_maps_onto_the_env_keys() {
        let cfg = parse(FULL, &p()).unwrap();
        assert_eq!(cfg.default_provider.as_deref(), Some("proxmox"));
        let px = cfg
            .providers
            .iter()
            .find_map(|e| match e {
                ProviderEntry::Proxmox(px) => Some(px),
                _ => None,
            })
            .unwrap();
        let k = proxmox_keys(px);
        assert_eq!(k["DELONIX_PROXMOX_URL"], "https://pve.invalid:8006");
        assert_eq!(k["DELONIX_PROXMOX_NODE"], "pve");
        assert_eq!(k["DELONIX_PROXMOX_TOKEN_ID"], "delonix@pve!engine");
        assert_eq!(
            k["DELONIX_PROXMOX_TOKEN_FILE"],
            "/etc/delonix/proxmox.token"
        );
        assert_eq!(k["DELONIX_PROXMOX_CA_FILE"], "/etc/delonix/pve-ca.pem");
        assert_eq!(k["DELONIX_PROXMOX_BRIDGE"], "vmbr1");
        assert_eq!(k["DELONIX_PROXMOX_VLAN"], "20");
        assert!(!k.contains_key("DELONIX_PROXMOX_INSECURE_TLS"));
    }

    #[test]
    fn an_inline_secret_is_refused_by_name() {
        for field in ["tokenSecret: abc", "password: abc"] {
            let y = format!(
                "apiVersion: config.delonix.io/v1\nproviders:\n  - type: proxmox\n    url: https://x:8006\n    node: pve\n    auth:\n      tokenId: 'a@pve!t'\n      {field}\n"
            );
            let e = parse(&y, &p()).unwrap_err().to_string();
            assert!(e.contains("tokenSecretFile"), "{e}");
        }
    }

    #[test]
    fn two_entries_of_one_type_and_a_renamed_entry_are_refused() {
        let twice =
            "apiVersion: config.delonix.io/v1\nproviders:\n  - type: libvirt\n  - type: libvirt\n";
        assert!(parse(twice, &p())
            .unwrap_err()
            .to_string()
            .contains("twice"));
        let renamed =
            "apiVersion: config.delonix.io/v1\nproviders:\n  - type: libvirt\n    name: other\n";
        assert!(parse(renamed, &p())
            .unwrap_err()
            .to_string()
            .contains("reserved"));
    }

    #[test]
    fn an_unknown_field_and_a_wrong_version_are_refused() {
        let unknown = "apiVersion: config.delonix.io/v1\nproviders:\n  - type: libvirt\n    uri: qemu:///system\n";
        assert!(parse(unknown, &p()).is_err());
        let version = "apiVersion: config.delonix.io/v2\n";
        assert!(parse(version, &p()).unwrap_err().to_string().contains("v2"));
        let kind = "apiVersion: config.delonix.io/v1\nproviders:\n  - type: openstack\n";
        assert!(parse(kind, &p()).is_err());
    }

    #[test]
    fn the_first_file_found_wins_and_an_explicit_path_must_exist() {
        let base = std::env::temp_dir().join(format!(
            "delonix-providers-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let system = base.join("etc.yaml");
        let xdg = base.join("xdg");
        std::fs::create_dir_all(xdg.join("delonix")).unwrap();
        let user = xdg.join("delonix/providers.yaml");
        let explicit = base.join("explicit.yaml");

        let none = |_: &str| None;
        assert_eq!(locate_with(&none, &system).unwrap(), None);

        std::fs::write(&system, "x").unwrap();
        assert_eq!(locate_with(&none, &system).unwrap(), Some(system.clone()));

        std::fs::write(&user, "x").unwrap();
        let xdg_s = xdg.display().to_string();
        let with_xdg = |k: &str| (k == "XDG_CONFIG_HOME").then(|| xdg_s.clone());
        assert_eq!(locate_with(&with_xdg, &system).unwrap(), Some(user.clone()));

        let ex_s = explicit.display().to_string();
        let with_explicit = |k: &str| match k {
            "DELONIX_PROVIDERS_CONFIG" => Some(ex_s.clone()),
            "XDG_CONFIG_HOME" => Some(xdg_s.clone()),
            _ => None,
        };
        assert!(locate_with(&with_explicit, &system).is_err());
        std::fs::write(&explicit, "x").unwrap();
        assert_eq!(
            locate_with(&with_explicit, &system).unwrap(),
            Some(explicit)
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_environment_replaces_the_file_entry_as_a_whole() {
        let cfg = parse(FULL, &p()).unwrap();
        let none = |_: &str| None;
        let from_file = proxmox_lookup_with(none, Some(&cfg));
        assert_eq!(from_file("DELONIX_PROXMOX_NODE").as_deref(), Some("pve"));

        // The env carries a URL: nothing of the file leaks in, not even a key
        // the env left out.
        let env = |k: &str| (k == "DELONIX_PROXMOX_URL").then(|| "https://other:8006".to_string());
        let from_env = proxmox_lookup_with(env, Some(&cfg));
        assert_eq!(
            from_env("DELONIX_PROXMOX_URL").as_deref(),
            Some("https://other:8006")
        );
        assert_eq!(from_env("DELONIX_PROXMOX_NODE"), None);
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "delonix-pc-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn setting_the_default_keeps_every_other_line_including_comments() {
        let before = "# written by hand\napiVersion: config.delonix.io/v1\ndefaultProvider: libvirt\n# a proxmox node\nproviders:\n  - type: libvirt\n";
        let after = with_default(before, Some("cloud-hypervisor"));
        assert_eq!(
            after,
            "# written by hand\napiVersion: config.delonix.io/v1\ndefaultProvider: cloud-hypervisor\n# a proxmox node\nproviders:\n  - type: libvirt\n"
        );
        let cleared = with_default(before, None);
        assert!(!cleared.contains("defaultProvider"));
        assert!(cleared.contains("# a proxmox node"));
        // A file without the line gets it right after apiVersion.
        let added = with_default("apiVersion: config.delonix.io/v1\n", Some("libvirt"));
        assert_eq!(
            added,
            "apiVersion: config.delonix.io/v1\ndefaultProvider: libvirt\n"
        );
    }

    #[test]
    fn set_default_in_creates_edits_clears_and_keeps_the_mode() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("set");
        let f = d.join("sub/providers.yaml");
        set_default_in(&f, None).unwrap();
        assert!(!f.exists(), "clearing a missing file must not create one");
        set_default_in(&f, Some("libvirt")).unwrap();
        let first = std::fs::read_to_string(&f).unwrap();
        assert!(first.contains("defaultProvider: libvirt"));
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o644
        );
        parse(&first, &f).unwrap();

        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o640)).unwrap();
        set_default_in(&f, Some("cloud-hypervisor")).unwrap();
        assert!(std::fs::read_to_string(&f)
            .unwrap()
            .contains("defaultProvider: cloud-hypervisor"));
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o640
        );

        set_default_in(&f, None).unwrap();
        assert!(!std::fs::read_to_string(&f)
            .unwrap()
            .contains("defaultProvider"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn set_default_in_refuses_to_write_a_file_this_build_would_refuse() {
        let d = scratch("bad");
        let f = d.join("providers.yaml");
        let bad = "apiVersion: config.delonix.io/v2\n";
        std::fs::write(&f, bad).unwrap();
        assert!(set_default_in(&f, Some("libvirt")).is_err());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), bad, "left untouched");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn validate_refuses_a_default_without_an_entry_and_a_readable_token() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("val");
        let origin = d.join("providers.yaml");
        let nopx = parse(
            "apiVersion: config.delonix.io/v1\ndefaultProvider: proxmox\nproviders:\n  - type: libvirt\n",
            &origin,
        )
        .unwrap();
        let e = validate(&nopx, &origin).unwrap_err().to_string();
        assert!(e.contains("proxmox"), "{e}");

        let local = parse(
            "apiVersion: config.delonix.io/v1\ndefaultProvider: libvirt\n",
            &origin,
        )
        .unwrap();
        validate(&local, &origin).unwrap();

        let token = d.join("token");
        std::fs::write(&token, "x").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
        let px = format!(
            "apiVersion: config.delonix.io/v1\ndefaultProvider: proxmox\nproviders:\n  - type: proxmox\n    url: https://pve.invalid:8006\n    node: pve\n    auth:\n      tokenId: 'a@pve!t'\n      tokenSecretFile: {}\n",
            token.display()
        );
        let cfg = parse(&px, &origin).unwrap();
        let e = validate(&cfg, &origin).unwrap_err().to_string();
        assert!(e.contains("chmod 600"), "{e}");
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        validate(&cfg, &origin).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_the_precedence_skips_is_listed_as_ignored() {
        let d = scratch("ign");
        let system = d.join("etc.yaml");
        let xdg = d.join("xdg");
        std::fs::create_dir_all(xdg.join("delonix")).unwrap();
        let user = xdg.join("delonix/providers.yaml");
        std::fs::write(&system, "x").unwrap();
        std::fs::write(&user, "x").unwrap();
        let xdg_s = xdg.display().to_string();
        let env = |k: &str| (k == "XDG_CONFIG_HOME").then(|| xdg_s.clone());
        assert_eq!(ignored_with(&env, &system, &user), vec![system.clone()]);
        let _ = std::fs::remove_dir_all(&d);
    }
    /// `docs/schema/v1/providers.json` is what an editor fetches, so it has to
    /// BE the generated one. Regenerate with:
    ///
    /// ```text
    /// delonix provider config schema > docs/schema/v1/providers.json
    /// ```
    #[test]
    fn the_published_providers_schema_is_the_generated_one() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/schema/v1/providers.json"
        );
        let published: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(path).expect("docs/schema/v1/providers.json is missing"),
        )
        .expect("the published providers schema is not valid JSON");
        assert_eq!(
            published,
            schema(),
            "the published providers schema is stale — regenerate it with \
             `delonix provider config schema > docs/schema/v1/providers.json`"
        );
    }

    /// The schema is as strict as the parser: an unknown key, a wrong
    /// `apiVersion` and an inline secret are all outside it.
    #[test]
    fn the_providers_schema_is_as_strict_as_the_parser() {
        let s = schema();
        assert_eq!(s["additionalProperties"], serde_json::Value::Bool(false));
        assert_eq!(s["properties"]["apiVersion"]["const"], API_VERSION);
        let text = s.to_string();
        assert!(
            !text.contains("tokenSecret\""),
            "an inline secret must not be offered"
        );
        assert!(text.contains("tokenSecretFile"));
        assert!(text.contains("\"proxmox\""));
    }
}
