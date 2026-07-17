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
        Self { headers: headers.iter().map(|h| h.to_string()).collect(), rows: Vec::new(), right: Vec::new() }
    }

    /// Alinha à direita a coluna `idx` (para tamanhos/contagens).
    pub fn right_align(mut self, idx: usize) -> Self {
        self.right.push(idx);
        self
    }

    pub fn row(&mut self, cells: Vec<String>) {
        debug_assert_eq!(cells.len(), self.headers.len(), "linha com aridade diferente do cabeçalho");
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
        self.lines.push(format!("  {k:<w$}{v}", k = k, v = val.as_ref(), w = KEY_W - 2));
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
    format!("{:04}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Idade relativa ao estilo do `docker ps` — "About a minute ago", "3 hours
/// ago". Função pura em `secs` para ser testável sem relógio.
pub fn fmt_age_secs(secs: u64) -> String {
    let d = fmt_duration_secs(secs);
    format!("{d} ago")
}

/// Duração legível, estilo docker: "5 seconds", "About a minute", "2 hours".
/// Deliberadamente grosseira — numa tabela, "3 days" é mais útil que "3d 4h 12m".
pub fn fmt_duration_secs(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    const MONTH: u64 = 30 * DAY;
    const YEAR: u64 = 365 * DAY;
    match secs {
        s if s < 1 => "Less than a second".to_string(),
        1 => "1 second".to_string(),
        s if s < MIN => format!("{s} seconds"),
        s if s < 2 * MIN => "About a minute".to_string(),
        s if s < HOUR => format!("{} minutes", s / MIN),
        s if s < 2 * HOUR => "About an hour".to_string(),
        s if s < DAY => format!("{} hours", s / HOUR),
        s if s < 2 * DAY => "About a day".to_string(),
        s if s < WEEK => format!("{} days", s / DAY),
        s if s < MONTH => format!("{} weeks", s / WEEK),
        s if s < 2 * MONTH => "About a month".to_string(),
        s if s < YEAR => format!("{} months", s / MONTH),
        s if s < 2 * YEAR => "About a year".to_string(),
        s => format!("{} years", s / YEAR),
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
    s.lines().find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())
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
    fn idade_com_relogio_no_futuro_nao_faz_underflow() {
        // `created_unix` no futuro (relógio corrigido para trás) dava um
        // underflow de u64 → "584 milhões de anos ago".
        let futuro = now_unix() + 3600;
        assert_eq!(fmt_age(futuro), "Less than a second ago");
    }

    #[test]
    fn tamanhos_legiveis() {
        assert_eq!(fmt_size(512), "512 B");
        assert_eq!(fmt_size(1536), "1.5 KiB");
        assert_eq!(fmt_size(2 * 1024 * 1024 * 1024), "2.00 GiB");
    }
}
