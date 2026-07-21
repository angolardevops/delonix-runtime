//! `kind: HTTPRoute` — declarative L7/HTTP reverse-proxy (routing by Host/path to
//! backend containers). `kind: Ingress` (k8s-shaped) ALSO compiles here (see
//! `ingress_to_httproute`); the L4 firewall lives under `kind: FirewallPolicy`
//! (see `cmd/firewall.rs`).
//!
//! **Architecture** (see `cmd/ingress_proxy.rs` / `realize` in Phase 4): the schema
//! here is the declarative surface; the physical plane is an embedded `hyper`
//! reverse-proxy, launched INSIDE the holder netns (where it reaches the backends by
//! IP), with the entry ports published on the host via the already existing
//! `publish` mechanism. TLS terminates at the proxy (BYO via `kind: Secret` or
//! self-signed).
//!
//! **"Ensure present" semantics** like all Kinds: no reconciler. This Phase (F1)
//! delivers only the **parsing + validation** (schema, `valid_*`, reference graph);
//! the proxy and the lifecycle come in the following phases.

use clap::Subcommand;
use serde::{Deserialize, Serialize};

use super::manifest::{self, ManifestDoc};
use delonix_runtime_core::{Error, Result};

/// `delonix httproute` — inspect/tear down the L7 reverse-proxy of the HTTPRoutes.
/// (`apply` is done via `stack apply`/`<kind> apply` — this group is operational.)
#[derive(Subcommand)]
pub enum HttpRouteCmd {
    /// State of the proxy + active routes (from the config in effect).
    Ls,
    /// Apply the HTTPRoutes of a manifest (brings up/reloads the proxy).
    Apply {
        /// Manifest file (default `./delonix-manifest.yaml`).
        #[arg(short, long)]
        file: Option<std::path::PathBuf>,
    },
    /// Stop the proxy and unpublish the ports (teardown).
    Rm,
}

pub fn run(action: HttpRouteCmd) -> Result<()> {
    match action {
        HttpRouteCmd::Ls => {
            if !ingress_proxy::is_running() {
                println!("httproute: proxy parado (nenhum HTTPRoute activo)");
                return Ok(());
            }
            let cfg = std::fs::read(ingress_proxy::config_path())
                .ok()
                .and_then(|b| serde_json::from_slice::<ProxyConfig>(&b).ok());
            match cfg {
                Some(c) => {
                    println!(
                        "httproute: proxy A SERVIR — {} listener(s), {} rota(s)",
                        c.listeners.len(),
                        c.routes.len()
                    );
                    for l in &c.listeners {
                        println!(
                            "  listener :{} {}",
                            l.port,
                            if l.tls { "(TLS)" } else { "" }
                        );
                    }
                    for r in &c.routes {
                        println!(
                            "  {} {} → {}",
                            if r.host.is_empty() { "*" } else { &r.host },
                            r.path,
                            r.backend
                        );
                    }
                }
                None => println!(
                    "httproute: {}",
                    super::po::t("proxy running but its config would not parse")
                ),
            }
            Ok(())
        }
        HttpRouteCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
        HttpRouteCmd::Rm => {
            // Remove only the MANUAL routes; the auto-registered ones (`--expose`)
            // survive and the proxy only stops if nothing else remains.
            ingress_proxy::clear_manual()?;
            if ingress_proxy::is_running() {
                println!("httproute: rotas manuais removidas — proxy mantém-se (há rotas auto-registadas)");
            } else {
                println!("httproute: proxy parado e portas despublicadas");
            }
            Ok(())
        }
    }
}

/// `spec` for `kind: HTTPRoute`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpRouteSpec {
    /// Entry points (ports where the proxy listens). Default: `[{port: 80}]`,
    /// plus an implicit `{port: 443, tls: true}` if `spec.tls` is defined.
    #[serde(default)]
    pub entrypoints: Vec<Entrypoint>,
    /// TLS configuration (optional). Without it, the proxy only serves HTTP.
    #[serde(default)]
    pub tls: Option<TlsSpec>,
    /// Routing rules (by Host and/or path prefix). Required and non-empty.
    #[serde(default)]
    pub rules: Vec<RouteRule>,
}

