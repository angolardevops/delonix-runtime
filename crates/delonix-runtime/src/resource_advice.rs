//! Facts about this host's resources, and the advice that follows from them.
//!
//! The advice is CODE, not prose and not a model. Three reasons, and the third
//! is the one that decides:
//!
//! 1. The findings have to be identical on a laptop and on the thousandth node
//!    of a fleet, so they can be counted, suppressed and tracked. Every rule
//!    therefore carries a STABLE id (`DLX-RES-nnn`) that outlives its wording.
//! 2. A gate can only fail on something deterministic.
//! 3. An advisor that runs a model consumes the very CPU, RAM and GPU it is
//!    advising about. Under pressure it becomes a cause of the pressure. The
//!    model's job is to EXPLAIN these findings and correlate them with logs —
//!    never to produce them.
//!
//! The three classes exist for the gate, not for decoration: `Config` and
//! `Capacity` describe something stable that an operator can act on, so a gate
//! may fail on them; `Load` describes this minute and must never fail a gate,
//! or the nightly run goes red because someone compiled something.

use crate::Psi;

/// How much a finding matters. Not a log level: `Blocking` means a promise this
/// engine makes is not being kept on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warn,
    Blocking,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Blocking => "blocking",
        }
    }
}

/// Whether a finding is stable enough for a gate to fail on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// The host is configured in a way that breaks a promise. Stable, fixable.
    Config,
    /// The host is too small or too full. Stable, not always fixable.
    Capacity,
    /// What is happening right now. Never gates.
    Load,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Config => "config",
            Class::Capacity => "capacity",
            Class::Load => "load",
        }
    }
    /// `true` if a `--strict` run may fail on this class.
    pub fn gates(self) -> bool {
        !matches!(self, Class::Load)
    }
}

/// A sentence the engine has to say, kept as a TEMPLATE plus its data rather
/// than as finished text.
///
/// Finished text cannot be translated: the CLI's catalogue is keyed on the
/// exact English string, and `format!("{n} flag(s)…")` produces a different
/// key for every host. Splitting them is what lets the same finding come out in
/// Portuguese without the engine crate growing an i18n layer it has no business
/// having — the engine says WHAT, the CLI decides in which language.
#[derive(Debug, Clone)]
pub struct Message {
    /// The English sentence, with `{name}` holes. `&'static str` on purpose:
    /// this is the catalogue key, so it has to be a literal in the source.
    pub template: &'static str,
    pub args: Vec<(&'static str, String)>,
}

impl Message {
    fn new(template: &'static str, args: &[(&'static str, String)]) -> Self {
        Self {
            template,
            args: args.to_vec(),
        }
    }
    /// The sentence in English. What a JSON consumer and an MCP client get:
    /// they carry the stable `id` for machines and this for humans.
    pub fn render(&self) -> String {
        let mut out = self.template.to_string();
        for (k, v) in &self.args {
            out = out.replace(&format!("{{{k}}}"), v);
        }
        out
    }
}

/// One finding. `finding` says what is true; `action` says what to do about it,
/// and is `None` when there is nothing the operator can do — saying "run
/// something" about an unfixable fact is how a report teaches people to ignore
/// it.
#[derive(Debug, Clone)]
pub struct Advice {
    pub id: &'static str,
    /// What the finding is ABOUT — a resource (`cpu`, `io`, `disk`) or a
    /// subsystem (`cgroup`, `slice`).
    ///
    /// The id alone is not enough to aggregate a fleet, and the corpus in
    /// `tests/advisor_fixtures.rs` is what showed it: a host contended on both
    /// CPU and I/O produced `DLX-RES-004` twice, identically. «Forty nodes have
    /// DLX-RES-004» is not actionable; «forty nodes have DLX-RES-004 on io» is.
    pub subject: &'static str,
    pub severity: Severity,
    pub class: Class,
    pub finding: Message,
    pub action: Option<Message>,
}

/// What a GPU offers, when there is one. `None` everywhere is a host without a
/// usable NVIDIA GPU, which is the normal case and not a fault.
#[derive(Debug, Clone, Default)]
pub struct GpuFacts {
    pub name: Option<String>,
    pub vram_total_mib: u64,
    pub vram_free_mib: u64,
    /// A CDI spec exists, so `--gpus all` can inject the driver.
    pub cdi_spec: bool,
    /// A display is attached to this GPU, so its VRAM is shared with the
    /// desktop and filling it makes the machine stutter.
    pub drives_display: bool,
}

