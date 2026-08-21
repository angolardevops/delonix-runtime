//! Shared resource-summary collector — the SINGLE place that aggregates
//! "how much is this engine using right now" (container/VM counts, cgroup
//! memory, network bytes, disk usage per area). Three consumers share this
//! exact function so none of them can drift from each other:
//!
//!  * `delonix-mgmt`'s `GET /metrics` (Prometheus gauges) and `GET /v1/dash`
//!    (JSON) — this crate, see `lib.rs`.
//!  * `delonix dash` (the TUI/`--once`/`--json` CLI) — `delonix-runtime-bin`
//!    depends on `delonix-mgmt` (not the other way around), so it calls
//!    straight into [`collect`] instead of re-implementing the aggregation.
//!
//! Lives here (not in `delonix-runtime-core`) because it needs the store/
//! cgroup/netns access of `delonix-runtime`/`delonix-vm`/`delonix-net`/
//! `delonix-image`/`delonix-volume` — `runtime-core` is a shared-types leaf
//! crate none of the higher-level crates depend on for this.

use std::path::Path;
use std::time::Duration;

use delonix_runtime_core::Status;

/// A point-in-time snapshot of engine-wide resource usage. `Serialize` so it
/// is BOTH the body of `GET /v1/dash` and the payload of `delonix dash --json`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DashSummary {
    pub containers_running: u64,
    pub containers_total: u64,
    pub vms_running: u64,
    pub vms_total: u64,
    pub networks_total: u64,
    pub volumes_total: u64,
    pub images_total: u64,
    pub secrets_total: u64,
    /// `memory.current` of the whole Delonix cgroup slice, in bytes.
    pub memory_bytes_used: u64,
    /// `memory.max` of the whole Delonix cgroup slice, in bytes (0 = unlimited/unknown — cgroup v2 reports the literal string `"max"`, which doesn't parse as a number).
    pub memory_bytes_limit: u64,
    /// Sum of received bytes across ALL of every running container's interfaces (not just the primary one — a `--net-connect`ed container's `eth1` counts too), cumulative since each interface's creation. `None` when the caller opted out of the netns reads (see [`collect`]'s `include_network`) — a real "0 bytes" is `Some(0)`, not `None`.
    pub network_rx_bytes: Option<u64>,
    pub network_tx_bytes: Option<u64>,
    /// How many of `containers_running` did NOT contribute to the sum above
    /// (always `0` when `network_rx_bytes`/`tx_bytes` are `None`, since then
    /// nothing was attempted at all). BUG FOUND (code review): a container
    /// on `--net host`/`--net none` has no netns for `container_net_bytes`
    /// to inspect, so it returns `None` there — this used to be folded into
    /// the sum as a silent "+0", making the total look complete and
    /// authoritative when it was actually a partial measurement on any host
    /// mixing network modes. Surfaced explicitly instead so a caller can
    /// show "N containers not measured" rather than a falsely-precise number.
    pub network_unmeasured_containers: u64,
    /// `None` when the caller opted out of the directory walk (see
    /// [`collect`]'s `include_storage`) — MEASURED on a real host with 49
    /// containers (several full `kindest/node` rootfs copies): 68 GiB,
    /// **over a minute** of disk I/O. Never safe to compute inline on a
    /// request/tick with a latency budget.
    pub storage_bytes_images: Option<u64>,
    pub storage_bytes_volumes: Option<u64>,
    pub storage_bytes_vm_images: Option<u64>,
    pub storage_bytes_containers: Option<u64>,
}

/// Recursive disk usage of a directory, `du`-style. Zero on any unreadable
/// path (a summary must never fail a scrape over one missing directory).
///
/// BUG FIXED HERE — this was a **third private copy** of the same walk
/// (alongside `delonix-volume`'s `dir_usage` and `cmd/system.rs::dir_size`),
/// and all three shared the same two defects: they summed the *apparent* size
/// (`m.len()`) and never deduplicated hardlinks, while calling themselves
/// `du`-style. Measured against `du` on a real ~94 GiB store, that came out
/// **+4.9 %**.
///
/// The old note here justified duplicating rather than sharing, on the grounds
/// that `delonix-mgmt` cannot depend on the `-bin` crate. True, but beside the
/// point: the corrected walk lives in **`delonix-volume`**, which this crate
/// already depends on (see `collect`'s `VolumeStore` use) and which exports
/// `measure` for exactly this reason. Routing through it removes the drift
/// instead of fixing the same bug in three places.
fn dir_size(p: &Path) -> u64 {
    delonix_volume::measure(p).bytes
}

