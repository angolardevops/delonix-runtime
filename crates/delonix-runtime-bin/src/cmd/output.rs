//! Camada de output partilhada da CLI — tabelas (`ls`, estilo `docker ps`) e
//! blocos de detalhe (`describe`, estilo `kubectl describe`).
//!
//! Antes disto cada grupo de comandos imprimia com `println!` e larguras
//! hardcoded (`{:<20}`), o que desalinhava a tabela toda assim que um nome ou
//! uma imagem passava a largura adivinhada. [`Table`] mede as colunas pelo
//! conteúdo real antes de imprimir.
//!
//! **Sem dependências novas**: este repo é público e não tem hoje nenhuma crate
//! de tabelas/cor/datas na árvore (`comfy-table`, `tabled`, `chrono`, …). Um
//! alinhador de colunas e um `localtime_r` são pequenos demais para justificar
//! aumentar a superfície de supply-chain de um runtime de containers.

/// Espaço entre colunas, como no `docker ps`.
const GAP: usize = 3;

// ---------------------------------------------------------------------------
// Cor
// ---------------------------------------------------------------------------

/// ANSI codes. Just these: a CLI that uses half a palette ends up with each
/// command inventing its own convention.
pub mod color {
    pub const RESET: &str = "\x1b[0m";
    pub const CYAN: &str = "\x1b[36m"; // info
    pub const YELLOW: &str = "\x1b[33m"; // warning
    pub const RED: &str = "\x1b[31m"; // error
    pub const GRAY: &str = "\x1b[90m"; // secondary (timestamps, details)
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

// (o antigo `tr(en, pt)` inline morreu a favor do catálogo `data/pt.po` —
// ver `cmd::po::t`; os pares espalhados pelo código eram intraduzíveis por
// ferramentas e impossíveis de rever num sítio só.)

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

/// Tabela alinhada pelo conteúdo: as colunas ficam com a largura da célula mais
/// larga (incluindo o cabeçalho). A última coluna nunca leva padding à direita,
/// para não deixar espaços em fim de linha.
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    /// Índices das colunas alinhadas à direita (números).
    right: Vec<usize>,
}

impl Table {
    pub fn new(headers: &[&str]) -> Self {
        Self {
            headers: headers.iter().map(|h| h.to_string()).collect(),
            rows: Vec::new(),
            right: Vec::new(),
        }
    }

