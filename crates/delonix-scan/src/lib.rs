//! `delonix-scan` — scanner de vulnerabilidades de imagens (cloud-native).
//!
//! Duas operações, sem root e sem correr a imagem:
//! 1. **SBOM** — extrai a lista de pacotes instalados (Alpine `apk`, Debian/Ubuntu
//!    `dpkg`) lendo os *layers* directamente do CAS (sem montar nada);
//! 2. **match** — compara o SBOM com uma base de *advisories* (CVE) e reporta os
//!    pacotes vulneráveis, com severidade.
//!
//! Pensado para correr **antes do build** (analisar a base `FROM`) e em **imagens
//! já existentes** — exactamente como o `trivy`/`grype`, mas embutido no engine.

use delonix_image::{Image, ImageStore};
use delonix_runtime_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;

pub mod pytree;

/// O gestor de pacotes que registou um pacote (determina a fonte de advisories).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ecosystem {
    /// Alpine (`/lib/apk/db/installed`).
    Apk,
    /// Debian/Ubuntu (`/var/lib/dpkg/status`).
    Dpkg,
    /// Python/PyPI (`requirements.txt` de módulos — ex.: módulos Odoo).
    PyPi,
}

/// Um pacote instalado na imagem.
#[derive(Serialize, Clone, Debug)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub ecosystem: Ecosystem,
}

/// Severidade de uma vulnerabilidade (ordenável para *gates* de CI).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    /// Analisa `"low"|"medium"|"high"|"critical"` (para `--fail-on`).
    pub fn parse(s: &str) -> Option<Severity> {
        match s.to_ascii_lowercase().as_str() {
            "low" => Some(Severity::Low),
            "medium" | "med" => Some(Severity::Medium),
            "high" => Some(Severity::High),
            "critical" | "crit" => Some(Severity::Critical),
            _ => None,
        }
    }
}

/// Um *advisory* (CVE): "o pacote X no ecossistema Y é vulnerável abaixo da
/// versão `fixed`".
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Advisory {
    pub id: String,
    pub package: String,
    pub ecosystem: Ecosystem,
    /// Primeira versão CORRIGIDA: a imagem é vulnerável se `version < fixed`.
    pub fixed: String,
    pub severity: Severity,
    pub summary: String,
}

/// Uma vulnerabilidade encontrada na imagem.
#[derive(Serialize, Clone, Debug)]
pub struct Finding {
    pub id: String,
    pub package: String,
    pub version: String,
    pub fixed: String,
    pub severity: Severity,
    pub summary: String,
}

// ---------------------------------------------------------------------------
// SBOM — extrair pacotes dos layers (lendo o CAS, sem montar nem correr)
// ---------------------------------------------------------------------------

/// Lê um *layer* (tar, opcionalmente gzip) e devolve o conteúdo do primeiro
/// ficheiro cujo caminho termine em `suffix`, se existir.
fn read_member(blob: &[u8], suffix: &str) -> Option<Vec<u8>> {
    let is_gzip = blob.len() >= 2 && blob[0] == 0x1f && blob[1] == 0x8b;
    let reader: Box<dyn Read> = if is_gzip {
        Box::new(flate2::read::GzDecoder::new(blob))
    } else {
        Box::new(blob)
    };
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().ok()? {
        let mut entry = entry.ok()?;
        let path = entry.path().ok()?.to_string_lossy().into_owned();
        if path.trim_start_matches("./").ends_with(suffix) {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).ok()?;
            return Some(buf);
        }
    }
    None
}

/// Pacotes de uma base de dados `apk` (`P:` nome, `V:` versão, registos por
/// linha em branco).
fn parse_apk(db: &str) -> Vec<Package> {
    let mut out = Vec::new();
    let mut name = None;
    for line in db.lines() {
        if let Some(n) = line.strip_prefix("P:") {
            name = Some(n.trim().to_string());
        } else if let Some(v) = line.strip_prefix("V:") {
            if let Some(n) = name.take() {
                out.push(Package {
                    name: n,
                    version: v.trim().to_string(),
                    ecosystem: Ecosystem::Apk,
                });
            }
        }
    }
    out
}