/// Collects a fresh [`DashSummary`]. Best-effort throughout: a store that
/// doesn't open counts as empty/zero (this must never fail a scrape or a
/// dashboard render over one missing/corrupt store).
///
/// `include_network`: per-container network totals require entering EACH
/// running container's netns to read `/proc/net/dev`
/// (`delonix_net::infra::container_net_bytes`, one `nsenter`+`cat` per
/// container) — real cost with many containers.
///
/// `include_storage`: a full recursive walk of `blobs`/`layers`/`volumes`/
/// `vm-images`/`containers` under `root` — MEASURED at over a minute on a
/// host with heavy containers (see the field doc on [`DashSummary`]).
///
/// Callers on a tight budget (an HTTP request handler, a TUI tick) MUST pass
/// `false` for whichever of these they can't afford synchronously, and
/// display the last known value from a slower background refresh instead —
/// see `cmd/dash.rs`'s TUI (a background thread) and this crate's `lib.rs`
/// (a background tokio task) for the two real callers of that pattern.
/// One-shot callers with no latency budget (`--once`/`--json`, a human
/// hitting `/v1/dash`) should pass `true` for both.
pub fn collect(root: &Path, include_network: bool, include_storage: bool) -> DashSummary {
    let mut containers_running = 0u64;
    let mut containers_total = 0u64;
    let mut running_ids: Vec<String> = Vec::new();
    if let Ok(store) = delonix_runtime_core::Store::open(root.join("containers")) {
        if let Ok(list) = store.list() {
            containers_total = list.len() as u64;
            for mut c in list {
                delonix_runtime::reconcile_status(&mut c);
                if c.status == Status::Running {
                    containers_running += 1;
                    running_ids.push(c.id.clone());
                }
            }
        }
    }

    let vms = delonix_vm::list(root).unwrap_or_default();
    let vms_running = vms.iter().filter(|v| v.status == Status::Running).count() as u64;
    let vms_total = vms.len() as u64;

    let networks_total = delonix_net::NetworkStore::open(root)
        .and_then(|s| s.list())
        .map(|l| l.len() as u64)
        .unwrap_or(0);
    let volumes_total = delonix_volume::VolumeStore::open(root)
        .and_then(|s| s.list())
        .map(|l| l.len() as u64)
        .unwrap_or(0);
    let images_total = delonix_image::ImageStore::open(root)
        .and_then(|s| s.list())
        .map(|l| l.len() as u64)
        .unwrap_or(0);
    let secrets_total = delonix_runtime_core::SecretStore::open(root)
        .map(|s| s.list().len() as u64)
        .unwrap_or(0);

    let (memory_bytes_limit, memory_bytes_used, ..) = delonix_runtime::slice_budget();

    let (network_rx_bytes, network_tx_bytes, network_unmeasured_containers) = if include_network {
        let mut rx = 0u64;
        let mut tx = 0u64;
        let mut unmeasured = 0u64;
        for id in &running_ids {
            match delonix_net::infra::container_net_bytes(id) {
                Some((r, t)) => {
                    rx += r;
                    tx += t;
                }
                // `--net host`/`--net none` containers have no netns to
                // inspect (see the field doc on `network_unmeasured_containers`)
                // — count them, don't silently treat as zero traffic.
                None => unmeasured += 1,
            }
        }
        (Some(rx), Some(tx), unmeasured)
    } else {
        (None, None, 0)
    };

    let (
        storage_bytes_images,
        storage_bytes_volumes,
        storage_bytes_vm_images,
        storage_bytes_containers,
    ) = if include_storage {
        (
            Some(dir_size(&root.join("blobs")) + dir_size(&root.join("layers"))),
            Some(dir_size(&root.join("volumes"))),
            Some(dir_size(&root.join("vm-images"))),
            Some(dir_size(&root.join("containers"))),
        )
    } else {
        (None, None, None, None)
    };

    DashSummary {
        containers_running,
        containers_total,
        vms_running,
        vms_total,
        networks_total,
        volumes_total,
        images_total,
        secrets_total,
        memory_bytes_used,
        memory_bytes_limit,
        network_rx_bytes,
        network_tx_bytes,
        network_unmeasured_containers,
        storage_bytes_images,
        storage_bytes_volumes,
        storage_bytes_vm_images,
        storage_bytes_containers,
    }
}