/// Everything `system resources` reads, in one place, so the rules below are a
/// pure function of it and can be tested without a host in that state.
#[derive(Debug, Clone, Default)]
pub struct ResourceSnapshot {
    pub rootless: bool,
    pub cgroup_base: Option<String>,
    pub delegated: Vec<String>,
    pub cpus: u64,
    pub mem_total: u64,
    pub mem_available: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub disk_free: u64,
    pub cpu_temp_c: Option<u64>,
    pub psi_cpu: Option<Psi>,
    pub psi_memory: Option<Psi>,
    pub psi_io: Option<Psi>,
    /// `delonix.slice` exists, so there is an aggregate ceiling over every
    /// workload rather than only per-container ones.
    pub aggregate_slice: bool,
    pub gpu: Option<GpuFacts>,
}

const GIB: u64 = 1024 * 1024 * 1024;

/// Sustained stall (avg300) at or above this is chronic, not a spike.
const CHRONIC_PCT: f64 = 10.0;
/// A spike worth mentioning when the 300s average says it is not chronic.
const SPIKE_PCT: f64 = 25.0;
/// Below this much free space, image pulls fail and a kubelet starts evicting.
const DISK_FLOOR: u64 = 10 * GIB;
/// Sustained package temperature above this means the fans are already the
/// answer, and the CPU ceiling should become one too.
const HOT_C: u64 = 85;

/// Every finding this host earns, most severe first.
pub fn advise(s: &ResourceSnapshot) -> Vec<Advice> {
    let mut out = Vec::new();
    let has = |c: &str| s.delegated.iter().any(|d| d == c);

    // --- Config -------------------------------------------------------------
    // Flags the engine documents and this host cannot honour. `io` is excluded
    // here and reported separately: it is not a delegation anyone forgot.
    let ignored: Vec<&str> = crate::RESOURCE_CONTROLLERS
        .iter()
        .filter(|c| **c != "io" && !has(c))
        .flat_map(|c| crate::flags_of_controller(c).iter().copied())
        .collect();
    if !ignored.is_empty() {
        out.push(Advice {
            id: "DLX-RES-001",
            subject: "cgroup",
            severity: Severity::Blocking,
            class: Class::Config,
            finding: Message::new(
                "{n} flag(s) are accepted and silently ignored: {flags}",
                &[
                    ("n", ignored.len().to_string()),
                    ("flags", ignored.join(" ")),
                ],
            ),
            action: Some(Message::new(
                "sudo delonix system setup --delegate, then log out and back in",
                &[],
            )),
        });
    }

    // Never `Blocking`, and never with an action: systemd does not delegate
    // `io` to an unprivileged user, so no rootless engine — this one, podman,
    // docker — can write `io.max`. Telling the operator to run `system setup`
    // would send them after a fix that does not exist.
    if s.rootless && !has("io") {
        out.push(Advice {
            id: "DLX-RES-002",
            subject: "io",
            severity: Severity::Info,
            class: Class::Config,
            finding: Message::new(
                "--io-weight and the --device-*-bps flags cannot apply: systemd never \
                 delegates the io controller to a rootless user",
                &[],
            ),
            action: None,
        });
    }

    if !s.aggregate_slice {
        out.push(Advice {
            id: "DLX-RES-003",
            subject: "slice",
            severity: Severity::Warn,
            class: Class::Config,
            finding: Message::new(
                "no aggregate ceiling: one workload with no --memory can take the whole host, \
                 and the thermal governor has no slice to lower",
                &[],
            ),
            action: Some(Message::new(
                "run the node as root for delonix.slice, or set a per-container --memory",
                &[],
            )),
        });
    }

    // --- Capacity -----------------------------------------------------------
    if s.disk_free < DISK_FLOOR {
        out.push(Advice {
            id: "DLX-RES-007",
            subject: "disk",
            severity: Severity::Warn,
            class: Class::Capacity,
            finding: Message::new(
                "{gib} GiB free under the state root — image pulls fail and a kubelet evicts \
                 below this",
                &[("gib", (s.disk_free / GIB).to_string())],
            ),
            action: Some(Message::new("delonix system prune", &[])),
        });
    }

    // --- Load ---------------------------------------------------------------
    for (res, psi) in [
        ("cpu", s.psi_cpu),
        ("memory", s.psi_memory),
        ("io", s.psi_io),
    ] {
        let Some(p) = psi else { continue };
        if p.avg300 >= CHRONIC_PCT {
            out.push(Advice {
                id: "DLX-RES-004",
                subject: res,
                severity: Severity::Warn,
                class: Class::Load,
                finding: Message::new(
                    "{res} has been stalled {avg300}% of the last 5 minutes — this is \
                     chronic, not a spike",
                    &[
                        ("res", res.to_string()),
                        ("avg300", format!("{:.0}", p.avg300)),
                    ],
                ),
                action: Some(Message::new(chronic_action(res), &[])),
            });
        } else if p.avg10 >= SPIKE_PCT {
            out.push(Advice {
                id: "DLX-RES-005",
                subject: res,
                severity: Severity::Info,
                class: Class::Load,
                finding: Message::new(
                    "{res} is stalled {avg10}% right now but only {avg300}% over 5 minutes \
                     — a spike",
                    &[
                        ("res", res.to_string()),
                        ("avg10", format!("{:.0}", p.avg10)),
                        ("avg300", format!("{:.0}", p.avg300)),
                    ],
                ),
                action: None,
            });
        }
    }

    // Swap alone is not a problem — a host with 30 GiB of cache and some cold
    // pages evicted is healthy. Swap PLUS memory pressure is thrashing.
    let thrashing = s.swap_used > GIB && s.psi_memory.is_some_and(|p| p.avg60 >= CHRONIC_PCT);
    if thrashing {
        out.push(Advice {
            id: "DLX-RES-006",
            subject: "memory",
            severity: Severity::Warn,
            class: Class::Load,
            finding: Message::new(
                "{gib} GiB swapped out while memory is under pressure — the host is thrashing",
                &[("gib", (s.swap_used / GIB).to_string())],
            ),
            action: Some(Message::new(
                "lower vm.swappiness, or cap the workloads with --memory",
                &[],
            )),
        });
    }

    if s.cpu_temp_c.is_some_and(|t| t >= HOT_C) {
        out.push(Advice {
            id: "DLX-RES-008",
            subject: "cpu",
            severity: Severity::Warn,
            class: Class::Load,
            finding: Message::new(
                "cpu at {celsius} °C",
                &[("celsius", s.cpu_temp_c.unwrap_or(0).to_string())],
            ),
            action: Some(Message::new(
                "delonix system thermal (lowers the engine's CPU ceiling until it cools)",
                &[],
            )),
        });
    }

    out.sort_by_key(|a| match a.severity {
        Severity::Blocking => 0,
        Severity::Warn => 1,
        Severity::Info => 2,
    });
    out
}