/// Pacotes de um `dpkg/status` (estrofes com `Package:` e `Version:`).
fn parse_dpkg(db: &str) -> Vec<Package> {
    let mut out = Vec::new();
    let mut name = None;
    for line in db.lines() {
        if let Some(n) = line.strip_prefix("Package:") {
            name = Some(n.trim().to_string());
        } else if let Some(v) = line.strip_prefix("Version:") {
            if let Some(n) = name.clone() {
                out.push(Package {
                    name: n,
                    version: v.trim().to_string(),
                    ecosystem: Ecosystem::Dpkg,
                });
            }
        } else if line.is_empty() {
            name = None;
        }
    }
    out
}

/// Extrai o SBOM de uma imagem, lendo os *layers* do topo para a base (a base
/// de dados de pacotes mais recente ganha).
pub fn extract_sbom(images: &ImageStore, image: &Image) -> Result<Vec<Package>> {
    let mut apk_db: Option<String> = None;
    let mut dpkg_db: Option<String> = None;
    for digest in image.layers.iter().rev() {
        let blob = images.cas().read(digest)?;
        if apk_db.is_none() {
            if let Some(b) = read_member(&blob, "lib/apk/db/installed") {
                apk_db = Some(String::from_utf8_lossy(&b).into_owned());
            }
        }
        if dpkg_db.is_none() {
            if let Some(b) = read_member(&blob, "var/lib/dpkg/status") {
                dpkg_db = Some(String::from_utf8_lossy(&b).into_owned());
            }
        }
    }
    let mut pkgs = Vec::new();
    if let Some(db) = apk_db {
        pkgs.extend(parse_apk(&db));
    }
    if let Some(db) = dpkg_db {
        pkgs.extend(parse_dpkg(&db));
    }
    if pkgs.is_empty() {
        return Err(Error::Invalid(
            "SBOM vazio: sem base de dados de pacotes apk/dpkg na imagem".into(),
        ));
    }
    pkgs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(pkgs)
}

// ---------------------------------------------------------------------------
// Comparação de versões (estilo apk/dpkg, simplificada)
// ---------------------------------------------------------------------------

/// Parte uma versão em sequências alternadas de dígitos / não-dígitos.
fn tokens(v: &str) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut digit = false;
    for c in v.chars() {
        if c.is_ascii_alphanumeric() {
            if c.is_ascii_digit() != digit && !cur.is_empty() {
                out.push((digit, std::mem::take(&mut cur)));
            }
            digit = c.is_ascii_digit();
            cur.push(c);
        } else if !cur.is_empty() {
            out.push((digit, std::mem::take(&mut cur)));
            cur.clear();
        }
    }
    if !cur.is_empty() {
        out.push((digit, cur));
    }
    out
}

