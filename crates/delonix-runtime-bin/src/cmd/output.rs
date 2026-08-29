//! Shared CLI output layer — tables (`ls`, `docker ps` style) and detail
//! blocks (`describe`, `kubectl describe` style).
//!
//! Before this, each command group printed with `println!` and hardcoded
//! widths (`{:<20}`), which misaligned the whole table as soon as a name or an
//! image exceeded the guessed width. [`Table`] measures the columns by the
//! real content before printing.
//!
//! **No new dependencies**: this repo is public and today has no table/color/
//! date crate in the tree (`comfy-table`, `tabled`, `chrono`, …). A column
//! aligner and a `localtime_r` are too small to justify growing the
//! supply-chain surface of a container runtime.

use super::kinds as k;

/// Gap between columns, like in `docker ps`.
const GAP: usize = 3;

// ---------------------------------------------------------------------------
// Output format (ADR-0005) — `-o/--output <table|json>` for listing commands
// ---------------------------------------------------------------------------

/// Output format shared by every listing command, so `-o json` behaves
/// identically everywhere. `table` (default) is the human view; `json` is the
/// machine view (stable, language-independent field names — see ADR-0005).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum OutputFormat {
    #[default]
    Table,
    Json,
}

/// Prints `items` as a pretty JSON **array** (one element per resource, `[]` when
/// empty) — the ADR-0005 contract for `-o json`. Newline-terminated, no human
/// chrome, safe to pipe into `jq`.
pub fn print_json<T: serde::Serialize>(items: &[T]) -> delonix_runtime_core::Result<()> {
    let s = serde_json::to_string_pretty(items)
        .map_err(|e| delonix_runtime_core::Error::Invalid(format!("json output: {e}")))?;
    println!("{s}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Color
// ---------------------------------------------------------------------------

/// ANSI codes. Just these: a CLI that uses half a palette ends up with each
/// command inventing its own convention.
pub mod color {
    pub const RESET: &str = "\x1b[0m";
    pub const CYAN: &str = "\x1b[36m"; // info
    pub const YELLOW: &str = "\x1b[33m"; // warning
    pub const RED: &str = "\x1b[31m"; // error
    pub const GRAY: &str = "\x1b[90m"; // secondary (timestamps, details)
    pub const GREEN: &str = "\x1b[32m"; // a step that finished
    pub const BOLD: &str = "\x1b[1m";
}

/// Is color enabled on stderr?
///
/// Three conditions, all required: it's a terminal (in a pipe/file the ANSI
/// codes become garbage that `grep`/`tee`/CI would capture), `NO_COLOR` is unset
/// (<https://no-color.org>, the convention every CLI should honor), and `TERM`
/// is not `dumb`.
pub fn color_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        // SAFETY: isatty has no preconditions; 2 = stderr.
        let tty = unsafe { libc::isatty(2) } == 1;
        let no_color = std::env::var_os("NO_COLOR").is_some();
        let dumb = std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false);
        tty && !no_color && !dumb
    })
}

fn paint(c: &str, s: &str) -> String {
    if color_enabled() {
        format!("{c}{s}{}", color::RESET)
    } else {
        s.to_string()
    }
}

// ---- i18n -----------------------------------------------------------------
//
// The runtime is PUBLIC and open source — output is **English by default** (what
// an international user expects from a CLI). `--l18n=pt` (or `DELONIX_L18N=pt`)
// switches to Angolan Portuguese. The locale is global and immutable once set at
// startup (`set_lang`, once, in `main`); an `AtomicU8` suffices and is lock-free.

static LANG: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0); // 0=en, 1=pt

/// Sets the locale from the `--l18n` flag / `$DELONIX_L18N` value.
/// Anything that isn't `pt*` stays English (the safe default).
pub fn set_lang(s: &str) {
    let is_pt = matches!(
        s.trim().to_lowercase().as_str(),
        "pt" | "pt_ao" | "pt-ao" | "pt_pt" | "pt-pt"
    );
    LANG.store(is_pt as u8, std::sync::atomic::Ordering::Relaxed);
}

pub fn is_pt() -> bool {
    LANG.load(std::sync::atomic::Ordering::Relaxed) == 1
}

// (the old inline `tr(en, pt)` died in favor of the `data/pt.po` catalog —
// see `cmd::po::t`; the pairs scattered across the code were untranslatable by
// tooling and impossible to review in a single place.)

fn label_warn() -> &'static str {
    super::po::t("warning")
}
fn label_error() -> &'static str {
    super::po::t("error")
}

/// An informational message (cyan).
pub fn info(msg: &str) {
    eprintln!("{} {msg}", paint(color::CYAN, "info")); // "info" is the same in EN/PT
}

/// A warning (yellow): it worked, but there's something the user should know.
pub fn warn(msg: &str) {
    eprintln!("{} {msg}", paint(color::YELLOW, label_warn()));
}

/// An error (red).
pub fn error(msg: &str) {
    eprintln!("{} {msg}", paint(color::RED, label_error()));
}

/// Secondary text (gray) — for details that shouldn't compete with the message.
pub fn dim(s: &str) -> String {
    paint(color::GRAY, s)
}

pub fn bold(s: &str) -> String {
    paint(color::BOLD, s)
}

/// Table aligned by content: columns take the width of the widest cell
/// (including the header). The last column never gets right padding, so it
/// doesn't leave trailing spaces at the end of a line.
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    /// Indices of the right-aligned columns (numbers).
    right: Vec<usize>,
}

