//! `delonix dash` — dashboard de resumo/KPIs do runtime. Global (`delonix dash`)
//! ou contextual por grupo (`delonix container dash`, `vm dash`, ...).
//!
//! Duas saídas a partir da MESMA `DashData` (snapshot puro dos stores):
//!  * **TUI interactivo** (default, num terminal): tiles + tabela + painel de
//!    problemas + sparkline de actividade, refrescado a cada ~1s até `q`.
//!  * **`--once`** (ou sem tty): imprime UM snapshot de texto (ANSI) — para
//!    scripts/CI e para o smoke test (não precisa de terminal).
//!
//! A recolha de dados (`DashData::collect`) e a formatação do snapshot são puras
//! sobre os stores — testáveis sem terminal. O TUI é uma casca fina por cima.

use std::collections::VecDeque;
use std::io::IsTerminal;
use std::time::{Duration, Instant};

use delonix_runtime_core::{Result, Status};

use super::util::state_root;

/// Âmbito do dashboard: global ou focado num grupo de recursos.
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

/// Um KPI (tile) — rótulo + valor grande + subtítulo.
#[derive(Debug, Clone, PartialEq)]
pub struct Tile {
    pub label: String,
    pub value: String,
    pub sub: String,
}

/// Uma linha da tabela de recursos.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub kind: String,
    pub name: String,
    pub status: String,
    pub extra: String,
    /// `true` = estado saudável (Running/present); pinta a verde vs vermelho.
    pub ok: bool,
}

/// Um problema identificado (o painel vermelho da direita).
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    pub resource: String,
    pub message: String,
}

/// Snapshot completo do dashboard num instante — puro (só lê stores).
#[derive(Debug, Clone, PartialEq)]
pub struct DashData {
    pub scope: DashScope,
    pub tiles: Vec<Tile>,
    pub rows: Vec<Row>,
    pub problems: Vec<Problem>,
    /// Métrica única para o sparkline de actividade (containers a correr).
    pub activity: u64,
}

fn tile(label: &str, value: impl ToString, sub: &str) -> Tile {
    Tile { label: label.to_string(), value: value.to_string(), sub: sub.to_string() }
}

impl DashData {
    /// Recolhe o snapshot dos stores para o `scope` dado. Best-effort: um store
    /// que não abra conta como 0 (o dashboard nunca deve rebentar por um store).
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
        let c_running = containers.iter().filter(|(_, s, _)| *s == Status::Running).count();

        // --- vms ---
        let vms = delonix_vm::list(&root).unwrap_or_default();
        let vm_running = vms.iter().filter(|v| v.status == Status::Running).count();

        // --- redes / volumes / imagens / segredos ---
        let networks = delonix_net::NetworkStore::open(&root).and_then(|s| s.list()).unwrap_or_default();
        let volumes = delonix_volume::VolumeStore::open(&root).and_then(|s| s.list()).unwrap_or_default();
        let images = delonix_image::ImageStore::open(&root).and_then(|s| s.list()).map(|v| v.len()).unwrap_or(0);
        let secrets = delonix_runtime_core::SecretStore::open(&root).map(|s| s.list().len()).unwrap_or(0);

        // --- tiles (por scope) ---
        let tiles = match scope {
            DashScope::Global => vec![
                tile("CONTAINERS", format!("{c_running}/{}", containers.len()), "a correr / total"),
                tile("VMs", format!("{vm_running}/{}", vms.len()), "a correr / total"),
                tile("REDES", networks.len(), "definidas"),
                tile("VOLUMES", volumes.len(), "+ storage de rede"),
                tile("IMAGENS", images, "em cache"),
                tile("SEGREDOS", secrets, "no cofre"),
            ],
            DashScope::Containers => vec![
                tile("A CORRER", c_running, "Running"),
                tile("TOTAL", containers.len(), "todos os estados"),
                tile("PARADOS", containers.len().saturating_sub(c_running), "não-Running"),
            ],
            DashScope::Vms => vec![
                tile("A CORRER", vm_running, "Running"),
                tile("TOTAL", vms.len(), "todas"),
            ],
            DashScope::Networks => vec![tile("REDES", networks.len(), "definidas")],
            DashScope::Storage => vec![tile("VOLUMES", volumes.len(), "locais + rede")],
            DashScope::Images => vec![tile("IMAGENS", images, "em cache")],
        };

