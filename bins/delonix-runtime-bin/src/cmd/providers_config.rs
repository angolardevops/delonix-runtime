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
//!   - type: opnsense                     # ADR-0059 F1
//!     url: https://fw.example
//!     auth: { keyFile: /etc/delonix/opnsense.key, secretFile: /etc/delonix/opnsense.secret }
//!   - type: powerdns                     # ADR-0064 D6: the engine's OWN DNS credential
//!     url: https://dns.example/api/v1/servers/localhost
//!     controllers: [pdnslab]             # the cluster's DNS controllers this server is
//!     auth: { keyFile: /etc/delonix/powerdns.key }
//! networkDefaults:                       # which provider answers a network role
//!   segment: proxmox
//!   gateway: opnsense
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
    /// Which provider answers each network role when nothing names one
    /// (ADR-0059 D3). `defaultProvider` stays the compute default.
    #[serde(default)]
    pub network_defaults: Option<NetworkDefaults>,
}

/// One provider type per network role (ADR-0059 D3). Parsed and shown from
/// catalog 1.1.0 on; the network Kinds resolve through it from ADR-0059 F2.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NetworkDefaults {
    /// The provider of `kind: NetworkZone` (today: `proxmox`).
    #[serde(default)]
    pub segment: Option<String>,
    /// The perimeter gateway of `kind: NetworkGateway` (today: `opnsense`).
    #[serde(default)]
    pub gateway: Option<String>,
    /// No provider serves this role yet (ADR-0059 F5): a value is refused.
    #[serde(default)]
    pub nat: Option<String>,
    /// No provider serves this role yet (ADR-0059 F5): a value is refused.
    #[serde(default)]
    pub ipam: Option<String>,
    /// No provider serves this role yet (ADR-0059 F5): a value is refused.
    #[serde(default)]
    pub dns: Option<String>,
}

impl NetworkDefaults {
    /// Every role with the value the file gives it.
    pub fn roles(&self) -> [(&'static str, Option<&str>); 5] {
        [
            ("segment", self.segment.as_deref()),
            ("gateway", self.gateway.as_deref()),
            ("nat", self.nat.as_deref()),
            ("ipam", self.ipam.as_deref()),
            ("dns", self.dns.as_deref()),
        ]
    }
}

/// The provider types that serve a network role in this build. A role with
/// none is refused by name until its port exists (ADR-0059 D1).
pub fn role_providers(role: &str) -> &'static [&'static str] {
    match role {
        "segment" => &["proxmox"],
        "gateway" => &["opnsense"],
        _ => &[],
    }
}

/// One provider. Tagged by `type`, the same name `--backend` and a VM record use.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ProviderEntry {
    Libvirt(LocalEntry),
    CloudHypervisor(LocalEntry),
    Proxmox(Box<ProxmoxEntry>),
    Opnsense(Box<OpnsenseEntry>),
    Powerdns(Box<PowerdnsEntry>),
}

impl ProviderEntry {
    fn type_name(&self) -> &'static str {
        match self {
            ProviderEntry::Libvirt(_) => "libvirt",
            ProviderEntry::CloudHypervisor(_) => "cloud-hypervisor",
            ProviderEntry::Proxmox(_) => "proxmox",
            ProviderEntry::Opnsense(_) => "opnsense",
            ProviderEntry::Powerdns(_) => "powerdns",
        }
    }
    fn name(&self) -> Option<&str> {
        match self {
            ProviderEntry::Libvirt(e) | ProviderEntry::CloudHypervisor(e) => e.name.as_deref(),
            ProviderEntry::Proxmox(e) => e.name.as_deref(),
            ProviderEntry::Opnsense(e) => e.name.as_deref(),
            ProviderEntry::Powerdns(e) => e.name.as_deref(),
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
    #[serde(default)]
    pub storage: Storage,
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

/// An OPNsense appliance reached through its REST API (ADR-0051, ADR-0059 F1).
/// Only a generated API key/secret pair authenticates (ADR-0051 phase 0).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OpnsenseEntry {
    #[serde(default)]
    pub name: Option<String>,
    pub url: String,
    pub auth: OpnsenseAuth,
    #[serde(default)]
    pub tls: Tls,
}

/// The API key pair, by reference: the key (an identifier, like a Proxmox
/// `tokenId`) inline or in a file, the secret only in a 0600 file or a
/// `kind: Secret` with `key` and `secret` fields.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OpnsenseAuth {
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub key_file: Option<String>,
    #[serde(default)]
    pub secret_file: Option<String>,
    #[serde(default)]
    pub secret_ref: Option<String>,
    // Out of the schema: it exists only to be refused by name.
    #[serde(default)]
    #[schemars(skip)]
    secret: Option<serde_yaml::Value>,
}

