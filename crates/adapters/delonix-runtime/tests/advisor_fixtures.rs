//! Golden fixtures for the advisor evaluation harness (`scripts/advisor_eval.py`).
//!
//! Each fixture is one host, split in two halves: `input` — the raw numbers a
//! model is shown — and `truth` — what this engine deterministically concludes
//! from them. The harness shows a model only the first half and scores it
//! against the second.
//!
//! **The truth is computed here, by the real rules, and never written by hand.**
//! That is the whole point: a hand-written expectation is a second
//! implementation of the rules, and the day the rules change it becomes a lie
//! that a passing test defends. This test regenerates the goldens on demand and
//! otherwise ASSERTS they still match, so changing a threshold in
//! `resource_advice` without regenerating fails here rather than silently
//! grading models against yesterday's answers.
//!
//!   regenerate:  DELONIX_UPDATE_FIXTURES=1 cargo test -p delonix-runtime --test advisor_fixtures
//!
//! The fixtures are synthetic on purpose. Real captures from one machine
//! cluster in one corner of the space — this host would only ever produce
//! "rootless, cpu memory pids, idle" — and a benchmark that only contains the
//! easy case measures nothing. Real captures are still welcome: the harness
//! reads any file of the same shape, so `delonix system resources -o json`
//! output can be dropped in beside these.

use delonix_runtime::resource_advice::{advise, local_inference, GpuFacts, ResourceSnapshot};
use delonix_runtime::{bottleneck, Psi};

const GIB: u64 = 1024 * 1024 * 1024;

fn psi(a10: f64, a60: f64, a300: f64) -> Option<Psi> {
    Some(Psi {
        avg10: a10,
        avg60: a60,
        avg300: a300,
    })
}

