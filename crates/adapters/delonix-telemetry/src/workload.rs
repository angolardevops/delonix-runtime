//! Per-workload contention series: whether a workload got the CPU and memory
//! its limits promised.
//!
//! Unlike the aggregate gauges in [`crate::metrics`], these carry labels
//! (`kind`, `id`, `name`: one series set per workload, never per process or per
//! thread), so they cannot live in a fixed `Gauge`: the set of workloads changes
//! between scrapes and a vanished workload must vanish from the output instead
//! of freezing at its last value. They are rendered by a [`Collector`] from a
//! snapshot the caller replaces right before a scrape
//! ([`crate::metrics::publish_workloads`]).
//!
//! This crate reads no cgroup (that needs `delonix-linux`, which a telemetry
//! crate must not depend on): the caller turns what it read into
//! [`WorkloadSeries`]. A group the host could not provide is `None` plus an
//! [`Unavailable`] entry, and renders as NO sample for that group and one
//! `workload_series_unavailable` sample naming why. Absent is not zero.
//!
//! Families (the registry prefix `delonix_` is added; counters gain `_total`):
//!
//! | name | type | labels beyond the workload's |
//! |---|---|---|
//! | `workload_cpu_usage_seconds` | counter | |
//! | `workload_memory_current_bytes` | gauge | |
//! | `workload_network_receive_bytes` | counter | |
//! | `workload_network_transmit_bytes` | counter | |
//! | `workload_cpu_periods` | counter | |
//! | `workload_cpu_throttled_periods` | counter | |
//! | `workload_cpu_throttled_seconds` | counter | |
//! | `workload_pressure_avg10_percent` | gauge | `resource`, `stall` |
//! | `workload_pressure_stall_seconds` | counter | `resource`, `stall` |
//! | `workload_memory_oom_events` | counter | |
//! | `workload_memory_oom_kills` | counter | |
//! | `workload_series_unavailable` | gauge (always 1) | `group`, `reason` |

use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use prometheus_client::collector::Collector;
use prometheus_client::encoding::{DescriptorEncoder, EncodeMetric};
use prometheus_client::metrics::counter::ConstCounter;
use prometheus_client::metrics::gauge::ConstGauge;
use prometheus_client::metrics::MetricType;
use prometheus_client::registry::Registry;

/// What kind of workload a series set belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadKind {
    Container,
    Vm,
}

impl WorkloadKind {
    fn as_str(self) -> &'static str {
        match self {
            WorkloadKind::Container => "container",
            WorkloadKind::Vm => "vm",
        }
    }
}

/// `cpu.stat` bandwidth counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuThrottle {
    pub nr_periods: u64,
    pub nr_throttled: u64,
    pub throttled_usec: u64,
}

/// One `some`/`full` line of a PSI file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stall {
    /// Percent of the last 10 s the workload was stalled.
    pub avg10_percent: f64,
    /// Cumulative stall, microseconds.
    pub total_usec: u64,
}

/// A PSI file: `some` always, `full` where the kernel writes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pressure {
    pub some: Stall,
    pub full: Option<Stall>,
}

/// `memory.events` counters; either may be absent on an old kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryEvents {
    pub oom: Option<u64>,
    pub oom_kill: Option<u64>,
}

/// A group of series with no value, and the reason (both closed sets: they are
/// label values).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    pub group: &'static str,
    pub reason: &'static str,
}

/// Everything exported for one workload.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkloadSeries {
    pub kind: WorkloadKind,
    pub id: String,
    pub name: String,
    /// `cpu.stat` `usage_usec`: total CPU time the workload consumed (rendered
    /// as `workload_cpu_usage_seconds_total`). Always available for a running
    /// workload, so it is the per-workload CPU signal that is never absent.
    pub cpu_usage_usec: Option<u64>,
    /// `memory.current`: bytes the workload charges right now (rendered as
    /// `workload_memory_current_bytes`).
    pub memory_current_bytes: Option<u64>,
    /// Cumulative bytes received across ALL of the workload's interfaces, read
    /// inside its own netns (rendered as `workload_network_receive_bytes_total`).
    /// `None` for a workload with no private netns (`--net host`/`--net none`)
    /// or one whose netns has not yet been sampled by the background refresh —
    /// in both cases no sample is emitted (absent is not zero).
    pub net_rx_bytes: Option<u64>,
    /// Cumulative bytes transmitted across ALL of the workload's interfaces
    /// (rendered as `workload_network_transmit_bytes_total`). Same `None`
    /// semantics as [`Self::net_rx_bytes`].
    pub net_tx_bytes: Option<u64>,
    pub cpu_throttle: Option<CpuThrottle>,
    pub cpu_pressure: Option<Pressure>,
    pub memory_pressure: Option<Pressure>,
    pub io_pressure: Option<Pressure>,
    pub memory_events: Option<MemoryEvents>,
    pub unavailable: Vec<Unavailable>,
}