/// A PowerDNS server the engine talks to with ITS OWN credential (ADR-0064
/// D6): to remove the gateway records a segment provider's node writes there
/// and never removes. The node's own controller credential is never read for
/// this (ADR-0064 D5), so the operator says here which of the cluster's DNS
/// controllers (`controllers`, by id) this server is.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PowerdnsEntry {
    #[serde(default)]
    pub name: Option<String>,
    /// `<scheme>://<host>:<port>/api/v1/servers/<id>` — the form a Proxmox
    /// `powerdns` controller takes.
    pub url: String,
    /// The DNS controller ids (on the segment provider) whose records live on
    /// this server. A zone's controller that no entry names is left as it is,
    /// and the teardown says so.
    pub controllers: Vec<String>,
    pub auth: PowerdnsAuth,
    /// `http://` sends the API key in the clear; PowerDNS's own webserver has
    /// no TLS. Refused unless this is `true`.
    #[serde(default)]
    pub allow_plain_http: bool,
    #[serde(default)]
    pub tls: Tls,
}

/// The API key, by reference only: a file only its owner reads.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PowerdnsAuth {
    #[serde(default)]
    pub key_file: Option<String>,
    // Out of the schema: it exists only to be refused by name.
    #[serde(default)]
    #[schemars(skip)]
    key: Option<serde_yaml::Value>,
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

