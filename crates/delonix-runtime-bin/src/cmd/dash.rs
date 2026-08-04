//! `delonix dash` — runtime summary/KPI dashboard. Global (`delonix dash`)
//! or contextual per group (`delonix container dash`, `vm dash`, ...).
//!
//! Two outputs from the SAME `DashData` (pure snapshot of the stores):
//!  * **interactive TUI** (default, in a terminal): tiles + table + problems
//!    panel + activity sparkline, refreshed every ~1s until `q`.
//!  * **`--once`** (or no tty): prints ONE text snapshot (ANSI) — for
//!    scripts/CI and for the smoke test (no terminal needed).
//!
//! Data collection (`DashData::collect`) and snapshot formatting are pure
//! over the stores — testable without a terminal. The TUI is a thin shell on top.

use std::collections::VecDeque;
use std::io::IsTerminal;
use std::time::{Duration, Instant};

use delonix_runtime_core::{Result, Status};
use serde::Serialize;

use super::po;
use super::util::state_root;

/// Dashboard scope: global or focused on a group of resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DashScope {
    Global,
    Containers,
    Vms,
    Networks,
    Storage,
    Images,
}

impl DashScope {
    fn title(self) -> &'static str {
        match self {
            DashScope::Global => po::t("DELONIX — SUMMARY"),
            DashScope::Containers => po::t("DELONIX — CONTAINERS"),
            DashScope::Vms => po::t("DELONIX — VMs"),
            DashScope::Networks => po::t("DELONIX — NETWORKS"),
            DashScope::Storage => po::t("DELONIX — STORAGE/VOLUMES"),
            DashScope::Images => po::t("DELONIX — IMAGES"),
        }
    }
}

/// A KPI (tile) — label + big value + subtitle.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tile {
    pub label: String,
    pub value: String,
    pub sub: String,
}

/// A row of the resource table.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row {
    pub kind: String,
    pub name: String,
    pub status: String,
    /// How long a Running resource has been up (`fmt_duration_secs`-style), or
    /// `"-"` when unknown/not applicable (only containers track a
    /// `pid_starttime` today — see the `up` field's collection below).
    pub up: String,
    pub extra: String,
    /// `true` = healthy state (Running/present); painted green vs red.
    pub ok: bool,
}

/// An identified problem (the red panel on the right).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Problem {
    pub resource: String,
    pub message: String,
}

/// Complete dashboard snapshot at an instant — pure (only reads stores).
/// `Serialize` backs `delonix dash --json` (one shot, script/CI-friendly —
/// the TUI/`--once` ANSI rendering stays a SEPARATE view over the same data).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DashData {
    pub scope: DashScope,
    pub tiles: Vec<Tile>,
    pub rows: Vec<Row>,
    pub problems: Vec<Problem>,
    /// Running-container count, tracked every tick for the TUI's activity sparkline history (see `tui::render`).
    pub activity: u64,
    /// `memory.current` of the whole Delonix cgroup slice, in bytes — the OTHER series the sparkline can show (`m` key toggles), and the source of the MEMORY tile.
    pub mem_bytes: u64,
}

fn tile(label: &str, value: impl ToString, sub: &str) -> Tile {
    Tile {
        label: label.to_string(),
        value: value.to_string(),
        sub: sub.to_string(),
    }
}

impl DashData {
    /// Collects the FULL snapshot for the given `scope`, including the
    /// expensive network/storage totals (a full recursive disk-usage walk —
    /// MEASURED at over a minute on a host with heavy containers, see
    /// `dashstats::collect`'s doc comment). Only for one-shot callers with no
    /// latency budget: `--once`/`--json` and the TUI's very first, pre-render
    /// snapshot. The TUI's per-tick refresh uses [`Self::collect_with`]
    /// instead, with a cheap-only or background-refreshed summary — see
    /// `tui::render`.
    pub fn collect(scope: DashScope) -> Result<DashData> {
        let root = state_root();
        let summary = delonix_mgmt::dashstats::collect(&root, true, true);
        Self::collect_with(scope, &root, summary)
    }