fn chronic_action(res: &str) -> &'static str {
    match res {
        "io" => "give the heavy workload a lower --io-weight, or move its volume to another disk",
        "memory" => "cap the workloads with --memory, or add RAM",
        _ => "give the heavy workload a lower --cpu-weight",
    }
}

// ---------------------------------------------------------------------------
// Is this host fit to run a language model locally?
// ---------------------------------------------------------------------------

/// The verdict for running inference (Ollama and the like) on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fitness {
    /// The GPU can hold a useful model without fighting anything else.
    Recommended,
    /// It fits, but it will cost the operator something they can feel.
    Marginal,
    /// It would be slower than useless, or would take the host down with it.
    NotRecommended,
}

impl Fitness {
    pub fn as_str(self) -> &'static str {
        match self {
            Fitness::Recommended => "recommended",
            Fitness::Marginal => "marginal",
            Fitness::NotRecommended => "not-recommended",
        }
    }
}

/// A verdict and the measured reasons behind it. The reasons are the point: a
/// bare "no" teaches nobody, and the operator is entitled to disagree with a
/// judgement whose inputs they can see.
#[derive(Debug, Clone)]
pub struct LocalInference {
    pub verdict: Fitness,
    pub reasons: Vec<String>,
    /// Largest dense model, in billions of parameters, that fits at `Q4_K_M`
    /// with a working context. `None` when nothing useful fits.
    pub largest_model_b: Option<u64>,
}