/// Where a local image goes on the node (ADR-0057): the dir storage it is
/// uploaded to (`import`, default `local`) and the storage its disk is
/// imported onto (`disk`, default `local-lvm`).
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    #[serde(default)]
    pub import: Option<String>,
    #[serde(default)]
    pub disk: Option<String>,
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
        if let ProviderEntry::Powerdns(d) = p {
            if d.auth.key.is_some() {
                return Err(Error::Invalid(po::tf(
                    "{path}: a secret VALUE is never written in this file — use `keyFile` (a 0600 \
                     file) for the PowerDNS API key",
                    &[("path", &at)],
                )));
            }
            if d.controllers.is_empty() {
                return Err(Error::Invalid(po::tf(
                    "{path}: the powerdns entry names no `controllers` — list the cluster's DNS \
                     controller ids whose records live on this server",
                    &[("path", &at)],
                )));
            }
            if let Some(bad) = d
                .controllers
                .iter()
                .find(|c| !delonix_networking::dns::valid_controller_id(c))
            {
                return Err(Error::Invalid(po::tf(
                    "{path}: powerdns controllers: '{id}' is not a DNS controller id (a letter, \
                     then letters and digits, at least 2)",
                    &[("path", &at), ("id", bad)],
                )));
            }
        }
        if let ProviderEntry::Opnsense(o) = p {
            if o.auth.secret.is_some() {
                return Err(Error::Invalid(po::tf(
                    "{path}: a secret VALUE is never written in this file — use `secretFile` \
                     (a 0600 file) or `secretRef` (a `kind: Secret` with `key` and `secret`)",
                    &[("path", &at)],
                )));
            }
        }
    }
    if cfg.default_provider.as_deref() == Some("powerdns") {
        return Err(Error::Invalid(po::tf(
            "{path}: defaultProvider is the COMPUTE default and powerdns is not a compute \
             provider — a powerdns entry only gives the engine its own DNS credential",
            &[("path", &at)],
        )));
    }
    if cfg.default_provider.as_deref() == Some("opnsense") {
        return Err(Error::Invalid(po::tf(
            "{path}: defaultProvider is the COMPUTE default and opnsense is not a compute \
             provider — a perimeter gateway goes in `networkDefaults.gateway`",
            &[("path", &at)],
        )));
    }
    if let Some(nd) = &cfg.network_defaults {
        for (role, value) in nd.roles() {
            let Some(v) = value else { continue };
            let serving = role_providers(role);
            if serving.is_empty() {
                return Err(Error::Invalid(po::tf(
                    "{path}: networkDefaults.{role}: no provider serves the role '{role}' in this \
                     build (ADR-0059 F5)",
                    &[("path", &at), ("role", role)],
                )));
            }
            if !serving.contains(&v) {
                return Err(Error::Invalid(po::tf(
                    "{path}: networkDefaults.{role}: '{name}' does not serve the role '{role}' \
                     (it is served by: {serving})",
                    &[
                        ("path", &at),
                        ("role", role),
                        ("name", v),
                        ("serving", &serving.join(", ")),
                    ],
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

/// `networkDefaults.<role>` and the path of the `providers.yaml` it came
/// from, for the resolution of a network Kind's provider (ADR-0059 D3). No
/// file is `(None, None)`: the count rule stays on. A file that does not
/// parse is an error here as everywhere else.
pub fn network_default(
    role: delonix_networking::resolve::Role,
) -> Result<(Option<String>, Option<String>)> {
    use delonix_networking::resolve::Role;
    match loaded() {
        Ok(None) => Ok((None, None)),
        Ok(Some((path, cfg))) => {
            let nd = cfg.network_defaults.as_ref();
            let value = match role {
                Role::Segment => nd.and_then(|n| n.segment.clone()),
                Role::Gateway => nd.and_then(|n| n.gateway.clone()),
            };
            Ok((value, Some(path.display().to_string())))
        }
        Err(e) => Err(Error::Invalid(e.to_string())),
    }
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
        ("DELONIX_PROXMOX_IMPORT_STORAGE", &px.storage.import),
        ("DELONIX_PROXMOX_DISK_STORAGE", &px.storage.disk),
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

/// The OPNsense entry as the `DELONIX_OPNSENSE_*` keys the gateway
/// registration reads — one reader for the file and the environment.
pub fn opnsense_keys(o: &OpnsenseEntry) -> HashMap<&'static str, String> {
    let mut m = HashMap::new();
    m.insert("DELONIX_OPNSENSE_URL", o.url.clone());
    let a = &o.auth;
    for (k, v) in [
        ("DELONIX_OPNSENSE_KEY", &a.key),
        ("DELONIX_OPNSENSE_KEY_FILE", &a.key_file),
        ("DELONIX_OPNSENSE_SECRET_FILE", &a.secret_file),
        ("DELONIX_OPNSENSE_CREDENTIAL", &a.secret_ref),
        ("DELONIX_OPNSENSE_CA_FILE", &o.tls.ca_file),
    ] {
        if let Some(v) = v {
            m.insert(k, v.clone());
        }
    }
    if o.tls.insecure_skip_verify {
        m.insert("DELONIX_OPNSENSE_INSECURE_TLS", "1".into());
    }
    m
}

/// Where the OPNsense target comes from for this process (ADR-0054 D4, applied
/// to the gateway): the environment as a whole when it carries
/// `DELONIX_OPNSENSE_URL`, else the file's entry — never field by field.
pub fn opnsense_lookup_with<'a>(
    env: impl Fn(&str) -> Option<String> + 'a,
    file: Option<&ProviderConfig>,
) -> Lookup<'a> {
    if env("DELONIX_OPNSENSE_URL").is_some() {
        return Box::new(env);
    }
    let keys = file
        .and_then(|c| {
            c.providers.iter().find_map(|p| match p {
                ProviderEntry::Opnsense(o) => Some(opnsense_keys(o)),
                _ => None,
            })
        })
        .unwrap_or_default();
    Box::new(move |k| keys.get(k).cloned())
}

/// The PowerDNS entry of the loaded providers file, if it has one.
pub fn powerdns_entry() -> Result<Option<&'static PowerdnsEntry>> {
    match loaded() {
        Ok(None) => Ok(None),
        Ok(Some((_, cfg))) => Ok(cfg.providers.iter().find_map(|p| match p {
            ProviderEntry::Powerdns(d) => Some(&**d),
            _ => None,
        })),
        Err(e) => Err(Error::Invalid(e.to_string())),
    }
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
    if let Some(nd) = &cfg.network_defaults {
        for (role, value) in nd.roles() {
            if let Some(v) = value.filter(|v| !listed.contains(v)) {
                return Err(Error::Invalid(po::tf(
                    "{path}: networkDefaults.{role} '{name}' has no entry in this file, so no \
                     process can serve it",
                    &[
                        ("path", &origin.display().to_string()),
                        ("role", role),
                        ("name", v),
                    ],
                )));
            }
        }
    }
    for p in &cfg.providers {
        if let ProviderEntry::Powerdns(d) = p {
            super::dns_cleanup::target_of(d)?;
        }
        if let ProviderEntry::Opnsense(o) = p {
            let keys = opnsense_keys(o);
            let lookup = |k: &str| keys.get(k).cloned();
            super::gatewayproviders::opnsense_target_with(&lookup)?;
        }
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
    storage:
      import: images-dir
      disk: fast-lvm
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
        assert_eq!(k["DELONIX_PROXMOX_IMPORT_STORAGE"], "images-dir");
        assert_eq!(k["DELONIX_PROXMOX_DISK_STORAGE"], "fast-lvm");
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
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
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
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
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
    }

    #[test]
    fn set_default_in_refuses_to_write_a_file_this_build_would_refuse() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let f = d.join("providers.yaml");
        let bad = "apiVersion: config.delonix.io/v2\n";
        std::fs::write(&f, bad).unwrap();
        assert!(set_default_in(&f, Some("libvirt")).is_err());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), bad, "left untouched");
    }

    #[test]
    fn validate_refuses_a_default_without_an_entry_and_a_readable_token() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
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
    }

    const OPN: &str = "apiVersion: config.delonix.io/v1
