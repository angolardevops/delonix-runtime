//! `delonix migrate assess` — what a compose file you already have would cost
//! to move here, key by key, BEFORE anything runs.
//!
//! # Why this is not `compose config`
//!
//! `delonix compose up` is deliberately fail-closed: the first key it does not
//! understand is refused rather than ignored, because "accepted and ignored" is
//! the failure this engine refuses by policy. That is right for running, and
//! wrong for DECIDING: a migration needs the whole list at once, not the first
//! obstacle followed by another edit-and-retry cycle for each of the others.
//!
//! So this verb reads the same file and answers all of it: for every key every
//! service uses, whether this engine serves it, refuses it with a written
//! reason, or does not implement it — and, when the capability exists under
//! another name, which command to use instead.
//!
//! # It reads the RAW YAML, on purpose
//!
//! The typed parser would refuse the file at the first unknown key, which is
//! precisely the set this command exists to enumerate. So the assessment walks
//! the raw mapping. That also means it never runs, creates, or pulls anything:
//! the answer costs one file read.

use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::Subcommand;
use delonix_model::{Error, Result};
use serde::Serialize;

use super::compose::{service_key_state, top_key_state, KeyState};
use super::output::{OutputFormat, Table};

#[derive(Subcommand)]
pub enum MigrateCmd {
    /// Score a `docker-compose.yml` against what this engine serves.
    ///
    /// Reads the file and reports every key it uses: served, refused with a
    /// reason, or missing (with the engine's equivalent command where there is
    /// one). Nothing is created, pulled or run.
    ///
    /// Only compose files today, and that is a measurement and not an
    /// oversight: the Compose Specification's key list was measured against the
    /// client that implements it, so the third state has a real denominator. A
    /// `Dockerfile` has no such matrix here, and a score with an invented
    /// denominator is the claim this engine refuses to make.
    Assess {
        /// Compose file (default: `./compose.yaml`, `./compose.yml`,
        /// `./docker-compose.yaml` or `./docker-compose.yml`, in that order).
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
        /// Output format: `table` (default) or `json` (ADR-0005).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: OutputFormat,
        /// Exit 2 when something this engine does not serve is used (0 = it all
        /// maps, 1 = error) — same contract as `stack plan --detailed-exitcode`,
        /// so a migration can be a CI gate instead of a reading exercise.
        #[arg(long)]
        detailed_exitcode: bool,
    },
}

/// One key, as USED by a concrete file.
#[derive(Serialize)]
pub(crate) struct Finding {
    /// `top-level` or the service's name — never invented: it is what the file says.
    pub scope: String,
    pub key: String,
    pub state: String,
    pub note: String,
}

#[derive(Serialize, Default)]
pub(crate) struct Assessment {
    pub file: String,
    pub served: usize,
    pub refused: usize,
    pub missing: usize,
    pub findings: Vec<Finding>,
}

fn cell(state: &KeyState) -> (&'static str, String) {
    match state {
        KeyState::Served => ("served", String::new()),
        KeyState::Refused { why, elsewhere } => (
            "refused",
            match elsewhere {
                // The `why` itself stays verbatim: those sentences are the
                // engine's own refusal text, and `compatibility docker` does
                // not translate its reasons either. What IS translated is the
                // frame around it, which is this command's own words.
                Some(flag) => super::po::tf(
                    "{why} — this engine has it as `delonix {flag}`",
                    &[("why", &super::po::t_dyn(why)), ("flag", flag)],
                ),
                None => (*why).to_string(),
            },
        ),
        KeyState::Missing => ("missing", String::new()),
    }
}

/// Pure: the whole assessment of one already-parsed document.
///
/// Takes the raw YAML because the typed parser refuses at the first unknown
/// key — see the module doc.
fn assess_doc(doc: &serde_yaml::Value, file: &str) -> Assessment {
    let mut a = Assessment {
        file: file.to_string(),
        ..Default::default()
    };
    let serde_yaml::Value::Mapping(top) = doc else {
        return a;
    };

    let push = |scope: &str, key: &str, state: KeyState, a: &mut Assessment| {
        let (st, note) = cell(&state);
        match state {
            KeyState::Served => a.served += 1,
            KeyState::Refused { .. } => a.refused += 1,
            KeyState::Missing => a.missing += 1,
        }
        a.findings.push(Finding {
            scope: scope.to_string(),
            key: key.to_string(),
            state: st.to_string(),
            note,
        });
    };

    for (k, _) in top {
        let Some(key) = k.as_str() else { continue };
        // `x-` extensions are the specification's own escape hatch and mean
        // nothing to any implementation. Scoring them would inflate the
        // "missing" count with keys the author already knows nobody reads.
        if key.starts_with("x-") {
            continue;
        }
        push("top-level", key, top_key_state(key), &mut a);
    }

    if let Some(serde_yaml::Value::Mapping(services)) = top.get("services") {
        // Sorted by service name so two runs of the same file read the same;
        // a YAML mapping's order is the file's, which is fine, but a diff of
        // two assessments is what a migration actually does with this.
        let mut by_name: BTreeMap<&str, &serde_yaml::Value> = BTreeMap::new();
        for (name, body) in services {
            if let Some(n) = name.as_str() {
                by_name.insert(n, body);
            }
        }
        for (name, body) in by_name {
            let serde_yaml::Value::Mapping(svc) = body else {
                continue;
            };
            for (k, _) in svc {
                let Some(key) = k.as_str() else { continue };
                if key.starts_with("x-") {
                    continue;
                }
                push(name, key, service_key_state(key), &mut a);
            }
        }
    }
    a
}