/// Runtime overhead of a loaded model beyond weights and cache (CUDA context,
/// graphs, buffers). Measured against llama.cpp-family runners, which is what
/// Ollama is.
const RUNNER_OVERHEAD_MIB: u64 = 700;
/// Weights at `Q4_K_M`, per billion parameters. Anchored on the size everyone
/// can check: an 8B at Q4_K_M is ~4.7 GiB.
const MIB_PER_B_Q4: u64 = 600;
/// KV cache per token for an 8B-class GQA model, in KiB. Wildly model-specific
/// — this is the order of magnitude, not a promise, and it is only used to
/// decide whether a class of model fits at all.
const KV_KIB_PER_TOKEN: u64 = 64;

/// Largest dense model (in billions of parameters) that fits in `vram_mib` at
/// `Q4_K_M` with `ctx` tokens of KV cache. `0` when nothing fits.
pub fn largest_q4_model_b(vram_mib: u64, ctx: u64) -> u64 {
    let kv_mib = ctx * KV_KIB_PER_TOKEN / 1024;
    vram_mib
        .saturating_sub(RUNNER_OVERHEAD_MIB)
        .saturating_sub(kv_mib)
        / MIB_PER_B_Q4
}

/// The smallest model class that can hold a tool-calling conversation about
/// this data and be trusted to emit valid JSON. Below it the model fills the
/// template but cannot weigh a trade-off.
const USEFUL_MODEL_B: u64 = 7;
/// Context a resource advisor needs: the snapshot, the advice, and room for the
/// tool schemas.
const ADVISOR_CTX: u64 = 8192;

/// Should this host run a model locally? Pure, so the gate can be tested
/// against hosts nobody has.
pub fn local_inference(s: &ResourceSnapshot) -> LocalInference {
    let mut reasons = Vec::new();
    let mut verdict = Fitness::Recommended;
    let degrade = |v: Fitness, r: String, reasons: &mut Vec<String>, cur: &mut Fitness| {
        reasons.push(r);
        if v == Fitness::NotRecommended || *cur == Fitness::Recommended {
            *cur = v;
        }
    };

    let Some(gpu) = s.gpu.as_ref() else {
        return LocalInference {
            verdict: Fitness::NotRecommended,
            reasons: vec![
                "no usable GPU — CPU-only inference on this class of host is single-digit \
                 tokens per second, which is slower than reading the numbers yourself"
                    .into(),
            ],
            largest_model_b: None,
        };
    };

    if !gpu.cdi_spec {
        degrade(
            Fitness::NotRecommended,
            "no CDI spec, so a container cannot reach the GPU at all (run the installer's \
             accel phase, or `sudo nvidia-ctk cdi generate`)"
                .into(),
            &mut reasons,
            &mut verdict,
        );
    }

    let largest = largest_q4_model_b(gpu.vram_free_mib, ADVISOR_CTX);
    if largest < USEFUL_MODEL_B {
        degrade(
            Fitness::NotRecommended,
            format!(
                "{} MiB of free VRAM fits at most a {largest}B model at Q4 with {ADVISOR_CTX} \
                 tokens of context; below {USEFUL_MODEL_B}B the model fills a template but \
                 cannot weigh a trade-off, and offloading to CPU drops it to a few tokens \
                 per second",
                gpu.vram_free_mib
            ),
            &mut reasons,
            &mut verdict,
        );
    }

    if gpu.drives_display {
        degrade(
            Fitness::Marginal,
            "this GPU also drives a display: a model that fills its VRAM makes the desktop \
             stutter, and the two can fight for memory until one is killed"
                .into(),
            &mut reasons,
            &mut verdict,
        );
    }

    // Offloading needs headroom in RAM, and a host already swapping has none.
    if s.mem_available < 8 * GIB {
        degrade(
            Fitness::Marginal,
            format!(
                "{} GiB of RAM available — any layer that does not fit in VRAM lands here",
                s.mem_available / GIB
            ),
            &mut reasons,
            &mut verdict,
        );
    }
    if s.swap_used > 4 * GIB {
        degrade(
            Fitness::Marginal,
            format!(
                "{} GiB already swapped out — CPU offload would swap, and swapped inference \
                 is unusable",
                s.swap_used / GIB
            ),
            &mut reasons,
            &mut verdict,
        );
    }

    if verdict == Fitness::Recommended {
        reasons.push(format!(
            "{} MiB of free VRAM fits a {largest}B model at Q4 with {ADVISOR_CTX} tokens of \
             context",
            gpu.vram_free_mib
        ));
    }
    LocalInference {
        verdict,
        reasons,
        largest_model_b: (largest > 0).then_some(largest),
    }
}