/// Per-container cumulative network rx/tx bytes (bytes since each
/// interface's creation), keyed by container id, for every RUNNING
/// container. Same cost profile as `collect`'s `include_network` totals —
/// one `nsenter`+`cat` per container — so this must never be called on a
/// tick budget; see [`collect_container_net_with_timeout`] and
/// `cmd/dash.rs`'s slow background refresh for the real caller.
pub fn collect_container_net(root: &Path) -> std::collections::HashMap<String, (u64, u64)> {
    let mut out = std::collections::HashMap::new();
    if let Ok(store) = delonix_runtime_core::Store::open(root.join("containers")) {
        if let Ok(list) = store.list() {
            for mut c in list {
                delonix_runtime::reconcile_status(&mut c);
                if c.status == Status::Running {
                    if let Some(bytes) = delonix_net::infra::container_net_bytes(&c.id) {
                        out.insert(c.id.clone(), bytes);
                    }
                }
            }
        }
    }
    out
}

/// Outcome of a bounded collection — three distinct states, because the
/// caller's log line must not claim a timeout when nothing was even attempted.
#[derive(Debug)]
pub enum Bounded<T> {
    /// The work finished within the deadline.
    Done(T),
    /// The work was started but did not finish in time. Its thread is now
    /// leaked (see [`run_bounded`]).
    TimedOut,
    /// Nothing was started: a PREVIOUS attempt is still stuck in the same
    /// underlying operation. This is the circuit breaker, not a failure of
    /// this attempt.
    Skipped,
}

impl<T> Bounded<T> {
    /// The value, if any — for callers that only care about "did I get data".
    pub fn ok(self) -> Option<T> {
        match self {
            Bounded::Done(v) => Some(v),
            _ => None,
        }
    }
}

/// A one-slot "is a worker still stuck in there?" latch, guarding a single
/// call site. Lives in a `static` at each call site so the breaker is
/// per-operation rather than global.
pub struct InFlight(std::sync::atomic::AtomicBool);

impl InFlight {
    pub const fn new() -> Self {
        InFlight(std::sync::atomic::AtomicBool::new(false))
    }
}

impl Default for InFlight {
    fn default() -> Self {
        Self::new()
    }
}

/// Clears the latch when the worker leaves `work` — by return OR by unwind.
///
/// The `Drop` matters: with a bare `store(false)` after the call, a panicking
/// `collect` would leave the latch stuck at `true` and every future attempt
/// would be `Skipped` forever. That trades a thread leak for a permanently
/// dead metric, which is worse — the breaker must only ever trip on a real
/// hang.
struct InFlightGuard(&'static InFlight);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0 .0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Runs `work` on a throwaway thread with a hard wall-clock ceiling, and —
/// this is the part that matters — **refuses to start a second worker while
/// the first is still stuck**.
///
/// BUG FIXED HERE (the thread leak was unbounded). The deadline was added
/// earlier to stop a hung I/O path from freezing the caller's loop forever,
/// and the worker thread is necessarily leaked on timeout (Rust cannot cancel
/// a thread blocked in a syscall). What was missed is that the caller is an
/// *infinite periodic loop*: `spawn_expensive_metrics_refresh` retries every
/// 120s deadline + 30s sleep, so a genuinely stuck operation leaked a thread
/// every ~150s — ~576 per day, each one parked in the same syscall, growing
/// without limit until the process runs out of threads.
///
/// The original note argued "leaking one more thread per attempt is the lesser
/// problem". That holds for a transient stall; it does not hold for a
/// PERMANENT one, and permanent is the realistic case: the documented trigger
/// is an unresponsive NFS mount, and NFS/CIFS/WebDAV volumes are a
/// first-class feature of this engine (`delonix_volume::is_network_driver`).
/// A NAS that goes away is an ordinary operational event, not an edge case.
///
/// With the latch, the leak is bounded at **exactly one thread** for as long
/// as the hang lasts, however long that is; the moment the underlying
/// operation unblocks, the latch clears and collection resumes on its own.
fn run_bounded<T: Send + 'static>(
    flag: &'static InFlight,
    timeout: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Bounded<T> {
    // `swap` is the whole breaker: if it was already `true`, a previous worker
    // never came back and we must not add another.
    if flag.0.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Bounded::Skipped;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _guard = InFlightGuard(flag);
        let _ = tx.send(work());
    });
    match rx.recv_timeout(timeout) {
        Ok(v) => Bounded::Done(v),
        Err(_) => Bounded::TimedOut,
    }
}