    /// Like [`Self::collect`], but with the KPI summary supplied by the
    /// caller instead of computed here — lets the TUI reuse a cheap
    /// (counts/memory only) or background-refreshed (network/storage too)
    /// summary on its 1s tick, instead of paying the full collection cost on
    /// every single frame.
    pub fn collect_with(
        scope: DashScope,
        root: &std::path::Path,
        summary: delonix_mgmt::dashstats::DashSummary,
    ) -> Result<DashData> {
        // --- containers (name/image/uptime for the table; counts too) ---
        let mut containers: Vec<(String, Status, String, Option<u64>)> = Vec::new();
        if let Ok((_img, store)) = super::util::open_stores() {
            for mut c in store.list().unwrap_or_default() {
                delonix_runtime::reconcile_status(&mut c);
                containers.push((
                    c.name.clone(),
                    c.status.clone(),
                    c.image.clone(),
                    c.pid_starttime,
                ));
            }
        }
        let c_running = containers
            .iter()
            .filter(|(_, s, _, _)| *s == Status::Running)
            .count();

        // --- vms (state RECONCILED with the backend, like containers) — a
        //     VM killed externally shows Stopped, not the persisted Running ---
        // BUG FOUND: `delonix_vm::list` ALREADY calls `status(base, &vm.name)`
        // per VM internally (see its own body) — the `.map(status)` below
        // used to reconcile every VM a SECOND time, doubling the `virsh`
        // subprocess spawns this dashboard forks every second (libvirt
        // backend: several `virsh` calls per VM per `status`). Redundant
        // work only, `list`'s result is already fully reconciled.
        let vms: Vec<delonix_runtime_core::Vm> = delonix_vm::list(root).unwrap_or_default();
        let vm_running = vms.iter().filter(|v| v.status == Status::Running).count();

        // --- networks / volumes / images / secrets ---
        let networks = delonix_net::NetworkStore::open(root)
            .and_then(|s| s.list())
            .unwrap_or_default();
        let volumes = delonix_volume::VolumeStore::open(root)
            .and_then(|s| s.list())
            .unwrap_or_default();
        let images = delonix_image::ImageStore::open(root)
            .and_then(|s| s.list())
            .unwrap_or_default();
        let secrets = delonix_runtime_core::SecretStore::open(root)
            .map(|s| s.list().len())
            .unwrap_or(0);

        // --- tiles (per scope); `summary`'s KPI fields (memory/network/
        // storage) come from the CALLER (see doc comments above) — the SAME
        // collector `delonix-mgmt` uses for `/metrics`/`/v1/dash`, so the
        // TUI, `--json`, and a Prometheus scrape never disagree on the
        // arithmetic, they may just be looking at snapshots of different ages.
        let mem_sub = if summary.memory_bytes_limit > 0 {
            po::tf(
                "of {limit} slice limit",
                &[(
                    "limit",
                    &super::output::fmt_size(summary.memory_bytes_limit),
                )],
            )
        } else {
            po::t("no configured limit").to_string()
        };
        let net_value = format!(
            "{} / {}",
            super::output::fmt_size(summary.network_rx_bytes.unwrap_or(0)),
            super::output::fmt_size(summary.network_tx_bytes.unwrap_or(0))
        );
        // BUG FOUND (code review): a `--net host`/`--net none` container has
        // no netns to inspect, so it never contributed to the sum above —
        // silently indistinguishable from "measured, zero traffic" unless
        // called out here. `network_unmeasured_containers` is always 0 when
        // network collection was skipped entirely (see the field doc).
        let net_sub = if summary.network_unmeasured_containers > 0 {
            po::tf(
                "rx / tx (cumulative) — {n} container(s) not measured (host/none network)",
                &[("n", &summary.network_unmeasured_containers.to_string())],
            )
        } else {
            po::t("rx / tx (cumulative)").to_string()
        };
        // `None` until the first background pass of the (expensive) disk-usage
        // walk completes — see `collect`/`collect_with`'s doc comments. Shown
        // as a translated placeholder, never a misleading "0 B".
        let storage_total = match (
            summary.storage_bytes_images,
            summary.storage_bytes_volumes,
            summary.storage_bytes_vm_images,
            summary.storage_bytes_containers,
        ) {
            (Some(i), Some(v), Some(vm), Some(c)) => Some(i + v + vm + c),
            _ => None,
        };
        let fmt_storage = |b: Option<u64>| {
            b.map(super::output::fmt_size)
                .unwrap_or_else(|| po::t("measuring…").to_string())
        };
        let tiles = match scope {
            DashScope::Global => vec![
                tile(
                    "CONTAINERS",
                    format!("{c_running}/{}", containers.len()),
                    po::t("running / total"),
                ),
                tile(
                    "VMs",
                    format!("{vm_running}/{}", vms.len()),
                    po::t("running / total"),
                ),
                tile(po::t("NETWORKS"), networks.len(), po::t("defined")),
                tile("VOLUMES", volumes.len(), po::t("+ network storage")),
                tile(po::t("IMAGES"), images.len(), po::t("cached")),
                tile(po::t("SECRETS"), secrets, po::t("in vault")),
                tile(
                    po::t("MEMORY"),
                    super::output::fmt_size(summary.memory_bytes_used),
                    &mem_sub,
                ),
                tile(po::t("TRAFFIC"), net_value.clone(), &net_sub),
                tile(
                    po::t("STORAGE"),
                    fmt_storage(storage_total),
                    po::t("images+volumes+VMs+containers"),
                ),
            ],
            DashScope::Containers => vec![
                tile(po::t("RUNNING"), c_running, "Running"),
                tile("TOTAL", containers.len(), po::t("all states")),
                tile(
                    po::t("STOPPED"),
                    containers.len().saturating_sub(c_running),
                    po::t("not Running"),
                ),
                tile(
                    po::t("MEMORY"),
                    super::output::fmt_size(summary.memory_bytes_used),
                    &mem_sub,
                ),
                tile(po::t("TRAFFIC"), net_value.clone(), &net_sub),
            ],
            DashScope::Vms => vec![
                tile(po::t("RUNNING"), vm_running, "Running"),
                tile("TOTAL", vms.len(), po::t("all")),
            ],
            DashScope::Networks => vec![
                tile(po::t("NETWORKS"), networks.len(), po::t("defined")),
                tile(po::t("TRAFFIC"), net_value.clone(), &net_sub),
            ],
            DashScope::Storage => vec![
                tile("VOLUMES", volumes.len(), po::t("local + network")),
                tile(
                    po::t("STORAGE"),
                    fmt_storage(storage_total),
                    po::t("images+volumes+VMs+containers"),
                ),
            ],
            DashScope::Images => vec![
                tile(po::t("IMAGES"), images.len(), po::t("cached")),
                tile(
                    po::t("STORAGE"),
                    fmt_storage(summary.storage_bytes_images),
                    po::t("this store only"),
                ),
            ],
        };

        // --- table rows (per scope) ---
        let mut rows = Vec::new();
        let want_c = matches!(scope, DashScope::Global | DashScope::Containers);
        let want_v = matches!(scope, DashScope::Global | DashScope::Vms);
        let want_n = matches!(scope, DashScope::Global | DashScope::Networks);
        let want_s = matches!(scope, DashScope::Global | DashScope::Storage);
        if want_c {
            for (name, st, img, starttime) in &containers {
                let up = starttime
                    .and_then(super::output::uptime_from_starttime)
                    .map(super::output::fmt_duration_secs)
                    .unwrap_or_else(|| "-".to_string());
                rows.push(Row {
                    kind: "Container".into(),
                    name: name.clone(),
                    status: st.to_string(),
                    up,
                    extra: img.clone(),
                    ok: *st == Status::Running,
                });
            }
        }
        if want_v {
            for v in &vms {
                rows.push(Row {
                    kind: "Vm".into(),
                    name: v.name.clone(),
                    status: v.status.to_string(),
                    up: "-".to_string(),
                    extra: v.ip.clone().unwrap_or_default(),
                    ok: v.status == Status::Running,
                });
            }
        }
        if want_n {
            for n in &networks {
                rows.push(Row {
                    kind: "Network".into(),
                    name: n.name.clone(),
                    status: n.driver.clone(),
                    up: "-".to_string(),
                    extra: n.subnet.clone(),
                    ok: true,
                });
            }
        }
        if want_s {
            for vol in &volumes {
                rows.push(Row {
                    kind: "Volume".into(),
                    name: vol.name.clone(),
                    status: vol.driver.clone(),
                    up: "-".to_string(),
                    extra: vol.mountpoint.clone(),
                    ok: true,
                });
            }
        }
        if matches!(scope, DashScope::Images) {
            for img in &images {
                let name = img
                    .repo_tags
                    .first()
                    .cloned()
                    .unwrap_or_else(|| img.short_id());
                rows.push(Row {
                    kind: "Image".into(),
                    name,
                    status: img.short_id(),
                    up: "-".to_string(),
                    extra: format!("{} layers", img.layers.len()),
                    ok: true,
                });
            }
        }

        // --- problems: derived from LIVE state (not from a manifest) ---
        let problems = derive_problems(&containers, &vms);

        Ok(DashData {
            scope,
            tiles,
            rows,
            problems,
            activity: c_running as u64,
            mem_bytes: summary.memory_bytes_used,
        })
    }
}