/// What a NAMESPACE cell says, for every listing that has one.
///
/// `default` becomes a dash when nobody asked for a namespace, so
/// [`Table::drop_uninformative`] removes the whole column on a host that uses
/// none — printing `default` on every row is a column of noise.
///
/// **Unless the operator named a namespace**: then it prints verbatim, because
/// dropping it would remove the one thing `--namespace default` was run to see.
/// Filtering is also the only way to read the value off a listing whose column
/// hides itself.
///
/// One owner for the rule, and not a copy in each of the six listings that have
/// the column: they have to agree, and the day one of them is edited is the day
/// the same host reads two different ways depending on which command was typed
/// — the discipline `fw_rule_tail` already enforces.
pub fn namespace_cell(namespace: &str, filtered: bool) -> String {
    if !filtered && (namespace.is_empty() || namespace == "default") {
        "-".to_string()
    } else if namespace.is_empty() {
        "default".to_string()
    } else {
        namespace.to_string()
    }
}

impl Table {
    pub fn new(headers: &[&str]) -> Self {
        Self {
            headers: headers.iter().map(|h| h.to_string()).collect(),
            rows: Vec::new(),
            right: Vec::new(),
        }
    }

    /// Right-aligns column `idx` (for sizes/counts).
    pub fn right_align(mut self, idx: usize) -> Self {
        self.right.push(idx);
        self
    }

    /// Drops every column whose rows say nothing — all `-`, or all empty.
    ///
    /// A listing gets richer by ANSWERING more questions, not by carrying more
    /// columns: on a host with no GPU passthrough and no cluster nodes, `GPU`
    /// and `ROLE` are two full columns of dashes, and the width they take is
    /// width the useful columns do not get. Where those VMs exist, the columns
    /// come back on their own.
    ///
    /// Rules that keep it from surprising anyone: the FIRST column is never
    /// dropped (it is the identity — a table with no name column is not a
    /// shorter table, it is an unreadable one), and a table with no rows drops
    /// nothing (no rows is not evidence that a column is uninformative; the
    /// header is the only thing an empty `ls` has to say).
    ///
    /// The TABLE only. `-o json` keeps every field always — it is the stable
    /// surface (ADR-0005) and a field that came and went with the host's
    /// contents would be unusable for automation, which is the opposite of what
    /// this does for a human.
    pub fn drop_uninformative(mut self) -> Self {
        if self.rows.is_empty() {
            return self;
        }
        let keep: Vec<bool> = (0..self.headers.len())
            .map(|i| {
                i == 0
                    || self.rows.iter().any(|r| {
                        let c = r.get(i).map(|s| s.trim()).unwrap_or("");
                        !c.is_empty() && c != "-"
                    })
            })
            .collect();
        fn keep_only(cells: &[String], keep: &[bool]) -> Vec<String> {
            cells
                .iter()
                .zip(keep)
                .filter(|(_, k)| **k)
                .map(|(c, _)| c.clone())
                .collect()
        }
        self.headers = keep_only(&self.headers, &keep);
        self.rows = self.rows.iter().map(|r| keep_only(r, &keep)).collect();
        // The right-aligned indices refer to the OLD positions; renumber them
        // against the kept ones, or a dropped column silently shifts the
        // alignment onto its neighbour.
        let mut new_idx = vec![usize::MAX; keep.len()];
        let mut n = 0;
        for (i, k) in keep.iter().enumerate() {
            if *k {
                new_idx[i] = n;
                n += 1;
            }
        }
        self.right = self
            .right
            .iter()
            .filter_map(|i| new_idx.get(*i).copied())
            .filter(|i| *i != usize::MAX)
            .collect();
        self
    }

    pub fn row(&mut self, cells: Vec<String>) {
        debug_assert_eq!(
            cells.len(),
            self.headers.len(),
            "linha com aridade diferente do cabeçalho"
        );
        self.rows.push(cells);
    }

    /// Prints the header and the rows. An `ls` with no results prints only the
    /// header — which is what `docker ps` does, telling the user the command
    /// ran and found nothing (instead of ambiguous silence).
    pub fn print(&self) {
        let widths = self.widths();
        println!("{}", self.render(&self.headers, &widths));
        for r in &self.rows {
            println!("{}", self.render(r, &widths));
        }
    }

    /// Like `print`, but returns the whole table (header + rows) as a String —
    /// to compose inside another output (e.g. `dash` puts the table in a panel).
    pub fn render_all(&self) -> String {
        let widths = self.widths();
        let mut out = String::new();
        out.push_str(&self.render(&self.headers, &widths));
        out.push('\n');
        for r in &self.rows {
            out.push_str(&self.render(r, &widths));
            out.push('\n');
        }
        out
    }

    fn widths(&self) -> Vec<usize> {
        let mut w: Vec<usize> = self.headers.iter().map(|h| display_width(h)).collect();
        for r in &self.rows {
            for (i, c) in r.iter().enumerate() {
                w[i] = w[i].max(display_width(c));
            }
        }
        w
    }

    fn render(&self, cells: &[String], widths: &[usize]) -> String {
        let mut out = String::new();
        let last = cells.len().saturating_sub(1);
        for (i, c) in cells.iter().enumerate() {
            let pad = widths[i].saturating_sub(display_width(c));
            if self.right.contains(&i) {
                out.push_str(&" ".repeat(pad));
                out.push_str(c);
            } else {
                out.push_str(c);
                // The last column gets no right padding.
                if i != last {
                    out.push_str(&" ".repeat(pad));
                }
            }
            if i != last {
                out.push_str(&" ".repeat(GAP));
            }
        }
        out
    }
}

/// Width in terminal columns, approximated by the number of `char`s (not
/// bytes — a name with accents would count double in `len()`). Does not handle
/// double-width CJK or emoji; container/image names are ASCII in practice and
/// it's not worth a `unicode-width` dependency for it.
fn display_width(s: &str) -> usize {
    s.chars().count()
}

