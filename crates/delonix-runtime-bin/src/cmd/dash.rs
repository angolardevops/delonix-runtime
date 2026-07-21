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

use super::util::state_root;

/// Dashboard scope: global or focused on a group of resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
            DashScope::Global => "DELONIX — RESUMO",
            DashScope::Containers => "DELONIX — CONTAINERS",
            DashScope::Vms => "DELONIX — VMs",
            DashScope::Networks => "DELONIX — REDES",
            DashScope::Storage => "DELONIX — STORAGE/VOLUMES",
            DashScope::Images => "DELONIX — IMAGENS",
        }
    }
}

/// A KPI (tile) — label + big value + subtitle.
#[derive(Debug, Clone, PartialEq)]
pub struct Tile {
    pub label: String,
    pub value: String,
    pub sub: String,
}

/// A row of the resource table.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub kind: String,
    pub name: String,
    pub status: String,
    pub extra: String,
    /// `true` = healthy state (Running/present); painted green vs red.
    pub ok: bool,
}

/// An identified problem (the red panel on the right).
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    pub resource: String,
    pub message: String,
}

/// Complete dashboard snapshot at an instant — pure (only reads stores).
#[derive(Debug, Clone, PartialEq)]
pub struct DashData {
    pub scope: DashScope,
    pub tiles: Vec<Tile>,
    pub rows: Vec<Row>,
    pub problems: Vec<Problem>,
    /// Single metric for the activity sparkline (running containers).
    pub activity: u64,
}

fn tile(label: &str, value: impl ToString, sub: &str) -> Tile {
    Tile {
        label: label.to_string(),
        value: value.to_string(),
        sub: sub.to_string(),
    }
}

