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

use std::collections::{HashMap, VecDeque};
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

    /// Short label for the in-TUI scope tabs (`Tab`/`Shift+Tab`/`1`-`6`) —
    /// distinct from [`Self::title`], which is the full banner line. Plain
    /// (not `po::t`-wrapped), same convention already used by the table's
    /// column headers (`KIND`/`NAME`/... below) — short technical chrome,
    /// not prose.
    fn short_label(self) -> &'static str {
        match self {
            DashScope::Global => "All",
            DashScope::Containers => "Containers",
            DashScope::Vms => "VMs",
            DashScope::Networks => "Networks",
            DashScope::Storage => "Storage",
            DashScope::Images => "Images",
        }
    }

    /// Fixed order the in-TUI tabs cycle through — also what `1`-`6` index into.
    const ALL: [DashScope; 6] = [
        DashScope::Global,
        DashScope::Containers,
        DashScope::Vms,
        DashScope::Networks,
        DashScope::Storage,
        DashScope::Images,
    ];

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|s| *s == self)
            .unwrap_or_default()
    }

    fn next(self) -> DashScope {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    fn prev(self) -> DashScope {
        let n = Self::ALL.len();
        Self::ALL[(self.index() + n - 1) % n]
    }

    fn from_digit(d: char) -> Option<DashScope> {
        d.to_digit(10)
            .and_then(|n| (n as usize).checked_sub(1))
            .and_then(|i| Self::ALL.get(i).copied())
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
    /// Raw monotonic `cpu.stat` counter (microseconds), for a Container only.
    /// NOT a percentage — the TUI keeps the previous sample per row and
    /// divides the delta by elapsed wall time (see `tui::render`'s
    /// `cpu_prev`). `None` for non-container kinds and for a stopped
    /// container (no live cgroup to read).
    pub cpu_usage_usec: Option<u64>,
    /// `memory.current` of the container's live cgroup, in bytes.
    pub mem_bytes: Option<u64>,
    /// Cumulative bytes read/written by the container's live cgroup
    /// (`io.stat`, summed across devices). `None` (not `Some(0)`) when the
    /// `io` controller isn't delegated to this cgroup — common in rootless.
    pub io_read_bytes: Option<u64>,
    pub io_write_bytes: Option<u64>,
    /// Cumulative rx/tx bytes across the container's interfaces (bytes since
    /// each interface's creation) — same figure as the aggregate TRAFFIC
    /// tile, broken down per container. Refreshed on the slow (network)
    /// cadence, not every tick (see `dashstats::collect_container_net`).
    pub net_rx_bytes: Option<u64>,
    pub net_tx_bytes: Option<u64>,
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
    /// `memory.current` of the whole Delonix cgroup slice, in bytes — one of the two series the sparklines show, and the source of the MEMORY tile/gauge.
    pub mem_bytes: u64,
    /// `memory.max` of the whole Delonix cgroup slice, in bytes (0 = unlimited/unknown) — lets the TUI draw a MEMORY `Gauge` (ratio of `mem_bytes`/this); with no configured limit the ratio is undefined and the TUI falls back to the plain tile.
    pub mem_bytes_limit: u64,
    /// Whole-slice `cpu.stat` `usage_usec` RAW counter (see `ContainerUsage::cpu_usage_usec` for why this isn't a percentage). `None` if the slice's `cpu.stat` couldn't be read (same rootless-delegation caveats as everywhere else in this file).
    pub cpu_usage_usec: Option<u64>,
}

/// One entry of the containers scratch list built by `DashData::collect_with`
/// before it's turned into `Row`s: name/status/image/starttime/id/live
/// resource usage/per-container network bytes. Named here (instead of an
/// inline tuple type) purely to keep clippy's `type_complexity` quiet — it
/// carries no behavior of its own.
type ContainerRow = (
    String,
    Status,
    String,
    Option<u64>,
    String,
    delonix_runtime::ContainerUsage,
    Option<(u64, u64)>,
);

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
        // Per-container network breakdown: same expensive-collection budget
        // as the aggregate TRAFFIC tile above (a one-shot `--once`/`--json`
        // caller already accepts a slow full collect — see `collect`'s doc
        // comment on `dashstats`).
        let per_container_net = delonix_mgmt::dashstats::collect_container_net(&root);
        Self::collect_with(scope, &root, summary, &per_container_net)
    }

    /// Like [`Self::collect`], but with the KPI summary supplied by the
    /// caller instead of computed here — lets the TUI reuse a cheap
    /// (counts/memory only) or background-refreshed (network/storage too)
    /// summary on its 1s tick, instead of paying the full collection cost on
    /// every single frame. `per_container_net` (id → (rx, tx)) is likewise
    /// caller-supplied for the same reason — it costs one `nsenter`+`cat`
    /// per running container, so the TUI keeps it on the same slow-refresh
    /// cadence as the network/storage tiles (empty map = "no per-container
    /// network data yet", never a misleading zero).
    pub fn collect_with(
        scope: DashScope,
        root: &std::path::Path,
        summary: delonix_mgmt::dashstats::DashSummary,
        per_container_net: &HashMap<String, (u64, u64)>,
    ) -> Result<DashData> {
        // --- containers (name/image/uptime/resource-usage for the table; counts too) ---
        let mut containers: Vec<ContainerRow> = Vec::new();
        if let Ok((_img, store)) = super::util::open_stores() {
            for mut c in store.list().unwrap_or_default() {
                delonix_runtime::reconcile_status(&mut c);
                let usage = delonix_runtime::container_usage(&c);
                let net = per_container_net.get(&c.id).copied();
                containers.push((
                    c.name.clone(),
                    c.status.clone(),
                    c.image.clone(),
                    c.pid_starttime,
                    c.id.clone(),
                    usage,
                    net,
                ));
            }
        }
        let c_running = containers
            .iter()
            .filter(|(_, s, ..)| *s == Status::Running)
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
            for (name, st, img, starttime, _id, usage, net) in &containers {
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
                    cpu_usage_usec: usage.cpu_usage_usec,
                    mem_bytes: usage.mem_bytes,
                    io_read_bytes: usage.io_read_bytes,
                    io_write_bytes: usage.io_write_bytes,
                    net_rx_bytes: net.map(|(r, _)| r),
                    net_tx_bytes: net.map(|(_, t)| t),
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
                    cpu_usage_usec: None,
                    mem_bytes: None,
                    io_read_bytes: None,
                    io_write_bytes: None,
                    net_rx_bytes: None,
                    net_tx_bytes: None,
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
                    cpu_usage_usec: None,
                    mem_bytes: None,
                    io_read_bytes: None,
                    io_write_bytes: None,
                    net_rx_bytes: None,
                    net_tx_bytes: None,
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
                    cpu_usage_usec: None,
                    mem_bytes: None,
                    io_read_bytes: None,
                    io_write_bytes: None,
                    net_rx_bytes: None,
                    net_tx_bytes: None,
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
                    cpu_usage_usec: None,
                    mem_bytes: None,
                    io_read_bytes: None,
                    io_write_bytes: None,
                    net_rx_bytes: None,
                    net_tx_bytes: None,
                });
            }
        }

        // --- problems: derived from LIVE state (not from a manifest) ---
        // `derive_problems` only needs the original 4 fields — mapped down
        // rather than widening its signature, so its existing unit tests
        // (fabricated 4-tuples) keep working untouched.
        let containers_for_problems: Vec<(String, Status, String, Option<u64>)> = containers
            .iter()
            .map(|(name, st, img, starttime, ..)| {
                (name.clone(), st.clone(), img.clone(), *starttime)
            })
            .collect();
        let problems = derive_problems(&containers_for_problems, &vms);

        Ok(DashData {
            scope,
            tiles,
            rows,
            problems,
            activity: c_running as u64,
            mem_bytes: summary.memory_bytes_used,
            mem_bytes_limit: summary.memory_bytes_limit,
            cpu_usage_usec: delonix_runtime::slice_cpu_usage_usec(),
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

/// Compact `R <size>/W <size>` for a table cell — `-` when neither side is
/// known (the `io` cgroup controller isn't delegated, see `ContainerUsage`'s
/// doc comment), never a fabricated `0 B`.
fn fmt_io_pair(read: Option<u64>, write: Option<u64>) -> String {
    match (read, write) {
        (None, None) => "-".to_string(),
        (r, w) => format!(
            "R {}/W {}",
            r.map(super::output::fmt_size)
                .unwrap_or_else(|| "-".to_string()),
            w.map(super::output::fmt_size)
                .unwrap_or_else(|| "-".to_string())
        ),
    }
}

/// Compact `↓<size>/↑<size>` for a table cell — cumulative bytes since the
/// container's interface(s) were created, same figure as the aggregate
/// TRAFFIC tile broken down per-container.
fn fmt_net_pair(rx: Option<u64>, tx: Option<u64>) -> String {
    match (rx, tx) {
        (None, None) => "-".to_string(),
        (r, t) => format!(
            "\u{2193}{}/\u{2191}{}",
            r.map(super::output::fmt_size)
                .unwrap_or_else(|| "-".to_string()),
            t.map(super::output::fmt_size)
                .unwrap_or_else(|| "-".to_string())
        ),
    }
}

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
        let mut t = super::output::Table::new(&[
            "KIND",
            "NAME",
            "STATUS",
            "UP",
            "CPU",
            "MEM",
            "I/O R/W",
            "NET DOWN/UP",
            "INFO",
        ]);
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
                // A one-shot snapshot has no previous sample to diff the raw
                // `cpu.stat` counter against — showing a rate here would mean
                // a SECOND collection just to compute a delta. `--once`/
                // `--json` show the counter is present, not a percentage;
                // the interactive TUI is what turns it into CPU%.
                r.cpu_usage_usec.map(|_| "n/a*").unwrap_or("-").to_string(),
                r.mem_bytes
                    .map(super::output::fmt_size)
                    .unwrap_or_else(|| "-".to_string()),
                fmt_io_pair(r.io_read_bytes, r.io_write_bytes),
                fmt_net_pair(r.net_rx_bytes, r.net_tx_bytes),
                r.extra.clone(),
            ]);
        }
        out.push_str(&t.render_all());
        out.push_str(&format!(
            "{}* {}{}\n",
            c(DIM),
            po::t("CPU%: interactive TUI only (needs two samples to compute a rate)"),
            c(RESET)
        ));
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
    use ratatui::widgets::{
        Block, Borders, Cell, Gauge, Paragraph, Row as TRow, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Sparkline, Table, TableState, Tabs,
    };
    use ratatui::Terminal;
    use std::io::stdout;
    use std::sync::atomic::{AtomicBool, Ordering};
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

    /// The background thread's latest expensive-collection result: the
    /// merged `DashSummary` fields (network/storage totals) AND the
    /// per-container network breakdown, bundled together so `render`'s 1s
    /// tick reads a single consistent snapshot instead of two that could be
    /// from different passes. `last_updated` is what the tiles' "(Xs ago)"
    /// age indicator reads — without it, a stalled slow refresh (a dead NFS
    /// mount, see `collect`'s own doc comment on that failure mode) looked
    /// identical to a fresh one.
    struct SlowSample {
        summary: delonix_mgmt::dashstats::DashSummary,
        per_container_net: HashMap<String, (u64, u64)>,
        last_updated: Instant,
    }

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
        let empty_net = HashMap::new();
        let data = DashData::collect_with(scope, &root, cheap.clone(), &empty_net)?;

        // The background thread owns the expensive half from here on,
        // publishing into `shared` every `SLOW_REFRESH` — `render`'s 1s tick
        // only ever reads it, never blocks on it. `collecting` is a pure UI
        // signal (a spinner next to the slow tiles) — it does not gate
        // anything.
        let shared = Arc::new(Mutex::new(SlowSample {
            summary: cheap,
            per_container_net: HashMap::new(),
            last_updated: Instant::now(),
        }));
        let collecting = Arc::new(AtomicBool::new(false));
        {
            let shared = shared.clone();
            let collecting = collecting.clone();
            let root = root.clone();
            thread::spawn(move || loop {
                collecting.store(true, Ordering::Relaxed);
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
                let full = delonix_mgmt::dashstats::collect_with_timeout(
                    &root,
                    true,
                    true,
                    COLLECT_TIMEOUT,
                )
                .ok();
                // Own latch (`CONTAINER_NET_IN_FLIGHT` in `dashstats`), own
                // timeout — a stuck netns read here must not also freeze the
                // unrelated network/storage totals above.
                let net = delonix_mgmt::dashstats::collect_container_net_with_timeout(
                    &root,
                    COLLECT_TIMEOUT,
                )
                .ok();
                if full.is_some() || net.is_some() {
                    if let Ok(mut slot) = shared.lock() {
                        if let Some(full) = full {
                            slot.summary = full;
                        }
                        if let Some(net) = net {
                            slot.per_container_net = net;
                        }
                        slot.last_updated = Instant::now();
                    }
                }
                collecting.store(false, Ordering::Relaxed);
                thread::sleep(SLOW_REFRESH);
            });
        }

        terminal::enable_raw_mode().ok();
        execute!(stdout(), terminal::EnterAlternateScreen).ok();
        let res = render(scope, data, root, shared, collecting);
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

    /// Which column the resource table is sorted by — cycled with `s`.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SortKey {
        Name,
        Status,
        Cpu,
        Mem,
    }

    impl SortKey {
        fn next(self) -> SortKey {
            match self {
                SortKey::Name => SortKey::Status,
                SortKey::Status => SortKey::Cpu,
                SortKey::Cpu => SortKey::Mem,
                SortKey::Mem => SortKey::Name,
            }
        }

        fn label(self) -> &'static str {
            match self {
                SortKey::Name => "name",
                SortKey::Status => "status",
                SortKey::Cpu => "cpu",
                SortKey::Mem => "mem",
            }
        }
    }

    /// Up/down/flat comparison of a metric against its previous tick's value
    /// — the tiles' trend arrow. `Unknown` on the very first tick, when
    /// there is no previous value to compare against yet.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Trend {
        Up,
        Down,
        Flat,
        Unknown,
    }

    impl Trend {
        fn of(prev: Option<u64>, cur: u64) -> Trend {
            match prev {
                None => Trend::Unknown,
                Some(p) if cur > p => Trend::Up,
                Some(p) if cur < p => Trend::Down,
                Some(_) => Trend::Flat,
            }
        }

        fn glyph(self) -> &'static str {
            match self {
                Trend::Up => "\u{2191}",
                Trend::Down => "\u{2193}",
                Trend::Flat => "\u{2192}",
                Trend::Unknown => "",
            }
        }

        fn color(self) -> Color {
            match self {
                Trend::Up => REDC, // more memory/traffic used is the "watch this" direction
                Trend::Down => GREENC,
                Trend::Flat => Color::DarkGray,
                Trend::Unknown => Color::DarkGray,
            }
        }
    }

    /// Trend of the tiles whose raw numbers are available tick-to-tick.
    /// `containers` uses "more running is good" semantics — the opposite
    /// coloring of memory/traffic — so it is intentionally NOT reused for
    /// those with a shared color rule.
    struct Trends {
        containers: Trend,
        memory: Trend,
        network: Trend,
        storage: Trend,
    }

    impl Trends {
        /// All-`Unknown` — the state before the very first tick has run
        /// (nothing to compare yet), and `for_tile` already treats
        /// `Unknown` as "don't draw an arrow", so callers never special-case
        /// this beyond constructing it.
        fn unknown() -> Trends {
            Trends {
                containers: Trend::Unknown,
                memory: Trend::Unknown,
                network: Trend::Unknown,
                storage: Trend::Unknown,
            }
        }

        fn compute(
            prev: Option<&delonix_mgmt::dashstats::DashSummary>,
            cur: &delonix_mgmt::dashstats::DashSummary,
        ) -> Trends {
            let prev_net =
                prev.map(|p| p.network_rx_bytes.unwrap_or(0) + p.network_tx_bytes.unwrap_or(0));
            let cur_net = cur.network_rx_bytes.unwrap_or(0) + cur.network_tx_bytes.unwrap_or(0);
            let prev_storage = prev.map(|p| {
                p.storage_bytes_images.unwrap_or(0)
                    + p.storage_bytes_volumes.unwrap_or(0)
                    + p.storage_bytes_vm_images.unwrap_or(0)
                    + p.storage_bytes_containers.unwrap_or(0)
            });
            let cur_storage = cur.storage_bytes_images.unwrap_or(0)
                + cur.storage_bytes_volumes.unwrap_or(0)
                + cur.storage_bytes_vm_images.unwrap_or(0)
                + cur.storage_bytes_containers.unwrap_or(0);
            Trends {
                containers: Trend::of(prev.map(|p| p.containers_running), cur.containers_running),
                memory: Trend::of(prev.map(|p| p.memory_bytes_used), cur.memory_bytes_used),
                network: Trend::of(prev_net, cur_net),
                storage: Trend::of(prev_storage, cur_storage),
            }
        }

        fn for_tile(&self, label: &str) -> Trend {
            match label {
                "CONTAINERS" => self.containers,
                "MEMORY" => self.memory,
                "TRAFFIC" => self.network,
                "STORAGE" => self.storage,
                _ => Trend::Unknown,
            }
        }
    }

    /// The drawing loop proper. Split out so that `run_interactive`
    /// can restore the terminal AFTERWARDS, whatever happens in here.
    /// `shared` is the background thread's latest EXPENSIVE summary (see
    /// `run_interactive`) — every 1s tick merges its network/storage fields
    /// onto a fresh CHEAP collection, so counts/memory are always current
    /// and network/storage are current as of the last `SLOW_REFRESH`.
    fn render(
        mut scope: DashScope,
        mut data: DashData,
        root: std::path::PathBuf,
        shared: Arc<Mutex<SlowSample>>,
        collecting: Arc<AtomicBool>,
    ) -> Result<()> {
        let backend = ratatui::backend::CrosstermBackend::new(stdout());
        let mut term = Terminal::new(backend).map_err(io_err)?;
        let mut activity_hist: VecDeque<u64> = VecDeque::with_capacity(120);
        let mut mem_hist: VecDeque<u64> = VecDeque::with_capacity(120);
        let mut last = Instant::now() - Duration::from_secs(2);

        let mut table_state = TableState::default();
        let mut sort_key = SortKey::Name;
        let mut filter = String::new();
        let mut searching = false;
        // Raw `cpu.stat` counters, one previous sample per row (keyed by
        // "KIND:NAME" — unique because container/VM names are already
        // unique within their own store) and one for the whole-slice CPU
        // tile — see `Row::cpu_usage_usec`'s doc comment for why a rate
        // needs two samples and cannot come out of a single collection.
        let mut cpu_prev: HashMap<String, (Instant, u64)> = HashMap::new();
        let mut slice_cpu_prev: Option<(Instant, u64)> = None;
        let mut cur_summary: Option<delonix_mgmt::dashstats::DashSummary> = None;
        let mut prev_summary: Option<delonix_mgmt::dashstats::DashSummary> = None;
        let mut slow_age = Duration::from_secs(0);

        loop {
            // Collect every ~1s (not every frame).
            if last.elapsed() >= Duration::from_secs(1) {
                let mut summary = delonix_mgmt::dashstats::collect(&root, false, false);
                let mut per_container_net = HashMap::new();
                if let Ok(slow) = shared.lock() {
                    summary.network_rx_bytes = slow.summary.network_rx_bytes;
                    summary.network_tx_bytes = slow.summary.network_tx_bytes;
                    summary.storage_bytes_images = slow.summary.storage_bytes_images;
                    summary.storage_bytes_volumes = slow.summary.storage_bytes_volumes;
                    summary.storage_bytes_vm_images = slow.summary.storage_bytes_vm_images;
                    summary.storage_bytes_containers = slow.summary.storage_bytes_containers;
                    per_container_net = slow.per_container_net.clone();
                    slow_age = slow.last_updated.elapsed();
                }
                prev_summary = cur_summary.take();
                cur_summary = Some(summary.clone());
                data = DashData::collect_with(scope, &root, summary, &per_container_net)
                    .unwrap_or(data);
                activity_hist.push_back(data.activity);
                // MiB, not raw bytes: the sparkline axis reads far better at
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

            // CPU% is TUI-owned state (see `cpu_prev`'s doc comment) —
            // recomputed every FRAME (not just every tick) so a fast poll
            // loop doesn't visibly stall the percentage between ticks; the
            // underlying counters only actually change once per tick.
            let now = Instant::now();
            let mut cpu_pct: HashMap<String, f64> = HashMap::new();
            for r in &data.rows {
                if let Some(cur) = r.cpu_usage_usec {
                    let key = format!("{}:{}", r.kind, r.name);
                    if let Some((t0, u0)) = cpu_prev.get(&key).copied() {
                        let elapsed = now.duration_since(t0);
                        if elapsed.as_millis() > 0 && cur >= u0 {
                            let pct = (cur - u0) as f64 / elapsed.as_micros() as f64 * 100.0;
                            cpu_pct.insert(key.clone(), pct);
                        }
                    }
                    cpu_prev.insert(key, (now, cur));
                }
            }
            let cpu_tile_pct = data.cpu_usage_usec.and_then(|cur| {
                let pct = slice_cpu_prev.and_then(|(t0, u0)| {
                    let elapsed = now.duration_since(t0);
                    if elapsed.as_millis() > 0 && cur >= u0 {
                        Some((cur - u0) as f64 / elapsed.as_micros() as f64 * 100.0)
                    } else {
                        None
                    }
                });
                slice_cpu_prev = Some((now, cur));
                pct
            });

            // `cur_summary` is `None` only before the very first tick has
            // run — which cannot happen here (`last` starts 2s in the past,
            // so the tick block above always fires on the loop's first
            // iteration) — but handled without a fallback collection
            // either way, since a "just in case" extra collect would be
            // wasted I/O on every frame if the reasoning above is ever wrong.
            let trends = match cur_summary.as_ref() {
                Some(cur) => Trends::compute(prev_summary.as_ref(), cur),
                None => Trends::unknown(),
            };

            let needle = filter.to_lowercase();
            let mut visible: Vec<&Row> = data
                .rows
                .iter()
                .filter(|r| needle.is_empty() || r.name.to_lowercase().contains(&needle))
                .collect();
            sort_rows(&mut visible, sort_key, &cpu_pct);
            if visible.is_empty() {
                table_state.select(None);
            } else {
                let sel = table_state.selected().unwrap_or(0).min(visible.len() - 1);
                table_state.select(Some(sel));
            }

            let spinner = if collecting.load(Ordering::Relaxed) {
                const FRAMES: [&str; 4] = ["\u{280B}", "\u{2819}", "\u{2839}", "\u{2838}"];
                FRAMES[(now.elapsed().subsec_millis() / 250) as usize % FRAMES.len()]
            } else {
                ""
            };

            if let Err(e) = term.draw(|f| {
                draw(
                    f,
                    scope,
                    &data,
                    &visible,
                    &mut table_state,
                    sort_key,
                    &filter,
                    searching,
                    &cpu_pct,
                    cpu_tile_pct,
                    &trends,
                    slow_age,
                    spinner,
                    &activity_hist,
                    &mem_hist,
                )
            }) {
                break Err(io_err(e));
            }

            // Keyboard poll with a short timeout (keeps the refresh smooth).
            match event::poll(Duration::from_millis(200)) {
                Ok(true) => {
                    if let Ok(Event::Key(k)) = event::read() {
                        if k.kind != KeyEventKind::Press {
                            continue;
                        }
                        if searching {
                            match k.code {
                                KeyCode::Esc => {
                                    filter.clear();
                                    searching = false;
                                }
                                KeyCode::Enter => searching = false,
                                KeyCode::Backspace => {
                                    filter.pop();
                                }
                                KeyCode::Char(c) => filter.push(c),
                                _ => {}
                            }
                            continue;
                        }
                        let quit = matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
                            || (k.code == KeyCode::Char('c')
                                && k.modifiers.contains(KeyModifiers::CONTROL));
                        if quit {
                            break Ok(());
                        }
                        match k.code {
                            KeyCode::Char('/') => searching = true,
                            KeyCode::Char('s') => sort_key = sort_key.next(),
                            KeyCode::Tab => {
                                scope = scope.next();
                                table_state.select(None);
                                last = Instant::now() - Duration::from_secs(2); // force an immediate re-collect for the new scope
                            }
                            KeyCode::BackTab => {
                                scope = scope.prev();
                                table_state.select(None);
                                last = Instant::now() - Duration::from_secs(2);
                            }
                            KeyCode::Char(c) if c.is_ascii_digit() => {
                                if let Some(s) = DashScope::from_digit(c) {
                                    scope = s;
                                    table_state.select(None);
                                    last = Instant::now() - Duration::from_secs(2);
                                }
                            }
                            KeyCode::Down => {
                                if !visible.is_empty() {
                                    let n = table_state.selected().unwrap_or(0);
                                    table_state.select(Some((n + 1).min(visible.len() - 1)));
                                }
                            }
                            KeyCode::Up => {
                                let n = table_state.selected().unwrap_or(0);
                                table_state.select(Some(n.saturating_sub(1)));
                            }
                            KeyCode::Home => {
                                table_state.select(if visible.is_empty() { None } else { Some(0) })
                            }
                            KeyCode::End if !visible.is_empty() => {
                                table_state.select(Some(visible.len() - 1));
                            }
                            _ => {}
                        }
                    }
                }
                Ok(false) => {}
                Err(e) => break Err(io_err(e)),
            }
        }
    }

    /// Sorts the VISIBLE rows in place — always by a stable secondary key
    /// (name) so equal-metric rows don't jitter position between ticks.
    /// `cpu_pct` is keyed the same way as `render`'s `cpu_prev` (`"KIND:NAME"`).
    fn sort_rows(rows: &mut [&Row], key: SortKey, cpu_pct: &HashMap<String, f64>) {
        rows.sort_by(|a, b| match key {
            SortKey::Name => a.name.cmp(&b.name),
            SortKey::Status => a.status.cmp(&b.status).then_with(|| a.name.cmp(&b.name)),
            SortKey::Cpu => {
                let ka = format!("{}:{}", a.kind, a.name);
                let kb = format!("{}:{}", b.kind, b.name);
                let pa = cpu_pct.get(&ka).copied().unwrap_or(-1.0);
                let pb = cpu_pct.get(&kb).copied().unwrap_or(-1.0);
                pb.partial_cmp(&pa)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.name.cmp(&b.name))
            }
            SortKey::Mem => b
                .mem_bytes
                .unwrap_or(0)
                .cmp(&a.mem_bytes.unwrap_or(0))
                .then_with(|| a.name.cmp(&b.name)),
        });
    }

    fn io_err(e: std::io::Error) -> delonix_runtime_core::Error {
        delonix_runtime_core::Error::Invalid(format!("dash TUI: {e}"))
    }

    const ORANGE: Color = Color::Rgb(255, 140, 0);
    const REDC: Color = Color::Rgb(230, 70, 60);
    const GREENC: Color = Color::Rgb(120, 200, 120);
    const YELLOWC: Color = Color::Rgb(230, 200, 60);

    #[allow(clippy::too_many_arguments)]
    fn draw(
        f: &mut ratatui::Frame,
        scope: DashScope,
        d: &DashData,
        visible: &[&Row],
        table_state: &mut TableState,
        sort_key: SortKey,
        filter: &str,
        searching: bool,
        cpu_pct: &HashMap<String, f64>,
        cpu_tile_pct: Option<f64>,
        trends: &Trends,
        slow_age: Duration,
        spinner: &str,
        activity_hist: &VecDeque<u64>,
        mem_hist: &VecDeque<u64>,
    ) {
        let area = f.area();
        let root = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // title
                Constraint::Length(1), // scope tabs
                Constraint::Length(4), // tiles
                Constraint::Min(6),    // table + problems
                Constraint::Length(6), // sparklines (side by side)
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

        draw_tabs(f, root[1], scope);
        draw_tiles(
            f,
            root[2],
            &d.tiles,
            d.mem_bytes,
            d.mem_bytes_limit,
            cpu_tile_pct,
            trends,
            slow_age,
            spinner,
        );

        let mid = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(root[3]);
        draw_table(f, mid[0], visible, table_state, sort_key, cpu_pct);
        draw_problems(f, mid[1], &d.problems);

        draw_sparklines(f, root[4], activity_hist, mem_hist);

        let footer = if searching {
            format!(
                "  /{filter}\u{2588}   {}",
                po::t("(Enter: apply · Esc: clear)")
            )
        } else {
            format!(
                "  {}",
                po::tf(
                    "q/Esc: quit · \u{2191}\u{2193}: select · Tab/1-6: scope · /: search · s: sort ({sort}) · refresh 1s",
                    &[("sort", sort_key.label())],
                )
            )
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                footer,
                Style::default().fg(Color::DarkGray),
            ))),
            root[5],
        );
    }

    fn draw_tabs(f: &mut ratatui::Frame, area: Rect, scope: DashScope) {
        let titles: Vec<Line> = DashScope::ALL
            .iter()
            .map(|s| Line::from(format!(" {} ", s.short_label())))
            .collect();
        let tabs = Tabs::new(titles)
            .select(scope.index())
            .style(Style::default().fg(Color::DarkGray))
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(ORANGE)
                    .add_modifier(Modifier::BOLD),
            )
            .divider("");
        f.render_widget(tabs, area);
    }

    /// `label:VALUE` painted with a trend arrow when one is available (only
    /// the tiles whose raw numbers `Trends` tracks get one). The MEMORY tile
    /// becomes a live `Gauge` — filled bar, colored by how close to the
    /// configured limit — when a limit is actually configured; with no
    /// limit the ratio is undefined and it stays the plain text tile,
    /// never a fabricated percentage.
    #[allow(clippy::too_many_arguments)]
    fn draw_tiles(
        f: &mut ratatui::Frame,
        area: Rect,
        tiles: &[Tile],
        mem_bytes: u64,
        mem_bytes_limit: u64,
        cpu_tile_pct: Option<f64>,
        trends: &Trends,
        slow_age: Duration,
        spinner: &str,
    ) {
        if tiles.is_empty() {
            return;
        }
        // Insert a CPU tile right after MEMORY when present (Global/
        // Containers scopes) — computed by the TUI (needs two samples,
        // see `cpu_tile_pct`'s caller), so it never comes out of `DashData`.
        let mut all_tiles: Vec<Tile> = Vec::with_capacity(tiles.len() + 1);
        for t in tiles {
            all_tiles.push(t.clone());
            if t.label == "MEMORY" {
                let value = match cpu_tile_pct {
                    Some(pct) => format!("{pct:.1}%"),
                    None => "…".to_string(),
                };
                all_tiles.push(tile("CPU", value, "of 1 core (may exceed 100%)"));
            }
        }

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(vec![
                Constraint::Ratio(1, all_tiles.len() as u32);
                all_tiles.len()
            ])
            .split(area);
        for (i, t) in all_tiles.iter().enumerate() {
            let block_title = if t.label == "TRAFFIC" || t.label == "STORAGE" {
                format!("{}{}", t.label, age_suffix(slow_age, spinner))
            } else {
                t.label.clone()
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(block_title, Style::default().fg(Color::Gray)));

            if t.label == "MEMORY" && mem_bytes_limit > 0 {
                let ratio = (mem_bytes as f64 / mem_bytes_limit as f64).clamp(0.0, 1.0);
                let color = if ratio > 0.9 {
                    REDC
                } else if ratio > 0.7 {
                    YELLOWC
                } else {
                    GREENC
                };
                let gauge = Gauge::default()
                    .block(block)
                    .ratio(ratio)
                    .gauge_style(Style::default().fg(color).bg(Color::Rgb(40, 40, 46)))
                    .label(Span::styled(
                        format!("{} ({:.0}%)", t.value, ratio * 100.0),
                        Style::default().add_modifier(Modifier::BOLD),
                    ));
                f.render_widget(gauge, cols[i]);
                continue;
            }

            let trend = trends.for_tile(&t.label);
            let mut value_line = vec![Span::styled(
                &t.value,
                Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
            )];
            if !matches!(trend, Trend::Unknown) {
                value_line.insert(
                    0,
                    Span::styled(
                        format!("{} ", trend.glyph()),
                        Style::default()
                            .fg(trend.color())
                            .add_modifier(Modifier::BOLD),
                    ),
                );
            }
            let body = vec![
                Line::from(value_line),
                Line::from(Span::styled(&t.sub, Style::default().fg(Color::DarkGray))),
            ];
            f.render_widget(Paragraph::new(body).block(block), cols[i]);
        }
    }

    /// `" (Xs ago)"` / `" (Xm ago)"` for a slow-refreshed tile, plus the
    /// spinner glyph while a background collection is actually in flight —
    /// answers "is this number from now or from a while back?", which the
    /// tile alone never said (see `SlowSample::last_updated`'s doc comment).
    fn age_suffix(age: Duration, spinner: &str) -> String {
        let secs = age.as_secs();
        let age_str = if secs < 60 {
            format!("{secs}s")
        } else {
            format!("{}m", secs / 60)
        };
        if spinner.is_empty() {
            format!(" ({age_str} ago)")
        } else {
            format!(" ({age_str} ago {spinner})")
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_table(
        f: &mut ratatui::Frame,
        area: Rect,
        rows: &[&Row],
        table_state: &mut TableState,
        sort_key: SortKey,
        cpu_pct: &HashMap<String, f64>,
    ) {
        let header = TRow::new(
            [
                "KIND",
                "NAME",
                "STATUS",
                "UP",
                "CPU",
                "MEM",
                "I/O R/W",
                "NET DOWN/UP",
                "INFO",
            ]
            .map(|h| {
                Cell::from(h).style(
                    Style::default()
                        .fg(Color::Gray)
                        .add_modifier(Modifier::BOLD),
                )
            }),
        );
        let trows: Vec<TRow> = rows
            .iter()
            .map(|r| {
                let sc = if r.ok { GREENC } else { REDC };
                let key = format!("{}:{}", r.kind, r.name);
                let cpu_cell = match cpu_pct.get(&key) {
                    Some(pct) => {
                        let color = if *pct > 90.0 {
                            REDC
                        } else if *pct > 50.0 {
                            YELLOWC
                        } else {
                            GREENC
                        };
                        Cell::from(format!("{pct:.1}%")).style(Style::default().fg(color))
                    }
                    None if r.cpu_usage_usec.is_some() => {
                        Cell::from("…").style(Style::default().fg(Color::DarkGray))
                    }
                    None => Cell::from("-").style(Style::default().fg(Color::DarkGray)),
                };
                let mem_cell = r
                    .mem_bytes
                    .map(super::super::output::fmt_size)
                    .unwrap_or_else(|| "-".to_string());
                let io_cell = fmt_io_pair(r.io_read_bytes, r.io_write_bytes);
                let net_cell = fmt_net_pair(r.net_rx_bytes, r.net_tx_bytes);
                TRow::new(vec![
                    Cell::from(r.kind.clone()),
                    Cell::from(r.name.clone()),
                    Cell::from(r.status.clone()).style(Style::default().fg(sc)),
                    Cell::from(r.up.clone()).style(Style::default().fg(Color::DarkGray)),
                    cpu_cell,
                    Cell::from(mem_cell).style(Style::default().fg(Color::DarkGray)),
                    Cell::from(io_cell).style(Style::default().fg(Color::DarkGray)),
                    Cell::from(net_cell).style(Style::default().fg(Color::DarkGray)),
                    Cell::from(r.extra.clone()).style(Style::default().fg(Color::DarkGray)),
                ])
            })
            .collect();
        let widths = [
            Constraint::Length(9),
            Constraint::Percentage(14),
            Constraint::Length(9),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(15),
            Constraint::Length(15),
            Constraint::Percentage(20),
        ];
        let title = format!(
            "{} ({}) · sort:{}",
            po::t("RESOURCES"),
            rows.len(),
            sort_key.label()
        );
        if rows.is_empty() {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(title);
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    po::t("no resources match the filter"),
                    Style::default().fg(Color::DarkGray),
                )))
                .block(block),
                area,
            );
            return;
        }
        let table = Table::new(trows, widths)
            .header(header)
            .row_highlight_style(
                Style::default()
                    .bg(Color::Rgb(50, 50, 60))
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("\u{25B8} ")
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(title),
            );
        f.render_stateful_widget(table, area, table_state);

        if rows.len() > area.height.saturating_sub(3) as usize {
            let mut sb_state =
                ScrollbarState::new(rows.len()).position(table_state.selected().unwrap_or(0));
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .style(Style::default().fg(Color::DarkGray));
            f.render_stateful_widget(scrollbar, area, &mut sb_state);
        }
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

    /// Both histories side by side, permanently — replaces the old
    /// single-chart-plus-`m`-toggle (direct answer to "why can't I see
    /// containers AND memory at once"). Both series are tracked
    /// unconditionally regardless of which is on screen (cheap, already
    /// part of every tick), so there is nothing to toggle anymore.
    fn draw_sparklines(
        f: &mut ratatui::Frame,
        area: Rect,
        activity_hist: &VecDeque<u64>,
        mem_hist: &VecDeque<u64>,
    ) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        let mut activity = activity_hist.clone();
        let mut mem = mem_hist.clone();
        draw_one_sparkline(
            f,
            cols[0],
            po::t("RUNNING CONTAINERS (last 2 min)"),
            activity.make_contiguous(),
        );
        draw_one_sparkline(
            f,
            cols[1],
            po::t("MEMORY USAGE, MiB (last 2 min)"),
            mem.make_contiguous(),
        );
    }

    fn draw_one_sparkline(f: &mut ratatui::Frame, area: Rect, title: &str, hist: &[u64]) {
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

    #[cfg(test)]
    mod tests {
        use super::*;

        fn row(name: &str, status: &str, mem: Option<u64>) -> Row {
            Row {
                kind: "Container".into(),
                name: name.into(),
                status: status.into(),
                up: "-".into(),
                extra: String::new(),
                ok: status == "running",
                cpu_usage_usec: None,
                mem_bytes: mem,
                io_read_bytes: None,
                io_write_bytes: None,
                net_rx_bytes: None,
                net_tx_bytes: None,
            }
        }

        #[test]
        fn sort_rows_por_nome_e_alfabetico() {
            let a = row("zeta", "running", None);
            let b = row("alfa", "running", None);
            let mut rows = vec![&a, &b];
            sort_rows(&mut rows, SortKey::Name, &HashMap::new());
            assert_eq!(rows[0].name, "alfa");
            assert_eq!(rows[1].name, "zeta");
        }

        #[test]
        fn sort_rows_por_memoria_poe_o_maior_primeiro() {
            let a = row("small", "running", Some(1024));
            let b = row("big", "running", Some(1024 * 1024));
            let mut rows = vec![&a, &b];
            sort_rows(&mut rows, SortKey::Mem, &HashMap::new());
            assert_eq!(rows[0].name, "big");
        }

        #[test]
        fn sort_rows_por_cpu_poe_o_maior_pct_primeiro() {
            let a = row("idle", "running", None);
            let b = row("busy", "running", None);
            let mut cpu = HashMap::new();
            cpu.insert("Container:idle".to_string(), 3.0);
            cpu.insert("Container:busy".to_string(), 87.0);
            let mut rows = vec![&a, &b];
            sort_rows(&mut rows, SortKey::Cpu, &cpu);
            assert_eq!(rows[0].name, "busy");
        }

        #[test]
        fn trend_of_compara_contra_a_amostra_anterior() {
            assert!(matches!(Trend::of(None, 10), Trend::Unknown));
            assert!(matches!(Trend::of(Some(5), 10), Trend::Up));
            assert!(matches!(Trend::of(Some(10), 5), Trend::Down));
            assert!(matches!(Trend::of(Some(10), 10), Trend::Flat));
        }

        #[test]
        fn sortkey_next_percorre_as_4_e_da_a_volta() {
            let mut k = SortKey::Name;
            for _ in 0..4 {
                k = k.next();
            }
            assert!(matches!(k, SortKey::Name));
        }
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
                    cpu_usage_usec: Some(1_000_000),
                    mem_bytes: Some(64 * 1024 * 1024),
                    io_read_bytes: Some(1024),
                    io_write_bytes: Some(2048),
                    net_rx_bytes: Some(4096),
                    net_tx_bytes: Some(8192),
                },
                Row {
                    kind: "Container".into(),
                    name: "db".into(),
                    status: "crashed".into(),
                    up: "-".into(),
                    extra: "postgres".into(),
                    ok: false,
                    cpu_usage_usec: None,
                    mem_bytes: None,
                    io_read_bytes: None,
                    io_write_bytes: None,
                    net_rx_bytes: None,
                    net_tx_bytes: None,
                },
            ],
            problems: vec![Problem {
                resource: "container/db".into(),
                message: "killed by signal (crash)".into(),
            }],
            activity: 2,
            mem_bytes: 128 * 1024 * 1024,
            mem_bytes_limit: 256 * 1024 * 1024,
            cpu_usage_usec: Some(5_000_000),
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

    #[test]
    fn snapshot_mostra_as_colunas_de_recursos_novas() {
        let s = render_snapshot(&sample(), false);
        assert!(s.contains("CPU"));
        assert!(s.contains("MEM"));
        assert!(s.contains("I/O R/W"));
        assert!(s.contains("NET DOWN/UP"));
        // "db" tem tudo a None → célula "-", nunca um zero fabricado.
        assert!(s.contains("R -/W -") || s.contains("- "));
    }

    #[test]
    fn fmt_io_pair_ambos_ausentes_e_traco() {
        assert_eq!(fmt_io_pair(None, None), "-");
    }

    #[test]
    fn fmt_io_pair_formata_leitura_e_escrita() {
        let s = fmt_io_pair(Some(1024), Some(2048));
        assert!(s.contains("R "));
        assert!(s.contains("W "));
    }

    #[test]
    fn fmt_net_pair_ambos_ausentes_e_traco() {
        assert_eq!(fmt_net_pair(None, None), "-");
    }

    #[test]
    fn fmt_net_pair_formata_down_e_up() {
        let s = fmt_net_pair(Some(4096), Some(8192));
        assert!(s.contains('\u{2193}'));
        assert!(s.contains('\u{2191}'));
    }

    #[test]
    fn dashscope_next_e_prev_percorrem_os_6_e_dao_a_volta() {
        let mut s = DashScope::Global;
        for _ in 0..6 {
            s = s.next();
        }
        assert_eq!(s, DashScope::Global, "6 next() têm de dar a volta completa");
        assert_eq!(DashScope::Global.prev(), DashScope::Images);
        assert_eq!(DashScope::Images.next(), DashScope::Global);
    }

    #[test]
    fn dashscope_from_digit_mapeia_1_a_6_e_recusa_o_resto() {
        assert_eq!(DashScope::from_digit('1'), Some(DashScope::Global));
        assert_eq!(DashScope::from_digit('6'), Some(DashScope::Images));
        assert_eq!(DashScope::from_digit('0'), None);
        assert_eq!(DashScope::from_digit('7'), None);
        assert_eq!(DashScope::from_digit('x'), None);
    }
}