/// `true` se `a < b` (comparação de versões). Tokens numéricos comparam-se como
/// números; o resto, lexicograficamente; quem acaba primeiro é "menor".
pub fn version_lt(a: &str, b: &str) -> bool {
    let (ta, tb) = (tokens(a), tokens(b));
    for i in 0..ta.len().max(tb.len()) {
        match (ta.get(i), tb.get(i)) {
            (None, Some(_)) => return true,  // a acabou -> a < b
            (Some(_), None) => return false, // b acabou -> a > b
            (Some((da, sa)), Some((db, sb))) => {
                let ord = if *da && *db {
                    sa.parse::<u64>()
                        .unwrap_or(0)
                        .cmp(&sb.parse::<u64>().unwrap_or(0))
                } else {
                    sa.cmp(sb)
                };
                if ord != std::cmp::Ordering::Equal {
                    return ord == std::cmp::Ordering::Less;
                }
            }
            (None, None) => break,
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Base de advisories + matching
// ---------------------------------------------------------------------------

/// A base de *advisories* (carregada de JSON; em produção sincronizaria de
/// OSV / NVD / Alpine secdb).
pub struct AdvisoryDb {
    advisories: Vec<Advisory>,
}

impl AdvisoryDb {
    /// Carrega a base a partir de JSON (`[ {id, package, ecosystem, fixed, ...} ]`).
    pub fn load(json: &str) -> Result<Self> {
        let advisories: Vec<Advisory> =
            serde_json::from_str(json).map_err(|e| Error::Invalid(format!("advisories: {e}")))?;
        Ok(Self { advisories })
    }

    /// Nº de advisories carregadas.
    pub fn len(&self) -> usize {
        self.advisories.len()
    }
    pub fn is_empty(&self) -> bool {
        self.advisories.is_empty()
    }

    /// Cruza o SBOM com as advisories: um pacote é vulnerável se houver uma
    /// advisory do mesmo ecossistema/nome com `version < fixed`.
    pub fn scan(&self, sbom: &[Package]) -> Vec<Finding> {
        let mut out = Vec::new();
        for adv in &self.advisories {
            for pkg in sbom {
                if pkg.ecosystem == adv.ecosystem
                    && pkg.name == adv.package
                    && version_lt(&pkg.version, &adv.fixed)
                {
                    out.push(Finding {
                        id: adv.id.clone(),
                        package: pkg.name.clone(),
                        version: pkg.version.clone(),
                        fixed: adv.fixed.clone(),
                        severity: adv.severity,
                        summary: adv.summary.clone(),
                    });
                }
            }
        }
        // mais grave primeiro
        out.sort_by_key(|f| std::cmp::Reverse(f.severity));
        out
    }
}

// ---------------------------------------------------------------------------
// Ingestão de feeds reais (OSV) + staleness da base
// ---------------------------------------------------------------------------

/// Mapeia o `ecosystem` do OSV (ex.: `Alpine:v3.18`, `Debian:11`, `Ubuntu:22.04`)
/// ao ecossistema de pacotes que sabemos cruzar. Devolve `None` para os que não
/// suportamos (npm, Go, …) — são ignorados, não convertidos a lixo.
fn map_osv_ecosystem(s: &str) -> Option<Ecosystem> {
    if s.starts_with("Alpine") {
        Some(Ecosystem::Apk)
    } else if s.starts_with("Debian") || s.starts_with("Ubuntu") {
        Some(Ecosystem::Dpkg)
    } else if s == "PyPI" {
        Some(Ecosystem::PyPi)
    } else {
        None
    }
}

/// Extrai uma severidade de um objeto OSV (vuln ou affected) a partir do
/// `database_specific.severity` / `ecosystem_specific.severity` (string). O CVSS
/// vector NÃO é interpretado (exigiria calcular o score) — quem não tiver rótulo
/// cai no default do chamador.
fn osv_severity_label(obj: &serde_json::Value) -> Option<Severity> {
    for key in ["database_specific", "ecosystem_specific"] {
        if let Some(s) = obj
            .get(key)
            .and_then(|d| d.get("severity"))
            .and_then(|v| v.as_str())
        {
            if let Some(sev) = Severity::parse(s) {
                return Some(sev);
            }
        }
    }
    None
}

/// Primeira versão CORRIGIDA (`{"fixed": …}`) nos `ranges[].events[]` de um
/// objeto `affected` do OSV.
fn osv_first_fixed(affected: &serde_json::Value) -> Option<String> {
    for range in affected.get("ranges")?.as_array()? {
        let Some(events) = range.get("events").and_then(|e| e.as_array()) else {
            continue;
        };
        for ev in events {
            if let Some(f) = ev.get("fixed").and_then(|v| v.as_str()) {
                return Some(f.to_string());
            }
        }
    }
    None
}

/// Converte um feed **OSV** (um array de vulns, ou `{"vulns":[…]}`) na base de
/// advisories interna. Best-effort: cada vuln gera uma advisory por
/// (pacote, ecossistema, 1.ª-versão-corrigida) para os ecossistemas suportados
/// (Alpine→apk, Debian/Ubuntu→dpkg); os restantes são ignorados. Sem rótulo de
/// severidade, assume `Medium`. Função pura — testável sem rede.
pub fn advisories_from_osv(json: &str) -> Result<Vec<Advisory>> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| Error::Invalid(format!("feed OSV inválido: {e}")))?;
    let vulns = if let Some(arr) = v.as_array() {
        arr.clone()
    } else if let Some(arr) = v.get("vulns").and_then(|x| x.as_array()) {
        arr.clone()
    } else {
        return Err(Error::Invalid(
            "feed OSV: esperado um array ou um objeto {\"vulns\":[…]}".into(),
        ));
    };
    let mut out = Vec::new();
    for vuln in &vulns {
        let id = vuln.get("id").and_then(|x| x.as_str()).unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let summary = vuln
            .get("summary")
            .and_then(|x| x.as_str())
            .or_else(|| vuln.get("details").and_then(|x| x.as_str()))
            .unwrap_or("")
            .chars()
            .take(200)
            .collect::<String>();
        let vuln_sev = osv_severity_label(vuln);
        let Some(affected) = vuln.get("affected").and_then(|x| x.as_array()) else {
            continue;
        };
        for aff in affected {
            let Some(pkg) = aff.get("package") else {
                continue;
            };
            let name = pkg.get("name").and_then(|x| x.as_str()).unwrap_or("");
            let eco = pkg.get("ecosystem").and_then(|x| x.as_str()).unwrap_or("");
            let (Some(ecosystem), false) = (map_osv_ecosystem(eco), name.is_empty()) else {
                continue;
            };
            let Some(fixed) = osv_first_fixed(aff) else {
                continue;
            };
            let severity = osv_severity_label(aff)
                .or(vuln_sev)
                .unwrap_or(Severity::Medium);
            out.push(Advisory {
                id: id.to_string(),
                package: name.to_string(),
                ecosystem,
                fixed,
                severity,
                summary: summary.clone(),
            });
        }
    }
    Ok(out)
}

/// `true` se a base de advisories está OBSOLETA (sincronizada há mais de
/// `max_age_days`, ou nunca sincronizada). Usado para avisar que um scan sem
/// achados pode não ser de confiança. Função pura.
pub fn db_is_stale(synced_unix: Option<u64>, now_unix: u64, max_age_days: u64) -> bool {
    match synced_unix {
        None => true,
        Some(t) => now_unix.saturating_sub(t) > max_age_days.saturating_mul(86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(version_lt("1.36.1-r20", "1.37.0")); // busybox vulnerável
        assert!(!version_lt("1.3.1-r0", "1.3")); // zlib já corrigido
        assert!(version_lt("3.1.7", "3.1.8"));
        assert!(!version_lt("3.1.8-r1", "3.1.8"));
        assert!(version_lt("2.36-9", "2.37"));
    }

    #[test]
    fn parses_apk_db() {
        let db = "C:Q1x\nP:busybox\nV:1.36.1-r20\nA:x86_64\n\nP:zlib\nV:1.3.1-r0\n";
        let pkgs = parse_apk(db);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "busybox");
        assert_eq!(pkgs[0].version, "1.36.1-r20");
    }

    #[test]
    fn parses_dpkg_status() {
        let db = "Package: libc6\nStatus: install ok installed\nVersion: 2.36-9\n\nPackage: bash\nVersion: 5.2-15\n";
        let pkgs = parse_dpkg(db);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[1].name, "bash");
    }

    #[test]
    fn matches_only_vulnerable() {
        let sbom = vec![
            Package {
                name: "busybox".into(),
                version: "1.36.1-r20".into(),
                ecosystem: Ecosystem::Apk,
            },
            Package {
                name: "zlib".into(),
                version: "1.3.1-r0".into(),
                ecosystem: Ecosystem::Apk,
            },
        ];
        let db = AdvisoryDb::load(
            r#"[
              {"id":"CVE-A","package":"busybox","ecosystem":"Apk","fixed":"1.37.0","severity":"high","summary":"x"},
              {"id":"CVE-B","package":"zlib","ecosystem":"Apk","fixed":"1.3","severity":"medium","summary":"y"}
            ]"#,
        )
        .unwrap();
        let f = db.scan(&sbom);
        assert_eq!(f.len(), 1); // só o busybox
        assert_eq!(f[0].id, "CVE-A");
    }

    #[test]
    fn osv_converte_alpine_e_ignora_ecossistemas_nao_suportados() {
        // Um feed OSV: uma vuln Alpine (convertível) + uma npm (ignorada).
        let feed = r#"{"vulns":[
          {
            "id":"CVE-2023-4567","summary":"busybox oops",
            "database_specific":{"severity":"HIGH"},
            "affected":[{
              "package":{"ecosystem":"Alpine:v3.18","name":"busybox"},
              "ranges":[{"type":"ECOSYSTEM","events":[{"introduced":"0"},{"fixed":"1.36.1-r5"}]}]
            }]
          },
          {
            "id":"GHSA-xxxx","summary":"left-pad",
            "affected":[{"package":{"ecosystem":"npm","name":"left-pad"},
              "ranges":[{"type":"SEMVER","events":[{"introduced":"0"},{"fixed":"1.3.0"}]}]}]
          }
        ]}"#;
        let advs = advisories_from_osv(feed).unwrap();
        assert_eq!(advs.len(), 1); // só a Alpine
        assert_eq!(advs[0].id, "CVE-2023-4567");
        assert_eq!(advs[0].package, "busybox");
        assert_eq!(advs[0].ecosystem, Ecosystem::Apk);
        assert_eq!(advs[0].fixed, "1.36.1-r5");
        assert_eq!(advs[0].severity, Severity::High);
    }

    #[test]
    fn osv_aceita_array_e_default_medium_sem_rotulo() {
        let feed = r#"[{
          "id":"CVE-1","details":"debian glibc",
          "affected":[{"package":{"ecosystem":"Debian:12","name":"libc6"},
            "ranges":[{"type":"ECOSYSTEM","events":[{"fixed":"2.36-9+deb12u1"}]}]}]
        }]"#;
        let advs = advisories_from_osv(feed).unwrap();
        assert_eq!(advs.len(), 1);
        assert_eq!(advs[0].ecosystem, Ecosystem::Dpkg);
        assert_eq!(advs[0].severity, Severity::Medium); // sem rótulo → default
                                                        // as advisories convertidas cruzam com um SBOM real.
        let db = AdvisoryDb::load(&serde_json::to_string(&advs_json(&advs)).unwrap()).unwrap();
        let sbom = vec![Package {
            name: "libc6".into(),
            version: "2.36-9".into(),
            ecosystem: Ecosystem::Dpkg,
        }];
        assert_eq!(db.scan(&sbom).len(), 1);
    }

    // helper: Advisory não é Serialize; reconstrói o JSON para o round-trip.
    fn advs_json(advs: &[Advisory]) -> Vec<serde_json::Value> {
        advs.iter().map(|a| serde_json::json!({
            "id": a.id, "package": a.package,
            "ecosystem": match a.ecosystem { Ecosystem::Apk => "Apk", Ecosystem::Dpkg => "Dpkg", Ecosystem::PyPi => "PyPi" },
            "fixed": a.fixed,
            "severity": match a.severity { Severity::Low=>"low", Severity::Medium=>"medium", Severity::High=>"high", Severity::Critical=>"critical" },
            "summary": a.summary,
        })).collect()
    }

    #[test]
    fn staleness_deteta_nunca_e_antigo() {
        let dia = 86_400u64;
        assert!(db_is_stale(None, 100 * dia, 30)); // nunca sincronizado
        assert!(db_is_stale(Some(0), 40 * dia, 30)); // 40 dias > 30
        assert!(!db_is_stale(Some(20 * dia), 40 * dia, 30)); // 20 dias <= 30
    }
}