/// An entry point (proxy listen port).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Entrypoint {
    pub port: u16,
    /// `true` = terminate TLS on this port (requires `spec.tls`). Default `false`.
    #[serde(default)]
    pub tls: bool,
}

/// Proxy TLS configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TlsSpec {
    /// `selfSigned` (default) or `secretRef`.
    #[serde(default)]
    pub mode: Option<String>,
    /// Name of a `kind: Secret` with the `tls.crt`/`tls.key` keys (PEM). Used
    /// when `mode: secretRef`.
    #[serde(default, rename = "secretRef")]
    pub secret_ref: Option<String>,
}

/// A routing rule: matches by `host` (optional — empty = any Host) and
/// dispatches by path prefix to a backend.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RouteRule {
    /// Host name to match (e.g.: `loja.exemplo.ao`). Empty/omitted = any Host.
    #[serde(default)]
    pub host: Option<String>,
    /// Sub-rules by path prefix. Required and non-empty.
    #[serde(default)]
    pub paths: Vec<PathRule>,
}

/// A path prefix and the backend it forwards to.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PathRule {
    /// Path prefix (e.g.: `/`, `/api`). Default `/`.
    #[serde(default = "default_path")]
    pub path: String,
    pub backend: Backend,
}

fn default_path() -> String {
    "/".to_string()
}

/// The destination of a path: a container (by name) and the port it listens on.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Backend {
    /// Name of the backend container (resolved to the record's IP at apply time).
    pub service: String,
    /// Container port where the service listens.
    pub port: u16,
}

/// Known fields of the `spec` (drift-guard — see `manifest::warn_unknown_fields`).
pub const HTTP_ROUTE_SPEC_FIELDS: &[&str] = &["entrypoints", "tls", "rules"];

