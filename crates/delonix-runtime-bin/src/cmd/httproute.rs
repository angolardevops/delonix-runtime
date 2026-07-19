//! `kind: HTTPRoute` — reverse-proxy L7/HTTP declarativo (roteamento por Host/path
//! para containers backend), distinto do `kind: Ingress` (que é firewall L4
//! *inbound* por-container — ver `cmd/firewall.rs`). O nome segue o Gateway API do
//! Kubernetes: `HTTPRoute` = L7, `Ingress`/`FirewallPolicy` = L4.
//!
//! **Arquitetura** (ver `cmd/ingress_proxy.rs` / `realize` na Fase 4): o schema
//! aqui é a superfície declarativa; o plano físico é um reverse-proxy `hyper`
//! embutido, lançado DENTRO do netns do holder (onde alcança os backends por IP),
//! com as portas de entrada publicadas no host via o mecanismo de `publish` já
//! existente. TLS termina no proxy (BYO via `kind: Secret` ou self-signed).
//!
//! **Semântica "garante presente"** como todos os Kinds: sem reconciler. Esta Fase
//! (F1) entrega só o **parsing + validação** (schema, `valid_*`, grafo de
//! referências); o proxy e o ciclo de vida vêm nas fases seguintes.

use clap::Subcommand;
use serde::Deserialize;

use super::manifest::{self, ManifestDoc};
use delonix_runtime_core::{Error, Result};

