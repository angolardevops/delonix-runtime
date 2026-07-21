//! `delonix image scan` — SBOM + CVE scan, and the admission policy on pull.
//!
//! The engine (`delonix-scan`) does the work: it extracts the SBOM by reading the layers
//! from the CAS (apk/dpkg, without mounting or running) and cross-references it with an OSV
//! advisory database. Here that is wired to the CLI and to the decision points (scan-on-pull).
//!
//! **Honest provenance**: the EMBEDDED database is a 5-entry placeholder — a
//! "no vulnerabilities" against it is NOT a clean bill of health, and the output
//! says so explicitly. Only a synced OSV feed (`scan --update`) gives a
//! trustworthy answer.

use delonix_image::{Image, ImageStore};
use delonix_runtime_core::{Error, Result};
use delonix_scan::{AdvisoryDb, Severity};

use super::output;
use super::util::{open_stores, resolve_or_pull, state_root};

/// The embedded placeholder database — 5 entries, so the scan doesn't blow up without a
/// synced feed. It is NEVER presented as definitive (see `Provenance`).
const EMBEDDED_ADVISORIES: &str = include_str!("../../../delonix-scan/data/advisories.json");

struct Provenance {
    label: String,
    synced_unix: Option<u64>,
    placeholder: bool,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Loads the advisory database: the synced one (`<root>/advisories.json`) takes
/// precedence; otherwise `$DELONIX_ADVISORIES`; otherwise the embedded placeholder.
fn load_advisories() -> Result<(AdvisoryDb, Provenance)> {
    let root = state_root();
    let synced = root.join("advisories.json");
    if let Ok(text) = std::fs::read_to_string(&synced) {
        let db = AdvisoryDb::load(&text)?;
        let (label, synced_unix) = std::fs::read_to_string(root.join("advisories.meta.json"))
            .ok()
            .and_then(|m| serde_json::from_str::<serde_json::Value>(&m).ok())
            .map(|m| {
                let src = m
                    .get("source")
                    .and_then(|v| v.as_str())
                    .unwrap_or("desconhecida")
                    .to_string();
                (
                    format!("sincronizada de {src}"),
                    m.get("synced_unix").and_then(|v| v.as_u64()),
                )
            })
            .unwrap_or_else(|| ("sincronizada".into(), None));
        return Ok((
            db,
            Provenance {
                label,
                synced_unix,
                placeholder: false,
            },
        ));
    }
    if let Ok(path) = std::env::var("DELONIX_ADVISORIES") {
        let db = AdvisoryDb::load(&std::fs::read_to_string(&path)?)?;
        return Ok((
            db,
            Provenance {
                label: format!("$DELONIX_ADVISORIES ({path})"),
                synced_unix: None,
                placeholder: false,
            },
        ));
    }
    let db = AdvisoryDb::load(EMBEDDED_ADVISORIES)?;
    Ok((
        db,
        Provenance {
            label: "base EMBEBIDA (placeholder)".into(),
            synced_unix: None,
            placeholder: true,
        },
    ))
}

/// `image scan <image>` — vulnerability dashboard. Pulls the image if missing
/// (like `docker scout`).
pub fn cmd_scan(image: &str, sbom: bool, fail_on: Option<&str>) -> Result<()> {
    let (images, _store) = open_stores()?;
    let img = match images.resolve(image) {
        Ok(img) => img,
        Err(Error::NotFound(_)) => {
            eprintln!(
                "{}",
                super::po::tf("image '{img}' is not local — pulling…", &[("img", image)])
            );
            resolve_or_pull(&images, image)?
        }
        Err(e) => return Err(e),
    };
    if sbom {
        let pkgs = delonix_scan::extract_sbom(&images, &img)?;
        let mut t = output::Table::new(&["PACKAGE", "VERSION", "ECOSYSTEM"]);
        for p in &pkgs {
            t.row(vec![
                p.name.clone(),
                p.version.clone(),
                format!("{:?}", p.ecosystem),
            ]);
        }
        println!("SBOM de {} — {} pacotes:", img.short_id(), pkgs.len());
        t.print();
        return Ok(());
    }
    let worst = scan_image(&images, &img)?;
    if let Some(threshold) = fail_on {
        let th = Severity::parse(threshold).ok_or_else(|| {
            Error::Invalid(format!(
                "severidade inválida: {threshold} (low|medium|high|critical)"
            ))
        })?;
        if worst.map(|w| w >= th).unwrap_or(false) {
            std::process::exit(1);
        }
    }
    Ok(())
}

/// Scans an image and prints the dashboard. Returns the worst severity
/// found (`None` = none). Reusable by scan-on-pull.
pub fn scan_image(images: &ImageStore, image: &Image) -> Result<Option<Severity>> {
    let sbom = delonix_scan::extract_sbom(images, image)?;
    let (db, prov) = load_advisories()?;
    let findings = db.scan(&sbom);

    println!(
        "{}",
        output::bold(&format!("Vulnerability Scan · {}", image.short_id()))
    );
    let count = |sev: Severity| findings.iter().filter(|f| f.severity == sev).count();
    let (crit, high, med, low) = (
        count(Severity::Critical),
        count(Severity::High),
        count(Severity::Medium),
        count(Severity::Low),
    );
    println!(
        "  {}   {}   {}",
        output::dim(&format!("SBOM: {} pacotes", sbom.len())),
        output::dim(&format!("advisories: {}", db.len())),
        output::dim(&format!("vulnerabilidades: {}", findings.len())),
    );
    println!("  {}", sev_line(crit, high, med, low));

    // HONEST provenance: without this, a "no vulnerabilities" against the placeholder
    // database looked like a clean bill of health — a false guarantee.
    let stale = delonix_scan::db_is_stale(prov.synced_unix, now_unix(), 14);
    println!(
        "  {}",
        output::dim(&format!(
            "fonte da base: {} ({} advisories)",
            prov.label,
            db.len()
        ))
    );
    if prov.placeholder {
        output::warn(&format!(
            "base de CVE EMBEBIDA (placeholder, {} entradas) — NÃO é um feed real; um \"sem vulnerabilidades\" não é de confiança. \
             Sincroniza: `delonix image scan --update --feed https://…/osv.json`",
            db.len()
        ));
    } else if stale {
        output::warn("base de advisories obsoleta (>14 dias sem sync) — corre `delonix image scan --update`.");
    }

    if findings.is_empty() {
        if prov.placeholder {
            println!(
                "  {}",
                output::dim("sem correspondências na base placeholder (não conclusivo)")
            );
        } else {
            println!("  ✔ sem vulnerabilidades conhecidas");
        }
        return Ok(None);
    }

    let mut t = output::Table::new(&["SEVERITY", "PACKAGE", "VERSION", "FIXED", "CVE"]);
    let mut worst = Severity::Low;
    for f in &findings {
        if f.severity > worst {
            worst = f.severity;
        }
        t.row(vec![
            format!("{:?}", f.severity),
            f.package.clone(),
            f.version.clone(),
            f.fixed.clone(),
            f.id.clone(),
        ]);
    }
    t.print();
    Ok(Some(worst))
}

fn sev_line(crit: usize, high: usize, med: usize, low: usize) -> String {
    if output::color_enabled() {
        format!(
            "\x1b[1;31m●\x1b[0m CRITICAL {crit}   \x1b[31m●\x1b[0m HIGH {high}   \x1b[33m●\x1b[0m MEDIUM {med}   \x1b[36m●\x1b[0m LOW {low}"
        )
    } else {
        format!("CRITICAL {crit}   HIGH {high}   MEDIUM {med}   LOW {low}")
    }
}

/// `image scan --update` — syncs an OSV (or native) feed to
/// `<root>/advisories.json`, merging with what already exists (never loses entries).
pub fn cmd_scan_update(feed: Option<String>) -> Result<()> {
    use std::collections::BTreeMap;
    let (images, _store) = open_stores()?;
    let source = feed
        .or_else(|| std::env::var("DELONIX_ADVISORY_FEED").ok())
        .ok_or_else(|| {
            Error::Invalid(
                "indica a fonte: --feed <url|ficheiro> (ou $DELONIX_ADVISORY_FEED)".into(),
            )
        })?;
    eprintln!("a sincronizar o feed de CVE de {source}…");
    let raw = if source.starts_with("http://") || source.starts_with("https://") {
        delonix_image::http_get(&source)?
    } else {
        let path = source.strip_prefix("file://").unwrap_or(&source);
        std::fs::read(path)?
    };
    let text = String::from_utf8_lossy(&raw);
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| Error::Invalid(format!("feed inválido: {e}")))?;
    // OSV: `{"vulns":…}` object or an array whose 1st element has `affected`.
    let is_osv = value.get("vulns").is_some()
        || value
            .as_array()
            .and_then(|a| a.first())
            .map(|e| e.get("affected").is_some())
            .unwrap_or(false);
    let incoming: Vec<serde_json::Value> = if is_osv {
        let advs = delonix_scan::advisories_from_osv(&text)?;
        eprintln!(
            "→ feed OSV detectado: {} advisories convertidas (Alpine/Debian/Ubuntu)",
            advs.len()
        );
        advs.iter()
            .filter_map(|a| serde_json::to_value(a).ok())
            .collect()
    } else {
        serde_json::from_value(value)
            .map_err(|e| Error::Invalid(format!("feed nativo inválido: {e}")))?
    };