/// A valid DNS host name to match against the `Host:` header. Strict on purpose
/// (the audit's `valid_*` discipline): letters/digits/`.`/`-`, no scheme, no
/// `/`, no port, no spaces — never reaches a `format!`/command with garbage.
pub fn valid_host(h: &str) -> bool {
    if h.is_empty() || h.len() > 253 {
        return false;
    }
    // Each label (separated by `.`) must be non-empty and not start/end with
    // `-` — otherwise `loja..exemplo`/`loja.`/`-loja` would pass and the rule
    // would be dead (a real `Host:` never has that form). Alphabet: alnum + `-`.
    h.split('.').all(|label| {
        !label.is_empty()
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// A valid path prefix: starts with `/`, no spaces, no `..`, printable ASCII.
/// Prefix match (not regex) — it needs no metacharacters.
pub fn valid_path_prefix(p: &str) -> bool {
    p.starts_with('/')
        && p.len() <= 2048
        && !p.contains("..")
        && !p.contains(char::is_whitespace)
        && p.chars().all(|c| c.is_ascii_graphic())
}

/// A valid backend service/container name — the same alphabet the rest of the
/// runtime uses for resource names (the container must exist; resolved at apply
/// time). No `/`/spaces/`:` so it never pollutes a control line.
pub fn valid_service(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Validates an already deserialized `HttpRouteSpec` (called at apply time and
/// reusable in `stack validate`). Touches nothing — only schema/`valid_*`.
pub fn validate_spec(name: &str, spec: &HttpRouteSpec) -> Result<()> {
    let err = |m: String| Error::Invalid(format!("HTTPRoute '{name}': {m}"));

    if spec.rules.is_empty() {
        return Err(err(
            "spec.rules não pode ser vazio (nada para rotear)".into()
        ));
    }
    // TLS: if any entrypoint asks for tls, or mode: secretRef, spec.tls has to
    // make sense.
    if let Some(tls) = &spec.tls {
        let mode = tls.mode.as_deref().unwrap_or("selfSigned");
        if !matches!(mode, "selfSigned" | "secretRef") {
            return Err(err(format!(
                "tls.mode inválido '{mode}' (usa selfSigned|secretRef)"
            )));
        }
        if mode == "secretRef" && tls.secret_ref.as_deref().unwrap_or("").is_empty() {
            return Err(err(
                "tls.mode: secretRef exige tls.secretRef (nome do Secret com tls.crt/tls.key)"
                    .into(),
            ));
        }
    }
    // An entrypoint with tls: true requires spec.tls defined (otherwise no cert).
    for ep in &spec.entrypoints {
        if ep.port == 0 {
            return Err(err("entrypoint com port: 0 inválido".into()));
        }
        if ep.tls && spec.tls.is_none() {
            return Err(err(format!(
                "entrypoint :{} pede tls mas spec.tls não está definido",
                ep.port
            )));
        }
    }
    // Exactly equal (host, path) pairs make one of the routes silently dead
    // (the F2 matcher picks one) — catch the conflict already at validation.
    let mut seen_routes: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    // Rules: each host (if present) and each path/backend valid.
    for (i, rule) in spec.rules.iter().enumerate() {
        if let Some(host) = &rule.host {
            if !valid_host(host) {
                return Err(err(format!("rules[{i}].host inválido '{host}'")));
            }
        }
        if rule.paths.is_empty() {
            return Err(err(format!(
                "rules[{i}] sem paths (nada para rotear neste host)"
            )));
        }
        for (j, pr) in rule.paths.iter().enumerate() {
            if !valid_path_prefix(&pr.path) {
                return Err(err(format!(
                    "rules[{i}].paths[{j}].path inválido '{}'",
                    pr.path
                )));
            }
            if !valid_service(&pr.backend.service) {
                return Err(err(format!(
                    "rules[{i}].paths[{j}].backend.service inválido '{}'",
                    pr.backend.service
                )));
            }
            if pr.backend.port == 0 {
                return Err(err(format!(
                    "rules[{i}].paths[{j}].backend.port: 0 inválido"
                )));
            }
            let route_key = (rule.host.clone().unwrap_or_default(), pr.path.clone());
            if !seen_routes.insert(route_key) {
                return Err(err(format!(
                    "rota duplicada host='{}' path='{}' — uma delas ficaria morta",
                    rule.host.as_deref().unwrap_or("*"),
                    pr.path
                )));
            }
        }
    }
    Ok(())
}

/// Deserializes + validates each `kind: HTTPRoute` of the manifest (without
/// applying anything yet — the proxy and the lifecycle come in Phase 4). Warns per
/// unknown field.
pub fn parse_and_validate(docs: &[ManifestDoc]) -> Result<Vec<(String, HttpRouteSpec)>> {
    let mut out = Vec::new();
    for doc in manifest::of_kind(docs, "HTTPRoute") {
        manifest::warn_unknown_fields(doc, HTTP_ROUTE_SPEC_FIELDS);
        let spec: HttpRouteSpec = manifest::spec_of(doc)?;
        validate_spec(&doc.metadata.name, &spec)?;
        out.push((doc.metadata.name.clone(), spec));
    }
    // `kind: Ingress` (k8s-shaped) compiles to the SAME L7 proxy — the k8s
    // Ingress IS the reverse-proxy, so it shares HTTPRoute's whole pipeline.
    for doc in manifest::of_kind(docs, "Ingress") {
        manifest::warn_unknown_fields(doc, INGRESS_SPEC_FIELDS);
        let spec = ingress_spec_of(doc)?;
        validate_spec(&doc.metadata.name, &spec)?;
        out.push((doc.metadata.name.clone(), spec));
    }
    Ok(out)
}

/// Parses a `kind: Ingress` doc and converts it to the internal `HttpRouteSpec`
/// — so the stack graph validation reuses the SAME backend/secret checks.
pub fn ingress_spec_of(doc: &ManifestDoc) -> Result<HttpRouteSpec> {
    let ing: IngressSpec = manifest::spec_of(doc)?;
    ingress_to_httproute(&doc.metadata.name, ing)
}

/// Dry-run: the `kind: HTTPRoute` spec with defaults materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: HttpRouteSpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

/// Dry-run: the `kind: Ingress` spec with defaults materialized — in its OWN k8s
/// shape (not the converted HttpRouteSpec), since that is what the user wrote.
pub fn ingress_spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: IngressSpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

// ============================================================================
// `kind: Ingress` — Kubernetes-shaped L7 HTTP Ingress (host/path → backend).
// Compiles to an `HttpRouteSpec` (the embedded reverse-proxy). This is the k8s
// networking.k8s.io/v1 Ingress schema; the L4 firewall that used to own this
// Kind now lives under `kind: FirewallPolicy` (direction: ingress).
// ============================================================================

/// Field names accepted in a `kind: Ingress` `spec` (unknown-field warning).
pub(crate) const INGRESS_SPEC_FIELDS: &[&str] = &[
    "rules",
    "tls",
    "defaultBackend",
    "ingressClassName",
    "entrypoints",
];

#[derive(Debug, Deserialize, Serialize)]
struct IngressSpec {
    #[serde(default)]
    rules: Vec<IngressRule>,
    /// k8s TLS block (a LIST). v1 uses a SINGLE cert (no SNI) — the first entry wins.
    #[serde(default)]
    tls: Vec<IngressTls>,
    /// Catch-all backend when no rule matches (→ a `host: any, path: /` route).
    #[serde(default, rename = "defaultBackend")]
    default_backend: Option<IngressBackend>,
    /// Accepted for k8s fidelity; the embedded proxy is the only ingress class.
    #[serde(default, rename = "ingressClassName")]
    #[allow(dead_code)]
    ingress_class_name: Option<String>,
    /// delonix extension: listener ports. Omit → 80 (+ 443 when `tls` is set).
    #[serde(default)]
    entrypoints: Vec<Entrypoint>,
}

#[derive(Debug, Deserialize, Serialize)]
struct IngressRule {
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    http: Option<IngressHttp>,
}

#[derive(Debug, Deserialize, Serialize)]
struct IngressHttp {
    #[serde(default)]
    paths: Vec<IngressPath>,
}

#[derive(Debug, Deserialize, Serialize)]
struct IngressPath {
    #[serde(default = "default_path")]
    path: String,
    /// `Prefix` (default) | `Exact` | `ImplementationSpecific`. The proxy matches
    /// by prefix; `Exact` is accepted but treated as prefix (documented limitation).
    #[serde(default, rename = "pathType")]
    #[allow(dead_code)]
    path_type: Option<String>,
    backend: IngressBackend,
}

#[derive(Debug, Deserialize, Serialize)]
struct IngressBackend {
    service: IngressServiceRef,
}

#[derive(Debug, Deserialize, Serialize)]
struct IngressServiceRef {
    name: String,
    port: IngressServicePort,
}

#[derive(Debug, Deserialize, Serialize)]
struct IngressServicePort {
    #[serde(default)]
    number: Option<u16>,
    /// Named ports are not supported — use `number`.
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
}

/// Converts a k8s-shaped `IngressSpec` into the internal `HttpRouteSpec`.
fn ingress_to_httproute(name: &str, ing: IngressSpec) -> Result<HttpRouteSpec> {
    let port_of = |b: &IngressBackend| -> Result<u16> {
        b.service.port.number.ok_or_else(|| {
            Error::Invalid(format!(
                "Ingress/{name}: backend service '{}' — named ports are not supported, use port.number",
                b.service.name
            ))
        })
    };
    let mut rules = Vec::new();
    for r in &ing.rules {
        let paths = r.http.as_ref().map(|h| h.paths.as_slice()).unwrap_or(&[]);
        let mut prules = Vec::new();
        for p in paths {
            prules.push(PathRule {
                path: p.path.clone(),
                backend: Backend {
                    service: p.backend.service.name.clone(),
                    port: port_of(&p.backend)?,
                },
            });
        }
        rules.push(RouteRule {
            host: r.host.clone(),
            paths: prules,
        });
    }
    // defaultBackend → a catch-all route (any host, path `/`).
    if let Some(db) = &ing.default_backend {
        rules.push(RouteRule {
            host: None,
            paths: vec![PathRule {
                path: "/".to_string(),
                backend: Backend {
                    service: db.service.name.clone(),
                    port: port_of(db)?,
                },
            }],
        });
    }
    // k8s TLS is a list (SNI); v1 serves a single cert — the first entry decides
    // selfSigned vs secretRef.
    let tls = ing.tls.into_iter().next().map(|t| TlsSpec {
        mode: Some(if t.secret_name.is_some() {
            "secretRef".to_string()
        } else {
            "selfSigned".to_string()
        }),
        secret_ref: t.secret_name,
    });
    Ok(HttpRouteSpec {
        entrypoints: ing.entrypoints,
        tls,
        rules,
    })
}

#[derive(Debug, Deserialize, Serialize)]
struct IngressTls {
    #[serde(default)]
    #[allow(dead_code)]
    hosts: Vec<String>,
    #[serde(default, rename = "secretName")]
    secret_name: Option<String>,
}

// ============================================================================
// Application (Phase 4b): resolves the HTTPRoutes into a ProxyConfig
// (backends→ip:port, TLS) and ensures the reverse-proxy is serving (see
// `cmd::ingress_proxy`).
// ============================================================================

use super::ingress_proxy::{self, Listener, ProxyConfig, Route, TlsMaterial};

/// Map container-name → IP on the SDN (from the record). Only containers with an IP
/// (on a custom network) can serve as a backend — those of `--net host/none` have no
/// IP reachable by the proxy in the holder netns.
fn container_ips() -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    if let Ok((_, cstore)) = super::util::open_stores() {
        if let Ok(list) = cstore.list() {
            for c in list {
                if let Some(ip) = c.ip {
                    m.insert(c.name, ip);
                }
            }
        }
    }
    m
}

/// Reads the cert/key pair (PEM) from a `kind: Secret`. Accepts the k8s-style keys
/// (`tls.crt`/`tls.key`) OR the variant with `_` (the vault does not allow `.` in
/// env keys — see `valid_env_key`), whichever is found.
fn tls_from_secret(name: &str) -> Result<TlsMaterial> {
    let store = delonix_runtime_core::SecretStore::open(super::util::state_root())?;
    let s = store.load(name)?;
    let pick = |a: &str, b: &str| s.data.get(a).or_else(|| s.data.get(b)).cloned();
    let cert = pick("tls_crt", "tls.crt").ok_or_else(|| {
        Error::Invalid(format!(
            "Secret '{name}': falta a chave tls_crt/tls.crt (cert PEM)"
        ))
    })?;
    let key = pick("tls_key", "tls.key").ok_or_else(|| {
        Error::Invalid(format!(
            "Secret '{name}': falta a chave tls_key/tls.key (chave PEM)"
        ))
    })?;
    Ok(TlsMaterial {
        cert_pem: cert,
        key_pem: key,
    })
}

/// Resolves ALL the manifest's HTTPRoutes into a single `ProxyConfig` (one proxy
/// serves all the routes). `None` = there are no HTTPRoutes (nothing to do).
fn resolve_config(specs: &[(String, HttpRouteSpec)]) -> Result<Option<ProxyConfig>> {
    if specs.is_empty() {
        return Ok(None);
    }
    let ips = container_ips();
    let mut listeners: Vec<Listener> = Vec::new();
    let mut routes: Vec<Route> = Vec::new();
    let mut all_hosts: Vec<String> = Vec::new();
    let mut tls_material: Option<TlsMaterial> = None;
    let mut secret_ref: Option<String> = None;

    for (name, spec) in specs {
        // Listeners: the declared ones, or the default (:80, and :443 tls if there
        // is spec.tls).
        let eps = if spec.entrypoints.is_empty() {
            let mut d = vec![Entrypoint {
                port: 80,
                tls: false,
            }];
            if spec.tls.is_some() {
                d.push(Entrypoint {
                    port: 443,
                    tls: true,
                });
            }
            d
        } else {
            spec.entrypoints.clone()
        };
        for ep in eps {
            // Dedup by port; on collision, TLS wins (more restrictive/secure).
            match listeners.iter_mut().find(|l| l.port == ep.port) {
                Some(l) => l.tls = l.tls || ep.tls,
                None => listeners.push(Listener {
                    port: ep.port,
                    tls: ep.tls,
                }),
            }
        }
        // TLS: memoize the secretRef (resolved at the end) or mark self-signed.
        if let Some(tls) = &spec.tls {
            if tls.mode.as_deref() == Some("secretRef") {
                secret_ref = tls.secret_ref.clone();
            }
        }
        // Routes: resolve each backend to the record's ip:port.
        for rule in &spec.rules {
            if let Some(h) = &rule.host {
                all_hosts.push(h.clone());
            }
            for pr in &rule.paths {
                let ip = ips.get(&pr.backend.service).ok_or_else(|| {
                    Error::Invalid(format!(
                        "HTTPRoute '{name}': backend '{}' não tem IP na SDN (existe e está numa rede custom?)",
                        pr.backend.service
                    ))
                })?;
                routes.push(Route {
                    host: rule.host.clone().unwrap_or_default(),
                    path: pr.path.clone(),
                    backend: format!("{ip}:{}", pr.backend.port),
                });
            }
        }
    }

    // TLS material, if any listener terminates TLS.
    if listeners.iter().any(|l| l.tls) {
        tls_material = Some(match secret_ref {
            Some(sref) => tls_from_secret(&sref)?,
            None => ingress_proxy::self_signed_pem(&all_hosts)?,
        });
    }

    Ok(Some(ProxyConfig {
        listeners,
        routes,
        tls: tls_material,
    }))
}

/// `stack apply` (and auto-registration, later): resolves the HTTPRoutes and
/// ensures the proxy is serving. Called AFTER the containers exist (needs the IPs).
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let specs = parse_and_validate(docs)?;
    let Some(cfg) = resolve_config(&specs)? else {
        return Ok(()); // no HTTPRoute — nothing to do
    };
    // Write the MANUAL part and recompose (composes with the auto-registered routes
    // of `--expose` containers, without one erasing the other).
    ingress_proxy::set_manual(&cfg)?;
    println!(
        "httproute: {} rota(s) em {} listener(s){} — proxy {}",
        cfg.routes.len(),
        cfg.listeners.len(),
        if cfg.tls.is_some() { " (TLS)" } else { "" },
        if ingress_proxy::is_running() {
            "a servir"
        } else {
            "arrancado"
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_host_rejeita_lixo_e_aceita_dns() {
        assert!(valid_host("loja.exemplo.ao"));
        assert!(valid_host("api-v2.example.com"));
        assert!(!valid_host(""));
        assert!(!valid_host("loja.exemplo.ao/path")); // with path
        assert!(!valid_host("loja:8080")); // with port
        assert!(!valid_host("a b")); // space
        assert!(!valid_host(".leading.dot"));
        assert!(!valid_host("host;rm -rf"));
    }

    #[test]
    fn valid_host_rejeita_labels_vazias_e_hifens_de_bordo() {
        assert!(!valid_host("loja..exemplo")); // empty label
        assert!(!valid_host("loja.")); // trailing dot
        assert!(!valid_host("-loja.com")); // hyphen at the head of the label
        assert!(!valid_host("loja-.com")); // hyphen at the end of the label
        assert!(valid_host("a.b.c")); // 1-char labels, ok
    }

    #[test]
    fn rota_duplicada_host_path_falha() {
        let r = parse(
            "rules:\n  - host: x.example\n    paths:\n      - { path: /, backend: { service: a, port: 80 } }\n  - host: x.example\n    paths:\n      - { path: /, backend: { service: b, port: 81 } }\n",
        );
        assert!(r.is_err());
    }

    #[test]
    fn ingress_k8s_shape_compiles_to_httproute() {
        let yaml = "\
ingressClassName: delonix
tls:
  - hosts: [shop.example.ao]
    secretName: shop-tls
rules:
  - host: shop.example.ao
    http:
      paths:
        - path: /
          pathType: Prefix
          backend:
            service:
              name: web
              port: { number: 80 }
        - path: /api
          backend:
            service:
              name: api
              port: { number: 8080 }
";
        let ing: IngressSpec = serde_yaml::from_str(yaml).unwrap();
        let hr = ingress_to_httproute("shop", ing).unwrap();
        assert_eq!(hr.rules.len(), 1);
        assert_eq!(hr.rules[0].host.as_deref(), Some("shop.example.ao"));
        assert_eq!(hr.rules[0].paths.len(), 2);
        assert_eq!(hr.rules[0].paths[0].path, "/");
        assert_eq!(hr.rules[0].paths[0].backend.service, "web");
        assert_eq!(hr.rules[0].paths[0].backend.port, 80);
        assert_eq!(hr.rules[0].paths[1].backend.service, "api");
        assert_eq!(hr.rules[0].paths[1].backend.port, 8080);
        let tls = hr.tls.unwrap();
        assert_eq!(tls.mode.as_deref(), Some("secretRef"));
        assert_eq!(tls.secret_ref.as_deref(), Some("shop-tls"));
    }

    #[test]
    fn ingress_default_backend_becomes_catch_all() {
        let yaml = "\
defaultBackend:
  service:
    name: fallback
    port: { number: 8080 }
";
        let ing: IngressSpec = serde_yaml::from_str(yaml).unwrap();
        let hr = ingress_to_httproute("x", ing).unwrap();
        assert_eq!(hr.rules.len(), 1);
        assert!(hr.rules[0].host.is_none());
        assert_eq!(hr.rules[0].paths[0].path, "/");
        assert_eq!(hr.rules[0].paths[0].backend.service, "fallback");
    }

    #[test]
    fn ingress_named_port_is_rejected() {
        let yaml = "\
rules:
  - http:
      paths:
        - backend:
            service:
              name: web
              port: { name: http }
";
        let ing: IngressSpec = serde_yaml::from_str(yaml).unwrap();
        assert!(ingress_to_httproute("x", ing).is_err());
    }

    #[test]
    fn valid_path_prefix_exige_barra_e_rejeita_traversal() {
        assert!(valid_path_prefix("/"));
        assert!(valid_path_prefix("/api/v2"));
        assert!(!valid_path_prefix("api")); // no leading slash
        assert!(!valid_path_prefix("/a/../b")); // traversal
        assert!(!valid_path_prefix("/a b")); // space
    }

    #[test]
    fn valid_service_alfabeto_de_nome_de_recurso() {
        assert!(valid_service("web"));
        assert!(valid_service("api-prod_2"));
        assert!(!valid_service(""));
        assert!(!valid_service("a/b"));
        assert!(!valid_service("host:80"));
    }

    fn parse(yaml: &str) -> Result<HttpRouteSpec> {
        let spec: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let hr: HttpRouteSpec = serde_yaml::from_value(spec).unwrap();
        validate_spec("t", &hr)?;
        Ok(hr)
    }

    #[test]
    fn spec_valido_passa() {
        let hr = parse(
            "rules:\n  - host: loja.exemplo.ao\n    paths:\n      - path: /\n        backend: { service: web, port: 8080 }\n",
        )
        .unwrap();
        assert_eq!(hr.rules.len(), 1);
        assert_eq!(hr.rules[0].paths[0].path, "/");
        assert_eq!(hr.rules[0].paths[0].backend.port, 8080);
    }

    #[test]
    fn path_default_e_barra() {
        let hr =
            parse("rules:\n  - paths:\n      - backend: { service: web, port: 80 }\n").unwrap();
        assert_eq!(hr.rules[0].paths[0].path, "/");
    }

    #[test]
    fn rules_vazio_falha() {
        assert!(parse("rules: []\n").is_err());
    }

    #[test]
    fn entrypoint_tls_sem_spec_tls_falha() {
        let r = parse(
            "entrypoints:\n  - { port: 443, tls: true }\nrules:\n  - paths:\n      - backend: { service: web, port: 80 }\n",
        );
        assert!(r.is_err());
    }

    #[test]
    fn tls_secretref_sem_nome_falha() {
        let r = parse(
            "tls: { mode: secretRef }\nrules:\n  - paths:\n      - backend: { service: web, port: 80 }\n",
        );
        assert!(r.is_err());
    }

    #[test]
    fn backend_service_malicioso_rejeitado() {
        let r =
            parse("rules:\n  - paths:\n      - backend: { service: \"web; rm -rf\", port: 80 }\n");
        assert!(r.is_err());
    }
}