// ---------------------------------------------------------------------------
// Reading the host
// ---------------------------------------------------------------------------

/// Parses one `nvidia-smi --query-gpu=name,memory.total,memory.free,\
/// display_active --format=csv,noheader,nounits` row.
///
/// Pure, because the shape of that output is the part that breaks: fields get
/// deprecated (`display_mode` already prints `[Requested functionality has been
/// deprecated]` on driver 580), and a report must degrade rather than panic.
pub fn parse_nvidia_smi_row(row: &str) -> Option<GpuFacts> {
    let f: Vec<&str> = row.split(',').map(str::trim).collect();
    if f.len() < 4 {
        return None;
    }
    Some(GpuFacts {
        name: (!f[0].is_empty()).then(|| f[0].to_string()),
        vram_total_mib: f[1].parse().ok()?,
        vram_free_mib: f[2].parse().ok()?,
        cdi_spec: false, // filled in by the caller; nvidia-smi knows nothing of CDI
        drives_display: f[3].eq_ignore_ascii_case("enabled"),
    })
}

/// `true` if any CDI spec is installed.
///
/// Deliberately only asks the question, and does not parse: the authority on
/// CONSUMING a spec is `cmd/cdi.rs` in the CLI crate, and duplicating its
/// parser here to answer a yes/no would be two implementations of the same
/// contract drifting apart.
fn cdi_spec_present() -> bool {
    ["/etc/cdi", "/var/run/cdi"].iter().any(|dir| {
        std::fs::read_dir(dir).is_ok_and(|rd| {
            rd.flatten().any(|e| {
                matches!(
                    e.path().extension().and_then(|x| x.to_str()),
                    Some("json" | "yaml" | "yml")
                )
            })
        })
    })
}

fn gpu_facts() -> Option<GpuFacts> {
    let out = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total,memory.free,display_active",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut facts = parse_nvidia_smi_row(String::from_utf8_lossy(&out.stdout).lines().next()?)?;
    facts.cdi_spec = cdi_spec_present();
    Some(facts)
}

fn meminfo_bytes(text: &str, key: &str) -> u64 {
    text.lines()
        .find(|l| l.starts_with(key))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0)
        * 1024
}

/// Free bytes on the filesystem holding `path`, via `statvfs(3)`. `0` when the
/// call fails: this feeds a report, never a gate that deletes anything.
fn fs_avail_bytes(path: &std::path::Path) -> u64 {
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return 0;
    };
    // SAFETY: `c_path` is a valid NUL-terminated string that outlives the call,
    // and `stat` is a fully-owned, correctly-sized destination.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return 0;
    }
    (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64)
}