/// Problems = resources in an unhealthy state (the red panel). Pure over the
/// already-reconciled states — split out to be testable without stores.
fn derive_problems(
    containers: &[(String, Status, String, Option<u64>)],
    vms: &[delonix_runtime_core::Vm],
) -> Vec<Problem> {
    let mut out = Vec::new();
    for (name, st, _, _) in containers {
        match st {
            Status::Failed(code) => out.push(Problem {
                resource: format!("container/{name}"),
                message: po::tf("exited with code {code}", &[("code", &code.to_string())]),
            }),
            Status::Crashed => out.push(Problem {
                resource: format!("container/{name}"),
                message: po::t("killed by signal (crash)").to_string(),
            }),
            _ => {}
        }
    }
    for v in vms {
        if matches!(v.status, Status::Failed(_) | Status::Crashed) {
            out.push(Problem {
                resource: format!("vm/{}", v.name),
                message: po::tf("state {state}", &[("state", &v.status.to_string())]),
            });
        }
    }
    out
}

// ===========================================================================
// Text snapshot (ANSI) — `--once` / no tty
// ===========================================================================

const RESET: &str = "\x1b[0m";
const ORANGE: &str = "\x1b[38;5;208m";
const RED: &str = "\x1b[38;5;203m";
const GREEN: &str = "\x1b[38;5;114m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";