    /// Alinha à direita a coluna `idx` (para tamanhos/contagens).
    pub fn right_align(mut self, idx: usize) -> Self {
        self.right.push(idx);
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

    /// Imprime o cabeçalho e as linhas. Um `ls` sem resultados imprime só o
    /// cabeçalho — é o que o `docker ps` faz, e diz ao utilizador que o comando
    /// correu e não encontrou nada (em vez de silêncio ambíguo).
    pub fn print(&self) {
        let widths = self.widths();
        println!("{}", self.render(&self.headers, &widths));
        for r in &self.rows {
            println!("{}", self.render(r, &widths));
        }
    }

    /// Como `print`, mas devolve a tabela inteira (cabeçalho + linhas) como String
    /// — para compor dentro doutra saída (ex.: o `dash` mete a tabela num painel).
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
                // A última coluna não leva padding à direita.
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

/// Largura em colunas de terminal, aproximada por número de `char`s (não de
/// bytes — um nome com acentos contaria a dobrar em `len()`). Não trata CJK
/// nem emoji com largura dupla; nomes de containers/imagens são ASCII na
/// prática e não vale a pena uma dependência `unicode-width` por isso.
fn display_width(s: &str) -> usize {
    s.chars().count()
}

/// Referência de imagem para MOSTRAR a um humano.
///
/// Uma referência fixada por digest (`kindest/node:v1.34.0@sha256:7416a61b…`,
/// 84 chars) é o que o motor precisa — reprodutibilidade — mas numa tabela só
/// empurra as colunas todas para fora do ecrã e o digest não diz nada a quem
/// lê. Com tag presente, o `@sha256:…` é redundante para efeitos de leitura:
/// mostra-se `kindest/node:v1.34.0`.
///
/// **Sem tag** (`repo@sha256:…`) o digest é a ÚNICA identificação que resta —
/// aí encurta-se, mas nunca se deita fora: `repo@sha256:7416a61b`.
pub fn display_ref(reference: &str) -> String {
    let Some((antes, digest)) = reference.split_once("@sha256:") else {
        return reference.to_string();
    };
    // `repo:tag@sha256:…` → a tag identifica; o digest é ruído na tabela.
    // Cuidado: um `:` no host com porta (`reg:5000/img`) não é uma tag — a tag
    // vem depois do ÚLTIMO '/'.
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

/// Trunca com reticências (`…`) se passar de `max` — para COMMAND/PORTS, que
/// podem ser arbitrariamente longos e rebentavam a tabela.
pub fn truncate(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

/// Bloco de detalhe ao estilo `kubectl describe`: coluna de chaves com largura
/// fixa, secções indentadas, listas com marcador.
pub struct Describe {
    lines: Vec<String>,
}

/// Largura da coluna de chaves — a mesma do `kubectl describe`.
const KEY_W: usize = 16;

impl Describe {
    pub fn new() -> Self {
        Self { lines: Vec::new() }
    }

    /// `Chave:        valor`
    pub fn field(&mut self, key: &str, val: impl AsRef<str>) -> &mut Self {
        let k = format!("{key}:");
        self.lines.push(format!("{k:<KEY_W$}{}", val.as_ref()));
        self
    }

    /// Campo opcional: omitido por inteiro quando é `None` (o `kubectl` faz o
    /// mesmo — não polui o detalhe com `<none>` para tudo o que não se aplica).
    pub fn field_opt(&mut self, key: &str, val: Option<impl AsRef<str>>) -> &mut Self {
        if let Some(v) = val {
            self.field(key, v.as_ref());
        }
        self
    }

    /// Cabeçalho de secção (`Mounts:`), cujo conteúdo vem indentado por
    /// [`Describe::item`].
    pub fn section(&mut self, key: &str) -> &mut Self {
        self.lines.push(format!("{key}:"));
        self
    }

    /// Secção com uma lista; vazia imprime `<none>` na mesma linha, para o
    /// leitor distinguir "não tem" de "esqueci-me de mostrar".
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

    /// Linha indentada dentro de uma secção.
    pub fn item(&mut self, val: impl AsRef<str>) -> &mut Self {
        self.lines.push(format!("  {}", val.as_ref()));
        self
    }

    /// Par chave/valor indentado dentro de uma secção.
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

    /// Como [`Describe::sub`], mas omitido por inteiro quando é `None`.
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

/// Formata um tamanho em bytes de forma legível (base 1024: B/KiB/MiB/GiB/TiB).
/// Uma barra de progresso de download numa linha, reescrita com `\r` (só em
/// tty; fora de um terminal — pipes/CI — não imprime nada, para não encher os
/// logs de linhas de progresso). `done`/`total` em bytes; `total` ausente
/// (resposta sem Content-Length) mostra só os bytes já lidos.
///
/// Barato de chamar a cada pedaço: o desenho real é feito pelo chamador, que
/// deve estrangular a frequência (ver `cmd::vmimage`); aqui só formatamos.
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

/// Fecha a linha da [`progress_bar`] (limpa-a e emite o `\n`), em tty.
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
    // 2 casas para GiB+, 1 casa para KiB/MiB — legível sem ruído.
    let prec = if unit >= 3 { 2 } else { 1 };
    format!("{val:.prec$} {}", UNITS[unit])
}

/// Formata um instante unix (segundos) como data/hora LOCAL "AAAA-MM-DD HH:MM".
/// Usa `localtime_r` (honra `/etc/localtime`/`TZ`); em falha, cai no valor cru.
pub fn fmt_local(unix: u64) -> String {
    let t = unix as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `t` é válido; `localtime_r` escreve em `tm` (buffer nosso, do
    // tamanho certo) e devolve NULL só em erro — que tratamos abaixo.
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

/// Idade relativa ao estilo do `docker ps` — "About a minute ago", "3 hours
/// ago". Função pura em `secs` para ser testável sem relógio.
pub fn fmt_age_secs(secs: u64) -> String {
    let d = fmt_duration_secs(secs);
    format!("{d} ago")
}

/// Duração legível, estilo docker: "5 seconds", "About a minute", "2 hours".
/// Deliberadamente grosseira — numa tabela, "3 days" é mais útil que "3d 4h 12m".
///
/// Porta o `units.HumanDuration` do docker à letra, incluindo a escolha de
/// baldes que à primeira vista parece arbitrária: os dias vão até às **2
/// semanas** (não 1), as semanas até aos **2 meses**, os meses até aos **2
/// anos**. É isso que evita o plural em falso — nenhum balde pode dar "1
/// weeks"/"1 months", porque cada um começa no 2. A primeira tentativa aqui
/// usou os limites "óbvios" (1 semana, 1 mês) mais "About a month/year" a
/// tapar o singular, e imprimia mesmo "1 weeks" para tudo entre 7 e 13 dias.
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

/// Idade a partir de um instante unix, tolerante a relógios que andaram para
/// trás (um `created_unix` no futuro dá 0, não um underflow gigante).
pub fn fmt_age(created_unix: u64) -> String {
    fmt_age_secs(now_unix().saturating_sub(created_unix))
}

/// Instante do boot (unix, segundos), do campo `btime` de `/proc/stat`.
fn boot_unix() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/stat").ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())
}

/// Há quantos segundos o processo de init do container arrancou, a partir do
/// `pid_starttime` (jiffies desde o boot, campo 22 de `/proc/<pid>/stat`).
///
/// **Porque não usar `created_unix`**: o `Up …` do `docker ps` é o tempo desde
/// o ARRANQUE, não desde a criação. Um container criado ontem e reiniciado há
/// 5 minutos (`container start`, política `--restart`) mostraria "Up 1 day" —
/// falso, e falso precisamente quando interessa (a depurar um crash-loop). O
/// `starttime` do processo é a única fonte que não mente.
pub fn uptime_from_starttime(starttime_jiffies: u64) -> Option<u64> {
    // SAFETY: `sysconf` é thread-safe e sem efeitos; devolve -1 em erro.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz <= 0 {
        return None;
    }
    let started_unix = boot_unix()? + starttime_jiffies / hz as u64;
    Some(now_unix().saturating_sub(started_unix))
}

/// Progresso ao estilo do `kind`, com **spinner animado**: cada passo mostra um
/// braille a girar (` ⠋ A arrancar o control-plane 🕹️ `) numa thread de fundo, e
/// a linha é reescrita com ` ✓ …` (ou ` ✗ …`) quando fecha.
///
/// # Porquê uma thread
///
/// O trabalho do passo (`node_exec_capture`) bloqueia o thread principal, às
/// vezes por minutos (`kubeadm init` puxa imagens). Sem uma thread a animar, a
/// linha ficava congelada e parecia pendurada. A thread só toca no stderr (o
/// output do passo vai para um ficheiro capturado, ver `node_exec_capture`), por
/// isso não há duas escritas a competir pela mesma linha.
///
/// **Sem TTY (pipe, CI, `2>&1 | tee`)** não há spinner nem `\r`: imprime-se só a
/// linha final, uma por passo — o que um log de CI quer.
pub struct Progress {
    tty: bool,
    msg: String,
    icon: String,
    spin: Option<SpinnerHandle>,
}

struct SpinnerHandle {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// Frames do spinner (braille, como o `kind`/`spinnies`).
const SPIN_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

impl Progress {
    pub fn new() -> Self {
        // SAFETY: isatty não tem pré-condições; 2 = stderr.
        let tty = unsafe { libc::isatty(2) } == 1;
        Self {
            tty,
            msg: String::new(),
            icon: String::new(),
            spin: None,
        }
    }

    /// Abre um passo e arranca o spinner (em TTY). `icon` é o emoji do fim.
    pub fn step(&mut self, msg: &str, icon: &str) {
        self.close_line('✗'); // fecha um passo anterior deixado em aberto
        self.msg = msg.to_string();
        self.icon = icon.to_string();
        if !self.tty {
            return;
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s2, msg, icon) = (stop.clone(), self.msg.clone(), self.icon.clone());
        let handle = std::thread::spawn(move || {
            use std::io::Write;
            let mut i = 0usize;
            while !s2.load(std::sync::atomic::Ordering::Relaxed) {
                // `\x1b[K` limpa até ao fim da linha (evita restos de um frame
                // mais longo). Sem `\n` — a linha é reescrita in-place.
                eprint!(
                    "\r {} {msg} {icon}\x1b[K",
                    SPIN_FRAMES[i % SPIN_FRAMES.len()]
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

    /// Fecha o passo actual com `✓`.
    pub fn ok(&mut self) {
        self.close_line('✓');
    }

    /// Pára o spinner (se houver) e escreve a linha final com `mark`. Idempotente
    /// — chamado pelo `ok`, pelo próximo `step` e pelo `Drop`.
    fn close_line(&mut self, mark: char) {
        let had_spinner = self.spin.is_some();
        if let Some(mut s) = self.spin.take() {
            s.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(h) = s.handle.take() {
                let _ = h.join();
            }
        } else if self.msg.is_empty() {
            return; // nada aberto
        }
        if self.tty {
            // `\r` + limpar a linha do spinner, depois a linha final.
            eprintln!("\r {mark} {} {}\x1b[K", self.msg, self.icon);
        } else if !self.msg.is_empty() {
            eprintln!(" {mark} {} {}", self.msg, self.icon);
        }
        let _ = had_spinner;
        self.msg.clear();
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        // Um passo deixado em aberto (erro a meio) fecha com ✗ em vez de ficar
        // com o spinner pendurado.
        self.close_line('✗');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabela_mede_pela_celula_mais_larga() {
        let mut t = Table::new(&["A", "B"]);
        t.row(vec!["um-nome-bem-comprido".into(), "x".into()]);
        t.row(vec!["curto".into(), "y".into()]);
        let w = t.widths();
        assert_eq!(w[0], "um-nome-bem-comprido".len());
        // Cabeçalho mais largo que o conteúdo ganha.
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
        // Conta chars, não bytes — senão um acento truncava cedo demais.
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
        // Regressão: um `image ls` real mostrou "1 weeks ago" para uma imagem de
        // 7 dias. Nenhuma duração pode produzir "1 <plural>" — no docker os
        // baldes de weeks/months/years começam todos no 2, por construção.
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
        // E os limites de balde, à letra do docker.
        assert_eq!(fmt_duration_secs(7 * DAY), "7 days");
        assert_eq!(fmt_duration_secs(13 * DAY), "13 days");
        assert_eq!(fmt_duration_secs(14 * DAY), "2 weeks");
        assert_eq!(fmt_duration_secs(60 * DAY), "2 months");
        assert_eq!(fmt_duration_secs(730 * DAY), "2 years");
    }

    #[test]
    fn idade_com_relogio_no_futuro_nao_faz_underflow() {
        // `created_unix` no futuro (relógio corrigido para trás) dava um
        // underflow de u64 → "584 milhões de anos ago".
        let futuro = now_unix() + 3600;
        assert_eq!(fmt_age(futuro), "Less than a second ago");
    }

    #[test]
    fn locale_reconhece_variantes_pt() {
        // A tradução em si vive no catálogo (`cmd::po`); aqui só o locale.
        set_lang("en");
        assert!(!is_pt());
        set_lang("pt");
        assert!(is_pt());
        // pt_AO e variantes contam como pt; qualquer outra coisa = en (default seguro).
        set_lang("pt_AO");
        assert!(is_pt());
        set_lang("fr");
        assert!(!is_pt());
        set_lang("en"); // repõe para não afectar outros testes
    }

    #[test]
    fn tamanhos_legiveis() {
        assert_eq!(fmt_size(512), "512 B");
        assert_eq!(fmt_size(1536), "1.5 KiB");
        assert_eq!(fmt_size(2 * 1024 * 1024 * 1024), "2.00 GiB");
    }

    #[test]
    fn display_ref_corta_o_digest_quando_ha_tag() {
        // O caso que motivou isto: um kindest/node fixado por digest.
        assert_eq!(
            display_ref("kindest/node:v1.34.0@sha256:7416a61b42b1662ca6ca89f02028ac133a309a2a30ba309614e8ec94d976dc5a"),
            "kindest/node:v1.34.0"
        );
        assert_eq!(
            display_ref("nginx:latest@sha256:abcdef0123"),
            "nginx:latest"
        );
        // Sem digest, intacto.
        assert_eq!(display_ref("alpine:3.19"), "alpine:3.19");
        assert_eq!(display_ref("nginx"), "nginx");
    }

    #[test]
    fn display_ref_sem_tag_encurta_o_digest_mas_nao_o_deita_fora() {
        // `repo@sha256:…` — o digest é a ÚNICA identificação; encurta-se, não se
        // remove (senão ficavam duas imagens diferentes com o mesmo nome).
        assert_eq!(
            display_ref("myrepo@sha256:7416a61b42b1662ca6ca89f0"),
            "myrepo@sha256:7416a61b"
        );
    }

    #[test]
    fn display_ref_nao_confunde_porta_do_registo_com_tag() {
        // `reg:5000/img@sha256:…` — o `:5000` é a porta do host, NÃO uma tag.
        // A tag, se houver, vem depois do último '/'. Aqui não há → encurta o digest.
        assert_eq!(
            display_ref("reg:5000/img@sha256:7416a61b42b1662c"),
            "reg:5000/img@sha256:7416a61b"
        );
        // Com porta E tag, a tag identifica → corta o digest.
        assert_eq!(
            display_ref("reg:5000/img:v2@sha256:7416a61b"),
            "reg:5000/img:v2"
        );
    }
}