    let dst = images.root().join("advisories.json");
    let mut by_id: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    // Starts from the embedded + whatever already exists (to never lose advisories).
    for src in [
        EMBEDDED_ADVISORIES.to_string(),
        std::fs::read_to_string(&dst).unwrap_or_default(),
    ] {
        if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&src) {
            for a in arr {
                if let Some(id) = a.get("id").and_then(|v| v.as_str()) {
                    by_id.insert(id.to_string(), a);
                }
            }
        }
    }
    let mut added = 0usize;
    for a in incoming {
        if let Some(id) = a.get("id").and_then(|v| v.as_str()).map(String::from) {
            if !by_id.contains_key(&id) {
                added += 1;
            }
            by_id.insert(id, a);
        }
    }
    let merged: Vec<serde_json::Value> = by_id.into_values().collect();
    let json = serde_json::to_string_pretty(&merged)?;
    AdvisoryDb::load(&json)?; // validates the schema before writing
    std::fs::write(&dst, &json)?;
    let meta = serde_json::json!({ "source": source, "synced_unix": now_unix(), "count": merged.len(), "format": if is_osv { "osv" } else { "native" } });
    let _ = std::fs::write(
        images.root().join("advisories.meta.json"),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    );
    println!(
        "base de advisories sincronizada: {} entradas ({added} novas) de {source}",
        merged.len()
    );
    Ok(())
}