/// Image reference to SHOW to a human.
///
/// A digest-pinned reference (`kindest/node:v1.34.0@sha256:7416a61b…`, 84
/// chars) is what the engine needs — reproducibility — but in a table it just
/// pushes all the columns off the screen and the digest says nothing to the
/// reader. With a tag present, the `@sha256:…` is redundant for reading
/// purposes: `kindest/node:v1.34.0` is shown.
///
/// **Without a tag** (`repo@sha256:…`) the digest is the ONLY identification
/// left — there it's shortened, but never thrown away: `repo@sha256:7416a61b`.
pub fn display_ref(reference: &str) -> String {
    let Some((antes, digest)) = reference.split_once("@sha256:") else {
        return reference.to_string();
    };
    // `repo:tag@sha256:…` → the tag identifies; the digest is noise in the table.
    // Careful: a `:` in the host with a port (`reg:5000/img`) is not a tag — the
    // tag comes after the LAST '/'.
    let tem_tag = antes
        .rsplit('/')
        .next()
        .map(|ultimo| ultimo.contains(':'))
        .unwrap_or(false);
    if tem_tag {
        return antes.to_string();
    }
    let curto: String = digest.chars().take(8).collect();
    format!("{antes}@sha256:{curto}")
}

/// Truncates with an ellipsis (`…`) if it exceeds `max` — for COMMAND/PORTS,
/// which can be arbitrarily long and used to blow up the table.
pub fn truncate(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

/// Detail block in `kubectl describe` style: a fixed-width key column,
/// indented sections, bulleted lists.
pub struct Describe {
    lines: Vec<String>,
}

/// Width of the key column — the same as `kubectl describe`.
const KEY_W: usize = 16;

impl Describe {
    pub fn new() -> Self {
        Self { lines: Vec::new() }
    }

    /// `Key:          value`
    pub fn field(&mut self, key: &str, val: impl AsRef<str>) -> &mut Self {
        let k = format!("{key}:");
        self.lines.push(format!("{k:<KEY_W$}{}", val.as_ref()));
        self
    }

    /// Optional field: omitted entirely when it's `None` (`kubectl` does the
    /// same — it doesn't pollute the detail with `<none>` for everything that
    /// doesn't apply).
    pub fn field_opt(&mut self, key: &str, val: Option<impl AsRef<str>>) -> &mut Self {
        if let Some(v) = val {
            self.field(key, v.as_ref());
        }
        self
    }

    /// Section header (`Mounts:`), whose content comes indented by
    /// [`Describe::item`].
    pub fn section(&mut self, key: &str) -> &mut Self {
        self.lines.push(format!("{key}:"));
        self
    }

    /// Section with a list; when empty it prints `<none>` on the same line, so
    /// the reader can tell "has none" apart from "I forgot to show it".
    pub fn list(&mut self, key: &str, items: &[String]) -> &mut Self {
        if items.is_empty() {
            return self.field(key, "<none>");
        }
        self.section(key);
        for i in items {
            self.item(i);
        }
        self
    }

    /// Indented line within a section.
    pub fn item(&mut self, val: impl AsRef<str>) -> &mut Self {
        self.lines.push(format!("  {}", val.as_ref()));
        self
    }

    /// Indented key/value pair within a section.
    pub fn sub(&mut self, key: &str, val: impl AsRef<str>) -> &mut Self {
        let k = format!("{key}:");
        self.lines.push(format!(
            "  {k:<w$}{v}",
            k = k,
            v = val.as_ref(),
            w = KEY_W - 2
        ));
        self
    }

    /// Like [`Describe::sub`], but omitted entirely when it's `None`.
    pub fn sub_opt(&mut self, key: &str, val: Option<impl AsRef<str>>) -> &mut Self {
        if let Some(v) = val {
            self.sub(key, v.as_ref());
        }
        self
    }

    pub fn print(&self) {
        for l in &self.lines {
            println!("{l}");
        }
    }
}

/// A single-line download progress bar, rewritten with `\r` (only on a tty;
/// outside a terminal — pipes/CI — it prints nothing, so as not to flood the
/// logs with progress lines). `done`/`total` in bytes; an absent `total`
/// (response without Content-Length) shows only the bytes already read.
///
/// Cheap to call for each chunk: the actual drawing is done by the caller,
/// which should throttle the frequency (see `cmd::vmimage`); here we only
/// format.
pub fn progress_bar(label: &str, done: u64, total: Option<u64>) {
    if !color_enabled() {
        return;
    }
    const WIDTH: usize = 24;
    match total {
        Some(t) if t > 0 => {
            let frac = (done as f64 / t as f64).clamp(0.0, 1.0);
            let filled = (frac * WIDTH as f64).round() as usize;
            let bar: String = "█".repeat(filled) + &"░".repeat(WIDTH - filled);
            eprint!(
                "\r\x1b[K{label}  {bar}  {:>3}%  {} / {}",
                (frac * 100.0) as u32,
                fmt_size(done),
                fmt_size(t)
            );
        }
        _ => {
            eprint!("\r\x1b[K{label}  {} downloaded", fmt_size(done));
        }
    }
    use std::io::Write;
    let _ = std::io::stderr().flush();
}

/// Closes the [`progress_bar`] line (clears it and emits the `\n`), on a tty.
pub fn progress_done() {
    if color_enabled() {
        eprintln!("\r\x1b[K");
    }
}

pub fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut val = bytes as f64;
    let mut unit = 0;
    while val >= 1024.0 && unit < UNITS.len() - 1 {
        val /= 1024.0;
        unit += 1;
    }
    // 2 decimals for GiB+, 1 for KiB/MiB — readable without noise.
    let prec = if unit >= 3 { 2 } else { 1 };
    format!("{val:.prec$} {}", UNITS[unit])
}

