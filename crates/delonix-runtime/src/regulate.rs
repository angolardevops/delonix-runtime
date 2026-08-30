//! The deterministic half of resource management: read pressure, decide, write
//! one cgroup knob, and undo it when the pressure goes away.
//!
//! Deliberately NOT a model, and the reason is not taste. A control loop that
//! throttles workloads has to be fast, repeatable and auditable; a model is
//! none of the three, and — the part that settles it — a model consumes the
//! very CPU and memory it would be regulating, so under pressure the thing
//! meant to relieve the pressure becomes a cause of it. The advisory half
//! (`resource_advice`) is what an agent reads; this is what acts.
//!
//! **Only `cpu.weight`, on purpose.** It is a SHARE, not a cap: with no
//! contention a throttled container still gets the whole machine, so the worst
//! case of a wrong decision is that something runs at its fair share instead of
//! above it. Nothing is ever killed, paused or capped. `memory.high` would be
//! the obvious second knob and is left out on purpose: set too low it wedges a
//! database into permanent reclaim, which is a decision that deserves its own
//! design rather than a line in this one.
//!
//! **Cause, not victim.** High pressure INSIDE a container means that container
//! is being starved — it is the victim. The cause is whoever is consuming the
//! most. Throttling by "highest own stall" would punish exactly the wrong
//! workload, so the target is picked by share of consumption.

/// One workload as the regulator sees it. Everything here is measured, so the
/// decision below is a pure function and can be tested against machines nobody
/// has to own.
#[derive(Debug, Clone)]
pub struct Workload {
    pub id: String,
    pub name: String,
    /// Absolute path of its live cgroup leaf.
    pub cgroup: String,
    /// `cpu.weight` right now. 100 is the kernel default.
    pub cpu_weight: u64,
    /// Share of the CPU time consumed by ALL sampled workloads over the sample
    /// window, 0.0–100.0.
    pub cpu_share_pct: f64,
    /// The weight this workload had before the regulator touched it, if it did.
    pub original_weight: Option<u64>,
}

/// What the regulator would do, or did. Every variant carries the reason: a
/// throttle nobody can explain afterwards is indistinguishable from a bug.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Throttle {
        id: String,
        name: String,
        from: u64,
        to: u64,
        reason: String,
    },
    Restore {
        id: String,
        name: String,
        to: u64,
        reason: String,
    },
}

impl Action {
    pub fn id(&self) -> &str {
        match self {
            Action::Throttle { id, .. } | Action::Restore { id, .. } => id,
        }
    }
    pub fn verb(&self) -> &'static str {
        match self {
            Action::Throttle { .. } => "throttle",
            Action::Restore { .. } => "restore",
        }
    }
}

/// The kernel default for `cpu.weight`, and therefore what a restore aims at
/// when nothing else was recorded.
pub const DEFAULT_WEIGHT: u64 = 100;
/// Stalled this much of the last 10 seconds and the host is contended NOW.
pub const HIGH_WATER: f64 = 25.0;
/// Stalled less than this over a full minute and it is over. Two thresholds on
/// two windows, not one on one: with a single mark the regulator throttles and
/// restores the same workload every tick, which is worse than doing nothing.
pub const LOW_WATER: f64 = 5.0;
/// A workload under this share of the CPU is not what is hurting anybody.
pub const CULPRIT_SHARE_PCT: f64 = 20.0;

