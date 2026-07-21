//! i18n via an EMBEDDED gettext (`.po`) catalog — the code source is 100%
//! English (the market standard in a public repo) and the translations live in
//! a standard `.po` file, compiled into the binary.
//!
//! Why `.po` and not inline `tr(en, pt)` pairs: the inline pair scatters the
//! translation across 30 code files (impossible to review or hand to a
//! translator), whereas a catalog is ONE file in the format that translation
//! tools (Poedit, Weblate, Crowdin) speak natively. Adding a new language =
//! adding a `.po`, zero code changes.
//!
//! A minimal in-house parser (~50 lines) instead of the `gettext` crate: the
//! project rule is not to grow the supply-chain surface for convenience (see
//! CLAUDE.md, Output section) — the subset we need (msgid/msgstr with
//! continuations and escapes) fits here and has tests.

use std::collections::HashMap;
use std::sync::OnceLock;

/// The pt_AO catalog, embedded in the binary. Regenerating entries: each new
/// UI string enters here with the EN as `msgid` — never in the code.
const PT_PO: &str = include_str!("../../data/pt.po");

fn catalog() -> &'static HashMap<String, String> {
    static CAT: OnceLock<HashMap<String, String>> = OnceLock::new();
    CAT.get_or_init(|| parse_po(PT_PO))
}

/// Translates `en` to the active language — the direct replacement for the old
/// `tr(en, pt)`: same return signature (`&'static str`, possible because the
/// catalog lives in a static `OnceLock`), but with the PT in `data/pt.po`
/// instead of scattered across the code. Without a translation (or in English),
/// it returns `en` itself — the UI never goes mute over a missing msgid.
pub fn t(en: &'static str) -> &'static str {
    if !super::output::is_pt() {
        return en;
    }
    match catalog().get(en) {
        Some(pt) if !pt.is_empty() => pt.as_str(),
        _ => en,
    }
}

/// `t()` for TEMPLATES with values: `format!` requires compile-time literals,
/// so interpolated messages translate the template and substitute the NAMED
/// placeholders afterwards (`{name}` — named on purpose: a translation may
/// reorder them, which positional `{}` don't allow).
///
///   tf("port {port} is taken by '{owner}'", &[("port", &hp), ("owner", &ow)])
pub fn tf(en: &'static str, subs: &[(&str, &str)]) -> String {
    let mut out = t(en).to_string();
    for (k, v) in subs {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

/// Variant for DYNAMIC strings — the clap help and the ERROR PRINTER of
/// main.rs (messages from the ENGINE crates arrive as already-formatted text;
/// those crates can't depend on this catalog, so translation happens at
/// output, by looking up the full EN text — messages with interpolated values
/// don't match and come out in EN, a known and documented limitation).
pub fn t_dyn(s: &str) -> String {
    t_owned(s)
}

fn t_owned(s: &str) -> String {
    if !super::output::is_pt() {
        return s.to_string();
    }
    match catalog().get(s) {
        Some(pt) if !pt.is_empty() => pt.clone(),
        _ => s.to_string(),
    }
}

/// Parser for the subset of `.po` we use: `msgid`/`msgstr` entries, with
/// continuation lines (`"..."`) and `\n`/`\"`/`\\` escapes. Comments (`#`) and
/// metadata (the header's empty msgid) are ignored.
fn parse_po(src: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let (mut id, mut val) = (String::new(), String::new());
    // 0 = outside, 1 = accumulating msgid, 2 = accumulating msgstr
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

/// Undoes the quotes and escapes of a `.po` string (`"a \"b\"\n"` → `a "b"<nl>`).
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

/// `t()` for HELP strings: the clap derive REMOVES the trailing period of the
/// doc-comment in the short help, but in the catalog the msgids keep natural
/// punctuation — if the exact lookup fails, it tries with `.` and strips it
/// again.
fn t_help(s: &str) -> String {
    let direct = t_owned(s);
    if direct != s {
        return direct;
    }
    let with_dot = format!("{s}.");
    let translated = t_owned(&with_dot);
    if translated != with_dot {
        translated.trim_end_matches('.').to_string()
    } else {
        s.to_string()
    }
}

/// Translates the help text of an entire `clap::Command` (about/long_about of
/// each subcommand + help of each argument), recursively, via the catalog.
///
/// The clap derive freezes the help strings at compile-time; this rewrites them
/// AFTER building the `Command` and BEFORE the parse — which is what lets us
/// have the source in EN and `--l18n=pt` serving the help in Portuguese from
/// the SAME binary.
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

/// Peeks the language in the argv/environment BEFORE the clap parse — the help
/// is generated DURING the parse, so the language has to be decided first
/// (`--l18n pt`, `--l18n=pt` or `$DELONIX_L18N`).
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
        // If someone breaks the data/pt.po, this test blows up BEFORE the release.
        assert!(!catalog().is_empty(), "data/pt.po vazio ou malformado");
    }
}