/// Formats a unix instant (seconds) as LOCAL date/time "YYYY-MM-DD HH:MM".
/// Uses `localtime_r` (honors `/etc/localtime`/`TZ`); on failure, falls back to
/// the raw value.
pub fn fmt_local(unix: u64) -> String {
    let t = unix as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `t` is valid; `localtime_r` writes into `tm` (our buffer, of the
    // right size) and returns NULL only on error — which we handle below.
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if !ok {
        return unix.to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Relative age in `docker ps` style — "About a minute ago", "3 hours ago".
/// Pure function on `secs` so it's testable without a clock.
pub fn fmt_age_secs(secs: u64) -> String {
    let d = fmt_duration_secs(secs);
    format!("{d} ago")
}

/// Human-readable duration, docker style: "5 seconds", "About a minute",
/// "2 hours". Deliberately coarse — in a table, "3 days" is more useful than
/// "3d 4h 12m".
///
/// Ports docker's `units.HumanDuration` to the letter, including the bucket
/// choice that at first glance looks arbitrary: days go up to **2 weeks**
/// (not 1), weeks up to **2 months**, months up to **2 years**. That is what
/// avoids the false plural — no bucket can yield "1 weeks"/"1 months", because
/// each one starts at 2. The first attempt here used the "obvious" limits
/// (1 week, 1 month) plus "About a month/year" to cover the singular, and it
/// actually printed "1 weeks" for everything between 7 and 13 days.
pub fn fmt_duration_secs(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    match secs {
        s if s < 1 => "Less than a second".to_string(),
        1 => "1 second".to_string(),
        s if s < MIN => format!("{s} seconds"),
        s if s < 2 * MIN => "About a minute".to_string(),
        s if s < HOUR => format!("{} minutes", s / MIN),
        s if s < 2 * HOUR => "About an hour".to_string(),
        s if s < 48 * HOUR => format!("{} hours", s / HOUR),
        s if s < 14 * DAY => format!("{} days", s / DAY),
        s if s < 60 * DAY => format!("{} weeks", s / (7 * DAY)),
        s if s < 730 * DAY => format!("{} months", s / (30 * DAY)),
        s => format!("{} years", s / (365 * DAY)),
    }
}

/// Age from a unix instant, tolerant of clocks that moved backwards (a
/// `created_unix` in the future yields 0, not a giant underflow).
pub fn fmt_age(created_unix: u64) -> String {
    fmt_age_secs(now_unix().saturating_sub(created_unix))
}

/// Boot instant (unix, seconds), from the `btime` field of `/proc/stat`.
fn boot_unix() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/stat").ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())
}

/// How many seconds ago the container's init process started, from
/// `pid_starttime` (jiffies since boot, field 22 of `/proc/<pid>/stat`).
///
/// **Why not use `created_unix`**: the `Up …` of `docker ps` is the time since
/// STARTUP, not since creation. A container created yesterday and restarted 5
/// minutes ago (`container start`, `--restart` policy) would show "Up 1 day" —
/// false, and false precisely when it matters (debugging a crash-loop). The
/// process `starttime` is the only source that doesn't lie.
pub fn uptime_from_starttime(starttime_jiffies: u64) -> Option<u64> {
    // SAFETY: `sysconf` is thread-safe and has no effects; returns -1 on error.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz <= 0 {
        return None;
    }
    let started_unix = boot_unix()? + starttime_jiffies / hz as u64;
    Some(now_unix().saturating_sub(started_unix))
}

/// The layers of a `stack apply`, rendered the way a CI renders a job: one line
/// per layer, in dependency order, each ticked with how long it took.
///
/// Why not the animated [`Progress`]: every layer prints its OWN per-resource
/// lines (`container/web: created`), which are the record of what happened. A
/// spinner rewriting its row would fight them for the same terminal line, and
/// folding them away would hide the one part worth keeping. The spinner belongs
/// where a step is silent for seconds (`image --vm build`, `vm create`); a layer that
/// narrates itself needs grouping and timing, not animation.
///
/// A layer with nothing to do says so instead of claiming a tick it did not
/// earn — «0 resources» is information, and a green ✓ over no work is the kind
/// of quiet lie this engine keeps removing.
/// Announces one labelled unit of work, runs it, and closes it with the time it
/// took — `✗` on failure, so the error is attributed to the unit that produced
/// it rather than to the run as a whole.
///
/// **No spinner, and that is the whole difference from [`Progress`].** This is
/// for work that prints its OWN lines: a per-resource record (`container/web:
/// created`), or a nested `Progress` of its own. An animation rewriting its line
/// would fight them for the same row, and folding them away would hide the one
/// part worth keeping. `Progress` is for the opposite case — a step that is
/// SILENT for seconds, where the line it draws is the only thing there is.
///
/// The two callers are `Layers::run` (one Kind of a `stack apply`) and
/// `pod create` (one member). They share this instead of each printing its own
/// pair, because a `•` that a later `✓` has to match is exactly the kind of
/// agreement that drifts when it is written twice.
pub fn announced<T, F>(label: &str, icon: &str, f: F) -> delonix_runtime_core::Result<T>
where
    F: FnOnce() -> delonix_runtime_core::Result<T>,
{
    eprintln!(" {} {label} {icon}", paint(color::YELLOW, "•"));
    let t0 = std::time::Instant::now();
    let r = f();
    let mark = if r.is_ok() {
        paint(color::GREEN, "✓")
    } else {
        paint(color::RED, "✗")
    };
    eprintln!(" {mark} {label} {icon} {}", dim(&fmt_elapsed(t0.elapsed())));
    r
}

pub struct Layers {
    counts: std::collections::BTreeMap<String, usize>,
    started: std::time::Instant,
    ran: usize,
}

impl Layers {
    pub fn new(counts: std::collections::BTreeMap<String, usize>) -> Self {
        Self {
            counts,
            started: std::time::Instant::now(),
            ran: 0,
        }
    }

    /// Runs one layer, announcing it before and ticking it after. The closure is
    /// the Kind's own `apply`; its error passes straight through, with the layer
    /// closed as ✗ first so the failure is attributed to the layer that produced
    /// it rather than to the apply as a whole.
    pub fn run<F>(&mut self, kind: &str, icon: &str, f: F) -> delonix_runtime_core::Result<()>
    where
        F: FnOnce() -> delonix_runtime_core::Result<()>,
    {
        let n = self.counts.get(kind).copied().unwrap_or(0);
        // `Ingress` folds into `HTTPRoute` at load, so both count toward that layer.
        let n = if kind == k::HTTP_ROUTE {
            n + self.counts.get(k::INGRESS).copied().unwrap_or(0)
        } else {
            n
        };
        if n == 0 {
            return f(); // nothing declared: no line, no tick, no noise
        }
        self.ran += 1;
        announced(&format!("{kind} ({n})"), icon, f)
    }

    /// The total, once every layer has run.
    pub fn done(&self) {
        if self.ran == 0 {
            return;
        }
        eprintln!(
            "{}",
            dim(&super::po::tf(
                "{n} layer(s) in {t}",
                &[
                    ("n", &self.ran.to_string()),
                    ("t", &fmt_elapsed(self.started.elapsed())),
                ],
            ))
        );
    }
}

/// `kind`-style progress, with an **animated spinner**: each step shows a
/// spinning braille (` ⠋ Starting the control-plane 🕹️ `) on a background
/// thread, and the line is rewritten with ` ✓ …` (or ` ✗ …`) when it closes.
///
/// # Why a thread
///
/// The step's work (`node_exec_capture`) blocks the main thread, sometimes for
/// minutes (`kubeadm init` pulls images). Without a thread animating, the line
/// stayed frozen and looked hung. The thread only touches stderr (the step's
/// output goes to a captured file, see `node_exec_capture`), so there are never
/// two writes competing for the same line.
///
/// **Without a TTY (pipe, CI, `2>&1 | tee`)** there is no spinner and no `\r`:
/// only the final line is printed, one per step — which is what a CI log wants.
pub struct Progress {
    tty: bool,
    msg: String,
    icon: String,
    spin: Option<SpinnerHandle>,
    started: std::time::Instant,
    verbose: bool,
    /// A step shorter than this prints NOTHING — see `step_after`.
    threshold: std::time::Duration,
}

/// Output of the tools a step runs, held back while the step is running.
///
/// A CI log reads as one line per step because the hundreds of lines each step
/// produces are behind a fold — the answer to "what is it doing, and did it
/// work" is not in them, right up until the step fails, when it is *only* in
/// them. This is that fold: [`capture_line`] diverts a tool's output here, and
/// the step decides whether anyone ever sees it.
///
/// Bounded on purpose: a runaway tool must not turn a progress display into a
/// memory leak, and the tail is the part that explains a failure anyway.
static CAPTURE: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);
const CAPTURE_MAX: usize = 500;