/// Does the admission policy reject? `worst >= threshold`.
pub fn admission_rejects(worst: Option<Severity>, policy: &str) -> bool {
    match Severity::parse(policy) {
        Some(th) => worst.map(|w| w >= th).unwrap_or(false),
        None => false,
    }
}

/// **CVE admission policy on pull** (supply-chain). Controlled by
/// `DELONIX_SCAN_ON_PULL`: unset/empty = off (no latency); `warn` = scan +
/// report; `low|medium|high|critical` = fail-closed GATE — removes the image and
/// refuses if there is a vulnerability >= that severity.
///
/// It is the enforcement mechanism the supply-chain audit flagged as missing:
/// without this, a `pull` accepts any image without looking at what it brings inside.
pub fn admission_scan_on_pull(images: &ImageStore, reference: &str, img: &Image) -> Result<()> {
    let policy = match std::env::var("DELONIX_SCAN_ON_PULL") {
        Ok(p) => p.trim().to_lowercase(),
        Err(_) => return Ok(()),
    };
    if policy.is_empty() {
        return Ok(());
    }
    eprintln!(
        "→ política de admissão: scan de CVE de '{reference}' (DELONIX_SCAN_ON_PULL={policy})…"
    );
    let worst = match scan_image(images, img) {
        Ok(w) => w,
        // No SBOM (scratch/distroless) or scan unavailable → don't block, warn.
        Err(e) => {
            output::warn(&format!(
                "scan de admissão indisponível ({e}); pull permitido."
            ));
            return Ok(());
        }
    };
    if admission_rejects(worst, &policy) {
        let _ = images.remove(reference); // undoes the pull (fail-closed)
        return Err(Error::Invalid(format!(
            "imagem '{reference}' RECUSADA pela política de admissão: vulnerabilidade >= {policy} \
             (DELONIX_SCAN_ON_PULL). Imagem removida. Corrige a imagem ou ajusta a política."
        )));
    }
    if policy != "warn" && Severity::parse(&policy).is_none() {
        output::warn(&format!("DELONIX_SCAN_ON_PULL='{policy}' inválido (warn|low|medium|high|critical); só reportado."));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admissao_rejeita_por_severidade() {
        // Gate `high`: rejects critical/high, accepts medium/low/nothing.
        assert!(admission_rejects(Some(Severity::Critical), "high"));
        assert!(admission_rejects(Some(Severity::High), "high"));
        assert!(!admission_rejects(Some(Severity::Medium), "high"));
        assert!(!admission_rejects(Some(Severity::Low), "high"));
        assert!(!admission_rejects(None, "high"));
        // `warn` is not a severity → never rejects (only reports).
        assert!(!admission_rejects(Some(Severity::Critical), "warn"));
    }

    #[test]
    fn base_embebida_parseia() {
        // If the embedded placeholder doesn't parse, the whole scan fails silently.
        assert!(AdvisoryDb::load(EMBEDDED_ADVISORIES).is_ok());
    }
}
