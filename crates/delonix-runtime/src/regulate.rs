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
    /// `memory.current`, in bytes.
    pub memory_current: u64,
    /// `memory.high` right now, or `None` for the kernel default (`max`).
    pub memory_high: Option<u64>,
    /// `true` if the regulator is the one who set `memory.high`.
    pub memory_regulated: bool,
}

/// The cgroup knob an action turns. Both are SOFT by design — neither can kill
/// anything, which is what makes an automatic decision acceptable at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Knob {
    /// A share of the CPU. With no contention, a halved share still gets the
    /// whole machine.
    CpuWeight,
    /// The memory THROTTLE. `memory.high` puts a workload over it into
    /// aggressive reclaim and slows it down; it never OOM-kills. `memory.max`
    /// would, and is deliberately never written here: a regulator that can kill
    /// a database because a five-minute average crossed a line is not a
    /// regulator, it is an incident.
    MemoryHigh,
}

impl Knob {
    pub fn file(self) -> &'static str {
        match self {
            Knob::CpuWeight => "cpu.weight",
            Knob::MemoryHigh => "memory.high",
        }
    }
    pub fn as_str(self) -> &'static str {
        self.file()
    }
}

/// What the regulator would do, or did. Every variant carries the reason: a
/// throttle nobody can explain afterwards is indistinguishable from a bug.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Throttle {
        id: String,
        name: String,
        knob: Knob,
        from: u64,
        to: u64,
        reason: String,
    },
    Restore {
        id: String,
        name: String,
        knob: Knob,
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
    pub fn knob(&self) -> Knob {
        match self {
            Action::Throttle { knob, .. } | Action::Restore { knob, .. } => *knob,
        }
    }
    pub fn reason(&self) -> &str {
        match self {
            Action::Throttle { reason, .. } | Action::Restore { reason, .. } => reason,
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

/// Never squeeze a workload below this much memory, whatever the pressure.
pub const MEMORY_FLOOR_BYTES: u64 = 128 * 1024 * 1024;
/// `memory.high` is set to this share of what the culprit is using now. A nudge
/// into reclaim, not a cliff: the workload keeps what it has and pays only for
/// growing, which is exactly what `memory.high` is for.
pub const MEMORY_SQUEEZE_PCT: u64 = 90;
/// A workload under this share of the engine's memory is not what is hurting
/// anybody.
pub const MEMORY_CULPRIT_PCT: f64 = 20.0;
/// How `memory.high = max` is spelled in this code — the kernel writes the
/// literal string, and `Option` would have meant a second shape for one value.
pub const MEMORY_NO_LIMIT: u64 = u64::MAX;

/// What to do about memory. Separate from the CPU decision on purpose: the two
/// resources rarely have the same culprit, and one observation must not be
/// credited with two decisions.
///
/// Only ever `memory.high`, never `memory.max`. `high` puts a workload over the
/// line into aggressive reclaim and slows it down; `max` OOM-kills it. A
/// regulator that can kill a database because a five-minute average crossed a
/// threshold is not a regulator, it is an incident.
pub fn plan_memory(stall_avg10: f64, stall_avg60: f64, workloads: &[Workload]) -> Vec<Action> {
    if stall_avg60 < LOW_WATER {
        return workloads
            .iter()
            .filter(|w| w.memory_regulated && w.memory_high.is_some())
            .map(|w| Action::Restore {
                id: w.id.clone(),
                name: w.name.clone(),
                knob: Knob::MemoryHigh,
                to: MEMORY_NO_LIMIT,
                reason: format!(
                    "memory stalled {stall_avg60:.1}% over 60s, below the {LOW_WATER:.0}% \
                     low-water mark"
                ),
            })
            .collect();
    }
    if stall_avg10 < HIGH_WATER {
        return Vec::new();
    }

    let total: u64 = workloads.iter().map(|w| w.memory_current).sum();
    if total == 0 {
        return Vec::new();
    }
    // Already-throttled workloads are excluded, and that is the whole guard
    // against a ratchet: squeezing the same one every tick would walk it to the
    // floor over four ticks for a single sustained event.
    let Some(culprit) = workloads
        .iter()
        .filter(|w| {
            w.memory_high.is_none()
                && w.memory_current as f64 * 100.0 / total as f64 >= MEMORY_CULPRIT_PCT
        })
        .max_by_key(|w| w.memory_current)
    else {
        return Vec::new();
    };

    let to = (culprit.memory_current / 100 * MEMORY_SQUEEZE_PCT).max(MEMORY_FLOOR_BYTES);
    if to >= culprit.memory_current {
        return Vec::new(); // already at or under the floor
    }
    vec![Action::Throttle {
        id: culprit.id.clone(),
        name: culprit.name.clone(),
        knob: Knob::MemoryHigh,
        from: culprit.memory_current,
        to,
        reason: format!(
            "memory stalled {stall_avg10:.1}% over 10s and this workload holds {:.0}% of the \
             engine's memory",
            culprit.memory_current as f64 * 100.0 / total as f64
        ),
    }]
}

/// What to do about the CPU: at most one throttle, or every restore.
///
/// At most ONE throttle per call: pressure is measured over ten seconds and a
/// weight change takes effect immediately, so throttling three workloads at
/// once acts three times on one observation and overshoots. Restores are not
/// rationed — undoing is always safe.
pub fn plan_cpu(
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
                    knob: Knob::CpuWeight,
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
        knob: Knob::CpuWeight,
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
fn memo(state_root: &std::path::Path, id: &str, knob: Knob) -> std::path::PathBuf {
    // One file per (workload, knob): a workload can be throttled on CPU and on
    // memory at the same time, and one file would make the second claim erase
    // the first.
    let name = match knob {
        Knob::CpuWeight => id.to_string(),
        Knob::MemoryHigh => format!("{id}.memory"),
    };
    state_root.join("regulate").join(name)
}

pub fn recorded_original(state_root: &std::path::Path, id: &str) -> Option<u64> {
    std::fs::read_to_string(memo(state_root, id, Knob::CpuWeight))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// `true` if the regulator is the one holding this workload's `memory.high`.
///
/// The VALUE is not recorded, unlike the CPU weight: restoring memory means
/// writing `max`, which is the kernel default and cannot be anybody else's
/// setting to clobber. What has to be remembered is only whether the claim is
/// ours — a `memory.high` a human set by hand has no memo, and is never touched.
pub fn memory_is_regulated(state_root: &std::path::Path, id: &str) -> bool {
    memo(state_root, id, Knob::MemoryHigh).exists()
}

fn read_u64(path: String) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The host's `some` pressure for one resource (`cpu`, `memory`, `io`).
pub fn stall(resource: &str) -> Option<crate::Psi> {
    crate::psi(resource)
}

/// The same reading for ONE cgroup, from its own `<res>.pressure`.
///
/// Present even for `io` on a rootless leaf, where the io CONTROLLER is not
/// delegated: the kernel accounts the stall regardless of whether anybody can
/// limit it. Seeing which workload is waiting on the disk is possible on hosts
/// where throttling it is not.
pub fn cgroup_stall(cgroup: &str, resource: &str) -> Option<crate::Psi> {
    crate::parse_psi_some(&std::fs::read_to_string(format!("{cgroup}/{resource}.pressure")).ok()?)
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
                memory_current: read_u64(format!("{cgroup}/memory.current")).unwrap_or(0),
                // `memory.high` reads the literal `max` when unset, which
                // `parse::<u64>()` refuses — and that refusal IS the answer.
                memory_high: read_u64(format!("{cgroup}/memory.high")),
                memory_regulated: false, // filled by the caller, which knows the state root
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
            // A memory memo is `<id>.memory`; the id is the part before the dot.
            let id = name.split('.').next().unwrap_or(&name).to_string();
            !live_ids.contains(&id) && std::fs::remove_file(e.path()).is_ok()
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
    let knob = action.knob();
    let path = format!("{}/{}", w.cgroup, knob.file());
    match action {
        Action::Throttle { from, to, .. } => {
            let dir = state_root.join("regulate");
            std::fs::create_dir_all(&dir)?;
            let m = memo(state_root, &w.id, knob);
            if !m.exists() {
                std::fs::write(m, from.to_string())?;
            }
            std::fs::write(path, to.to_string())
        }
        Action::Restore { to, .. } => {
            // `MEMORY_NO_LIMIT` is the literal `max`; a number would be a
            // ceiling, and restoring must leave none.
            let value = if *to == MEMORY_NO_LIMIT {
                "max".to_string()
            } else {
                to.to_string()
            };
            std::fs::write(path, value)?;
            match std::fs::remove_file(memo(state_root, &w.id, knob)) {
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
            memory_current: 0,
            memory_high: None,
            memory_regulated: false,
        }
    }

    /// One workload seen through the memory lens: `bytes` in use, and whether
    /// `memory.high` is already set (and by whom).
    fn m(name: &str, bytes: u64, high: Option<u64>, ours: bool) -> Workload {
        Workload {
            memory_current: bytes,
            memory_high: high,
            memory_regulated: ours,
            ..w(name, 100, 0.0, None)
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

        let first = &plan_cpu(60.0, 40.0, std::slice::from_ref(&wl), 20)[0];
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
        let second = &plan_cpu(60.0, 40.0, std::slice::from_ref(&wl), 20)[0];
        apply(&root, &wl, second).unwrap();
        assert_eq!(
            std::fs::read_to_string(leaf.join("cpu.weight")).unwrap(),
            "25"
        );
        assert_eq!(recorded_original(&root, &wl.id), Some(100), "o memo mudou");

        // Calm again: back to 100, and the claim is dropped.
        wl.cpu_weight = 25;
        let back = &plan_cpu(1.0, 1.0, std::slice::from_ref(&wl), 20)[0];
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

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn memory_squeezes_the_biggest_holder_and_never_kills_it() {
        let ws = [m("db", 8 * GIB, None, false), m("api", GIB, None, false)];
        let p = plan_memory(60.0, 40.0, &ws);
        assert_eq!(p.len(), 1);
        match &p[0] {
            Action::Throttle {
                name,
                knob,
                to,
                reason,
                ..
            } => {
                assert_eq!(name, "db");
                // `memory.high`, NEVER `memory.max`: this throttles into
                // reclaim, it does not OOM-kill.
                assert_eq!(*knob, Knob::MemoryHigh);
                assert_eq!(*to, 8 * GIB / 100 * MEMORY_SQUEEZE_PCT);
                assert!(*to < 8 * GIB, "tem de apertar");
                assert!(reason.contains("89%") || reason.contains("88%"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_small_holder_is_never_squeezed() {
        // Nobody holds a fifth of the memory: the pressure is coming from
        // outside the engine, and squeezing a small workload would not help.
        let ws = [
            m("a", GIB, None, false),
            m("b", GIB, None, false),
            m("c", GIB, None, false),
            m("d", GIB, None, false),
            m("e", GIB, None, false),
            m("f", GIB, None, false),
        ];
        assert!(plan_memory(80.0, 70.0, &ws).is_empty());
    }

    #[test]
    fn the_squeeze_does_not_ratchet() {
        // Already throttled: not squeezed again. Without this, one sustained
        // event walks a workload to the floor over four ticks.
        let ws = [m("db", 8 * GIB, Some(7 * GIB), true)];
        assert!(plan_memory(90.0, 80.0, &ws).is_empty());
        // And nothing is ever taken below the floor.
        let ws = [m("tiny", 100 * 1024 * 1024, None, false)];
        assert!(
            plan_memory(90.0, 80.0, &ws).is_empty(),
            "abaixo do piso não se aperta"
        );
    }

    #[test]
    fn memory_recovery_writes_max_and_only_for_our_own_claims() {
        // Ours: released, and released to `max` — a number would still be a
        // ceiling.
        let ws = [m("db", 8 * GIB, Some(7 * GIB), true)];
        let p = plan_memory(0.0, 0.0, &ws);
        assert_eq!(p.len(), 1);
        assert_eq!(
            p[0],
            Action::Restore {
                id: "id-db".into(),
                name: "db".into(),
                knob: Knob::MemoryHigh,
                to: MEMORY_NO_LIMIT,
                reason: "memory stalled 0.0% over 60s, below the 5% low-water mark".into(),
            }
        );

        // Somebody set `memory.high` by hand. The regulator has no memo for it
        // and must not "restore" what it never took.
        let theirs = [m("db", 8 * GIB, Some(7 * GIB), false)];
        assert!(plan_memory(0.0, 0.0, &theirs).is_empty());
    }

    #[test]
    fn memory_and_cpu_are_two_decisions_with_two_culprits() {
        // The CPU hog is not the memory hog, which is the normal case and the
        // reason the two planners are separate: one observation, one decision
        // per resource, and never the wrong workload punished for the other's
        // sin.
        let mut hog = m("build", 512 * 1024 * 1024, None, false);
        hog.cpu_share_pct = 95.0;
        let mut db = m("db", 8 * GIB, None, false);
        db.cpu_share_pct = 5.0;
        let ws = [hog, db];

        let cpu = plan_cpu(60.0, 40.0, &ws, 20);
        let mem = plan_memory(60.0, 40.0, &ws);
        assert_eq!(cpu.len(), 1);
        assert_eq!(mem.len(), 1);
        assert_eq!(cpu[0].id(), "id-build");
        assert_eq!(mem[0].id(), "id-db");
    }

    #[test]
    fn the_two_memos_do_not_erase_each_other() {
        let root = std::env::temp_dir().join(format!("dlx-memo2-{}", std::process::id()));
        let leaf = root.join("leaf");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(leaf.join("cpu.weight"), "100").unwrap();
        std::fs::write(leaf.join("memory.high"), "max").unwrap();

        let mut wl = m("db", 8 * GIB, None, false);
        wl.cpu_share_pct = 95.0;
        wl.cgroup = leaf.to_string_lossy().into_owned();

        apply(
            &root,
            &wl,
            &plan_cpu(60.0, 40.0, std::slice::from_ref(&wl), 20)[0],
        )
        .unwrap();
        apply(
            &root,
            &wl,
            &plan_memory(60.0, 40.0, std::slice::from_ref(&wl))[0],
        )
        .unwrap();

        // A workload can be throttled on both at once; one memo file per knob,
        // or the second claim erases the first and one of them never comes back.
        assert_eq!(recorded_original(&root, &wl.id), Some(100));
        assert!(memory_is_regulated(&root, &wl.id));
        assert_eq!(
            std::fs::read_to_string(leaf.join("cpu.weight")).unwrap(),
            "50"
        );
        assert_eq!(
            std::fs::read_to_string(leaf.join("memory.high")).unwrap(),
            (8 * GIB / 100 * MEMORY_SQUEEZE_PCT).to_string()
        );

        // Releasing memory writes the literal `max` and drops only its own memo.
        wl.memory_high = Some(8 * GIB / 100 * MEMORY_SQUEEZE_PCT);
        wl.memory_regulated = true;
        apply(
            &root,
            &wl,
            &plan_memory(0.0, 0.0, std::slice::from_ref(&wl))[0],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(leaf.join("memory.high")).unwrap(),
            "max"
        );
        assert!(!memory_is_regulated(&root, &wl.id));
        assert_eq!(recorded_original(&root, &wl.id), Some(100), "o memo do cpu");

        // And the sweep of dead workloads sees through the `.memory` suffix.
        assert_eq!(forget_gone(&root, &[]), 1);
        assert_eq!(recorded_original(&root, &wl.id), None);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_quiet_host_is_left_alone() {
        let ws = [w("db", 100, 90.0, None)];
        // Not contended: 90% of the CPU is not a problem when nobody is waiting.
        assert!(plan_cpu(0.0, 0.0, &ws, 20).is_empty());
        assert!(plan_cpu(2.0, 1.0, &ws, 20).is_empty());
    }

    #[test]
    fn the_cause_is_throttled_and_the_victim_is_not() {
        let ws = [
            w("build", 100, 85.0, None),
            w("api", 100, 5.0, None), // starving, and NOT the one to punish
        ];
        let p = plan_cpu(60.0, 40.0, &ws, 20);
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
        assert!(plan_cpu(80.0, 70.0, &ws, 20).is_empty());
    }

    #[test]
    fn the_floor_holds_and_stops_the_ratchet() {
        let ws = [w("build", 40, 90.0, Some(100))];
        // 40 → 20, not below.
        assert!(matches!(
            &plan_cpu(60.0, 40.0, &ws, 20)[0],
            Action::Throttle { to: 20, .. }
        ));
        // Already at the floor: nothing left to take, and no no-op action.
        let ws = [w("build", 20, 90.0, Some(100))];
        assert!(plan_cpu(60.0, 40.0, &ws, 20).is_empty());
        // A floor of 0 would be `cpu.weight 0`, which the kernel refuses; the
        // planner clamps rather than emitting an impossible write.
        let ws = [w("build", 2, 90.0, None)];
        assert!(matches!(
            &plan_cpu(60.0, 40.0, &ws, 0)[0],
            Action::Throttle { to: 1, .. }
        ));
    }

    #[test]
    fn recovery_gives_the_weight_back_and_beats_a_fresh_spike() {
        let ws = [w("build", 50, 90.0, Some(100)), w("api", 100, 5.0, None)];
        // The minute average says it is over, even though this instant spiked.
        // Without recovery winning here, one build costs a workload half its
        // share until the node reboots.
        let p = plan_cpu(90.0, 1.0, &ws, 20);
        assert_eq!(p.len(), 1);
        assert_eq!(
            p[0],
            Action::Restore {
                id: "id-build".into(),
                name: "build".into(),
                knob: Knob::CpuWeight,
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
        assert!(plan_cpu(0.0, 0.0, &ws, 20).is_empty());
        // And one it did touch, already back at its original, needs no action.
        let ws = [w("db", 100, 5.0, Some(100))];
        assert!(plan_cpu(0.0, 0.0, &ws, 20).is_empty());
    }

    #[test]
    fn hysteresis_leaves_a_band_where_nothing_happens() {
        let ws = [w("build", 100, 90.0, None)];
        // Between the two marks: not contended enough to act, not calm enough
        // to restore. Doing nothing here is what stops the flapping.
        assert!(plan_cpu(10.0, 10.0, &ws, 20).is_empty());
    }
}