/// `delonix httproute` — inspeccionar/derrubar o reverse-proxy L7 dos HTTPRoute.
/// (O `apply` faz-se por `stack apply`/`<kind> apply` — este grupo é operação.)
#[derive(Subcommand)]
pub enum HttpRouteCmd {
    /// Estado do proxy + rotas activas (da config em vigor).
    Ls,
    /// Aplica os HTTPRoute de um manifesto (sobe/recarrega o proxy).
    Apply {
        /// Ficheiro do manifesto (default `./delonix-manifest.yaml`).
        #[arg(short, long)]
        file: Option<std::path::PathBuf>,
    },
    /// Pára o proxy e despublica as portas (teardown).
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
                None => println!("httproute: proxy a correr mas a config não leu"),
            }
            Ok(())
        }
        HttpRouteCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
        HttpRouteCmd::Rm => {
            // Remove só as rotas MANUAIS; as auto-registadas (`--expose`) sobrevivem
            // e o proxy só pára se nada mais restar.
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

/// `spec` de `kind: HTTPRoute`.
#[derive(Debug, Clone, Deserialize)]
pub struct HttpRouteSpec {
    /// Pontos de entrada (portas onde o proxy escuta). Default: `[{port: 80}]`,
    /// mais `{port: 443, tls: true}` implícito se `spec.tls` estiver definido.
    #[serde(default)]
    pub entrypoints: Vec<Entrypoint>,
    /// Configuração de TLS (opcional). Sem isto, o proxy só serve HTTP.
    #[serde(default)]
    pub tls: Option<TlsSpec>,
    /// Regras de roteamento (por Host e/ou prefixo de path). Obrigatório e não-vazio.
    #[serde(default)]
    pub rules: Vec<RouteRule>,
}

/// Um ponto de entrada (porta de escuta do proxy).
#[derive(Debug, Clone, Deserialize)]
pub struct Entrypoint {
    pub port: u16,
    /// `true` = termina TLS nesta porta (exige `spec.tls`). Default `false`.
    #[serde(default)]
    pub tls: bool,
}

/// Configuração de TLS do proxy.
#[derive(Debug, Clone, Deserialize)]
pub struct TlsSpec {
    /// `selfSigned` (default) ou `secretRef`.
    #[serde(default)]
    pub mode: Option<String>,
    /// Nome de um `kind: Secret` com as chaves `tls.crt`/`tls.key` (PEM). Usado
    /// quando `mode: secretRef`.
    #[serde(default, rename = "secretRef")]
    pub secret_ref: Option<String>,
}

/// Uma regra de roteamento: casa por `host` (opcional — vazio = qualquer Host) e
/// despacha por prefixo de path para um backend.
#[derive(Debug, Clone, Deserialize)]
pub struct RouteRule {
    /// Nome de host a casar (ex.: `loja.exemplo.ao`). Vazio/omisso = qualquer Host.
    #[serde(default)]
    pub host: Option<String>,
    /// Sub-regras por prefixo de path. Obrigatório e não-vazio.
    #[serde(default)]
    pub paths: Vec<PathRule>,
}

/// Um prefixo de path e o backend para onde encaminha.
#[derive(Debug, Clone, Deserialize)]
pub struct PathRule {
    /// Prefixo de path (ex.: `/`, `/api`). Default `/`.
    #[serde(default = "default_path")]
    pub path: String,
    pub backend: Backend,
}

fn default_path() -> String {
    "/".to_string()
}

/// O destino de um path: um container (por nome) e a porta onde ele escuta.
#[derive(Debug, Clone, Deserialize)]
pub struct Backend {
    /// Nome do container backend (resolvido para o IP do record no apply).
    pub service: String,
    /// Porta do container onde o serviço escuta.
    pub port: u16,
}

/// Campos conhecidos do `spec` (drift-guard — ver `manifest::warn_unknown_fields`).
pub const HTTP_ROUTE_SPEC_FIELDS: &[&str] = &["entrypoints", "tls", "rules"];

/// Um nome de host DNS válido para casar no `Host:` header. Estrito de propósito
/// (disciplina `valid_*` da auditoria): letras/dígitos/`.`/`-`, sem esquema, sem
/// `/`, sem porta, sem espaços — nunca chega a um `format!`/comando com lixo.
pub fn valid_host(h: &str) -> bool {
    if h.is_empty() || h.len() > 253 {
        return false;
    }
    // Cada label (separada por `.`) tem de ser não-vazia e não começar/acabar em
    // `-` — senão `loja..exemplo`/`loja.`/`-loja` passariam e a regra ficaria
    // morta (o `Host:` real nunca tem essa forma). Alfabeto: alnum + `-`.
    h.split('.').all(|label| {
        !label.is_empty()
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// Um prefixo de path válido: começa em `/`, sem espaços, sem `..`, ASCII
/// imprimível. Match por prefixo (não regex) — não precisa de metacaracteres.
pub fn valid_path_prefix(p: &str) -> bool {
    p.starts_with('/')
        && p.len() <= 2048
        && !p.contains("..")
        && !p.contains(char::is_whitespace)
        && p.chars().all(|c| c.is_ascii_graphic())
}

/// Um nome de serviço/container backend válido — o mesmo alfabeto que o resto do
/// runtime usa para nomes de recurso (o container tem de existir; resolvido no
/// apply). Sem `/`/espaços/`:` para nunca poluir uma linha de controlo.
pub fn valid_service(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Valida um `HttpRouteSpec` já desserializado (chamado no apply e reutilizável no
/// `stack validate`). Não toca em nada — só schema/`valid_*`.
pub fn validate_spec(name: &str, spec: &HttpRouteSpec) -> Result<()> {
    let err = |m: String| Error::Invalid(format!("HTTPRoute '{name}': {m}"));

    if spec.rules.is_empty() {
        return Err(err(
            "spec.rules não pode ser vazio (nada para rotear)".into()
        ));
    }
    // TLS: se algum entrypoint pede tls, ou mode: secretRef, o spec.tls tem de
    // fazer sentido.
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
    // Um entrypoint com tls: true exige spec.tls definido (senão não há cert).
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
    // Pares (host, path) exactamente iguais tornam uma das rotas morta em silêncio
    // (o matcher da F2 escolhe uma) — apanha o conflito já na validação.
    let mut seen_routes: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    // Regras: cada host (se presente) e cada path/backend válidos.
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

/// Desserializa + valida cada `kind: HTTPRoute` do manifesto (sem aplicar nada
/// ainda — o proxy e o ciclo de vida vêm na Fase 4). Avisa por campo desconhecido.
pub fn parse_and_validate(docs: &[ManifestDoc]) -> Result<Vec<(String, HttpRouteSpec)>> {
    let mut out = Vec::new();
    for doc in manifest::of_kind(docs, "HTTPRoute") {
        manifest::warn_unknown_fields(doc, HTTP_ROUTE_SPEC_FIELDS);
        let spec: HttpRouteSpec = manifest::spec_of(doc)?;
        validate_spec(&doc.metadata.name, &spec)?;
        out.push((doc.metadata.name.clone(), spec));
    }
    Ok(out)
}

// ============================================================================
// Aplicação (Fase 4b): resolve os HTTPRoute numa ProxyConfig (backends→ip:porta,
// TLS) e garante o reverse-proxy a servir (ver `cmd::ingress_proxy`).
// ============================================================================

use super::ingress_proxy::{self, Listener, ProxyConfig, Route, TlsMaterial};

/// Mapa nome-de-container → IP na SDN (do record). Só containers com IP (numa
/// rede custom) servem de backend — os de `--net host/none` não têm IP alcançável
/// pelo proxy no netns do holder.
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

/// Lê o par cert/chave (PEM) de um `kind: Secret`. Aceita as chaves estilo k8s
/// (`tls.crt`/`tls.key`) OU a variante com `_` (o cofre não permite `.` nas
/// chaves de env — ver `valid_env_key`), o que for encontrado.
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

/// Resolve TODOS os HTTPRoute do manifesto numa única `ProxyConfig` (um proxy
/// serve todas as rotas). `None` = não há HTTPRoute (nada a fazer).
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
        // Listeners: os declarados, ou o default (:80, e :443 tls se houver spec.tls).
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
            // Dedup por porta; se colidir, TLS ganha (mais restritivo/seguro).
            match listeners.iter_mut().find(|l| l.port == ep.port) {
                Some(l) => l.tls = l.tls || ep.tls,
                None => listeners.push(Listener {
                    port: ep.port,
                    tls: ep.tls,
                }),
            }
        }
        // TLS: memoriza o secretRef (resolve-se no fim) ou marca self-signed.
        if let Some(tls) = &spec.tls {
            if tls.mode.as_deref() == Some("secretRef") {
                secret_ref = tls.secret_ref.clone();
            }
        }
        // Rotas: resolve cada backend para ip:porta do record.
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

    // Material TLS, se algum listener termina TLS.
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

/// `stack apply` (e o auto-registo, mais tarde): resolve os HTTPRoute e garante o
/// proxy a servir. Chamado DEPOIS dos containers existirem (precisa dos IPs).
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let specs = parse_and_validate(docs)?;
    let Some(cfg) = resolve_config(&specs)? else {
        return Ok(()); // sem HTTPRoute — nada a fazer
    };
    // Escreve a parte MANUAL e recompõe (compõe com as rotas auto-registadas de
    // containers `--expose`, sem que uma apague a outra).
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
        assert!(!valid_host("loja.exemplo.ao/path")); // com path
        assert!(!valid_host("loja:8080")); // com porta
        assert!(!valid_host("a b")); // espaço
        assert!(!valid_host(".leading.dot"));
        assert!(!valid_host("host;rm -rf"));
    }

    #[test]
    fn valid_host_rejeita_labels_vazias_e_hifens_de_bordo() {
        assert!(!valid_host("loja..exemplo")); // label vazia
        assert!(!valid_host("loja.")); // ponto final
        assert!(!valid_host("-loja.com")); // hífen à cabeça da label
        assert!(!valid_host("loja-.com")); // hífen no fim da label
        assert!(valid_host("a.b.c")); // labels de 1 char, ok
    }

    #[test]
    fn rota_duplicada_host_path_falha() {
        let r = parse(
            "rules:\n  - host: x.example\n    paths:\n      - { path: /, backend: { service: a, port: 80 } }\n  - host: x.example\n    paths:\n      - { path: /, backend: { service: b, port: 81 } }\n",
        );
        assert!(r.is_err());
    }

    #[test]
    fn valid_path_prefix_exige_barra_e_rejeita_traversal() {
        assert!(valid_path_prefix("/"));
        assert!(valid_path_prefix("/api/v2"));
        assert!(!valid_path_prefix("api")); // sem barra inicial
        assert!(!valid_path_prefix("/a/../b")); // traversal
        assert!(!valid_path_prefix("/a b")); // espaço
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