type Labels = Vec<(&'static str, String)>;
type Rows<M> = Vec<(Labels, M)>;

/// `prometheus-client` writes a label value as given, so a name holding `"`,
/// `\` or a newline would break the line (and the scrape with it). The
/// OpenMetrics text format escapes exactly these three.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

fn base_labels(s: &WorkloadSeries) -> Labels {
    vec![
        ("kind", s.kind.as_str().to_string()),
        ("id", escape(&s.id)),
        ("name", escape(&s.name)),
    ]
}

fn with(mut l: Labels, extra: &[(&'static str, &str)]) -> Labels {
    l.extend(extra.iter().map(|(k, v)| (*k, (*v).to_string())));
    l
}

fn seconds(usec: u64) -> f64 {
    usec as f64 / 1_000_000.0
}

/// One metric family: the HELP/TYPE header is written only when there is at
/// least one sample, so a host that cannot provide a group exports no empty
/// family.
fn emit<M: EncodeMetric>(
    enc: &mut DescriptorEncoder,
    name: &str,
    help: &str,
    ty: MetricType,
    rows: &Rows<M>,
) -> Result<(), std::fmt::Error> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut me = enc.encode_descriptor(name, help, None, ty)?;
    for (labels, metric) in rows {
        metric.encode(me.encode_family(labels)?)?;
    }
    Ok(())
}

fn encode_series(
    mut enc: DescriptorEncoder,
    series: &[WorkloadSeries],
) -> Result<(), std::fmt::Error> {
    let mut cpu_usage: Rows<ConstCounter<f64>> = Vec::new();
    let mut mem_current: Rows<ConstGauge<i64>> = Vec::new();
    let mut net_rx: Rows<ConstCounter<u64>> = Vec::new();
    let mut net_tx: Rows<ConstCounter<u64>> = Vec::new();
    let mut periods: Rows<ConstCounter<u64>> = Vec::new();
    let mut throttled: Rows<ConstCounter<u64>> = Vec::new();
    let mut throttled_secs: Rows<ConstCounter<f64>> = Vec::new();
    let mut avg10: Rows<ConstGauge<f64>> = Vec::new();
    let mut stall_secs: Rows<ConstCounter<f64>> = Vec::new();
    let mut oom: Rows<ConstCounter<u64>> = Vec::new();
    let mut oom_kill: Rows<ConstCounter<u64>> = Vec::new();
    let mut unavailable: Rows<ConstGauge<i64>> = Vec::new();

    for s in series {
        let base = base_labels(s);
        if let Some(u) = s.cpu_usage_usec {
            cpu_usage.push((base.clone(), ConstCounter::new(seconds(u))));
        }
        if let Some(b) = s.memory_current_bytes {
            // A gauge: bytes charged now, which falls as the workload frees memory.
            mem_current.push((base.clone(), ConstGauge::new(b as i64)));
        }
        if let Some(v) = s.net_rx_bytes {
            net_rx.push((base.clone(), ConstCounter::new(v)));
        }
        if let Some(v) = s.net_tx_bytes {
            net_tx.push((base.clone(), ConstCounter::new(v)));
        }
        if let Some(c) = s.cpu_throttle {
            periods.push((base.clone(), ConstCounter::new(c.nr_periods)));
            throttled.push((base.clone(), ConstCounter::new(c.nr_throttled)));
            throttled_secs.push((base.clone(), ConstCounter::new(seconds(c.throttled_usec))));
        }
        for (resource, p) in [
            ("cpu", s.cpu_pressure),
            ("memory", s.memory_pressure),
            ("io", s.io_pressure),
        ] {
            let Some(p) = p else { continue };
            for (stall, line) in [("some", Some(p.some)), ("full", p.full)] {
                let Some(line) = line else { continue };
                let l = with(base.clone(), &[("resource", resource), ("stall", stall)]);
                avg10.push((l.clone(), ConstGauge::new(line.avg10_percent)));
                stall_secs.push((l, ConstCounter::new(seconds(line.total_usec))));
            }
        }
        if let Some(m) = s.memory_events {
            if let Some(v) = m.oom {
                oom.push((base.clone(), ConstCounter::new(v)));
            }
            if let Some(v) = m.oom_kill {
                oom_kill.push((base.clone(), ConstCounter::new(v)));
            }
        }
        for u in &s.unavailable {
            let l = with(base.clone(), &[("group", u.group), ("reason", u.reason)]);
            unavailable.push((l, ConstGauge::new(1)));
        }
    }

    let (c, g) = (MetricType::Counter, MetricType::Gauge);
    emit(
        &mut enc,
        "workload_cpu_usage_seconds",
        "Total CPU time the workload consumed (cpu.stat usage_usec); always present for a running workload",
        c,
        &cpu_usage,
    )?;
    emit(
        &mut enc,
        "workload_memory_current_bytes",
        "Memory the workload charges right now, in bytes (memory.current)",
        g,
        &mem_current,
    )?;
    emit(
        &mut enc,
        "workload_network_receive_bytes",
        "Cumulative bytes received across all of the workload's interfaces, read in its netns (/proc/net/dev)",
        c,
        &net_rx,
    )?;
    emit(
        &mut enc,
        "workload_network_transmit_bytes",
        "Cumulative bytes transmitted across all of the workload's interfaces, read in its netns (/proc/net/dev)",
        c,
        &net_tx,
    )?;
    emit(
        &mut enc,
        "workload_cpu_periods",
        "CPU bandwidth enforcement periods the workload ran in (cpu.stat nr_periods)",
        c,
        &periods,
    )?;
    emit(
        &mut enc,
        "workload_cpu_throttled_periods",
        "Periods in which the workload hit its CPU quota and was throttled (cpu.stat nr_throttled)",
        c,
        &throttled,
    )?;
    emit(
        &mut enc,
        "workload_cpu_throttled_seconds",
        "Time the workload spent throttled by its CPU quota (cpu.stat throttled_usec)",
        c,
        &throttled_secs,
    )?;
    emit(
        &mut enc,
        "workload_pressure_avg10_percent",
        "Percent of the last 10 s the workload was stalled on a resource (PSI avg10; stall=some: any task, full: all tasks)",
        g,
        &avg10,
    )?;
    emit(
        &mut enc,
        "workload_pressure_stall_seconds",
        "Cumulative time the workload was stalled on a resource (PSI total; stall=some: any task, full: all tasks)",
        c,
        &stall_secs,
    )?;
    emit(
        &mut enc,
        "workload_memory_oom_events",
        "Times the workload hit its memory limit and the kernel entered out-of-memory handling (memory.events oom)",
        c,
        &oom,
    )?;
    emit(
        &mut enc,
        "workload_memory_oom_kills",
        "Processes of the workload killed by the out-of-memory killer (memory.events oom_kill)",
        c,
        &oom_kill,
    )?;
    emit(
        &mut enc,
        "workload_series_unavailable",
        "1 for each group of per-workload series this host could not provide, with the reason; the group has no samples",
        g,
        &unavailable,
    )
}

/// Renders the snapshot it holds; the snapshot is replaced as a whole.
#[derive(Debug, Default)]
pub struct WorkloadCollector {
    snapshot: Mutex<Vec<WorkloadSeries>>,
}

impl WorkloadCollector {
    /// Replaces the snapshot: workloads absent from `series` stop being exported.
    pub fn replace(&self, series: Vec<WorkloadSeries>) {
        *self.snapshot.lock().unwrap_or_else(PoisonError::into_inner) = series;
    }
}

impl Collector for WorkloadCollector {
    fn encode(&self, encoder: DescriptorEncoder) -> Result<(), std::fmt::Error> {
        let snapshot = self.snapshot.lock().unwrap_or_else(PoisonError::into_inner);
        encode_series(encoder, &snapshot)
    }
}

/// The collector registered in the shared registry.
pub(crate) static GLOBAL: LazyLock<Arc<WorkloadCollector>> =
    LazyLock::new(|| Arc::new(WorkloadCollector::default()));

/// Renders only the per-workload families for `series`, in the OpenMetrics text
/// format, with the same prefix and names as the shared registry. For tests and
/// diagnostics: it touches no global state.
pub fn render(series: &[WorkloadSeries]) -> String {
    let collector = Arc::new(WorkloadCollector::default());
    collector.replace(series.to_vec());
    let mut registry = Registry::with_prefix("delonix");
    registry.register_collector(Box::new(collector));
    let mut out = String::new();
    let _ = prometheus_client::encoding::text::encode(&mut out, &registry);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(id: &str, name: &str) -> WorkloadSeries {
        WorkloadSeries {
            kind: WorkloadKind::Container,
            id: id.into(),
            name: name.into(),
            cpu_usage_usec: None,
            memory_current_bytes: None,
            net_rx_bytes: None,
            net_tx_bytes: None,
            cpu_throttle: None,
            cpu_pressure: None,
            memory_pressure: None,
            io_pressure: None,
            memory_events: None,
            unavailable: Vec::new(),
        }
    }

    fn full() -> WorkloadSeries {
        let p = Pressure {
            some: Stall {
                avg10_percent: 12.5,
                total_usec: 4_500_000,
            },
            full: Some(Stall {
                avg10_percent: 2.25,
                total_usec: 500_000,
            }),
        };
        WorkloadSeries {
            cpu_usage_usec: Some(9_500_000),
            memory_current_bytes: Some(4_194_304),
            net_rx_bytes: Some(123_456),
            net_tx_bytes: Some(7_890),
            cpu_throttle: Some(CpuThrottle {
                nr_periods: 200,
                nr_throttled: 50,
                throttled_usec: 1_500_000,
            }),
            cpu_pressure: Some(p),
            memory_pressure: Some(p),
            io_pressure: Some(p),
            memory_events: Some(MemoryEvents {
                oom: Some(2),
                oom_kill: Some(1),
            }),
            ..series("c1", "web")
        }
    }

    #[test]
    fn a_workload_renders_every_family_with_the_workload_labels() {
        let out = render(&[full()]);
        let l = r#"kind="container",id="c1",name="web""#;
        for want in [
            format!("delonix_workload_cpu_usage_seconds_total{{{l}}} 9.5"),
            format!("delonix_workload_memory_current_bytes{{{l}}} 4194304"),
            format!("delonix_workload_network_receive_bytes_total{{{l}}} 123456"),
            format!("delonix_workload_network_transmit_bytes_total{{{l}}} 7890"),
            format!("delonix_workload_cpu_periods_total{{{l}}} 200"),
            format!("delonix_workload_cpu_throttled_periods_total{{{l}}} 50"),
            format!("delonix_workload_cpu_throttled_seconds_total{{{l}}} 1.5"),
            format!(
                r#"delonix_workload_pressure_avg10_percent{{{l},resource="cpu",stall="some"}} 12.5"#
            ),
            format!(
                r#"delonix_workload_pressure_avg10_percent{{{l},resource="io",stall="full"}} 2.25"#
            ),
            format!(
                r#"delonix_workload_pressure_stall_seconds_total{{{l},resource="memory",stall="some"}} 4.5"#
            ),
            format!(
                r#"delonix_workload_pressure_stall_seconds_total{{{l},resource="memory",stall="full"}} 0.5"#
            ),
            format!("delonix_workload_memory_oom_events_total{{{l}}} 2"),
            format!("delonix_workload_memory_oom_kills_total{{{l}}} 1"),
            "# TYPE delonix_workload_cpu_periods counter".to_string(),
            "# TYPE delonix_workload_pressure_avg10_percent gauge".to_string(),
        ] {
            assert!(out.contains(&want), "missing `{want}` in:\n{out}");
        }
        assert!(!out.contains("series_unavailable"), "{out}");
        assert!(out.trim_end().ends_with("# EOF"));
    }

    #[test]
    fn a_missing_group_has_no_samples_and_one_reason_sample() {
        let mut w = full();
        w.io_pressure = None;
        w.unavailable = vec![Unavailable {
            group: "io_pressure",
            reason: "file_missing",
        }];
        let out = render(&[w]);
        assert!(!out.contains(r#"resource="io""#), "{out}");
        assert!(out.contains(r#"resource="cpu""#));
        assert!(
            out.contains(r#"delonix_workload_series_unavailable{kind="container",id="c1",name="web",group="io_pressure",reason="file_missing"} 1"#),
            "{out}"
        );
    }

    #[test]
    fn a_workload_with_nothing_readable_renders_only_the_reasons() {
        let mut w = series("c2", "db");
        w.unavailable = [
            "cpu_throttle",
            "cpu_pressure",
            "memory_pressure",
            "io_pressure",
            "memory_events",
        ]
        .into_iter()
        .map(|group| Unavailable {
            group,
            reason: "cgroup_missing",
        })
        .collect();
        let out = render(&[w]);
        assert_eq!(
            out.matches("delonix_workload_series_unavailable{").count(),
            5
        );
        assert!(!out.contains("delonix_workload_cpu_"), "{out}");
        assert!(!out.contains("pressure_avg10"), "{out}");
        assert!(!out.contains("memory_oom"), "{out}");
    }

    #[test]
    fn a_partial_memory_events_exports_only_the_counter_it_has() {
        let mut w = series("c3", "old");
        w.memory_events = Some(MemoryEvents {
            oom: Some(4),
            oom_kill: None,
        });
        let out = render(&[w]);
        assert!(out.contains("delonix_workload_memory_oom_events_total{"));
        assert!(!out.contains("oom_kills"), "{out}");
    }

    #[test]
    fn a_pressure_file_without_a_full_line_exports_some_only() {
        let mut w = series("c4", "cpu-only");
        w.cpu_pressure = Some(Pressure {
            some: Stall {
                avg10_percent: 0.5,
                total_usec: 10,
            },
            full: None,
        });
        let out = render(&[w]);
        assert!(out.contains(r#"stall="some""#));
        assert!(!out.contains(r#"stall="full""#), "{out}");
    }

    #[test]
    fn a_workload_with_no_sampled_netns_emits_no_network_sample() {
        // `--net host`/`--net none`, or a netns the background refresh has not
        // yet read: both leave the two byte fields `None`, and absent is not
        // zero — no network sample at all for this workload.
        let mut w = full();
        w.net_rx_bytes = None;
        w.net_tx_bytes = None;
        let out = render(&[w]);
        assert!(!out.contains("workload_network_receive_bytes"), "{out}");
        assert!(!out.contains("workload_network_transmit_bytes"), "{out}");
        // The other families are unaffected.
        assert!(out.contains("delonix_workload_cpu_usage_seconds_total{"));
    }

    #[test]
    fn series_are_one_set_per_workload() {
        let out = render(&[full(), {
            let mut w = full();
            w.id = "c2".into();
            w.name = "worker".into();
            w.kind = WorkloadKind::Vm;
            w
        }]);
        // 3 resources x (some, full) = 6 avg10 samples per workload, and exactly
        // one cpu_periods sample each.
        assert_eq!(
            out.matches("delonix_workload_cpu_periods_total{").count(),
            2
        );
        assert_eq!(
            out.matches("delonix_workload_pressure_avg10_percent{")
                .count(),
            12
        );
        assert!(out.contains(r#"kind="vm",id="c2",name="worker""#));
        // Each family header is written once, not once per workload.
        assert_eq!(
            out.matches("# TYPE delonix_workload_cpu_periods ").count(),
            1
        );
    }

    #[test]
    fn replacing_the_snapshot_drops_vanished_workloads() {
        let c = Arc::new(WorkloadCollector::default());
        let mut registry = Registry::with_prefix("delonix");
        registry.register_collector(Box::new(c.clone()));
        let render_now = || {
            let mut s = String::new();
            prometheus_client::encoding::text::encode(&mut s, &registry).unwrap();
            s
        };
        c.replace(vec![full(), {
            let mut w = full();
            w.id = "gone".into();
            w
        }]);
        assert!(render_now().contains(r#"id="gone""#));
        c.replace(vec![full()]);
        let out = render_now();
        assert!(!out.contains(r#"id="gone""#), "{out}");
        assert!(out.contains(r#"id="c1""#));
        c.replace(Vec::new());
        assert!(!render_now().contains("delonix_workload_"));
    }

    #[test]
    fn label_values_are_escaped_so_an_odd_name_cannot_break_the_line() {
        let mut w = series("c\"5", "na\\me\nx");
        w.cpu_throttle = Some(CpuThrottle {
            nr_periods: 1,
            nr_throttled: 0,
            throttled_usec: 0,
        });
        let out = render(&[w]);
        assert!(
            out.contains(r#"delonix_workload_cpu_periods_total{kind="container",id="c\"5",name="na\\me\nx"} 1"#),
            "{out}"
        );
        // Every sample stays on one line.
        for line in out.lines().filter(|l| l.starts_with("delonix_workload_")) {
            assert!(line.contains('{') && line.contains('}'), "{line}");
        }
    }
}