        // --- linhas da tabela (por scope) ---
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
                rows.push(Row { kind: "Network".into(), name: n.name.clone(), status: n.driver.clone(), extra: n.subnet.clone(), ok: true });
            }
        }
        if want_s {
            for vol in &volumes {
                rows.push(Row { kind: "Volume".into(), name: vol.name.clone(), status: vol.driver.clone(), extra: vol.mountpoint.clone(), ok: true });
            }
        }

        // --- problemas: derivados do estado VIVO (não de um manifesto) ---
        let problems = derive_problems(&containers, &vms);

        Ok(DashData { scope, tiles, rows, problems, activity: c_running as u64 })
    }
}

/// Problemas = recursos em estado não-saudável (o painel vermelho). Puro sobre os
/// estados já reconciliados — separado para ser testável sem stores.
fn derive_problems(
    containers: &[(String, Status, String)],
    vms: &[delonix_runtime_core::Vm],
) -> Vec<Problem> {
    let mut out = Vec::new();
    for (name, st, _) in containers {
        match st {
            Status::Failed(code) => out.push(Problem { resource: format!("container/{name}"), message: format!("saiu com código {code}") }),
            Status::Crashed => out.push(Problem { resource: format!("container/{name}"), message: "morto por sinal (crash)".into() }),
            _ => {}
        }
    }
    for v in vms {
        if matches!(v.status, Status::Failed(_) | Status::Crashed) {
            out.push(Problem { resource: format!("vm/{}", v.name), message: format!("estado {}", v.status) });
        }
    }
    out
}

// ===========================================================================
// Snapshot de texto (ANSI) — `--once` / sem tty
// ===========================================================================

const RESET: &str = "\x1b[0m";
const ORANGE: &str = "\x1b[38;5;208m";
const RED: &str = "\x1b[38;5;203m";
const GREEN: &str = "\x1b[38;5;114m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";

/// Formata o snapshot como texto colorido (ANSI). Puro sobre `DashData` — o teste
/// exercita-o com dados fabricados, sem stores nem terminal.
pub fn render_snapshot(d: &DashData, color: bool) -> String {
    let c = |code: &'static str| if color { code } else { "" };
    let mut out = String::new();
    out.push_str(&format!("{}{}{}{}\n\n", c(BOLD), c(ORANGE), d.scope.title(), c(RESET)));

    // Tiles — uma linha "LABEL  valor (sub)".
    for t in &d.tiles {
        out.push_str(&format!(
            "  {}{:<12}{} {}{}{:>8}{} {}{}{}\n",
            c(DIM), t.label, c(RESET), c(BOLD), c(ORANGE), t.value, c(RESET), c(DIM), t.sub, c(RESET)
        ));
    }
    out.push('\n');

    // Tabela de recursos.
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

    // Painel de problemas.
    if d.problems.is_empty() {
        out.push_str(&format!("{}{}✓ sem problemas identificados{}\n", c(BOLD), c(GREEN), c(RESET)));
    } else {
        out.push_str(&format!("{}{}⚠ PROBLEMAS IDENTIFICADOS ({}){}\n", c(BOLD), c(RED), d.problems.len(), c(RESET)));
        for p in &d.problems {
            out.push_str(&format!("  {}{}{} — {}\n", c(RED), p.resource, c(RESET), p.message));
        }
    }
    out
}

// ===========================================================================
// Entrypoint
// ===========================================================================