const CANDIDATES: &[&str] = &[
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

fn resolve(file: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(f) = file {
        return if f.is_file() {
            Ok(f)
        } else {
            Err(Error::NotFound(format!("compose file: {}", f.display())))
        };
    }
    CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .ok_or_else(|| {
            Error::Invalid(format!(
                "no compose file here — looked for {}; pass -f",
                CANDIDATES.join(", ")
            ))
        })
}

pub fn run(cmd: MigrateCmd) -> Result<()> {
    match cmd {
        MigrateCmd::Assess {
            file,
            output,
            detailed_exitcode,
        } => {
            let path = resolve(file)?;
            let text = std::fs::read_to_string(&path)?;
            let doc: serde_yaml::Value = serde_yaml::from_str(&text)
                .map_err(|e| Error::Invalid(format!("parsing compose file: {e}")))?;
            let a = assess_doc(&doc, &path.display().to_string());

            match output {
                OutputFormat::Json => {
                    let s = serde_json::to_string_pretty(&a)
                        .map_err(|e| Error::Invalid(format!("json output: {e}")))?;
                    println!("{s}");
                }
                OutputFormat::Table => print_assessment(&a),
            }

            if detailed_exitcode && (a.missing > 0 || a.refused > 0) {
                std::process::exit(2);
            }
            Ok(())
        }
    }
}

fn print_assessment(a: &Assessment) {
    println!(
        "{}",
        super::po::tf(
            "{file}: {served} key use(s) served, {refused} refused with a reason, {missing} not implemented.",
            &[
                ("file", &a.file),
                ("served", &a.served.to_string()),
                ("refused", &a.refused.to_string()),
                ("missing", &a.missing.to_string()),
            ],
        )
    );
    println!(
        "{}",
        super::po::t(
            "Counts are key USES, not distinct keys: a key three services set is three decisions."
        )
    );
    println!();
    let mut t = Table::new(&["SCOPE", "KEY", "STATE", "NOTE"]);
    for f in &a.findings {
        t.row(vec![
            f.scope.clone(),
            f.key.clone(),
            super::po::t_dyn(&f.state),
            f.note.clone(),
        ]);
    }
    t.print();
    if a.missing == 0 && a.refused == 0 {
        println!();
        println!(
            "{}",
            super::po::t("Everything this file uses is served — `delonix compose up -f <file>`.")
        );
    } else {
        println!();
        println!(
            "{}",
            super::po::t(
                "`delonix compose up` REFUSES a key it does not serve rather than ignoring it, \
so the rows above are what it would stop on — every one of them, not just the first."
            )
        );
    }
    println!(
        "{}",
        super::po::t("Full coverage table: `delonix compatibility compose`.")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assess(yaml: &str) -> Assessment {
        assess_doc(&serde_yaml::from_str(yaml).unwrap(), "test.yaml")
    }

    #[test]
    fn a_file_this_engine_fully_serves_scores_clean() {
        let a =
            assess("services:\n  web:\n    image: nginx\n    ports: ['80']\n    restart: always\n");
        assert_eq!((a.served, a.refused, a.missing), (4, 0, 0)); // 3 keys + `services`
    }

    #[test]
    fn a_key_the_engine_has_elsewhere_carries_that_command() {
        let a = assess("services:\n  web:\n    image: nginx\n    devices: ['/dev/null']\n");
        // `devices:` is REFUSED by `compose` today, with a dedicated message
        // naming the flag — so the assessment says refused, not missing, and
        // the note is the same command that error gives. The two are read one
        // command apart and must agree.
        let f = a.findings.iter().find(|f| f.key == "devices").unwrap();
        assert_eq!(f.state, "refused");
        assert!(f.note.contains("container run --device"), "{}", f.note);
    }

    #[test]
    fn a_refused_key_carries_its_reason() {
        let a = assess("include: ['./other.yaml']\nservices:\n  web:\n    image: nginx\n");
        let f = a.findings.iter().find(|f| f.key == "include").unwrap();
        assert_eq!(f.state, "refused");
        assert!(f.note.contains("-f a -f b"), "{}", f.note);
    }

    #[test]
    fn the_same_key_in_three_services_counts_three_times() {
        // The header says "key USES, not distinct keys" — this fixes it, because
        // the number a migration cares about is how many decisions it has to
        // make, and three services with `devices:` are three.
        let a = assess(
            "services:\n  a:\n    shm_size: 1g\n  b:\n    shm_size: 1g\n  c:\n    shm_size: 1g\n",
        );
        assert_eq!(a.missing, 3);
    }

    #[test]
    fn an_x_extension_is_not_scored() {
        // `x-` is the specification's own escape hatch: no implementation reads
        // it, so counting it as missing would inflate the number with keys the
        // author already knows are inert.
        let a =
            assess("x-anchors:\n  foo: bar\nservices:\n  web:\n    image: nginx\n    x-mine: 1\n");
        assert!(a.findings.iter().all(|f| !f.key.starts_with("x-")));
        assert_eq!(a.missing, 0);
    }

    #[test]
    fn services_are_reported_in_a_stable_order() {
        let a = assess("services:\n  zeta:\n    image: a\n  alpha:\n    image: b\n");
        let scopes: Vec<&str> = a
            .findings
            .iter()
            .filter(|f| f.scope != "top-level")
            .map(|f| f.scope.as_str())
            .collect();
        assert_eq!(scopes, vec!["alpha", "zeta"]);
    }
}