providers:
  - type: proxmox
    url: https://pve.invalid:8006
    node: pve
    auth:
      tokenId: 'a@pve!t'
      tokenSecretFile: /etc/delonix/proxmox.token
  - type: opnsense
    url: https://fw.invalid
    auth:
      keyFile: /etc/delonix/opnsense.key
      secretFile: /etc/delonix/opnsense.secret
    tls:
      caFile: /etc/delonix/fw-ca.pem
networkDefaults:
  segment: proxmox
  gateway: opnsense
";

    #[test]
    fn an_opnsense_entry_and_network_defaults_parse_and_map_onto_the_env_keys() {
        let cfg = parse(OPN, &p()).unwrap();
        let nd = cfg.network_defaults.as_ref().unwrap();
        assert_eq!(nd.segment.as_deref(), Some("proxmox"));
        assert_eq!(nd.gateway.as_deref(), Some("opnsense"));
        let none = |_: &str| None;
        let l = opnsense_lookup_with(none, Some(&cfg));
        assert_eq!(
            l("DELONIX_OPNSENSE_URL").as_deref(),
            Some("https://fw.invalid")
        );
        assert_eq!(
            l("DELONIX_OPNSENSE_KEY_FILE").as_deref(),
            Some("/etc/delonix/opnsense.key")
        );
        assert_eq!(
            l("DELONIX_OPNSENSE_SECRET_FILE").as_deref(),
            Some("/etc/delonix/opnsense.secret")
        );
        assert_eq!(
            l("DELONIX_OPNSENSE_CA_FILE").as_deref(),
            Some("/etc/delonix/fw-ca.pem")
        );
        assert_eq!(l("DELONIX_OPNSENSE_INSECURE_TLS"), None);
    }

    #[test]
    fn the_environment_replaces_the_opnsense_entry_as_a_whole() {
        let cfg = parse(OPN, &p()).unwrap();
        let env =
            |k: &str| (k == "DELONIX_OPNSENSE_URL").then(|| "https://other.invalid".to_string());
        let l = opnsense_lookup_with(env, Some(&cfg));
        assert_eq!(
            l("DELONIX_OPNSENSE_URL").as_deref(),
            Some("https://other.invalid")
        );
        assert_eq!(l("DELONIX_OPNSENSE_KEY_FILE"), None, "never field by field");
    }

    #[test]
    fn an_inline_opnsense_secret_and_an_opnsense_compute_default_are_refused() {
        let inline = "apiVersion: config.delonix.io/v1\nproviders:\n  - type: opnsense\n    url: https://fw.invalid\n    auth:\n      key: k\n      secret: s\n";
        let e = parse(inline, &p()).unwrap_err().to_string();
        assert!(e.contains("secretFile"), "{e}");
        let compute = "apiVersion: config.delonix.io/v1\ndefaultProvider: opnsense\nproviders:\n  - type: opnsense\n    url: https://fw.invalid\n    auth:\n      key: k\n      secretFile: /x\n";
        let e = parse(compute, &p()).unwrap_err().to_string();
        assert!(e.contains("networkDefaults.gateway"), "{e}");
    }

    #[test]
    fn a_network_default_must_serve_its_role_and_a_role_without_a_port_is_refused() {
        let wrong = "apiVersion: config.delonix.io/v1\nnetworkDefaults:\n  gateway: proxmox\n";
        let e = parse(wrong, &p()).unwrap_err().to_string();
        assert!(
            e.contains("does not serve") && e.contains("opnsense"),
            "{e}"
        );
        let unserved = "apiVersion: config.delonix.io/v1\nnetworkDefaults:\n  nat: opnsense\n";
        let e = parse(unserved, &p()).unwrap_err().to_string();
        assert!(e.contains("F5"), "{e}");
        let unknown = "apiVersion: config.delonix.io/v1\nnetworkDefaults:\n  lb: x\n";
        assert!(
            parse(unknown, &p()).is_err(),
            "an unknown role is an unknown field"
        );
    }

    #[test]
    fn validate_refuses_a_network_default_without_an_entry_and_an_opnsense_secret_others_read() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let origin = d.join("providers.yaml");
        let unlisted = parse(
            "apiVersion: config.delonix.io/v1\nnetworkDefaults:\n  gateway: opnsense\n",
            &origin,
        )
        .unwrap();
        let e = validate(&unlisted, &origin).unwrap_err().to_string();
        assert!(e.contains("networkDefaults.gateway"), "{e}");

        let secret = d.join("secret");
        std::fs::write(&secret, "s").unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();
        let y = format!(
            "apiVersion: config.delonix.io/v1\nproviders:\n  - type: opnsense\n    url: https://fw.invalid\n    auth:\n      key: k\n      secretFile: {}\nnetworkDefaults:\n  gateway: opnsense\n",
            secret.display()
        );
        let cfg = parse(&y, &origin).unwrap();
        let e = validate(&cfg, &origin).unwrap_err().to_string();
        assert!(e.contains("chmod 600"), "{e}");
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        validate(&cfg, &origin).unwrap();
    }

    #[test]
    fn a_file_the_precedence_skips_is_listed_as_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let system = d.join("etc.yaml");
        let xdg = d.join("xdg");
        std::fs::create_dir_all(xdg.join("delonix")).unwrap();
        let user = xdg.join("delonix/providers.yaml");
        std::fs::write(&system, "x").unwrap();
        std::fs::write(&user, "x").unwrap();
        let xdg_s = xdg.display().to_string();
        let env = |k: &str| (k == "XDG_CONFIG_HOME").then(|| xdg_s.clone());
        assert_eq!(ignored_with(&env, &system, &user), vec![system.clone()]);
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

    /// ADR-0064 D6: a `powerdns` entry parses with its controllers; an inline
    /// key, an entry with no controller or a malformed one, and powerdns as the
    /// compute default are each refused by name.
    #[test]
    fn a_powerdns_entry_parses_and_its_mistakes_are_refused_by_name() {
        let ok = "apiVersion: config.delonix.io/v1\nproviders:\n  - type: powerdns\n    url: http://dns.invalid:8081/api/v1/servers/localhost\n    controllers: [pdnslab]\n    allowPlainHttp: true\n    auth:\n      keyFile: /etc/delonix/powerdns.key\n";
        let cfg = parse(ok, Path::new("p.yaml")).expect("parse");
        let ProviderEntry::Powerdns(d) = &cfg.providers[0] else {
            panic!("not a powerdns entry");
        };
        assert_eq!(d.controllers, vec!["pdnslab"]);
        assert!(d.allow_plain_http);
        let inline = ok.replace("keyFile: /etc/delonix/powerdns.key", "key: s3cr3t");
        let e = parse(&inline, Path::new("p.yaml")).unwrap_err().to_string();
        assert!(e.contains("keyFile") && !e.contains("s3cr3t"), "{e}");
        let none = ok.replace("controllers: [pdnslab]", "controllers: []");
        assert!(parse(&none, Path::new("p.yaml"))
            .unwrap_err()
            .to_string()
            .contains("names no `controllers`"));
        let bad = ok.replace("controllers: [pdnslab]", "controllers: [pdns-lab]");
        assert!(parse(&bad, Path::new("p.yaml"))
            .unwrap_err()
            .to_string()
            .contains("'pdns-lab'"));
        let compute = ok.replace("providers:", "defaultProvider: powerdns\nproviders:");
        assert!(parse(&compute, Path::new("p.yaml"))
            .unwrap_err()
            .to_string()
            .contains("not a compute provider"));
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