/// The latch guarding [`collect_with_timeout`]. One process only ever needs a
/// single expensive collection in flight at a time.
static COLLECT_IN_FLIGHT: InFlight = InFlight::new();

/// Like [`collect`], but with a hard wall-clock ceiling AND a circuit breaker
/// so a permanently-hung I/O path cannot leak an unbounded number of threads.
/// See [`run_bounded`] for both mechanisms and why each is needed.
pub fn collect_with_timeout(
    root: &Path,
    include_network: bool,
    include_storage: bool,
    timeout: Duration,
) -> Bounded<DashSummary> {
    let root = root.to_path_buf();
    run_bounded(&COLLECT_IN_FLIGHT, timeout, move || {
        collect(&root, include_network, include_storage)
    })
}

/// The latch guarding [`collect_container_net_with_timeout`] — its OWN latch
/// (not [`COLLECT_IN_FLIGHT`]), so a stuck netns read here never blocks the
/// unrelated `DashSummary` collection, and vice-versa.
static CONTAINER_NET_IN_FLIGHT: InFlight = InFlight::new();

/// Bounded/circuit-broken form of [`collect_container_net`] — same two
/// mechanisms as [`collect_with_timeout`] (see [`run_bounded`]).
pub fn collect_container_net_with_timeout(
    root: &Path,
    timeout: Duration,
) -> Bounded<std::collections::HashMap<String, (u64, u64)>> {
    let root = root.to_path_buf();
    run_bounded(&CONTAINER_NET_IN_FLIGHT, timeout, move || {
        collect_container_net(&root)
    })
}