fn delegated(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// The decision space, one host per corner of it. Names are the fixture file
/// names, and they say what the host IS, not what the answer should be.
fn hosts() -> Vec<(&'static str, ResourceSnapshot)> {
    let full = delegated(&["cpu", "cpuset", "io", "memory", "pids"]);
    let rootless_typical = delegated(&["cpu", "memory", "pids"]);

    // A healthy, fully delegated root node. Nothing to say about it, and a
    // model that invents a finding here is worse than one that says nothing.
    let healthy = ResourceSnapshot {
        rootless: false,
        cgroup_base: Some("/sys/fs/cgroup/delonix.slice".into()),
        delegated: full.clone(),
        cpus: 16,
        mem_total: 64 * GIB,
        mem_available: 48 * GIB,
        swap_total: 8 * GIB,
        swap_used: 0,
        disk_free: 800 * GIB,
        cpu_temp_c: Some(45),
        psi_cpu: psi(0.1, 0.2, 0.3),
        psi_memory: psi(0.0, 0.0, 0.0),
        psi_io: psi(0.4, 0.5, 0.6),
        aggregate_slice: true,
        gpu: None,
    };

    vec![
        ("healthy-root-node", healthy.clone()),
        (
            // The machine this was written on.
            "rootless-laptop-idle",
            ResourceSnapshot {
                rootless: true,
                cgroup_base: Some(
                    "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/dlx-containers"
                        .into(),
                ),
                delegated: rootless_typical.clone(),
                cpus: 32,
                mem_total: 30 * GIB,
                mem_available: 17 * GIB,
                swap_total: 32 * GIB,
                swap_used: 10 * GIB,
                disk_free: 342 * GIB,
                cpu_temp_c: Some(54),
                psi_cpu: psi(0.0, 0.0, 0.0),
                psi_memory: psi(0.0, 0.0, 0.2),
                psi_io: psi(1.4, 0.7, 1.9),
                aggregate_slice: false,
                gpu: Some(GpuFacts {
                    name: Some("NVIDIA GeForce RTX 5060 Laptop GPU".into()),
                    vram_total_mib: 8151,
                    vram_free_mib: 7581,
                    cdi_spec: true,
                    drives_display: true,
                }),
            },
        ),
        (
            // Chronic I/O, the reading a slow disk under a database gives.
            "io-bound-chronic",
            ResourceSnapshot {
                psi_io: psi(31.0, 29.0, 27.0),
                ..healthy.clone()
            },
        ),
        (
            // The trap: the instant looks terrible, the five minutes do not.
            // A model that calls this chronic has failed the interesting case.
            "io-spike-transient",
            ResourceSnapshot {
                psi_io: psi(45.0, 6.0, 1.5),
                ..healthy.clone()
            },
        ),
        (
            // Two resources over the line at once; the CPU is worse.
            "cpu-and-io-contended",
            ResourceSnapshot {
                psi_cpu: psi(70.0, 55.0, 40.0),
                psi_io: psi(20.0, 18.0, 15.0),
                ..healthy.clone()
            },
        ),
        (
            // CPU chronic and nothing else.
            "cpu-bound-chronic",
            ResourceSnapshot {
                psi_cpu: psi(38.0, 34.0, 30.0),
                ..healthy.clone()
            },
        ),
        (
            // The instant is bad, the five minutes are not.
            "cpu-spike-transient",
            ResourceSnapshot {
                psi_cpu: psi(52.0, 8.0, 2.0),
                ..healthy.clone()
            },
        ),
        (
            // Memory chronic WITHOUT swap: 004 applies, 006 does not. The pair
            // that catches a model reciting "memory pressure ⇒ thrashing".
            "memory-chronic-no-swap",
            ResourceSnapshot {
                swap_total: 0,
                swap_used: 0,
                mem_available: 2 * GIB,
                psi_memory: psi(28.0, 26.0, 24.0),
                ..healthy.clone()
            },
        ),
        (
            // All three over the line; memory is worst on avg10.
            "everything-contended",
            ResourceSnapshot {
                psi_cpu: psi(35.0, 33.0, 31.0),
                psi_memory: psi(66.0, 60.0, 55.0),
                psi_io: psi(48.0, 44.0, 40.0),
                swap_used: 5 * GIB,
                mem_available: GIB,
                ..healthy.clone()
            },
        ),
        (
            // Swapping AND stalling: thrashing.
            "memory-thrashing",
            ResourceSnapshot {
                mem_available: GIB,
                swap_used: 6 * GIB,
                psi_memory: psi(40.0, 35.0, 30.0),
                ..healthy.clone()
            },
        ),
        (
            // The other trap: 10 GiB swapped out on an idle host is HEALTH,
            // not a problem. Cold pages evicted are what swap is for.
            "swap-used-but-calm",
            ResourceSnapshot {
                swap_total: 32 * GIB,
                swap_used: 10 * GIB,
                psi_memory: psi(0.0, 0.0, 0.1),
                ..healthy.clone()
            },
        ),
        (
            "disk-nearly-full",
            ResourceSnapshot {
                disk_free: 3 * GIB,
                ..healthy.clone()
            },
        ),
        (
            // Nothing delegated: every ceiling flag is a lie on this host.
            "no-delegation-at-all",
            ResourceSnapshot {
                rootless: true,
                delegated: Vec::new(),
                aggregate_slice: false,
                ..healthy.clone()
            },
        ),
        (
            // A headless server card with room to spare.
            "headless-gpu-server",
            ResourceSnapshot {
                gpu: Some(GpuFacts {
                    name: Some("NVIDIA L4".into()),
                    vram_total_mib: 24564,
                    vram_free_mib: 24000,
                    cdi_spec: true,
                    drives_display: false,
                }),
                ..healthy.clone()
            },
        ),
        (
            // Driver present, spec never generated: the container cannot reach
            // the card at all, however big it is.
            "gpu-without-cdi-spec",
            ResourceSnapshot {
                gpu: Some(GpuFacts {
                    name: Some("NVIDIA L4".into()),
                    vram_total_mib: 24564,
                    vram_free_mib: 24000,
                    cdi_spec: false,
                    drives_display: false,
                }),
                ..healthy.clone()
            },
        ),
        (
            // A small card: fits a 4B, and a 4B fills a template without being
            // able to weigh a trade-off.
            "gpu-too-small-for-an-advisor",
            ResourceSnapshot {
                gpu: Some(GpuFacts {
                    name: Some("NVIDIA T400 4GB".into()),
                    vram_total_mib: 4096,
                    vram_free_mib: 3900,
                    cdi_spec: true,
                    drives_display: false,
                }),
                ..healthy.clone()
            },
        ),
        (
            // A kernel without CONFIG_PSI: no pressure signal at all. The right
            // answer is "no bottleneck", never a guessed one.
            "kernel-without-psi",
            ResourceSnapshot {
                psi_cpu: None,
                psi_memory: None,
                psi_io: None,
                ..healthy
            },
        ),
    ]
}

fn to_json(name: &str, s: &ResourceSnapshot) -> serde_json::Value {
    let pressure = [
        ("cpu", s.psi_cpu),
        ("memory", s.psi_memory),
        ("io", s.psi_io),
    ];
    let findings = advise(s);
    let inference = local_inference(s);
    serde_json::json!({
        "name": name,
        // What a model is shown. Nothing here hints at the answer.
        "input": {
            "rootless": s.rootless,
            "cgroup_base": s.cgroup_base,
            "delegated_controllers": s.delegated,
            "aggregate_slice": s.aggregate_slice,
            "cpus": s.cpus,
            "memory_bytes": s.mem_total,
            "memory_available_bytes": s.mem_available,
            "swap_bytes": s.swap_total,
            "swap_used_bytes": s.swap_used,
            "state_root_free_bytes": s.disk_free,
            "cpu_temperature_c": s.cpu_temp_c,
            "pressure": pressure.iter().map(|(r, p)| serde_json::json!({
                "resource": r,
                "avg10": p.map(|p| p.avg10),
                "avg60": p.map(|p| p.avg60),
                "avg300": p.map(|p| p.avg300),
            })).collect::<Vec<_>>(),
            "gpu": s.gpu.as_ref().map(|g| serde_json::json!({
                "name": g.name,
                "vram_total_mib": g.vram_total_mib,
                "vram_free_mib": g.vram_free_mib,
                "cdi_spec": g.cdi_spec,
                "drives_display": g.drives_display,
            })),
        },
        // What the engine concludes. Computed, never written by hand.
        "truth": {
            "bottleneck": bottleneck(&pressure),
            // `id` alone repeats when two resources earn the same rule, so the
            // scorable key is the PAIR.
            "findings": findings.iter()
                .map(|f| format!("{}:{}", f.id, f.subject))
                .collect::<Vec<_>>(),
            "gating_findings": findings.iter()
                .filter(|f| f.class.gates())
                .map(|f| format!("{}:{}", f.id, f.subject))
                .collect::<Vec<_>>(),
            "local_inference_verdict": inference.verdict.as_str(),
            "largest_model_b_q4": inference.largest_model_b,
        },
    })
}

fn dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/advisor")
}