/// Formats the snapshot as colored text (ANSI). Pure over `DashData` — the test
/// exercises it with fabricated data, without stores or terminal.
pub fn render_snapshot(d: &DashData, color: bool) -> String {
    let c = |code: &'static str| if color { code } else { "" };
    let mut out = String::new();
    out.push_str(&format!(
        "{}{}{}{}\n\n",
        c(BOLD),
        c(ORANGE),
        d.scope.title(),
        c(RESET)
    ));

    // Tiles — one line "LABEL  value (sub)".
    for t in &d.tiles {
        out.push_str(&format!(
            "  {}{:<12}{} {}{}{:>8}{} {}{}{}\n",
            c(DIM),
            t.label,
            c(RESET),
            c(BOLD),
            c(ORANGE),
            t.value,
            c(RESET),
            c(DIM),
            t.sub,
            c(RESET)
        ));
    }
    out.push('\n');

    // Resource table.
    if !d.rows.is_empty() {
        let mut t = super::output::Table::new(&["KIND", "NAME", "STATUS", "UP", "INFO"]);
        for r in &d.rows {
            let st = if color {
                format!("{}{}{}", if r.ok { GREEN } else { RED }, r.status, RESET)
            } else {
                r.status.clone()
            };
            t.row(vec![
                r.kind.clone(),
                r.name.clone(),
                st,
                r.up.clone(),
                r.extra.clone(),
            ]);
        }
        out.push_str(&t.render_all());
        out.push('\n');
    }

    // Problems panel.
    if d.problems.is_empty() {
        out.push_str(&format!(
            "{}{}✓ {}{}\n",
            c(BOLD),
            c(GREEN),
            po::t("no problems identified"),
            c(RESET)
        ));
    } else {
        out.push_str(&format!(
            "{}{}⚠ {}{}\n",
            c(BOLD),
            c(RED),
            po::tf(
                "PROBLEMS IDENTIFIED ({n})",
                &[("n", &d.problems.len().to_string())]
            ),
            c(RESET)
        ));
        for p in &d.problems {
            out.push_str(&format!(
                "  {}{}{} — {}\n",
                c(RED),
                p.resource,
                c(RESET),
                p.message
            ));
        }
    }
    out
}

