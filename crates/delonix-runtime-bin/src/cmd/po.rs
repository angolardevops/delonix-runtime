//! i18n por catálogo gettext (`.po`) EMBUTIDO — a fonte do código é 100%
//! inglês (o padrão de mercado num repo público) e as traduções vivem num
//! ficheiro `.po` standard, compilado para dentro do binário.
//!
//! Porquê `.po` e não pares inline `tr(en, pt)`: o par inline espalha a
//! tradução por 30 ficheiros de código (impossível de rever ou entregar a um
//! tradutor), enquanto um catálogo é UM ficheiro com o formato que as
//! ferramentas de tradução (Poedit, Weblate, Crowdin) falam nativamente.
//! Adicionar uma língua nova = adicionar um `.po`, zero mudanças de código.
//!
//! Parser próprio mínimo (~50 linhas) em vez do crate `gettext`: a regra do
//! projecto é não aumentar a superfície de supply-chain por conveniência
//! (ver CLAUDE.md, secção Output) — o subconjunto que precisamos (msgid/
//! msgstr com continuações e escapes) cabe aqui e tem testes.

use std::collections::HashMap;
use std::sync::OnceLock;

/// O catálogo pt_AO, embutido no binário. Regenerar entradas: cada string
/// nova de UI entra aqui com o EN como `msgid` — nunca no código.
const PT_PO: &str = include_str!("../../data/pt.po");

fn catalog() -> &'static HashMap<String, String> {
    static CAT: OnceLock<HashMap<String, String>> = OnceLock::new();
    CAT.get_or_init(|| parse_po(PT_PO))
}

/// Traduz `en` para a língua activa. Sem tradução no catálogo (ou em inglês),
/// devolve o próprio `en` — a UI nunca fica muda por um msgid em falta.
pub fn t(en: &str) -> String {
    if !super::output::is_pt() {
        return en.to_string();
    }
    match catalog().get(en) {
        Some(pt) if !pt.is_empty() => pt.clone(),
        _ => en.to_string(),
    }
}

/// Parser do subconjunto de `.po` que usamos: entradas `msgid`/`msgstr`, com
/// linhas de continuação (`"..."`) e escapes `\n`/`\"`/`\\`. Comentários (`#`)
/// e metadados (o msgid vazio do cabeçalho) são ignorados.
fn parse_po(src: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let (mut id, mut val) = (String::new(), String::new());
    // 0 = fora, 1 = a acumular msgid, 2 = a acumular msgstr
    let mut state = 0u8;
    let mut flush = |id: &mut String, val: &mut String| {
        if !id.is_empty() {
            map.insert(std::mem::take(id), std::mem::take(val));
        } else {
            id.clear();
            val.clear();
        }
    };
    for line in src.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("msgid ") {
            if state == 2 {
                flush(&mut id, &mut val);
            }
            state = 1;
            id.push_str(&unquote(rest));
        } else if let Some(rest) = line.strip_prefix("msgstr ") {
            state = 2;
            val.push_str(&unquote(rest));
        } else if line.starts_with('"') {
            match state {
                1 => id.push_str(&unquote(line)),
                2 => val.push_str(&unquote(line)),
                _ => {}
            }
        } else if (line.is_empty() || line.starts_with('#')) && state == 2 {
            flush(&mut id, &mut val);
            state = 0;
        }
    }
    if state == 2 {
        flush(&mut id, &mut val);
    }
    map
}

/// Desfaz as aspas e escapes de uma string `.po` (`"a \"b\"\n"` → `a "b"<nl>`).
fn unquote(s: &str) -> String {
    let inner = s.trim().trim_start_matches('"').trim_end_matches('"');
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `t()` para strings de HELP: o clap derive REMOVE o ponto final do
/// doc-comment na ajuda curta, mas no catálogo os msgids ficam com pontuação
/// natural — se o lookup exacto falhar, tenta com `.` e volta a tirá-lo.
fn t_help(s: &str) -> String {
    let direct = t(s);
    if direct != s {
        return direct;
    }
    let with_dot = format!("{s}.");
    let translated = t(&with_dot);
    if translated != with_dot {
        translated.trim_end_matches('.').to_string()
    } else {
        s.to_string()
    }
}

/// Traduz o texto de ajuda de um `clap::Command` inteiro (about/long_about de
/// cada subcomando + help de cada argumento), recursivamente, via catálogo.
///
/// O clap derive congela as strings de help em compile-time; isto reescreve-as
/// DEPOIS de construir o `Command` e ANTES do parse — é o que permite ter a
/// fonte em EN e `--l18n=pt` a servir o help em português do MESMO binário.
pub fn translate_help(mut cmd: clap::Command) -> clap::Command {
    if let Some(about) = cmd.get_about().map(|s| s.to_string()) {
        cmd = cmd.about(t_help(&about));
    }
    if let Some(long) = cmd.get_long_about().map(|s| s.to_string()) {
        cmd = cmd.long_about(t_help(&long));
    }
    let arg_ids: Vec<String> = cmd
        .get_arguments()
        .map(|a| a.get_id().as_str().to_string())
        .collect();
    for id in arg_ids {
        cmd = cmd.mut_arg(&id, |a| {
            let a = match a.get_help().map(|h| h.to_string()) {
                Some(h) => a.help(t_help(&h)),
                None => a,
            };
            match a.get_long_help().map(|h| h.to_string()) {
                Some(h) => a.long_help(t_help(&h)),
                None => a,
            }
        });
    }
    let sub_names: Vec<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_string())
        .collect();
    for name in sub_names {
        cmd = cmd.mut_subcommand(name, translate_help);
    }
    cmd
}

/// Espreita a língua no argv/ambiente ANTES do parse do clap — o help é
/// gerado DURANTE o parse, logo a língua tem de estar decidida primeiro
/// (`--l18n pt`, `--l18n=pt` ou `$DELONIX_L18N`).
pub fn peek_lang() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--l18n" {
            return args.next();
        }
        if let Some(v) = a.strip_prefix("--l18n=") {
            return Some(v.to_string());
        }
    }
    std::env::var("DELONIX_L18N").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_po_entrada_simples() {
        let m = parse_po("msgid \"hello\"\nmsgstr \"olá\"\n");
        assert_eq!(m.get("hello").map(String::as_str), Some("olá"));
    }

    #[test]
    fn parse_po_continuacao_e_escapes() {
        let src = "msgid \"\"\n\"line one \\\"quoted\\\"\\n\"\n\"line two\"\nmsgstr \"\"\n\"linha um\\n\"\n\"linha dois\"\n";
        let m = parse_po(src);
        assert_eq!(
            m.get("line one \"quoted\"\nline two").map(String::as_str),
            Some("linha um\nlinha dois")
        );
    }

    #[test]
    fn parse_po_ignora_cabecalho_e_comentarios() {
        let src =
            "# comentário\nmsgid \"\"\nmsgstr \"meta\"\n\n# outro\nmsgid \"a\"\nmsgstr \"b\"\n";
        let m = parse_po(src);
        assert!(!m.contains_key(""));
        assert_eq!(m.get("a").map(String::as_str), Some("b"));
    }

    #[test]
    fn catalogo_embutido_parseia_e_tem_entradas() {
        // Se alguém partir o data/pt.po, este teste rebenta ANTES da release.
        assert!(!catalog().is_empty(), "data/pt.po vazio ou malformado");
    }
}