/// Everything the rules need, read from this host. The only impure function in
/// this module, on purpose: every judgement above it is a pure function of what
/// this returns, so a host nobody owns can still be tested.
pub fn collect(state_root: &std::path::Path) -> ResourceSnapshot {
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let swap_total = meminfo_bytes(&meminfo, "SwapTotal:");
    let (cgroup_base, delegated) = crate::enforceable_controllers();
    ResourceSnapshot {
        rootless: crate::is_rootless(),
        cgroup_base,
        delegated,
        cpus: crate::host_ncpu(),
        mem_total: crate::host_mem_bytes(),
        mem_available: meminfo_bytes(&meminfo, "MemAvailable:"),
        swap_total,
        swap_used: swap_total.saturating_sub(meminfo_bytes(&meminfo, "SwapFree:")),
        disk_free: fs_avail_bytes(state_root),
        cpu_temp_c: crate::max_cpu_temp_c(),
        psi_cpu: crate::psi("cpu"),
        psi_memory: crate::psi("memory"),
        psi_io: crate::psi("io"),
        aggregate_slice: crate::slice_path().is_some_and(|p| std::path::Path::new(&p).is_dir()),
        gpu: gpu_facts(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn psi(v: f64) -> Option<Psi> {
        Some(Psi {
            avg10: v,
            avg60: v,
            avg300: v,
        })
    }

    /// The host this was written on: rootless, `cpu memory pids` delegated.
    fn kaeso() -> ResourceSnapshot {
        ResourceSnapshot {
            rootless: true,
            cgroup_base: Some("/sys/fs/cgroup/user.slice/…/dlx-containers".into()),
            delegated: vec!["cpu".into(), "memory".into(), "pids".into()],
            cpus: 32,
            mem_total: 30 * GIB,
            mem_available: 17 * GIB,
            swap_total: 32 * GIB,
            swap_used: 10 * GIB,
            disk_free: 342 * GIB,
            cpu_temp_c: Some(54),
            psi_cpu: psi(0.0),
            psi_memory: psi(0.7),
            psi_io: psi(1.4),
            aggregate_slice: false,
            gpu: Some(GpuFacts {
                name: Some("NVIDIA GeForce RTX 5060 Laptop GPU".into()),
                vram_total_mib: 8151,
                vram_free_mib: 7581,
                cdi_spec: true,
                drives_display: true,
            }),
        }
    }

    fn ids(a: &[Advice]) -> Vec<&str> {
        a.iter().map(|x| x.id).collect()
    }

    #[test]
    fn the_nvidia_row_parses_and_degrades_instead_of_panicking() {
        // The exact line driver 580 printed on this host.
        let g = parse_nvidia_smi_row("NVIDIA GeForce RTX 5060 Laptop GPU, 8151, 7581, Enabled")
            .unwrap();
        assert_eq!(g.vram_total_mib, 8151);
        assert_eq!(g.vram_free_mib, 7581);
        assert!(g.drives_display);
        assert!(!g.cdi_spec, "o nvidia-smi não sabe nada de CDI");

        let headless = parse_nvidia_smi_row("NVIDIA L4, 24564, 24000, Disabled").unwrap();
        assert!(!headless.drives_display);

        // Everything that is not that shape: no GPU facts, no panic.
        assert!(parse_nvidia_smi_row("").is_none());
        assert!(parse_nvidia_smi_row("NVIDIA, 1, 2").is_none());
        assert!(parse_nvidia_smi_row("NVIDIA, [N/A], 7581, Enabled").is_none());
    }

    #[test]
    fn the_measured_host_earns_exactly_its_three_findings() {
        let a = advise(&kaeso());
        assert_eq!(ids(&a), vec!["DLX-RES-001", "DLX-RES-003", "DLX-RES-002"]);
        // Most severe first, so a truncated report still shows what matters.
        assert_eq!(a[0].severity, Severity::Blocking);
        assert!(a[0].finding.render().contains("--cpuset"));
        // `io` is NOT in the blocking flag list: it is a separate, unfixable
        // finding, and mixing them would put an impossible action on a gate.
        assert!(!a[0].finding.render().contains("--io-weight"));
        assert_eq!(a[2].id, "DLX-RES-002");
        assert_eq!(
            a.iter().map(|x| x.subject).collect::<Vec<_>>(),
            vec!["cgroup", "slice", "io"]
        );
        assert!(
            a[2].action.is_none(),
            "não se manda corrigir o incorrigível"
        );
    }

    #[test]
    fn a_fully_delegated_root_host_earns_nothing() {
        let s = ResourceSnapshot {
            rootless: false,
            delegated: crate::RESOURCE_CONTROLLERS
                .iter()
                .map(|c| c.to_string())
                .collect(),
            aggregate_slice: true,
            disk_free: 500 * GIB,
            psi_cpu: psi(0.0),
            psi_memory: psi(0.0),
            psi_io: psi(0.0),
            ..Default::default()
        };
        assert!(advise(&s).is_empty(), "{:?}", advise(&s));
    }

    #[test]
    fn chronic_pressure_and_a_spike_are_different_findings() {
        let base = ResourceSnapshot {
            rootless: false,
            delegated: crate::RESOURCE_CONTROLLERS
                .iter()
                .map(|c| c.to_string())
                .collect(),
            aggregate_slice: true,
            disk_free: 500 * GIB,
            ..Default::default()
        };
        // Stalled a third of the last five minutes: chronic.
        let chronic = ResourceSnapshot {
            psi_io: Some(Psi {
                avg10: 30.0,
                avg60: 30.0,
                avg300: 33.0,
            }),
            ..base.clone()
        };
        let a = advise(&chronic);
        assert_eq!(ids(&a), vec!["DLX-RES-004"]);
        assert!(a[0]
            .action
            .as_ref()
            .unwrap()
            .render()
            .contains("--io-weight"));

        // Same instant, five quiet minutes behind it: a spike, and it must not
        // be able to fail a gate.
        let spike = ResourceSnapshot {
            psi_io: Some(Psi {
                avg10: 30.0,
                avg60: 4.0,
                avg300: 1.0,
            }),
            ..base
        };
        let a = advise(&spike);
        assert_eq!(ids(&a), vec!["DLX-RES-005"]);
        assert!(!a[0].class.gates());
    }

    #[test]
    fn swap_alone_is_not_thrashing() {
        let base = ResourceSnapshot {
            rootless: false,
            delegated: crate::RESOURCE_CONTROLLERS
                .iter()
                .map(|c| c.to_string())
                .collect(),
            aggregate_slice: true,
            disk_free: 500 * GIB,
            swap_used: 10 * GIB,
            ..Default::default()
        };
        // Ten gigabytes of cold pages evicted on an idle host is health, not a
        // problem — this is the reading the measured host actually gives.
        assert!(advise(&base).is_empty());
        // The same swap WITH memory pressure is thrashing.
        let hot = ResourceSnapshot {
            psi_memory: psi(40.0),
            ..base
        };
        assert!(ids(&advise(&hot)).contains(&"DLX-RES-006"));
    }

    #[test]
    fn only_stable_classes_can_fail_a_gate() {
        assert!(Class::Config.gates());
        assert!(Class::Capacity.gates());
        assert!(
            !Class::Load.gates(),
            "a carga de um minuto não reprova nada"
        );
    }

    #[test]
    fn vram_arithmetic_matches_the_sizes_people_can_check() {
        // The card measured here: 7581 MiB free, 8k of context. An 8B at Q4
        // fits with room; a 14B does not.
        assert_eq!(largest_q4_model_b(7581, 8192), 10);
        // 32k of context costs two gigabytes of KV cache and drops the ceiling
        // by two billion parameters — the context is not free, and sizing that
        // ignores it is how a model that "fits" gets killed mid-answer.
        assert_eq!(largest_q4_model_b(7581, 32768), 8);
        // A 4 GiB card cannot hold a useful advisor.
        assert!(largest_q4_model_b(4096, 8192) < USEFUL_MODEL_B);
        // Nothing at all, without panicking.
        assert_eq!(largest_q4_model_b(512, 8192), 0);
        assert_eq!(largest_q4_model_b(0, 1_000_000), 0);
    }

    #[test]
    fn the_measured_host_is_marginal_for_local_inference_and_says_why() {
        let f = local_inference(&kaeso());
        assert_eq!(f.verdict, Fitness::Marginal);
        assert_eq!(f.largest_model_b, Some(10));
        let why = f.reasons.join(" | ");
        assert!(why.contains("drives a display"), "{why}");
        assert!(why.contains("swapped out"), "{why}");
    }

    #[test]
    fn no_gpu_and_no_cdi_are_both_a_refusal_with_a_reason() {
        let none = local_inference(&ResourceSnapshot {
            gpu: None,
            ..Default::default()
        });
        assert_eq!(none.verdict, Fitness::NotRecommended);
        assert!(none.reasons[0].contains("CPU-only"));
        assert_eq!(none.largest_model_b, None);

        // A perfectly sized GPU is still unreachable without a CDI spec, and
        // the reason names the command that fixes it.
        let no_cdi = local_inference(&ResourceSnapshot {
            mem_available: 32 * GIB,
            gpu: Some(GpuFacts {
                vram_total_mib: 24576,
                vram_free_mib: 24576,
                cdi_spec: false,
                drives_display: false,
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(no_cdi.verdict, Fitness::NotRecommended);
        assert!(no_cdi.reasons.iter().any(|r| r.contains("nvidia-ctk")));
    }

    #[test]
    fn a_headless_card_with_room_is_recommended() {
        let f = local_inference(&ResourceSnapshot {
            mem_available: 32 * GIB,
            swap_used: 0,
            gpu: Some(GpuFacts {
                vram_total_mib: 24576,
                vram_free_mib: 24000,
                cdi_spec: true,
                drives_display: false,
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(f.verdict, Fitness::Recommended);
        assert_eq!(f.reasons.len(), 1, "um sim não precisa de desculpas");
        assert!(f.largest_model_b.unwrap() >= 30);
    }
}