/// Offers a line to the fold. `true` when it was taken (the caller must not
/// print it), `false` when no step is collecting and the caller owns it —
/// which is what keeps every command that does NOT use `Progress` printing
/// exactly as it did before.
pub fn capture_line(s: &str) -> bool {
    let mut g = match CAPTURE.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    match g.as_mut() {
        Some(v) => {
            if v.len() == CAPTURE_MAX {
                v.remove(0);
            }
            v.push(s.to_string());
            true
        }
        None => false,
    }
}

fn capture_start() {
    let mut g = match CAPTURE.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    *g = Some(Vec::new());
}

fn capture_take() -> Vec<String> {
    let mut g = match CAPTURE.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    g.take().unwrap_or_default()
}

/// `1.2s` / `1m03s` — the shape a CI step timer has.
pub fn fmt_elapsed(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

struct SpinnerHandle {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// Spinner frames (braille, like `kind`/`spinnies`).
const SPIN_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Waits `quiet_for` out, giving up early if the step closed first.
/// `true` = the work proved slow enough to be worth drawing.
///
/// Shared by BOTH ways a step announces itself — the spinner on a TTY and the
/// single line off one. They had drifted apart, which is the whole defect this
/// exists to prevent: one honoured the threshold and the other did not, so the
/// closing line's rule («nothing was announced below the threshold») held on one
/// path and not the other.
///
/// Waited in SLICES, never in one `sleep(quiet_for)`: `close_line` joins this
/// thread, so a single long sleep would make every fast step block until the
/// threshold elapsed — the delay meant to REMOVE chrome would have added latency
/// to every `run` instead. Caught by measurement: the step reported 0.8s inside
/// a run whose total was 0.3s, which is impossible unless the wait was the step.
fn wait_out(stop: &std::sync::atomic::AtomicBool, quiet_for: std::time::Duration) -> bool {
    let mut waited = std::time::Duration::ZERO;
    while waited < quiet_for {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        let slice = std::time::Duration::from_millis(20).min(quiet_for - waited);
        std::thread::sleep(slice);
        waited += slice;
    }
    !stop.load(std::sync::atomic::Ordering::Relaxed)
}

impl Progress {
    pub fn new() -> Self {
        // SAFETY: isatty has no preconditions; 2 = stderr.
        let tty = unsafe { libc::isatty(2) } == 1;
        Self {
            tty,
            msg: String::new(),
            icon: String::new(),
            spin: None,
            started: std::time::Instant::now(),
            threshold: std::time::Duration::ZERO,
            // The escape hatch out of the fold, for the run where the collapsed
            // line is exactly what you do not want. Also honoured by `--verbose`
            // where the command has one.
            verbose: std::env::var("DELONIX_VERBOSE").is_ok_and(|v| v != "0"),
        }
    }

    /// Streams every step's output instead of folding it away.
    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = self.verbose || verbose;
        self
    }

    /// Announces the whole plan up front, one `○` per step still to come.
    ///
    /// **What this adds is the answer to «how much is left».** A spinner says
    /// what is happening now and a closed line says what happened; neither says
    /// how far along you are, and on a `cluster kubeadm` — six to nine steps,
    /// several minutes each — that is the question being asked.
    ///
    /// Printed ONCE, and deliberately not redrawn. The alternative was to keep a
    /// live block and move the cursor up on every change, which looks better on
    /// a TTY and is worse everywhere else: this output goes to stderr, and
    /// stderr here lands in pipes, in `tee`, in CI logs and in `2>&1` files
    /// where cursor movement is garbage. The same reason `close_line` already
    /// branches on `tty`. So the reader gets the map first, then walks it — the
    /// steps close underneath, in order, exactly as before.
    ///
    /// Does nothing when the list is empty, so a caller that cannot enumerate
    /// its steps (the `build`, whose instruction count depends on a file it has
    /// not parsed yet) simply does not call it.
    pub fn plan(&mut self, steps: &[String]) {
        if steps.is_empty() {
            return;
        }
        for s in steps {
            eprintln!(" {} {}", dim("○"), dim(s));
        }
    }

    /// Like [`step`](Self::step), but SILENT while the work stays under `after`:
    /// no spinner, and no final line either.
    ///
    /// Measured, which is why it exists: `container run` with the image already
    /// in the store takes 0.43s end to end, so a spinner there is a flash of
    /// chrome over work nobody was waiting for. The same call on the FIRST run
    /// of an image extracts every layer (`ensure_layers`) with no output at all
    /// — tens of seconds of silence on a big image. One threshold covers both:
    /// nothing is drawn for the fast path, and the slow one animates.
    pub fn step_after(&mut self, msg: &str, icon: &str, after: std::time::Duration) {
        self.threshold = after;
        self.step(msg, icon);
    }

    /// Opens a step and starts the spinner (on a TTY). `icon` is the ending emoji.
    pub fn step(&mut self, msg: &str, icon: &str) {
        self.close_line('✗'); // closes a previous step left open
        self.msg = msg.to_string();
        self.icon = icon.to_string();
        self.started = std::time::Instant::now();
        // Verbose keeps the tools' output on screen, and a spinner rewriting
        // its line underneath a stream of output produces neither.
        if !self.verbose {
            capture_start();
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s2, msg, icon) = (stop.clone(), self.msg.clone(), self.icon.clone());
        let quiet_for = self.threshold;
        if !self.tty || self.verbose {
            // Without a spinner the step still has to ANNOUNCE itself, or a
            // minutes-long step is a silent terminal.
            //
            // With no threshold that is immediate, as it always was. With one,
            // the announcement WAITS it out exactly like the spinner does —
            // which this branch used to skip, printing the bullet at once while
            // `close_line` suppresses the closing line for anything faster than
            // the threshold, on the stated reasoning that «under the threshold
            // the step never announced itself». That is true on a TTY, where the
            // spinner thread waits the threshold out before drawing anything,
            // and false here.
            //
            // So off a TTY a fast step opened a line that nothing ever closed.
            // Measured on `container run` with the image already in the store:
            // one `•`, zero `✓` — in CI, in a pipe, in any redirect, which is
            // where a stale «in progress» is least likely to be noticed and most
            // likely to be read later. The fast path was meant to keep the
            // output it always had; instead it gained a line that never resolves.
            if quiet_for.is_zero() {
                eprintln!(" {} {msg} {icon}", paint(color::YELLOW, "•"));
                return;
            }
            let handle = std::thread::spawn(move || {
                if !wait_out(&s2, quiet_for) {
                    return;
                }
                eprintln!(" {} {msg} {icon}", paint(color::YELLOW, "•"));
            });
            self.spin = Some(SpinnerHandle {
                stop,
                handle: Some(handle),
            });
            return;
        }
        let handle = std::thread::spawn(move || {
            use std::io::Write;
            if !wait_out(&s2, quiet_for) {
                return;
            }
            let mut i = 0usize;
            while !s2.load(std::sync::atomic::Ordering::Relaxed) {
                // `\x1b[K` clears to the end of the line (avoids leftovers from
                // a longer frame). No `\n` — the line is rewritten in-place.
                eprint!(
                    "\r {} {msg} {icon}\x1b[K",
                    paint(color::YELLOW, SPIN_FRAMES[i % SPIN_FRAMES.len()])
                );
                let _ = std::io::stderr().flush();
                i += 1;
                std::thread::sleep(std::time::Duration::from_millis(90));
            }
        });
        self.spin = Some(SpinnerHandle {
            stop,
            handle: Some(handle),
        });
    }

    /// Closes the current step with `✓`.
    pub fn ok(&mut self) {
        self.close_line('✓');
    }

    /// Stops the spinner (if any) and writes the final line with `mark`.
    /// Idempotent — called by `ok`, by the next `step` and by `Drop`.
    fn close_line(&mut self, mark: char) {
        let had_spinner = self.spin.is_some();
        if let Some(mut s) = self.spin.take() {
            s.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(h) = s.handle.take() {
                let _ = h.join();
            }
        } else if self.msg.is_empty() {
            return; // nothing open
        }
        let folded = capture_take();
        let elapsed = self.started.elapsed();
        // Under the threshold the step never announced itself, so closing it
        // would print a tick for a line the reader never saw starting.
        if elapsed < self.threshold {
            self.threshold = std::time::Duration::ZERO;
            self.msg.clear();
            let _ = folded;
            return;
        }
        self.threshold = std::time::Duration::ZERO;
        let took = dim(&fmt_elapsed(elapsed));
        let mark_s = match mark {
            '✓' => paint(color::GREEN, "✓"),
            '✗' => paint(color::RED, "✗"),
            other => other.to_string(),
        };
        // O ícone é OPCIONAL desde que o `build` passou a abrir um passo por
        // instrução: `RUN apt-get …  1.0s` com dois espaços a meio lê-se como
        // desalinhamento, e uma instrução de Dockerfile não tem emoji que a
        // descreva melhor do que o próprio texto. Um `format!` condicional em vez
        // de um espaço fixo — o mesmo cuidado que a `Table` já tem em medir as
        // colunas pelo conteúdo em vez de as fixar.
        let icon = if self.icon.is_empty() {
            String::new()
        } else {
            format!(" {}", self.icon)
        };
        if self.tty && !self.verbose {
            // `\r` + clear the spinner line, then the final line.
            eprintln!("\r {mark_s} {}{icon} {took}\x1b[K", self.msg);
        } else if !self.msg.is_empty() {
            eprintln!(" {mark_s} {}{icon} {took}", self.msg);
        }
        // A FAILED step always unfolds. Hiding the output of the one thing that
        // broke is the single case where the fold costs more than it saves —
        // and it is the case the reader is in when they care at all.
        if mark == '✗' && !folded.is_empty() {
            for l in &folded {
                eprintln!("   {}", dim(l));
            }
        }
        let _ = had_spinner;
        self.msg.clear();
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        // A step left open (error midway) closes with ✗ instead of leaving the
        // spinner hanging.
        self.close_line('✗');
    }
}

#[cfg(test)]
mod namespace_cell_tests {
    use super::{namespace_cell, Table};

    /// The rule the six listings share, pinned in one place.
    ///
    /// `default` unfiltered is a dash so the whole column can disappear; named
    /// explicitly it prints, because dropping it would remove the one thing
    /// `--namespace default` was run to see. An EMPTY namespace is `default` —
    /// records written before the field existed have it empty, and printing an
    /// empty cell would read as «this row has no namespace», which no workload
    /// ever has.
    #[test]
    fn default_hides_unless_it_was_asked_for() {
        assert_eq!(namespace_cell("default", false), "-");
        assert_eq!(namespace_cell("", false), "-");
        assert_eq!(namespace_cell("default", true), "default");
        assert_eq!(namespace_cell("", true), "default");
        assert_eq!(namespace_cell("inquilino-b", false), "inquilino-b");
        assert_eq!(namespace_cell("inquilino-b", true), "inquilino-b");
    }

    /// The two halves have to work TOGETHER: the cell makes the column droppable
    /// and `drop_uninformative` is what drops it. Testing either alone proves
    /// nothing about what an operator sees.
    #[test]
    fn a_node_with_no_namespaces_loses_the_column_entirely() {
        let mut t = Table::new(&["NAME", "NAMESPACE"]);
        for name in ["a", "b"] {
            t.row(vec![name.to_string(), namespace_cell("default", false)]);
        }
        let out = t.drop_uninformative().render_all();
        assert!(
            !out.contains("NAMESPACE"),
            "a column of dashes shows no boundary; got:\n{out}"
        );

        let mut t = Table::new(&["NAME", "NAMESPACE"]);
        t.row(vec!["a".to_string(), namespace_cell("default", false)]);
        t.row(vec!["b".to_string(), namespace_cell("inquilino-b", false)]);
        let out = t.drop_uninformative().render_all();
        assert!(
            out.contains("NAMESPACE") && out.contains("inquilino-b"),
            "two namespaces IS the boundary worth showing; got:\n{out}"
        );
    }
}

#[cfg(test)]
mod tests {

    /// Uma listagem fica mais rica por RESPONDER a mais perguntas, não por
    /// carregar mais colunas: num host sem passthrough e sem nós de cluster, o
    /// `GPU` e o `ROLE` são duas colunas inteiras de traços, e a largura que
    /// ocupam é largura que as úteis não têm.
    #[test]
    fn uma_coluna_sem_nada_a_dizer_desaparece_da_tabela() {
        let mut t = Table::new(&["NAME", "IMAGE", "ROLE", "GPU"]);
        t.row(vec!["a".into(), "alpine".into(), "-".into(), "-".into()]);
        t.row(vec!["b".into(), "debian".into(), "-".into(), "".into()]);
        let out = t.drop_uninformative().render_all();
        assert!(out.contains("IMAGE"), "{out}");
        assert!(!out.contains("ROLE"), "coluna só de traços ficou: {out}");
        assert!(!out.contains("GPU"), "coluna vazia ficou: {out}");

        // Uma linha que diga alguma coisa segura a coluna inteira.
        let mut t = Table::new(&["NAME", "ROLE"]);
        t.row(vec!["a".into(), "-".into()]);
        t.row(vec!["b".into(), "worker".into()]);
        assert!(t.drop_uninformative().render_all().contains("ROLE"));
    }

    /// Duas regras que a impedem de surpreender: a identidade nunca cai, e uma
    /// tabela VAZIA não deita nada fora — sem linhas não há prova de que uma
    /// coluna seja inútil, e o cabeçalho é a única coisa que um `ls` sem
    /// resultados tem para dizer.
    #[test]
    fn a_primeira_coluna_e_uma_tabela_vazia_nunca_perdem_colunas() {
        let mut t = Table::new(&["NAME", "ROLE"]);
        t.row(vec!["-".into(), "-".into()]);
        let out = t.drop_uninformative().render_all();
        assert!(out.contains("NAME"), "a coluna de identidade caiu: {out}");

        let vazia = Table::new(&["NAME", "ROLE", "GPU"]).drop_uninformative();
        let out = vazia.render_all();
        for c in ["NAME", "ROLE", "GPU"] {
            assert!(out.contains(c), "{c} caiu numa tabela sem linhas: {out}");
        }
    }

    /// O alinhamento à direita é guardado por ÍNDICE, e largar uma coluna
    /// renumera as que ficam — sem reescrever esses índices, o alinhamento
    /// passa em silêncio para a coluna vizinha, que é o género de defeito que
    /// só se vê quando os números ficam tortos numa tabela larga.
    #[test]
    fn largar_uma_coluna_nao_desloca_o_alinhamento_para_a_vizinha() {
        // VCPUS (idx 3) é a alinhada; a IMAGE (idx 1) vai cair.
        let mut t = Table::new(&["NAME", "IMAGE", "BACKEND", "VCPUS"]).right_align(3);
        t.row(vec!["a".into(), "-".into(), "libvirt".into(), "4".into()]);
        t.row(vec!["bb".into(), "-".into(), "libvirt".into(), "16".into()]);
        let t = t.drop_uninformative();
        assert_eq!(
            t.right,
            vec![2],
            "o índice não foi renumerado: {:?}",
            t.right
        );
        // E o efeito visível: os números alinham à direita entre si.
        let out = t.render_all();
        assert!(out.contains(" 4") && out.contains("16"), "{out}");
    }

    use super::*;

    #[test]
    fn tabela_mede_pela_celula_mais_larga() {
        let mut t = Table::new(&["A", "B"]);
        t.row(vec!["um-nome-bem-comprido".into(), "x".into()]);
        t.row(vec!["curto".into(), "y".into()]);
        let w = t.widths();
        assert_eq!(w[0], "um-nome-bem-comprido".len());
        // A header wider than the content wins.
        assert_eq!(w[1], 1);
    }

    #[test]
    fn ultima_coluna_sem_padding_a_direita() {
        let mut t = Table::new(&["A", "B"]);
        t.row(vec!["a".into(), "b".into()]);
        let line = t.render(&t.rows[0], &t.widths());
        assert!(!line.ends_with(' '), "linha com espaços no fim: {line:?}");
    }

    #[test]
    fn truncate_respeita_o_maximo() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
        // Counts chars, not bytes — otherwise an accent would truncate too early.
        assert_eq!(truncate("ãããã", 4), "ãããã");
    }

    #[test]
    fn idade_estilo_docker() {
        assert_eq!(fmt_age_secs(3), "3 seconds ago");
        assert_eq!(fmt_age_secs(90), "About a minute ago");
        assert_eq!(fmt_age_secs(600), "10 minutes ago");
        assert_eq!(fmt_age_secs(3 * 3600), "3 hours ago");
        assert_eq!(fmt_age_secs(5 * 86400), "5 days ago");
    }

    #[test]
    fn nenhum_balde_imprime_um_plural_em_falso() {
        // Regression: a real `image ls` showed "1 weeks ago" for a 7-day image.
        // No duration can produce "1 <plural>" — in docker the weeks/months/
        // years buckets all start at 2, by construction.
        const DAY: u64 = 86400;
        for s in [
            0u64,
            1,
            30,
            90,
            3600,
            5400,
            47 * 3600,
            7 * DAY,
            13 * DAY,
            14 * DAY,
            59 * DAY,
            60 * DAY,
            729 * DAY,
            730 * DAY,
            3650 * DAY,
        ] {
            let d = fmt_duration_secs(s);
            assert!(
                !d.starts_with("1 ") || d == "1 second",
                "plural em falso para {s}s: {d:?}"
            );
        }
        // And the bucket limits, to the letter of docker.
        assert_eq!(fmt_duration_secs(7 * DAY), "7 days");
        assert_eq!(fmt_duration_secs(13 * DAY), "13 days");
        assert_eq!(fmt_duration_secs(14 * DAY), "2 weeks");
        assert_eq!(fmt_duration_secs(60 * DAY), "2 months");
        assert_eq!(fmt_duration_secs(730 * DAY), "2 years");
    }

    #[test]
    fn idade_com_relogio_no_futuro_nao_faz_underflow() {
        // `created_unix` in the future (clock corrected backwards) used to give
        // a u64 underflow → "584 million years ago".
        let futuro = now_unix() + 3600;
        assert_eq!(fmt_age(futuro), "Less than a second ago");
    }

    #[test]
    fn locale_reconhece_variantes_pt() {
        // The translation itself lives in the catalog (`cmd::po`); here it's
        // only the locale.
        set_lang("en");
        assert!(!is_pt());
        set_lang("pt");
        assert!(is_pt());
        // pt_AO and variants count as pt; anything else = en (safe default).
        set_lang("pt_AO");
        assert!(is_pt());
        set_lang("fr");
        assert!(!is_pt());
        set_lang("en"); // reset so it doesn't affect other tests
    }

    #[test]
    fn tamanhos_legiveis() {
        assert_eq!(fmt_size(512), "512 B");
        assert_eq!(fmt_size(1536), "1.5 KiB");
        assert_eq!(fmt_size(2 * 1024 * 1024 * 1024), "2.00 GiB");
    }

    #[test]
    fn display_ref_corta_o_digest_quando_ha_tag() {
        // The case that motivated this: a kindest/node pinned by digest.
        assert_eq!(
            display_ref("kindest/node:v1.34.0@sha256:7416a61b42b1662ca6ca89f02028ac133a309a2a30ba309614e8ec94d976dc5a"),
            "kindest/node:v1.34.0"
        );
        assert_eq!(
            display_ref("nginx:latest@sha256:abcdef0123"),
            "nginx:latest"
        );
        // Without a digest, untouched.
        assert_eq!(display_ref("alpine:3.19"), "alpine:3.19");
        assert_eq!(display_ref("nginx"), "nginx");
    }

    #[test]
    fn display_ref_sem_tag_encurta_o_digest_mas_nao_o_deita_fora() {
        // `repo@sha256:…` — the digest is the ONLY identification; it's
        // shortened, not removed (otherwise two different images would end up
        // with the same name).
        assert_eq!(
            display_ref("myrepo@sha256:7416a61b42b1662ca6ca89f0"),
            "myrepo@sha256:7416a61b"
        );
    }

    #[test]
    fn display_ref_nao_confunde_porta_do_registo_com_tag() {
        // `reg:5000/img@sha256:…` — the `:5000` is the host port, NOT a tag.
        // The tag, if any, comes after the last '/'. Here there is none →
        // shorten the digest.
        assert_eq!(
            display_ref("reg:5000/img@sha256:7416a61b42b1662c"),
            "reg:5000/img@sha256:7416a61b"
        );
        // With a port AND a tag, the tag identifies → cut the digest.
        assert_eq!(
            display_ref("reg:5000/img:v2@sha256:7416a61b"),
            "reg:5000/img:v2"
        );
    }
}
