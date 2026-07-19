//! `delonix image scan` — SBOM + scan de CVE, e a política de admissão no pull.
//!
//! O motor (`delonix-scan`) faz o trabalho: extrai o SBOM lendo os layers do CAS
//! (apk/dpkg, sem montar nem correr) e cruza-o com uma base de advisories OSV.
//! Aqui liga-se isso à CLI e aos pontos de decisão (scan-on-pull).
//!
//! **Proveniência honesta**: a base EMBEBIDA é um placeholder de 5 entradas — um
//! "sem vulnerabilidades" contra ela NÃO é um atestado de saúde, e o output
//! di-lo explicitamente. Só um feed OSV sincronizado (`scan --update`) dá uma
//! resposta de confiança.

use delonix_image::{Image, ImageStore};
use delonix_runtime_core::{Error, Result};
use delonix_scan::{AdvisoryDb, Severity};

use super::output;
use super::util::{open_stores, resolve_or_pull, state_root};

/// A base placeholder embebida — 5 entradas, para o scan não rebentar sem um
/// feed sincronizado. NUNCA é apresentada como definitiva (ver `Provenance`).
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

/// Carrega a base de advisories: a sincronizada (`<root>/advisories.json`) tem
/// precedência; senão `$DELONIX_ADVISORIES`; senão a placeholder embebida.
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

/// `image scan <image>` — dashboard de vulnerabilidades. Puxa a imagem se faltar
/// (como o `docker scout`).
pub fn cmd_scan(image: &str, sbom: bool, fail_on: Option<&str>) -> Result<()> {
    let (images, _store) = open_stores()?;
    let img = match images.resolve(image) {
        Ok(img) => img,
        Err(Error::NotFound(_)) => {
            eprintln!("imagem '{image}' não está local — a puxar…");
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

/// Analisa uma imagem e imprime o dashboard. Devolve a pior severidade
/// encontrada (`None` = nenhuma). Reutilizável pelo scan-on-pull.
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

    // Proveniência HONESTA: sem isto, um "sem vulnerabilidades" contra a base
    // placeholder parecia um atestado de saúde — falsa garantia.
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

/// `image scan --update` — sincroniza um feed OSV (ou nativo) para
/// `<root>/advisories.json`, fundindo com o que já existe (nunca perde entradas).
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
    // OSV: objeto `{"vulns":…}` ou array cujo 1.º elemento tem `affected`.
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
    // Arranca do embebido + do que já existir (para nunca perder advisories).
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
    AdvisoryDb::load(&json)?; // valida o schema antes de gravar
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

/// A política de admissão rejeita? `worst >= threshold`.
pub fn admission_rejects(worst: Option<Severity>, policy: &str) -> bool {
    match Severity::parse(policy) {
        Some(th) => worst.map(|w| w >= th).unwrap_or(false),
        None => false,
    }
}

/// **Política de admissão de CVE no pull** (supply-chain). Controlada por
/// `DELONIX_SCAN_ON_PULL`: unset/vazio = off (sem latência); `warn` = scan +
/// reporta; `low|medium|high|critical` = GATE fail-closed — remove a imagem e
/// recusa se houver uma vulnerabilidade >= essa severidade.
///
/// É o mecanismo de enforcement que a auditoria supply-chain apontava em falta:
/// sem isto, um `pull` aceita qualquer imagem sem olhar para o que traz dentro.
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
        // Sem SBOM (scratch/distroless) ou scan indisponível → não bloquear, avisar.
        Err(e) => {
            output::warn(&format!(
                "scan de admissão indisponível ({e}); pull permitido."
            ));
            return Ok(());
        }
    };
    if admission_rejects(worst, &policy) {
        let _ = images.remove(reference); // desfaz o pull (fail-closed)
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
        // Gate `high`: rejeita critical/high, aceita medium/low/nada.
        assert!(admission_rejects(Some(Severity::Critical), "high"));
        assert!(admission_rejects(Some(Severity::High), "high"));
        assert!(!admission_rejects(Some(Severity::Medium), "high"));
        assert!(!admission_rejects(Some(Severity::Low), "high"));
        assert!(!admission_rejects(None, "high"));
        // `warn` não é uma severidade → nunca rejeita (só reporta).
        assert!(!admission_rejects(Some(Severity::Critical), "warn"));
    }

    #[test]
    fn base_embebida_parseia() {
        // Se o placeholder embebido não parsear, todo o scan falha em silêncio.
        assert!(AdvisoryDb::load(EMBEDDED_ADVISORIES).is_ok());
    }
}
