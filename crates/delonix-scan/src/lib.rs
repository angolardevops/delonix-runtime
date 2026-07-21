//! `delonix-scan` — image vulnerability scanner (cloud-native).
//!
//! Two operations, without root and without running the image:
//! 1. **SBOM** — extracts the list of installed packages (Alpine `apk`, Debian/Ubuntu
//!    `dpkg`) by reading the *layers* directly from the CAS (without mounting anything);
//! 2. **match** — compares the SBOM against a database of *advisories* (CVE) and reports
//!    the vulnerable packages, with severity.
//!
//! Designed to run **before the build** (analyzing the `FROM` base) and on **already
//! existing images** — exactly like `trivy`/`grype`, but embedded in the engine.

use delonix_image::{Image, ImageStore};
use delonix_runtime_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;

pub mod pytree;

/// The package manager that registered a package (determines the advisory source).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ecosystem {
    /// Alpine (`/lib/apk/db/installed`).
    Apk,
    /// Debian/Ubuntu (`/var/lib/dpkg/status`).
    Dpkg,
    /// Python/PyPI (`requirements.txt` of modules — e.g. Odoo modules).
    PyPi,
}

/// A package installed in the image.
#[derive(Serialize, Clone, Debug)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub ecosystem: Ecosystem,
}

/// Severity of a vulnerability (orderable for CI *gates*).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    /// Parses `"low"|"medium"|"high"|"critical"` (for `--fail-on`).
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

/// An *advisory* (CVE): "package X in ecosystem Y is vulnerable below
/// version `fixed`".
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Advisory {
    pub id: String,
    pub package: String,
    pub ecosystem: Ecosystem,
    /// First FIXED version: the image is vulnerable if `version < fixed`.
    pub fixed: String,
    pub severity: Severity,
    pub summary: String,
}

/// A vulnerability found in the image.
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
// SBOM — extract packages from the layers (reading the CAS, without mounting or running)
// ---------------------------------------------------------------------------

/// Reads a *layer* (tar, optionally gzip) and returns the content of the first
/// file whose path ends in `suffix`, if any.
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

/// Packages from an `apk` database (`P:` name, `V:` version, records separated
/// by a blank line).
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

/// Packages from a `dpkg/status` (stanzas with `Package:` and `Version:`).
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

/// Extracts the SBOM of an image, reading the *layers* from the top down to the
/// base (the most recent package database wins).
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
            "empty SBOM: no apk/dpkg package database in the image".into(),
        ));
    }
    pkgs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(pkgs)
}

// ---------------------------------------------------------------------------
// Version comparison (apk/dpkg style, simplified)
// ---------------------------------------------------------------------------

/// Splits a version into alternating sequences of digits / non-digits.
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

/// `true` if `a < b` (version comparison). Numeric tokens compare as
/// numbers; the rest, lexicographically; whoever ends first is "smaller".
pub fn version_lt(a: &str, b: &str) -> bool {
    let (ta, tb) = (tokens(a), tokens(b));
    for i in 0..ta.len().max(tb.len()) {
        match (ta.get(i), tb.get(i)) {
            (None, Some(_)) => return true,  // a ended -> a < b
            (Some(_), None) => return false, // b ended -> a > b
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
// Advisory database + matching
// ---------------------------------------------------------------------------

/// The *advisory* database (loaded from JSON; in production it would sync from
/// OSV / NVD / Alpine secdb).
pub struct AdvisoryDb {
    advisories: Vec<Advisory>,
}

impl AdvisoryDb {
    /// Loads the database from JSON (`[ {id, package, ecosystem, fixed, ...} ]`).
    pub fn load(json: &str) -> Result<Self> {
        let advisories: Vec<Advisory> =
            serde_json::from_str(json).map_err(|e| Error::Invalid(format!("advisories: {e}")))?;
        Ok(Self { advisories })
    }

    /// Number of loaded advisories.
    pub fn len(&self) -> usize {
        self.advisories.len()
    }
    pub fn is_empty(&self) -> bool {
        self.advisories.is_empty()
    }

    /// Cross-references the SBOM with the advisories: a package is vulnerable if
    /// there is an advisory of the same ecosystem/name with `version < fixed`.
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
        // most severe first
        out.sort_by_key(|f| std::cmp::Reverse(f.severity));
        out
    }
}

// ---------------------------------------------------------------------------
// Ingestion of real feeds (OSV) + database staleness
// ---------------------------------------------------------------------------

/// Maps the OSV `ecosystem` (e.g. `Alpine:v3.18`, `Debian:11`, `Ubuntu:22.04`)
/// to the package ecosystem we know how to cross-reference. Returns `None` for
/// the ones we don't support (npm, Go, …) — they are ignored, not turned into junk.
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

/// Extracts a severity from an OSV object (vuln or affected) from the
/// `database_specific.severity` / `ecosystem_specific.severity` (string). The CVSS
/// vector is NOT interpreted (it would require computing the score) — anything
/// without a label falls back to the caller's default.
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

/// First FIXED version (`{"fixed": …}`) in the `ranges[].events[]` of an
/// OSV `affected` object.
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

/// Converts an **OSV** feed (an array of vulns, or `{"vulns":[…]}`) into the
/// internal advisory database. Best-effort: each vuln generates one advisory per
/// (package, ecosystem, first-fixed-version) for the supported ecosystems
/// (Alpine→apk, Debian/Ubuntu→dpkg); the rest are ignored. Without a severity
/// label, it assumes `Medium`. Pure function — testable without the network.
pub fn advisories_from_osv(json: &str) -> Result<Vec<Advisory>> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| Error::Invalid(format!("invalid OSV feed: {e}")))?;
    let vulns = if let Some(arr) = v.as_array() {
        arr.clone()
    } else if let Some(arr) = v.get("vulns").and_then(|x| x.as_array()) {
        arr.clone()
    } else {
        return Err(Error::Invalid(
            "OSV feed: expected an array or a {\"vulns\":[…]} object".into(),
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

/// `true` if the advisory database is STALE (synced more than `max_age_days`
/// ago, or never synced). Used to warn that a scan with no findings may not
/// be trustworthy. Pure function.
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
        assert!(version_lt("1.36.1-r20", "1.37.0")); // busybox vulnerable
        assert!(!version_lt("1.3.1-r0", "1.3")); // zlib already fixed
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
        assert_eq!(f.len(), 1); // only busybox
        assert_eq!(f[0].id, "CVE-A");
    }

    #[test]
    fn osv_converte_alpine_e_ignora_ecossistemas_nao_suportados() {
        // An OSV feed: one Alpine vuln (convertible) + one npm (ignored).
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
        assert_eq!(advs.len(), 1); // only the Alpine one
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
        assert_eq!(advs[0].severity, Severity::Medium); // no label → default
                                                        // the converted advisories cross-reference against a real SBOM.
        let db = AdvisoryDb::load(&serde_json::to_string(&advs_json(&advs)).unwrap()).unwrap();
        let sbom = vec![Package {
            name: "libc6".into(),
            version: "2.36-9".into(),
            ecosystem: Ecosystem::Dpkg,
        }];
        assert_eq!(db.scan(&sbom).len(), 1);
    }

    // helper: Advisory is not Serialize; rebuilds the JSON for the round-trip.
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
        assert!(db_is_stale(None, 100 * dia, 30)); // never synced
        assert!(db_is_stale(Some(0), 40 * dia, 30)); // 40 days > 30
        assert!(!db_is_stale(Some(20 * dia), 40 * dia, 30)); // 20 days <= 30
    }
}