/// Corre o dashboard. `once` (ou stdout não-tty) → um snapshot de texto; senão,
/// o TUI interactivo.
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
// TUI interactivo (ratatui) — casca fina; a lógica está em DashData/render
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
        terminal::enable_raw_mode().ok();
        let mut so = stdout();
        execute!(so, terminal::EnterAlternateScreen).ok();
        let backend = ratatui::backend::CrosstermBackend::new(so);
        let mut term = Terminal::new(backend).map_err(io_err)?;

        let mut history: VecDeque<u64> = VecDeque::with_capacity(120);
        let mut last = Instant::now() - Duration::from_secs(2);
        let mut data = DashData::collect(scope)?;

        let res = loop {
            // Recolhe a cada ~1s (não a cada frame).
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
            // Poll de teclado com timeout curto (mantém o refresh fluido).
            match event::poll(Duration::from_millis(200)) {
                Ok(true) => {
                    if let Ok(Event::Key(k)) = event::read() {
                        if k.kind == KeyEventKind::Press {
                            let quit = matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
                                || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL));
                            if quit {
                                break Ok(());
                            }
                        }
                    }
                }
                Ok(false) => {}
                Err(e) => break Err(io_err(e)),
            }
        };

        terminal::disable_raw_mode().ok();
        execute!(term.backend_mut(), terminal::LeaveAlternateScreen).ok();
        term.show_cursor().ok();
        res
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
                Constraint::Length(1),  // título
                Constraint::Length(4),  // tiles
                Constraint::Min(6),     // tabela + problemas
                Constraint::Length(7),  // sparkline
                Constraint::Length(1),  // rodapé
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
                Line::from(Span::styled(&t.value, Style::default().fg(ORANGE).add_modifier(Modifier::BOLD))),
                Line::from(Span::styled(&t.sub, Style::default().fg(Color::DarkGray))),
            ];
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(t.label.clone(), Style::default().fg(Color::Gray)));
            f.render_widget(Paragraph::new(body).block(block), cols[i]);
        }
    }

    fn draw_table(f: &mut ratatui::Frame, area: Rect, rows: &[Row]) {
        let header = TRow::new(["KIND", "NAME", "STATUS", "INFO"].map(|h| {
            Cell::from(h).style(Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD))
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
        let widths = [Constraint::Length(10), Constraint::Percentage(35), Constraint::Length(12), Constraint::Percentage(40)];
        let table = Table::new(trows, widths)
            .header(header)
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)).title("RECURSOS"));
        f.render_widget(table, area);
    }

    fn draw_problems(f: &mut ratatui::Frame, area: Rect, problems: &[Problem]) {
        let (title, border) = if problems.is_empty() { ("OK", GREENC) } else { ("PROBLEMA IDENTIFICADO", REDC) };
        let mut lines: Vec<Line<'static>> = Vec::new();
        if problems.is_empty() {
            lines.push(Line::from(Span::styled("✓ sem problemas", Style::default().fg(GREENC))));
        } else {
            for p in problems {
                lines.push(Line::from(Span::styled(p.resource.clone(), Style::default().fg(REDC).add_modifier(Modifier::BOLD))));
                lines.push(Line::from(Span::styled(format!("  {}", p.message), Style::default().fg(Color::Gray))));
            }
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border))
            .title(Span::styled(title, Style::default().fg(border).add_modifier(Modifier::BOLD)));
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_sparkline(f: &mut ratatui::Frame, area: Rect, hist: &[u64]) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled("CONTAINERS A CORRER (ao longo do tempo)", Style::default().fg(Color::Gray)));
        let spark = Sparkline::default().block(block).data(hist).style(Style::default().fg(ORANGE));
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
                Row { kind: "Container".into(), name: "web".into(), status: "running".into(), extra: "nginx".into(), ok: true },
                Row { kind: "Container".into(), name: "db".into(), status: "crashed".into(), extra: "postgres".into(), ok: false },
            ],
            problems: vec![Problem { resource: "container/db".into(), message: "morto por sinal (crash)".into() }],
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
        // sem cor → sem sequências ANSI.
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
        assert!(ps.iter().any(|p| p.resource == "container/job" && p.message.contains("código 2")));
    }
}