/// What to do about `workloads`, given how stalled the host's CPU is.
///
/// At most ONE throttle per call: pressure is measured over ten seconds and a
/// weight change takes effect immediately, so throttling three workloads at
/// once acts three times on one observation and overshoots. Restores are not
/// rationed — undoing is always safe.
pub fn plan(
    host_stall_avg10: f64,
    host_stall_avg60: f64,
    workloads: &[Workload],
    floor: u64,
) -> Vec<Action> {
    let floor = floor.max(1);

    // Recovery first, and it wins: a host that is no longer contended must give
    // the weight back even if this tick's avg10 spiked again, or a workload
    // throttled during one build stays at half share until the node reboots.
    if host_stall_avg60 < LOW_WATER {
        return workloads
            .iter()
            .filter_map(|w| {
                let original = w.original_weight?;
                (w.cpu_weight != original).then(|| Action::Restore {
                    id: w.id.clone(),
                    name: w.name.clone(),
                    to: original,
                    reason: format!(
                        "cpu stalled {host_stall_avg60:.1}% over 60s, below the {LOW_WATER:.0}% \
                         low-water mark"
                    ),
                })
            })
            .collect();
    }

    if host_stall_avg10 < HIGH_WATER {
        return Vec::new();
    }

    let Some(culprit) = workloads
        .iter()
        .filter(|w| w.cpu_share_pct >= CULPRIT_SHARE_PCT && w.cpu_weight > floor)
        .max_by(|a, b| a.cpu_share_pct.total_cmp(&b.cpu_share_pct))
    else {
        return Vec::new();
    };

    let to = (culprit.cpu_weight / 2).max(floor);
    if to == culprit.cpu_weight {
        return Vec::new();
    }
    vec![Action::Throttle {
        id: culprit.id.clone(),
        name: culprit.name.clone(),
        from: culprit.cpu_weight,
        to,
        reason: format!(
            "cpu stalled {host_stall_avg10:.1}% over 10s and this workload is taking {:.0}% of \
             the engine's cpu time",
            culprit.cpu_share_pct
        ),
    }]
}

// ---------------------------------------------------------------------------
// Reading the host, remembering what was changed, and changing it
// ---------------------------------------------------------------------------

use crate::Container;

/// Where the regulator remembers the weight a workload had before it touched
/// it — one small file per container under the state root.
///
/// This engine is daemonless, so there is no process to hold that memory, and
/// guessing the kernel default on restore would clobber anybody who set
/// `--cpu-weight` by hand. The file IS the audit trail: it exists exactly while
/// a workload is throttled, and its absence means the regulator has no claim on
/// that workload.
fn memo(state_root: &std::path::Path, id: &str) -> std::path::PathBuf {
    state_root.join("regulate").join(id)
}

pub fn recorded_original(state_root: &std::path::Path, id: &str) -> Option<u64> {
    std::fs::read_to_string(memo(state_root, id))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn read_u64(path: String) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The `some avg10`/`avg60` of a cgroup's own `cpu.pressure`, or the host's
/// when `cgroup` is `None`.
pub fn cpu_stall(cgroup: Option<&str>) -> Option<crate::Psi> {
    let path = match cgroup {
        Some(c) => format!("{c}/cpu.pressure"),
        None => "/proc/pressure/cpu".to_string(),
    };
    crate::parse_psi_some(&std::fs::read_to_string(path).ok()?)
}

/// Samples every running container twice, `window` apart, and turns the two
/// samples into a share of the engine's CPU time.
///
/// Two samples and not one because `cpu.stat`'s `usage_usec` is a monotonic
/// counter since the container started: a single read says which container has
/// burned the most CPU since last Tuesday, which is not the question. A
/// container that started an hour ago and is idle now would win it.
pub fn sample(containers: &[Container], window: std::time::Duration) -> Vec<Workload> {
    let first: Vec<(String, Option<u64>)> = containers
        .iter()
        .map(|c| (c.id.clone(), crate::container_usage(c).cpu_usage_usec))
        .collect();
    std::thread::sleep(window);

    let deltas: Vec<(usize, u64)> = containers
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let before = first.get(i).and_then(|(_, u)| *u)?;
            let after = crate::container_usage(c).cpu_usage_usec?;
            Some((i, after.saturating_sub(before)))
        })
        .collect();
    let total: u64 = deltas.iter().map(|(_, d)| *d).sum();

    deltas
        .into_iter()
        .map(|(i, delta)| {
            let c = &containers[i];
            let cgroup = crate::live_cgroup(c);
            Workload {
                id: c.id.clone(),
                name: c.name.clone(),
                cpu_weight: read_u64(format!("{cgroup}/cpu.weight")).unwrap_or(DEFAULT_WEIGHT),
                // No CPU burned by anyone at all is 0% each, not a division by
                // zero and not an even split.
                cpu_share_pct: if total == 0 {
                    0.0
                } else {
                    delta as f64 * 100.0 / total as f64
                },
                original_weight: None, // filled by the caller, which knows the state root
                cgroup,
            }
        })
        .collect()
}

