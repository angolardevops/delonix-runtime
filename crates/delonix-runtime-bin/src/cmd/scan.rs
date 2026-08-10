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
                    .unwrap_or(super::po::t("unknown"))
                    .to_string();
                (
                    super::po::tf("synced from {src}", &[("src", &src)]),
                    m.get("synced_unix").and_then(|v| v.as_u64()),
                )
            })
            .unwrap_or_else(|| (super::po::t("synced").into(), None));
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
            label: super::po::t("EMBEDDED database (placeholder)").into(),
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
            // A VM image is NOT a container image, and the difference has to be said before
            // the pull. Measured: `image scan delonix-vm-base:ubuntu-24.04` — an image that
            // IS on this node — announced "not local", went to Docker Hub for
            // `library/delonix-vm-base` and died on `HTTP 401 Unauthorized`. The user asked
            // to scan something they have and got an auth error from a public registry.
            //
            // Refused rather than implemented: scanning a qcow2 means walking the GUEST
            // filesystem (libguestfs), which is a different SBOM path entirely — and a scan
            // that silently does nothing useful is the failure this command exists to avoid.
            if super::vmimage::VmImageStore::open(state_root())
                .ok()
                .and_then(|st| st.get(image).ok())
                .is_some()
            {
                return Err(Error::Invalid(super::po::tf(
                    "'{img}' is a VM image, not a container image — scanning a qcow2 guest \
                     filesystem is not implemented. Scan the container images it runs, or \
                     inspect the guest yourself (`virt-filesystems`/`guestfish`).",
                    &[("img", image)],
                )));
            }
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
        println!(
            "{}",
            super::po::tf(
                "SBOM of {img} — {n} package(s):",
                &[("img", &img.short_id()), ("n", &pkgs.len().to_string())],
            )
        );
        t.print();
        return Ok(());
    }
    let worst = scan_image(&images, &img)?;
    if let Some(threshold) = fail_on {
        let th = Severity::parse(threshold).ok_or_else(|| {
            Error::Invalid(super::po::tf(
                "invalid severity: {threshold} (low|medium|high|critical)",
                &[("threshold", threshold)],
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
        output::dim(&super::po::tf(
            "SBOM: {n} package(s)",
            &[("n", &sbom.len().to_string())],
        )),
        output::dim(&super::po::tf(
            "advisories: {n}",
            &[("n", &db.len().to_string())],
        )),
        output::dim(&super::po::tf(
            "vulnerabilities: {n}",
            &[("n", &findings.len().to_string())],
        )),
    );
    println!("  {}", sev_line(crit, high, med, low));

    // HONEST provenance: without this, a "no vulnerabilities" against the placeholder
    // database looked like a clean bill of health — a false guarantee.
    let stale = delonix_scan::db_is_stale(prov.synced_unix, now_unix(), 14);
    println!(
        "  {}",
        output::dim(&super::po::tf(
            "database source: {label} ({n} advisories)",
            &[("label", &prov.label), ("n", &db.len().to_string())],
        ))
    );
    if prov.placeholder {
        output::warn(&super::po::tf(
            "EMBEDDED CVE database (placeholder, {n} entries) — NOT a real feed; a \"no vulnerabilities\" is not to be trusted. \
             Sync: `delonix image scan --update --feed https://…/osv.json`",
            &[("n", &db.len().to_string())],
        ));
    } else if stale {
        output::warn(super::po::t(
            "stale advisories database (>14 days without sync) — run `delonix image scan --update`.",
        ));
    }

    if findings.is_empty() {
        if prov.placeholder {
            println!(
                "  {}",
                output::dim(super::po::t(
                    "no matches in the placeholder database (not conclusive)"
                ))
            );
        } else {
            println!("  ✔ {}", super::po::t("no known vulnerabilities"));
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
                super::po::t("indicate the source: --feed <url|file> (or $DELONIX_ADVISORY_FEED)")
                    .into(),
            )
        })?;
    eprintln!(
        "{}",
        super::po::tf(
            "syncing the CVE feed from {source}…",
            &[("source", &source)]
        )
    );
    let raw = if source.starts_with("http://") || source.starts_with("https://") {
        delonix_image::http_get(&source)?
    } else {
        let path = source.strip_prefix("file://").unwrap_or(&source);
        std::fs::read(path)?
    };
    let text = String::from_utf8_lossy(&raw);
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| Error::Invalid(format!("{}: {e}", super::po::t("invalid feed"))))?;
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
            "{}",
            super::po::tf(
                "→ OSV feed detected: {n} advisories converted (Alpine/Debian/Ubuntu)",
                &[("n", &advs.len().to_string())],
            )
        );
        advs.iter()
            .filter_map(|a| serde_json::to_value(a).ok())
            .collect()
    } else {
        serde_json::from_value(value)
            .map_err(|e| Error::Invalid(format!("{}: {e}", super::po::t("invalid native feed"))))?
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
        "{}",
        super::po::tf(
            "advisories database synced: {n} entries ({added} new) from {source}",
            &[
                ("n", &merged.len().to_string()),
                ("added", &added.to_string()),
                ("source", &source),
            ],
        )
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

/// `true` for the only two shapes `DELONIX_SCAN_ON_PULL` accepts: `warn`, or
/// a real severity threshold. Factored out so the fail-closed validation in
/// `admission_scan_on_pull` is unit-testable without an `ImageStore`.
fn valid_admission_policy(policy: &str) -> bool {
    policy == "warn" || Severity::parse(policy).is_some()
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
    // BUG FOUND: an unrecognized policy value (a typo — "criticl" for
    // "critical") used to fall through `admission_rejects` (Severity::parse
    // fails → `false`, meaning "does not reject") and only get flagged with
    // a stderr warning AFTER the pull had already succeeded — a
    // misconfigured GATE silently downgraded to advisory-only. This is
    // documented as a "fail-closed GATE"; validate the policy string BEFORE
    // scanning anything, so a bad value refuses instead of admitting.
    if !valid_admission_policy(&policy) {
        return Err(Error::Invalid(super::po::tf(
            "DELONIX_SCAN_ON_PULL='{policy}' invalid (warn|low|medium|high|critical) — \
             refused (the policy is a fail-closed gate; an unknown value is not treated \
             as 'no policy')",
            &[("policy", &policy)],
        )));
    }
    eprintln!(
        "{}",
        super::po::tf(
            "→ admission policy: CVE scan of '{reference}' (DELONIX_SCAN_ON_PULL={policy})…",
            &[("reference", reference), ("policy", &policy)],
        )
    );
    let worst = match scan_image(images, img) {
        Ok(w) => w,
        // No SBOM (scratch/distroless) or scan unavailable → don't block, warn.
        Err(e) => {
            output::warn(&super::po::tf(
                "admission scan unavailable ({e}); pull allowed.",
                &[("e", &e.to_string())],
            ));
            return Ok(());
        }
    };
    if admission_rejects(worst, &policy) {
        let _ = images.remove(reference); // undoes the pull (fail-closed)
        return Err(Error::Invalid(super::po::tf(
            "image '{reference}' REJECTED by the admission policy: vulnerability >= {policy} \
             (DELONIX_SCAN_ON_PULL). Image removed. Fix the image or adjust the policy.",
            &[("reference", reference), ("policy", &policy)],
        )));
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
    fn valid_admission_policy_recusa_valores_desconhecidos() {
        // BUG regression guard: a typo like "criticl" (for "critical") used
        // to fall through Severity::parse → admission_rejects returns
        // false → the pull was silently ADMITTED, only warned about after
        // the fact — a misconfigured fail-closed gate degrading to
        // advisory-only. Validating up front turns that into a hard error.
        assert!(valid_admission_policy("warn"));
        assert!(valid_admission_policy("low"));
        assert!(valid_admission_policy("critical"));
        assert!(valid_admission_policy("crit")); // Severity::parse's own alias
        assert!(!valid_admission_policy("criticl")); // the actual typo this bug was found from
        assert!(!valid_admission_policy(""));
        assert!(valid_admission_policy("Critical")); // Severity::parse lowercases internally
    }

    #[test]
    fn base_embebida_parseia() {
        // If the embedded placeholder doesn't parse, the whole scan fails silently.
        assert!(AdvisoryDb::load(EMBEDDED_ADVISORIES).is_ok());
    }
}