/// Pushes a [`DashSummary`] into the shared Prometheus registry
/// (`delonix_runtime_core::metrics`). Fields collected as `None` (an
/// `include_network`/`include_storage` opt-out) simply leave that gauge at
/// its last-published value — callers are expected to publish the cheap
/// fields often and the expensive ones from a slower background refresh
/// (see `collect`'s doc comment), so a gauge is stale, never wrong-and-zero.
pub fn publish_to_metrics(s: &DashSummary) {
    delonix_runtime_core::metrics::set_containers(s.containers_running, s.containers_total);
    delonix_runtime_core::metrics::set_vms(s.vms_running, s.vms_total);
    delonix_runtime_core::metrics::set_memory(s.memory_bytes_used, s.memory_bytes_limit);
    if let (Some(rx), Some(tx)) = (s.network_rx_bytes, s.network_tx_bytes) {
        delonix_runtime_core::metrics::set_network(rx, tx, s.network_unmeasured_containers);
    }
    if let (Some(images), Some(volumes), Some(vm_images), Some(containers)) = (
        s.storage_bytes_images,
        s.storage_bytes_volumes,
        s.storage_bytes_vm_images,
        s.storage_bytes_containers,
    ) {
        delonix_runtime_core::metrics::set_storage(images, volumes, vm_images, containers);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_sobre_root_vazio_nao_rebenta_e_da_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let s = collect(dir.path(), true, true);
        assert_eq!(s.containers_running, 0);
        assert_eq!(s.containers_total, 0);
        assert_eq!(s.vms_total, 0);
        assert_eq!(s.storage_bytes_images, Some(0));
        assert_eq!(s.network_rx_bytes, Some(0));
    }

    #[test]
    fn collect_sem_include_network_nem_storage_devolve_none() {
        let dir = tempfile::tempdir().unwrap();
        let s = collect(dir.path(), false, false);
        assert_eq!(s.network_rx_bytes, None);
        assert_eq!(s.network_tx_bytes, None);
        assert_eq!(s.storage_bytes_images, None);
        assert_eq!(s.storage_bytes_containers, None);
    }

    /// The two tests below both drive the PROCESS-WIDE `COLLECT_IN_FLIGHT`
    /// latch, so they cannot run concurrently — that interference is the
    /// circuit breaker working as designed, not a test artifact (any two
    /// concurrent `collect_with_timeout` callers in one process behave exactly
    /// this way). Serialize them, and wait for the latch to actually clear
    /// before handing over: a test that only takes the mutex would still race
    /// the previous test's worker thread on its way out.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Blocks until no collection is in flight, so the next test starts from a
    /// clean latch.
    fn wait_for_idle_latch() {
        for _ in 0..500 {
            if !COLLECT_IN_FLIGHT
                .0
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("a latch de colheita nunca ficou livre");
    }

    #[test]
    fn collect_with_timeout_devolve_some_dentro_do_prazo() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        wait_for_idle_latch();
        let dir = tempfile::tempdir().unwrap();
        let s = collect_with_timeout(dir.path(), true, true, Duration::from_secs(10));
        assert!(matches!(s, Bounded::Done(_)));
    }

    /// Doesn't (and can't, without a way to inject an artificially hung
    /// `collect`) exercise the ACTUAL timeout firing — it proves the ceiling
    /// itself is real: `collect` on an empty tempdir is fast, so a timeout far
    /// shorter than that must still return in time and `TimedOut` must be a
    /// real, reachable outcome of the function's own logic, not dead code.
    #[test]
    fn collect_with_timeout_expira_se_o_prazo_e_zero() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        wait_for_idle_latch();
        let dir = tempfile::tempdir().unwrap();
        let s = collect_with_timeout(dir.path(), true, true, Duration::from_nanos(1));
        assert!(matches!(s, Bounded::TimedOut));
        // O worker desta colheita ainda está a correr (só o PRAZO expirou);
        // espera que largue a latch antes de devolver o mutex.
        wait_for_idle_latch();
    }

    /// REGRESSION (unbounded resource leak): while a worker is STILL STUCK,
    /// further attempts must be `Skipped` without spawning anything.
    ///
    /// This is the bug the circuit breaker exists for: the real caller is an
    /// infinite periodic loop, so without the latch a permanently-hung NFS
    /// volume leaked one parked thread per cycle (~576/day) until the process
    /// ran out of threads. Removing the `swap` guard in `run_bounded` makes the
    /// spawn counter below come out at 5 instead of 1, and this test fails.
    ///
    /// Uses `run_bounded` directly with its OWN latch (not the shared
    /// `COLLECT_IN_FLIGHT`) so it neither disturbs nor is disturbed by the
    /// other tests running in parallel, and so the "work" can be made to hang
    /// deliberately — something `collect` itself offers no way to inject.
    #[test]
    fn run_bounded_nao_lanca_uma_2a_thread_enquanto_a_1a_esta_presa() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        static TEST_FLAG: InFlight = InFlight::new();

        let spawns = Arc::new(AtomicUsize::new(0));
        // Released only at the end of the test — until then every worker that
        // actually starts stays parked, exactly like a wedged `read` on a dead
        // NFS mount.
        let unblock = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

        let hang = {
            let spawns = Arc::clone(&spawns);
            let unblock = Arc::clone(&unblock);
            move || {
                spawns.fetch_add(1, Ordering::SeqCst);
                let (lock, cv) = &*unblock;
                let mut done = lock.lock().unwrap();
                while !*done {
                    done = cv.wait(done).unwrap();
                }
                42u32
            }
        };

        let deadline = Duration::from_millis(150);
        // 1st attempt: starts a worker, which hangs → TimedOut, thread leaked.
        assert!(matches!(
            run_bounded(&TEST_FLAG, deadline, hang.clone()),
            Bounded::TimedOut
        ));
        // Every subsequent attempt, for as long as the hang lasts, must be
        // SKIPPED — this is the whole fix.
        for i in 0..4 {
            assert!(
                matches!(
                    run_bounded(&TEST_FLAG, deadline, hang.clone()),
                    Bounded::Skipped
                ),
                "tentativa {i} devia ter sido saltada, não relançada"
            );
        }
        assert_eq!(
            spawns.load(Ordering::SeqCst),
            1,
            "a fuga tem de ficar limitada a UMA thread, por muito que o hang dure"
        );

        // Assim que a operação desbloqueia, o disjuntor rearma-se sozinho.
        {
            let (lock, cv) = &*unblock;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
        // Espera o worker sair (o guard limpa a latch no Drop).
        let rearmed = (0..200).any(|_| {
            std::thread::sleep(Duration::from_millis(10));
            matches!(run_bounded(&TEST_FLAG, deadline, || 7u32), Bounded::Done(7))
        });
        assert!(
            rearmed,
            "depois do hang resolver, a colheita tem de voltar a correr sozinha"
        );
    }

    /// O disjuntor NÃO pode ficar preso por um `work` que entra em pânico —
    /// senão trocava-se uma fuga de threads por uma métrica morta para sempre.
    /// É para isto que a latch é limpa por `Drop` e não por um `store` a seguir
    /// à chamada.
    #[test]
    fn run_bounded_rearma_depois_de_um_panico_no_worker() {
        static TEST_FLAG: InFlight = InFlight::new();
        let deadline = Duration::from_secs(5);

        // O worker entra em pânico; o `recv` vê o canal fechado → TimedOut.
        assert!(matches!(
            run_bounded(&TEST_FLAG, deadline, || -> u32 { panic!("boom") }),
            Bounded::TimedOut
        ));
        // A latch tem de ter sido limpa no unwind: a chamada seguinte corre.
        let rearmed = (0..200).any(|_| {
            std::thread::sleep(Duration::from_millis(10));
            matches!(run_bounded(&TEST_FLAG, deadline, || 9u32), Bounded::Done(9))
        });
        assert!(rearmed, "um pânico no worker deixou o disjuntor preso");
    }

    /// `dir_size` desce a árvore toda e conta o que o `du` conta.
    ///
    /// Não fixa mais o total exacto em bytes aparentes (`5 + 10 == 15`): essa
    /// asserção codificava a semântica ERRADA que este walk tinha — soma de
    /// `m.len()` sem deduplicação de hardlinks. Agora delega em
    /// `delonix_volume::measure` (blocos alocados, dedup por `(dev, ino)`), por
    /// isso o total depende do tamanho de bloco do filesystem e o que se afirma
    /// é o comportamento, não um número de um sistema de ficheiros em concreto.
    #[test]
    fn dir_size_soma_recursivamente() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"12345").unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("b"), b"1234567890").unwrap();

        let total = dir_size(dir.path());
        let only_root = dir_size(&dir.path().join("sub"));
        assert!(total > 0, "a árvore não pode medir zero");
        assert!(
            total > only_root,
            "o total ({total}) tem de incluir o ficheiro da raiz além do da subpasta ({only_root})"
        );
        // Um caminho inexistente continua a valer zero (nunca falha um scrape).
        assert_eq!(dir_size(&dir.path().join("nao-existe")), 0);
    }

    /// REGRESSION: o `system df`/dashboard não pode contar N vezes um ficheiro
    /// com N hardlinks. Esta era a MESMA classe de erro que a quota de volumes
    /// tinha, em três cópias privadas do mesmo walk — agora há só uma.
    #[test]
    fn dir_size_nao_conta_hardlinks_varias_vezes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("original"), vec![0u8; 64 * 1024]).unwrap();
        let one = dir_size(dir.path());
        for i in 0..5 {
            std::fs::hard_link(
                dir.path().join("original"),
                dir.path().join(format!("l{i}")),
            )
            .unwrap();
        }
        assert_eq!(
            dir_size(dir.path()),
            one,
            "6 nomes do mesmo inode contaram como 6 ficheiros"
        );
    }
}