// ===========================================================================
// Entrypoint
// ===========================================================================

/// Runs the dashboard. `json` → one `DashData` snapshot as JSON (scripts/CI/
/// Grafana JSON-datasource, no ANSI); `once` (or non-tty stdout) → one ANSI
/// text snapshot; otherwise, the interactive TUI.
pub fn run(scope: DashScope, once: bool, json: bool) -> Result<()> {
    if json {
        let data = DashData::collect(scope)?;
        let out = serde_json::to_string_pretty(&data)
            .map_err(|e| delonix_runtime_core::Error::Invalid(format!("dash --json: {e}")))?;
        println!("{out}");
        return Ok(());
    }
    let is_tty = std::io::stdout().is_terminal();
    if once || !is_tty {
        let data = DashData::collect(scope)?;
        print!("{}", render_snapshot(&data, is_tty));
        return Ok(());
    }
    tui::run_interactive(scope)
}

// ===========================================================================
// interactive TUI (ratatui) — thin shell; the logic lives in DashData/render
// ===========================================================================

mod tui {
    use super::*;
    use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use ratatui::crossterm::{execute, terminal};
    use ratatui::layout::{Constraint, Direction, Layout, Rect};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row as TRow, Sparkline, Table};
    use ratatui::Terminal;
    use std::io::stdout;
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// How long to WAIT, at minimum, after a completed (or timed-out)
    /// expensive collect before starting the next one. Independent of the 1s
    /// tile/table refresh — see `render`. NOT a periodic guarantee: a slow
    /// but legitimate collect (measured live at ~1 minute on a host with
    /// heavy containers) already pushes the real cadence well past this.
    const SLOW_REFRESH: Duration = Duration::from_secs(15);
    /// Ceiling on a single expensive collect — independent of `SLOW_REFRESH`
    /// (a real collect can legitimately take longer than the retry cadence;
    /// conflating the two would make a normal slow host look "timed out"
    /// forever). Matches `delonix-mgmt`'s equivalent background refresh.
    const COLLECT_TIMEOUT: Duration = Duration::from_secs(120);

    pub fn run_interactive(scope: DashScope) -> Result<()> {
        let root = state_root();
        // Cheap-only FIRST snapshot (counts + cgroup memory, no netns reads,
        // no disk walk): MEASURED live on a host with heavy containers, the
        // full collection took over a minute — starting the TUI with that
        // inline would leave the terminal looking hung before a single frame
        // draws. Collected BEFORE touching the terminal: if it fails, return
        // the error with the terminal still intact (no raw mode / alt screen
        // left to clean up). From here on, ALL exit paths restore the
        // terminal (the central `render` function does the cleanup once at
        // the end).
        let cheap = delonix_mgmt::dashstats::collect(&root, false, false);
        let data = DashData::collect_with(scope, &root, cheap.clone())?;

        // The background thread owns the expensive half from here on,
        // publishing into `shared` every `SLOW_REFRESH` — `render`'s 1s tick
        // only ever reads it, never blocks on it.
        let shared = Arc::new(Mutex::new(cheap));
        {
            let shared = shared.clone();
            let root = root.clone();
            thread::spawn(move || loop {
                // BUG FOUND (code review): this used to call `collect` (no
                // ceiling) directly — a genuinely stuck disk/netns operation
                // would freeze this background thread forever, silently
                // leaving the MEMORY/TRAFFIC/STORAGE tiles stuck at whatever
                // they last showed for the rest of the TUI session (with no
                // indication anything was wrong). Bounded the same way the
                // mgmt server's equivalent refresh loop is.
                // `.ok()` folds TimedOut and Skipped together on purpose here:
                // the TUI has nowhere to put a diagnostic (it owns the whole
                // terminal), so either way it just keeps showing the last known
                // values. The mgmt loop, which HAS a log, distinguishes them.
                if let Some(full) = delonix_mgmt::dashstats::collect_with_timeout(
                    &root,
                    true,
                    true,
                    COLLECT_TIMEOUT,
                )
                .ok()
                {
                    if let Ok(mut slot) = shared.lock() {
                        *slot = full;
                    }
                }
                thread::sleep(SLOW_REFRESH);
            });
        }

        terminal::enable_raw_mode().ok();
        execute!(stdout(), terminal::EnterAlternateScreen).ok();
        let res = render(scope, data, root, shared);
        // ALWAYS restore (even if `render` returned Err) — otherwise the shell
        // is left with no echo and on the alternate screen.
        terminal::disable_raw_mode().ok();
        execute!(
            stdout(),
            terminal::LeaveAlternateScreen,
            ratatui::crossterm::cursor::Show
        )
        .ok();
        res
    }

    /// Which series the bottom sparkline plots — toggled with the `m` key.
    /// Two histories are tracked unconditionally (both are cheap, already
    /// part of every `DashData` snapshot) so switching never loses trend.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum HistoryMode {
        Activity,
        Memory,
    }

    /// The drawing loop proper. Split out so that `run_interactive`
    /// can restore the terminal AFTERWARDS, whatever happens in here.
    /// `shared` is the background thread's latest EXPENSIVE summary (see
    /// `run_interactive`) — every 1s tick merges its network/storage fields
    /// onto a fresh CHEAP collection, so counts/memory are always current
    /// and network/storage are current as of the last `SLOW_REFRESH`.
    fn render(
        scope: DashScope,
        mut data: DashData,
        root: std::path::PathBuf,
        shared: Arc<Mutex<delonix_mgmt::dashstats::DashSummary>>,
    ) -> Result<()> {
        let backend = ratatui::backend::CrosstermBackend::new(stdout());
        let mut term = Terminal::new(backend).map_err(io_err)?;
        let mut activity_hist: VecDeque<u64> = VecDeque::with_capacity(120);
        let mut mem_hist: VecDeque<u64> = VecDeque::with_capacity(120);
        let mut mode = HistoryMode::Memory;
        let mut last = Instant::now() - Duration::from_secs(2);

        loop {
            // Collect every ~1s (not every frame).
            if last.elapsed() >= Duration::from_secs(1) {
                let mut summary = delonix_mgmt::dashstats::collect(&root, false, false);
                if let Ok(slow) = shared.lock() {
                    summary.network_rx_bytes = slow.network_rx_bytes;
                    summary.network_tx_bytes = slow.network_tx_bytes;
                    summary.storage_bytes_images = slow.storage_bytes_images;
                    summary.storage_bytes_volumes = slow.storage_bytes_volumes;
                    summary.storage_bytes_vm_images = slow.storage_bytes_vm_images;
                    summary.storage_bytes_containers = slow.storage_bytes_containers;
                }
                data = DashData::collect_with(scope, &root, summary).unwrap_or(data);
                activity_hist.push_back(data.activity);
                // MiB, not raw bytes: the `Sparkline` axis reads far better at
                // a human granularity, and 120 samples of a multi-GiB value
                // in bytes risks nothing but does nobody any favours either.
                mem_hist.push_back(data.mem_bytes / (1024 * 1024));
                if activity_hist.len() > 120 {
                    activity_hist.pop_front();
                }
                if mem_hist.len() > 120 {
                    mem_hist.pop_front();
                }
                last = Instant::now();
            }
            let hist: &[u64] = match mode {
                HistoryMode::Activity => activity_hist.make_contiguous(),
                HistoryMode::Memory => mem_hist.make_contiguous(),
            };
            if let Err(e) = term.draw(|f| draw(f, &data, mode, hist)) {
                break Err(io_err(e));
            }
            // Keyboard poll with a short timeout (keeps the refresh smooth).
            match event::poll(Duration::from_millis(200)) {
                Ok(true) => {
                    if let Ok(Event::Key(k)) = event::read() {
                        if k.kind == KeyEventKind::Press {
                            let quit = matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
                                || (k.code == KeyCode::Char('c')
                                    && k.modifiers.contains(KeyModifiers::CONTROL));
                            if quit {
                                break Ok(());
                            }
                            if k.code == KeyCode::Char('m') {
                                mode = match mode {
                                    HistoryMode::Activity => HistoryMode::Memory,
                                    HistoryMode::Memory => HistoryMode::Activity,
                                };
                            }
                        }
                    }
                }
                Ok(false) => {}
                Err(e) => break Err(io_err(e)),
            }
        }
    }

    fn io_err(e: std::io::Error) -> delonix_runtime_core::Error {
        delonix_runtime_core::Error::Invalid(format!("dash TUI: {e}"))
    }

    const ORANGE: Color = Color::Rgb(255, 140, 0);
    const REDC: Color = Color::Rgb(230, 70, 60);
    const GREENC: Color = Color::Rgb(120, 200, 120);

    fn draw(f: &mut ratatui::Frame, d: &DashData, mode: HistoryMode, hist: &[u64]) {
        let area = f.area();
        let root = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // title
                Constraint::Length(4), // tiles
                Constraint::Min(6),    // table + problems
                Constraint::Length(7), // sparkline
                Constraint::Length(1), // footer
            ])
            .split(area);

        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                d.scope.title(),
                Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
            ))),
            root[0],
        );

        draw_tiles(f, root[1], &d.tiles);

        let mid = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(root[2]);
        draw_table(f, mid[0], &d.rows);
        draw_problems(f, mid[1], &d.problems);

        draw_sparkline(f, root[3], mode, hist);

        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(
                    "  {}",
                    po::t("q/Esc: quit   ·   m: toggle chart   ·   refresh 1s")
                ),
                Style::default().fg(Color::DarkGray),
            ))),
            root[4],
        );
    }

    fn draw_tiles(f: &mut ratatui::Frame, area: Rect, tiles: &[Tile]) {
        if tiles.is_empty() {
            return;
        }
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(vec![Constraint::Ratio(1, tiles.len() as u32); tiles.len()])
            .split(area);
        for (i, t) in tiles.iter().enumerate() {
            let body = vec![
                Line::from(Span::styled(
                    &t.value,
                    Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(&t.sub, Style::default().fg(Color::DarkGray))),
            ];
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    t.label.clone(),
                    Style::default().fg(Color::Gray),
                ));
            f.render_widget(Paragraph::new(body).block(block), cols[i]);
        }
    }

    fn draw_table(f: &mut ratatui::Frame, area: Rect, rows: &[Row]) {
        let header = TRow::new(["KIND", "NAME", "STATUS", "UP", "INFO"].map(|h| {
            Cell::from(h).style(
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            )
        }));
        let trows: Vec<TRow> = rows
            .iter()
            .map(|r| {
                let sc = if r.ok { GREENC } else { REDC };
                TRow::new(vec![
                    Cell::from(r.kind.clone()),
                    Cell::from(r.name.clone()),
                    Cell::from(r.status.clone()).style(Style::default().fg(sc)),
                    Cell::from(r.up.clone()).style(Style::default().fg(Color::DarkGray)),
                    Cell::from(r.extra.clone()).style(Style::default().fg(Color::DarkGray)),
                ])
            })
            .collect();
        let widths = [
            Constraint::Length(10),
            Constraint::Percentage(30),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Percentage(35),
        ];
        let table = Table::new(trows, widths).header(header).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(po::t("RESOURCES")),
        );
        f.render_widget(table, area);
    }

    fn draw_problems(f: &mut ratatui::Frame, area: Rect, problems: &[Problem]) {
        let (title, border) = if problems.is_empty() {
            ("OK", GREENC)
        } else {
            (po::t("PROBLEM IDENTIFIED"), REDC)
        };
        let mut lines: Vec<Line<'static>> = Vec::new();
        if problems.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("✓ {}", po::t("no problems")),
                Style::default().fg(GREENC),
            )));
        } else {
            for p in problems {
                lines.push(Line::from(Span::styled(
                    p.resource.clone(),
                    Style::default().fg(REDC).add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(Span::styled(
                    format!("  {}", p.message),
                    Style::default().fg(Color::Gray),
                )));
            }
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border))
            .title(Span::styled(
                title,
                Style::default().fg(border).add_modifier(Modifier::BOLD),
            ));
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_sparkline(f: &mut ratatui::Frame, area: Rect, mode: HistoryMode, hist: &[u64]) {
        let title = match mode {
            HistoryMode::Activity => po::t("RUNNING CONTAINERS (last 2 min) — press m for memory"),
            HistoryMode::Memory => po::t("MEMORY USAGE, MiB (last 2 min) — press m for containers"),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(title, Style::default().fg(Color::Gray)));
        let spark = Sparkline::default()
            .block(block)
            .data(hist)
            .style(Style::default().fg(ORANGE));
        f.render_widget(spark, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DashData {
        DashData {
            scope: DashScope::Global,
            tiles: vec![tile("CONTAINERS", "2/3", "running / total")],
            rows: vec![
                Row {
                    kind: "Container".into(),
                    name: "web".into(),
                    status: "running".into(),
                    up: "2h3m".into(),
                    extra: "nginx".into(),
                    ok: true,
                },
                Row {
                    kind: "Container".into(),
                    name: "db".into(),
                    status: "crashed".into(),
                    up: "-".into(),
                    extra: "postgres".into(),
                    ok: false,
                },
            ],
            problems: vec![Problem {
                resource: "container/db".into(),
                message: "killed by signal (crash)".into(),
            }],
            activity: 2,
            mem_bytes: 128 * 1024 * 1024,
        }
    }

    #[test]
    fn snapshot_sem_cor_tem_titulo_tiles_e_problemas() {
        let s = render_snapshot(&sample(), false);
        assert!(s.contains("DELONIX — SUMMARY"));
        assert!(s.contains("CONTAINERS"));
        assert!(s.contains("2/3"));
        assert!(s.contains("web"));
        assert!(s.contains("2h3m"));
        assert!(s.contains("PROBLEMS IDENTIFIED (1)"));
        assert!(s.contains("container/db"));
        // no color → no ANSI sequences.
        assert!(!s.contains("\x1b["));
    }

    #[test]
    fn snapshot_com_cor_tem_ansi() {
        let s = render_snapshot(&sample(), true);
        assert!(s.contains("\x1b["));
    }

    #[test]
    fn sem_problemas_mostra_ok() {
        let mut d = sample();
        d.problems.clear();
        let s = render_snapshot(&d, false);
        assert!(s.contains("no problems identified"));
        assert!(!s.contains("PROBLEMS IDENTIFIED"));
    }

    #[test]
    fn dash_data_serializa_para_json() {
        let s = sample();
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"scope\":\"global\""));
        assert!(json.contains("\"name\":\"web\""));
        assert!(json.contains("\"up\":\"2h3m\""));
        assert!(json.contains("\"mem_bytes\":134217728"));
    }

    #[test]
    fn derive_problems_apanha_crash_e_failed() {
        let containers = vec![
            (
                "web".to_string(),
                Status::Running,
                "nginx".to_string(),
                None,
            ),
            ("db".to_string(), Status::Crashed, "pg".to_string(), None),
            (
                "job".to_string(),
                Status::Failed(2),
                "busybox".to_string(),
                None,
            ),
        ];
        let ps = derive_problems(&containers, &[]);
        assert_eq!(ps.len(), 2);
        assert!(ps.iter().any(|p| p.resource == "container/db"));
        assert!(ps
            .iter()
            .any(|p| p.resource == "container/job" && p.message.contains("code 2")));
    }
}