#[test]
fn goldens_match_the_rules_as_they_are_today() {
    let update = std::env::var_os("DELONIX_UPDATE_FIXTURES").is_some();
    let dir = dir();
    if update {
        std::fs::create_dir_all(&dir).expect("fixture dir");
    }
    let mut missing = Vec::new();
    let mut stale = Vec::new();

    for (name, snap) in hosts() {
        let want = format!(
            "{}\n",
            serde_json::to_string_pretty(&to_json(name, &snap)).unwrap()
        );
        let path = dir.join(format!("{name}.json"));
        if update {
            std::fs::write(&path, &want).expect("write fixture");
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(have) if have == want => {}
            Ok(_) => stale.push(name),
            Err(_) => missing.push(name),
        }
    }
    assert!(
        missing.is_empty() && stale.is_empty(),
        "the goldens no longer match the rules — missing: {missing:?}, stale: {stale:?}\n\
         Regenerate them in the SAME commit as the rule change:\n  \
         DELONIX_UPDATE_FIXTURES=1 cargo test -p delonix-runtime --test advisor_fixtures"
    );
}

/// The corpus is only worth running a model against if it actually disagrees
/// with itself. A dozen hosts that all answer "no bottleneck, no findings"
/// would score every model at 100% and rank nothing.
#[test]
fn the_corpus_covers_more_than_one_answer() {
    let mut bottlenecks = std::collections::BTreeSet::new();
    let mut verdicts = std::collections::BTreeSet::new();
    let mut ids = std::collections::BTreeSet::new();
    let mut clean = 0;

    for (_, s) in hosts() {
        let pressure = [
            ("cpu", s.psi_cpu),
            ("memory", s.psi_memory),
            ("io", s.psi_io),
        ];
        bottlenecks.insert(format!("{:?}", bottleneck(&pressure)));
        verdicts.insert(local_inference(&s).verdict.as_str());
        let f = advise(&s);
        if f.is_empty() {
            clean += 1;
        }
        ids.extend(f.iter().map(|a| format!("{}:{}", a.id, a.subject)));
    }

    assert!(
        bottlenecks.len() >= 3,
        "só {} respostas de gargalo distintas: {bottlenecks:?}",
        bottlenecks.len()
    );
    assert_eq!(verdicts.len(), 3, "as três aptidões têm de aparecer");
    assert!(
        ids.len() >= 5,
        "só {} achados distintos: {ids:?}",
        ids.len()
    );
    assert!(
        clean >= 1,
        "sem um anfitrião saudável, nada mede a invenção de achados"
    );
}

/// A benchmark has to be able to say no.
///
/// The trivial answer — `{"bottleneck": null, "findings": []}` — is what a model
/// that understood nothing produces, and the first version of this corpus gave
/// it 69% on the bottleneck: nine of thirteen hosts were quiet. A ranking where
/// silence scores 69% cannot separate a good model from a mute one. This is the
/// floor the corpus must keep the trivial answer under, and it fails when
/// somebody adds five more quiet hosts without adding contended ones.
#[test]
fn the_trivial_answer_scores_badly() {
    let all = hosts();
    let n = all.len() as f64;
    let mut null_bottleneck = 0.0;
    let mut empty_findings = 0.0;

    for (_, s) in &all {
        let pressure = [
            ("cpu", s.psi_cpu),
            ("memory", s.psi_memory),
            ("io", s.psi_io),
        ];
        if bottleneck(&pressure).is_none() {
            null_bottleneck += 1.0;
        }
        if advise(s).is_empty() {
            empty_findings += 1.0;
        }
    }

    let bottleneck_score = 100.0 * null_bottleneck / n;
    let findings_score = 100.0 * empty_findings / n;
    assert!(
        bottleneck_score <= 60.0,
        "a resposta trivial acerta {bottleneck_score:.0}% dos gargalos — faltam \
         anfitriões em disputa no corpus"
    );
    assert!(
        findings_score <= 60.0,
        "a resposta trivial acerta {findings_score:.0}% dos achados — faltam \
         anfitriões com problemas no corpus"
    );
}