/// Drops memos for workloads that are no longer running, and returns how many.
///
/// Found by running the thing: a container throttled to the floor exited while
/// still contended, so the recovery tick that would have removed its memo never
/// came, and the file stayed behind. Nothing breaks — ids are random hex and
/// are not reused — but a directory that only ever grows is a leak, and a memo
/// for a dead workload is a claim on something that does not exist.
pub fn forget_gone(state_root: &std::path::Path, live_ids: &[String]) -> usize {
    let dir = state_root.join("regulate");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return 0;
    };
    rd.flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            !live_ids.contains(&name) && std::fs::remove_file(e.path()).is_ok()
        })
        .count()
}

/// Carries out one action: writes `cpu.weight`, and keeps the memo in step.
///
/// The memo is written BEFORE the throttle and removed AFTER the restore. If
/// the process dies between the two, the worst case is a memo for a workload at
/// its original weight — which the planner reads as "nothing to do" — never a
/// throttled workload nobody remembers having touched.
pub fn apply(state_root: &std::path::Path, w: &Workload, action: &Action) -> std::io::Result<()> {
    match action {
        Action::Throttle { from, to, .. } => {
            let dir = state_root.join("regulate");
            std::fs::create_dir_all(&dir)?;
            if recorded_original(state_root, w.id.as_str()).is_none() {
                std::fs::write(memo(state_root, &w.id), from.to_string())?;
            }
            std::fs::write(format!("{}/cpu.weight", w.cgroup), to.to_string())
        }
        Action::Restore { to, .. } => {
            std::fs::write(format!("{}/cpu.weight", w.cgroup), to.to_string())?;
            match std::fs::remove_file(memo(state_root, &w.id)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(name: &str, weight: u64, share: f64, original: Option<u64>) -> Workload {
        Workload {
            id: format!("id-{name}"),
            name: name.into(),
            cgroup: format!("/sys/fs/cgroup/…/dlx-{name}"),
            cpu_weight: weight,
            cpu_share_pct: share,
            original_weight: original,
        }
    }

    #[test]
    fn the_memo_is_the_whole_memory_and_it_survives_a_second_throttle() {
        // A fake leaf: `cpu.weight` is just a file, which is exactly what it is
        // under /sys/fs/cgroup — so the write path is tested for real without
        // needing a delegated cgroup or a running container.
        let root = std::env::temp_dir().join(format!("dlx-reg-{}", std::process::id()));
        let leaf = root.join("leaf");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(leaf.join("cpu.weight"), "100").unwrap();
        let mut wl = w("build", 100, 90.0, None);
        wl.cgroup = leaf.to_string_lossy().into_owned();

        let first = &plan(60.0, 40.0, std::slice::from_ref(&wl), 20)[0];
        apply(&root, &wl, first).unwrap();
        assert_eq!(
            std::fs::read_to_string(leaf.join("cpu.weight")).unwrap(),
            "50"
        );
        assert_eq!(recorded_original(&root, &wl.id), Some(100));

        // Still contended: it drops again, and the memo must keep the ORIGINAL
        // 100, not be overwritten with the 50 it has now. Losing that is how a
        // workload gets "restored" to half its real share.
        wl.cpu_weight = 50;
        wl.original_weight = recorded_original(&root, &wl.id);
        let second = &plan(60.0, 40.0, std::slice::from_ref(&wl), 20)[0];
        apply(&root, &wl, second).unwrap();
        assert_eq!(
            std::fs::read_to_string(leaf.join("cpu.weight")).unwrap(),
            "25"
        );
        assert_eq!(recorded_original(&root, &wl.id), Some(100), "o memo mudou");

        // Calm again: back to 100, and the claim is dropped.
        wl.cpu_weight = 25;
        let back = &plan(1.0, 1.0, std::slice::from_ref(&wl), 20)[0];
        apply(&root, &wl, back).unwrap();
        assert_eq!(
            std::fs::read_to_string(leaf.join("cpu.weight")).unwrap(),
            "100"
        );
        assert_eq!(recorded_original(&root, &wl.id), None);
        // Restoring twice is not an error: the second call has nothing to
        // remove, and a regulator that panics while undoing is worse than one
        // that never ran.
        apply(&root, &wl, back).unwrap();

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_memo_for_a_workload_that_is_gone_is_dropped() {
        let root = std::env::temp_dir().join(format!("dlx-forget-{}", std::process::id()));
        let dir = root.join("regulate");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("alive"), "100").unwrap();
        std::fs::write(dir.join("dead"), "100").unwrap();

        assert_eq!(forget_gone(&root, &["alive".to_string()]), 1);
        assert!(dir.join("alive").exists(), "o vivo não se apaga");
        assert!(!dir.join("dead").exists());
        // Idempotent, and an absent directory is not an error: the regulator
        // may never have run on this node.
        assert_eq!(forget_gone(&root, &["alive".to_string()]), 0);
        assert_eq!(forget_gone(&root.join("nope"), &[]), 0);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_quiet_host_is_left_alone() {
        let ws = [w("db", 100, 90.0, None)];
        // Not contended: 90% of the CPU is not a problem when nobody is waiting.
        assert!(plan(0.0, 0.0, &ws, 20).is_empty());
        assert!(plan(2.0, 1.0, &ws, 20).is_empty());
    }

    #[test]
    fn the_cause_is_throttled_and_the_victim_is_not() {
        let ws = [
            w("build", 100, 85.0, None),
            w("api", 100, 5.0, None), // starving, and NOT the one to punish
        ];
        let p = plan(60.0, 40.0, &ws, 20);
        assert_eq!(p.len(), 1, "um por observação, nunca três");
        match &p[0] {
            Action::Throttle {
                name,
                from,
                to,
                reason,
                ..
            } => {
                assert_eq!(name, "build");
                assert_eq!((*from, *to), (100, 50));
                assert!(reason.contains("85%"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_small_workload_is_never_the_culprit() {
        // Everything is small: the contention is coming from outside the engine
        // (a compile on the host, say), and halving somebody's share would not
        // help and would not be honest.
        let ws = [w("a", 100, 10.0, None), w("b", 100, 9.0, None)];
        assert!(plan(80.0, 70.0, &ws, 20).is_empty());
    }

    #[test]
    fn the_floor_holds_and_stops_the_ratchet() {
        let ws = [w("build", 40, 90.0, Some(100))];
        // 40 → 20, not below.
        assert!(matches!(
            &plan(60.0, 40.0, &ws, 20)[0],
            Action::Throttle { to: 20, .. }
        ));
        // Already at the floor: nothing left to take, and no no-op action.
        let ws = [w("build", 20, 90.0, Some(100))];
        assert!(plan(60.0, 40.0, &ws, 20).is_empty());
        // A floor of 0 would be `cpu.weight 0`, which the kernel refuses; the
        // planner clamps rather than emitting an impossible write.
        let ws = [w("build", 2, 90.0, None)];
        assert!(matches!(
            &plan(60.0, 40.0, &ws, 0)[0],
            Action::Throttle { to: 1, .. }
        ));
    }

    #[test]
    fn recovery_gives_the_weight_back_and_beats_a_fresh_spike() {
        let ws = [w("build", 50, 90.0, Some(100)), w("api", 100, 5.0, None)];
        // The minute average says it is over, even though this instant spiked.
        // Without recovery winning here, one build costs a workload half its
        // share until the node reboots.
        let p = plan(90.0, 1.0, &ws, 20);
        assert_eq!(p.len(), 1);
        assert_eq!(
            p[0],
            Action::Restore {
                id: "id-build".into(),
                name: "build".into(),
                to: 100,
                reason: "cpu stalled 1.0% over 60s, below the 5% low-water mark".into(),
            }
        );
    }

    #[test]
    fn a_workload_the_regulator_never_touched_is_never_restored() {
        // Someone ran `--cpu-weight 400` by hand. Recovery must not "restore"
        // it to 100: the regulator only undoes what it did.
        let ws = [w("db", 400, 5.0, None)];
        assert!(plan(0.0, 0.0, &ws, 20).is_empty());
        // And one it did touch, already back at its original, needs no action.
        let ws = [w("db", 100, 5.0, Some(100))];
        assert!(plan(0.0, 0.0, &ws, 20).is_empty());
    }

    #[test]
    fn hysteresis_leaves_a_band_where_nothing_happens() {
        let ws = [w("build", 100, 90.0, None)];
        // Between the two marks: not contended enough to act, not calm enough
        // to restore. Doing nothing here is what stops the flapping.
        assert!(plan(10.0, 10.0, &ws, 20).is_empty());
    }
}