impl DashData {
    /// Collects the store snapshot for the given `scope`. Best-effort: a store
    /// that doesn't open counts as 0 (the dashboard should never blow up over a store).
    pub fn collect(scope: DashScope) -> Result<DashData> {
        let root = state_root();

        // --- containers ---
        let mut containers: Vec<(String, Status, String)> = Vec::new();
        if let Ok((_img, store)) = super::util::open_stores() {
            for mut c in store.list().unwrap_or_default() {
                delonix_runtime::reconcile_status(&mut c);
                let img = c.image.clone();
                containers.push((c.name.clone(), c.status.clone(), img));
            }
        }
        let c_running = containers
            .iter()
            .filter(|(_, s, _)| *s == Status::Running)
            .count();

        // --- vms (state RECONCILED with the backend, like containers) — a
        //     VM killed externally shows Stopped, not the persisted Running ---
        let vms: Vec<delonix_runtime_core::Vm> = delonix_vm::list(&root)
            .unwrap_or_default()
            .into_iter()
            .map(|v| delonix_vm::status(&root, &v.name).unwrap_or(v))
            .collect();
        let vm_running = vms.iter().filter(|v| v.status == Status::Running).count();

        // --- networks / volumes / images / secrets ---
        let networks = delonix_net::NetworkStore::open(&root)
            .and_then(|s| s.list())
            .unwrap_or_default();
        let volumes = delonix_volume::VolumeStore::open(&root)
            .and_then(|s| s.list())
            .unwrap_or_default();
        let images = delonix_image::ImageStore::open(&root)
            .and_then(|s| s.list())
            .unwrap_or_default();
        let secrets = delonix_runtime_core::SecretStore::open(&root)
            .map(|s| s.list().len())
            .unwrap_or(0);

        // --- tiles (per scope) ---
        let tiles = match scope {
            DashScope::Global => vec![
                tile(
                    "CONTAINERS",
                    format!("{c_running}/{}", containers.len()),
                    "a correr / total",
                ),
                tile(
                    "VMs",
                    format!("{vm_running}/{}", vms.len()),
                    "a correr / total",
                ),
                tile("REDES", networks.len(), "definidas"),
                tile("VOLUMES", volumes.len(), "+ storage de rede"),
                tile("IMAGENS", images.len(), "em cache"),
                tile("SEGREDOS", secrets, "no cofre"),
            ],
            DashScope::Containers => vec![
                tile("A CORRER", c_running, "Running"),
                tile("TOTAL", containers.len(), "todos os estados"),
                tile(
                    "PARADOS",
                    containers.len().saturating_sub(c_running),
                    "não-Running",
                ),
            ],
            DashScope::Vms => vec![
                tile("A CORRER", vm_running, "Running"),
                tile("TOTAL", vms.len(), "todas"),
            ],
            DashScope::Networks => vec![tile("REDES", networks.len(), "definidas")],
            DashScope::Storage => vec![tile("VOLUMES", volumes.len(), "locais + rede")],
            DashScope::Images => vec![tile("IMAGENS", images.len(), "em cache")],
        };

        // --- table rows (per scope) ---
        let mut rows = Vec::new();
        let want_c = matches!(scope, DashScope::Global | DashScope::Containers);
        let want_v = matches!(scope, DashScope::Global | DashScope::Vms);
        let want_n = matches!(scope, DashScope::Global | DashScope::Networks);
        let want_s = matches!(scope, DashScope::Global | DashScope::Storage);
        if want_c {
            for (name, st, img) in &containers {
                rows.push(Row {
                    kind: "Container".into(),
                    name: name.clone(),
                    status: st.to_string(),
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
        })
    }
}

/// Problems = resources in an unhealthy state (the red panel). Pure over the
/// already-reconciled states — split out to be testable without stores.
fn derive_problems(
    containers: &[(String, Status, String)],
    vms: &[delonix_runtime_core::Vm],
) -> Vec<Problem> {
    let mut out = Vec::new();
    for (name, st, _) in containers {
        match st {
            Status::Failed(code) => out.push(Problem {
                resource: format!("container/{name}"),
                message: format!("saiu com código {code}"),
            }),
            Status::Crashed => out.push(Problem {
                resource: format!("container/{name}"),
                message: "morto por sinal (crash)".into(),
            }),
            _ => {}
        }
    }
    for v in vms {
        if matches!(v.status, Status::Failed(_) | Status::Crashed) {
            out.push(Problem {
                resource: format!("vm/{}", v.name),
                message: format!("estado {}", v.status),
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
        let mut t = super::output::Table::new(&["KIND", "NAME", "STATUS", "INFO"]);
        for r in &d.rows {
            let st = if color {
                format!("{}{}{}", if r.ok { GREEN } else { RED }, r.status, RESET)
            } else {
                r.status.clone()
            };
            t.row(vec![r.kind.clone(), r.name.clone(), st, r.extra.clone()]);
        }
        out.push_str(&t.render_all());
        out.push('\n');
    }

    // Problems panel.
    if d.problems.is_empty() {
        out.push_str(&format!(
            "{}{}✓ sem problemas identificados{}\n",
            c(BOLD),
            c(GREEN),
            c(RESET)
        ));
    } else {
        out.push_str(&format!(
            "{}{}⚠ PROBLEMAS IDENTIFICADOS ({}){}\n",
            c(BOLD),
            c(RED),
            d.problems.len(),
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

/// Runs the dashboard. `once` (or non-tty stdout) → a text snapshot; otherwise,
/// the interactive TUI.
pub fn run(scope: DashScope, once: bool) -> Result<()> {
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

    pub fn run_interactive(scope: DashScope) -> Result<()> {
        // Collect the FIRST snapshot BEFORE touching the terminal: if it fails,
        // return the error with the terminal still intact (no raw mode / alt
        // screen left to clean up). From here on, ALL exit paths restore
        // the terminal (the central `render` function does the cleanup once at the end).
        let data = DashData::collect(scope)?;

        terminal::enable_raw_mode().ok();
        execute!(stdout(), terminal::EnterAlternateScreen).ok();
        let res = render(scope, data);
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

    /// The drawing loop proper. Split out so that `run_interactive`
    /// can restore the terminal AFTERWARDS, whatever happens in here.
    fn render(scope: DashScope, mut data: DashData) -> Result<()> {
        let backend = ratatui::backend::CrosstermBackend::new(stdout());
        let mut term = Terminal::new(backend).map_err(io_err)?;
        let mut history: VecDeque<u64> = VecDeque::with_capacity(120);
        let mut last = Instant::now() - Duration::from_secs(2);

        loop {
            // Collect every ~1s (not every frame).
            if last.elapsed() >= Duration::from_secs(1) {
                data = DashData::collect(scope).unwrap_or(data);
                history.push_back(data.activity);
                if history.len() > 120 {
                    history.pop_front();
                }
                last = Instant::now();
            }
            if let Err(e) = term.draw(|f| draw(f, &data, history.make_contiguous())) {
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

    fn draw(f: &mut ratatui::Frame, d: &DashData, hist: &[u64]) {
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

        draw_sparkline(f, root[3], hist);

        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  q/Esc: sair   ·   refresh 1s",
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
        let header = TRow::new(["KIND", "NAME", "STATUS", "INFO"].map(|h| {
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
                    Cell::from(r.extra.clone()).style(Style::default().fg(Color::DarkGray)),
                ])
            })
            .collect();
        let widths = [
            Constraint::Length(10),
            Constraint::Percentage(35),
            Constraint::Length(12),
            Constraint::Percentage(40),
        ];
        let table = Table::new(trows, widths).header(header).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title("RECURSOS"),
        );
        f.render_widget(table, area);
    }

    fn draw_problems(f: &mut ratatui::Frame, area: Rect, problems: &[Problem]) {
        let (title, border) = if problems.is_empty() {
            ("OK", GREENC)
        } else {
            ("PROBLEMA IDENTIFICADO", REDC)
        };
        let mut lines: Vec<Line<'static>> = Vec::new();
        if problems.is_empty() {
            lines.push(Line::from(Span::styled(
                "✓ sem problemas",
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

    fn draw_sparkline(f: &mut ratatui::Frame, area: Rect, hist: &[u64]) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                "CONTAINERS A CORRER (ao longo do tempo)",
                Style::default().fg(Color::Gray),
            ));
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
            tiles: vec![tile("CONTAINERS", "2/3", "a correr / total")],
            rows: vec![
                Row {
                    kind: "Container".into(),
                    name: "web".into(),
                    status: "running".into(),
                    extra: "nginx".into(),
                    ok: true,
                },
                Row {
                    kind: "Container".into(),
                    name: "db".into(),
                    status: "crashed".into(),
                    extra: "postgres".into(),
                    ok: false,
                },
            ],
            problems: vec![Problem {
                resource: "container/db".into(),
                message: "morto por sinal (crash)".into(),
            }],
            activity: 2,
        }
    }

    #[test]
    fn snapshot_sem_cor_tem_titulo_tiles_e_problemas() {
        let s = render_snapshot(&sample(), false);
        assert!(s.contains("DELONIX — RESUMO"));
        assert!(s.contains("CONTAINERS"));
        assert!(s.contains("2/3"));
        assert!(s.contains("web"));
        assert!(s.contains("PROBLEMAS IDENTIFICADOS (1)"));
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
        assert!(s.contains("sem problemas identificados"));
        assert!(!s.contains("PROBLEMAS IDENTIFICADOS"));
    }

    #[test]
    fn derive_problems_apanha_crash_e_failed() {
        let containers = vec![
            ("web".to_string(), Status::Running, "nginx".to_string()),
            ("db".to_string(), Status::Crashed, "pg".to_string()),
            ("job".to_string(), Status::Failed(2), "busybox".to_string()),
        ];
        let ps = derive_problems(&containers, &[]);
        assert_eq!(ps.len(), 2);
        assert!(ps.iter().any(|p| p.resource == "container/db"));
        assert!(ps
            .iter()
            .any(|p| p.resource == "container/job" && p.message.contains("código 2")));
    }
}
