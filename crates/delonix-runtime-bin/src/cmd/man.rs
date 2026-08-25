//! `delonix man` — the manual pages, in roff, generated from the SAME
//! `clap::Command` tree that produces `--help` and drives the parser.
//!
//! # Why generated and not written
//!
//! A hand-written manpage is a third copy of the CLI's surface (after the
//! parser and the docs site), and the only one with nothing watching it: a flag
//! renamed today leaves the page wrong until a user hits it. Here `NAME`,
//! `SYNOPSIS`, `OPTIONS` and `COMMANDS` are read off the live `Command`, and
//! `EXAMPLES`/`SEE ALSO` come from `manual::entry` — the same table `--help`
//! renders. There is no text here that is not derived from something the
//! binary already had to be right about.
//!
//! # Why a roff writer of our own and not `clap_mangen`
//!
//! Explicit decision, in the spirit of the repo rule on new dependencies (see
//! `AGENTS.md`): the two exceptions on record (`ratatui`, `schemars`) were both
//! taken deliberately, and this did not need to be a third. The cost is that
//! roff escaping is now ours to get right — hence `esc`, and hence the tests
//! that pin the three cases that actually bite.
//!
//! # One page per command
//!
//! `delonix-container-run.1`, not one 234-command page: it is what `docker` and
//! `git` do, and it is what makes `man delonix-container-run` work — the way
//! anyone actually reaches a manpage.

use std::io::Write;
use std::path::PathBuf;

use delonix_runtime_core::{Error, Result};

use super::manual;
use super::po;

#[derive(clap::Args)]
pub struct ManArgs {
    /// Command to document (e.g. `container run`). With none, the top-level page.
    #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::man_commands))]
    pub command: Vec<String>,
    /// Write EVERY page as `<dir>/man1/delonix*.1` instead of printing one to stdout.
    #[arg(long, value_name = "DIR")]
    pub dir: Option<PathBuf>,
    /// With `--dir`, also write the gzip-friendly index (`delonix-man-pages.txt`) listing what was generated.
    #[arg(long, requires = "dir")]
    pub index: bool,
}

/// roff-escapes a run of text.
///
/// Four cases, each a real defect if missed:
///
/// * a literal backslash would start an escape sequence;
/// * a line STARTING with `.` or `'` is read as a roff request and the line
///   vanishes from the rendered page;
/// * a bare `-` renders as a typographic hyphen, which breaks copy-paste of
///   every flag in OPTIONS — the most common thing anyone copies out of a
///   manpage;
/// * anything non-ASCII has to go out as a `\[uXXXX]` escape. This text is
///   full of `—`, `·` and Portuguese accents, and raw UTF-8 makes troff emit
///   `invalid input character code 128` — measured: **149 of the 237 pages**.
///   `man-db` hides it by running `preconv` for us, which is exactly why it
///   would have shipped unnoticed; plain `groff`, and anyone piping the page
///   somewhere else, sees mojibake. The escape is understood everywhere and
///   depends on no preprocessor.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for (i, line) in s.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if line.starts_with('.') || line.starts_with('\'') {
            out.push_str("\\&");
        }
        for c in line.chars() {
            match c {
                '\\' => out.push_str("\\e"),
                '-' => out.push_str("\\-"),
                c if (c as u32) < 128 => out.push(c),
                c => out.push_str(&format!("\\[u{:04X}]", c as u32)),
            }
        }
    }
    out
}

/// Page name for a command path: `container run` -> `delonix-container-run`.
fn page_name(path: &str) -> String {
    if path.is_empty() {
        "delonix".to_string()
    } else {
        format!("delonix-{}", path.replace(' ', "-"))
    }
}

/// Renders one page.
fn render(cmd: &clap::Command, path: &str) -> String {
    let name = page_name(path);
    let full = if path.is_empty() {
        "delonix".to_string()
    } else {
        format!("delonix {path}")
    };
    let mut o = String::new();
    // `DELONIX_BUILD_DATE` and not the clock: two runs of the generator on the
    // same commit have to produce identical bytes, or the CI check that the
    // committed pages ARE the generated ones fails on every unrelated build.
    o.push_str(&format!(
        ".TH {} 1 \"{}\" \"delonix {}\" \"{}\"\n",
        name.to_uppercase().replace('-', "\\-"),
        env!("DELONIX_BUILD_DATE"),
        env!("CARGO_PKG_VERSION"),
        // Through `esc` like everything else: under `--l18n=pt` this is
        // "Manual do Delonix Runtime", and the `ã` is exactly the kind of
        // character that would reach troff raw — in the page header, on every
        // single page.
        esc(po::t("Delonix Runtime Manual")),
    ));

    o.push_str(".SH NAME\n");
    let about = cmd
        .get_about()
        .map(|s| s.to_string())
        .unwrap_or_else(|| full.clone());
    // A `NAME` line is one line by convention (`whatis`/`apropos` parse it);
    // a newline inside the about would silently split the entry.
    let one_line = about.split_whitespace().collect::<Vec<_>>().join(" ");
    o.push_str(&format!("{} \\- {}\n", esc(&full), esc(&one_line)));

    o.push_str(".SH SYNOPSIS\n");
    o.push_str(&format!(".B {}\n", esc(&full)));
    // The usage line comes out of the RENDERED help, not `render_usage()`.
    // Measured: on a subcommand lifted out of its parent — even after
    // `build()` — `render_usage()` returns the bare name and drops
    // `[OPTIONS] <IMAGE> …`, leaving the one section of a manpage nobody can
    // do without saying nothing at all. `render_help()` carries the full line
    // because that is what `--help` itself prints.
    let usage_line = cmd
        .clone()
        .render_help()
        .to_string()
        .lines()
        .find(|l| l.trim_start().starts_with("Usage:"))
        .map(|l| l.trim().to_string())
        .unwrap_or_default();
    // What is wanted is only what comes AFTER the command name — the name is
    // already on the `.B` line. Cutting by the full path (`delonix container
    // run`) does not work: lifted out of its parent, clap writes the usage with
    // the SHORT name (`Usage: run [OPTIONS] …`), the prefix never matches, and
    // the SYNOPSIS silently comes out empty. Cut at the last occurrence of the
    // name clap actually used.
    let after_colon = usage_line
        .split_once(':')
        .map(|(_, r)| r)
        .unwrap_or_default()
        .trim();
    let leaf = cmd.get_name();
    let tail = match after_colon.rfind(leaf) {
        Some(i) => after_colon[i + leaf.len()..].trim().to_string(),
        None => String::new(),
    };
    if !tail.is_empty() {
        o.push_str(&format!("{}\n", esc(&tail)));
    }

    o.push_str(".SH DESCRIPTION\n");
    let desc = cmd
        .get_long_about()
        .or_else(|| cmd.get_about())
        .map(|s| s.to_string())
        .unwrap_or_default();
    for para in desc.split("\n\n") {
        let p = para.split_whitespace().collect::<Vec<_>>().join(" ");
        if !p.is_empty() {
            o.push_str(&format!("{}\n.PP\n", esc(&p)));
        }
    }

    // Positionals and flags, each as a tagged paragraph — the shape a reader
    // scans for.
    let args: Vec<&clap::Arg> = cmd.get_arguments().filter(|a| !a.is_hide_set()).collect();
    let (pos, opts): (Vec<_>, Vec<_>) = args.iter().partition(|a| a.is_positional());
    if !pos.is_empty() {
        o.push_str(".SH ARGUMENTS\n");
        for a in pos {
            o.push_str(&format!(".TP\n.B {}\n", esc(&arg_label(a))));
            o.push_str(&format!("{}\n", esc(&arg_help(a))));
        }
    }
    if !opts.is_empty() {
        o.push_str(".SH OPTIONS\n");
        for a in opts {
            o.push_str(&format!(".TP\n.B {}\n", esc(&arg_label(a))));
            o.push_str(&format!("{}\n", esc(&arg_help(a))));
        }
    }

    let subs: Vec<&clap::Command> = cmd
        .get_subcommands()
        .filter(|s| s.get_name() != "help" && !s.is_hide_set())
        .collect();
    if !subs.is_empty() {
        o.push_str(".SH COMMANDS\n");
        for s in subs {
            o.push_str(&format!(".TP\n.B {}\n", esc(s.get_name())));
            let a = s.get_about().map(|x| x.to_string()).unwrap_or_default();
            o.push_str(&format!(
                "{}\n",
                esc(&a.split_whitespace().collect::<Vec<_>>().join(" "))
            ));
        }
    }

    if let Some(e) = manual::entry(path) {
        if !e.examples.is_empty() {
            // Translated, unlike NAME/SYNOPSIS/DESCRIPTION/OPTIONS above.
            // The split is deliberate: those are the STRUCTURAL headings that
            // `whatis`/`apropos`/`man -k` index by, and renaming them would
            // make the page unfindable by the tools; EXAMPLES and SEE ALSO are
            // editorial, they already carry translated prose underneath, and
            // leaving them in English would be the one English line on an
            // otherwise Portuguese page — the same inconsistency `--help`
            // already avoids.
            o.push_str(&format!(".SH {}\n", esc(po::t("EXAMPLES"))));
            for (comment, line) in e.examples {
                o.push_str(&format!(".PP\n{}\n", esc(po::t(comment))));
                // `.EX`/`.EE` is the man macro for a literal example block: it
                // keeps the command on one line and in a monospace font, which
                // is what makes it paste-able.
                o.push_str(&format!(".EX\n{}\n.EE\n", esc(line)));
            }
        }
        if !e.see_also.is_empty() {
            o.push_str(&format!(".SH \"{}\"\n", esc(po::t("SEE ALSO"))));
            let refs: Vec<String> = e
                .see_also
                .iter()
                .map(|r| format!(".BR {} (1)", page_name(r).replace('-', "\\-")))
                .collect();
            o.push_str(&refs.join(",\n"));
            o.push('\n');
        }
    }

    o.push_str(".SH AUTHOR\n");
    o.push_str("Walter Angolar and the Delonix Runtime contributors.\n");
    o.push_str(".SH LICENSE\n");
    o.push_str("Apache\\-2.0. https://github.com/angolardevops/delonix\\-runtime\n");
    o
}

/// `--flag, -f <VALUE>` as the OPTIONS heading.
fn arg_label(a: &clap::Arg) -> String {
    if a.is_positional() {
        return format!(
            "<{}>",
            a.get_value_names()
                .and_then(|n| n.first().map(|s| s.to_string()))
                .unwrap_or_else(|| a.get_id().to_string().to_uppercase())
        );
    }
    let mut parts = Vec::new();
    if let Some(s) = a.get_short() {
        parts.push(format!("-{s}"));
    }
    if let Some(l) = a.get_long() {
        parts.push(format!("--{l}"));
    }
    let mut label = parts.join(", ");
    if a.get_action().takes_values() {
        let v = a
            .get_value_names()
            .and_then(|n| n.first().map(|s| s.to_string()))
            .unwrap_or_else(|| a.get_id().to_string().to_uppercase());
        label.push_str(&format!(" <{v}>"));
    }
    label
}

fn arg_help(a: &clap::Arg) -> String {
    let h = a
        .get_long_help()
        .or_else(|| a.get_help())
        .map(|s| s.to_string())
        .unwrap_or_default();
    let mut out = h.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(d) = a.get_default_values().first() {
        if !d.is_empty() {
            out.push_str(&format!(" [default: {}]", d.to_string_lossy()));
        }
    }
    out
}

/// Walks the tree, calling `f` for every documented command.
fn walk(cmd: &clap::Command, path: String, f: &mut impl FnMut(&clap::Command, &str)) {
    f(cmd, &path);
    for s in cmd.get_subcommands() {
        if s.get_name() == "help" || s.is_hide_set() {
            continue;
        }
        let child = if path.is_empty() {
            s.get_name().to_string()
        } else {
            format!("{path} {}", s.get_name())
        };
        walk(s, child, f);
    }
}

/// Finds a subcommand by path, so `delonix man container run` is an error when
/// the path is wrong rather than an empty page.
fn find<'a>(root: &'a clap::Command, path: &[String]) -> Option<&'a clap::Command> {
    let mut cur = root;
    for seg in path {
        cur = cur.get_subcommands().find(|s| s.get_name() == seg)?;
    }
    Some(cur)
}

pub fn run(args: ManArgs) -> Result<()> {
    let root = crate::build_command();
    if let Some(dir) = args.dir {
        let man1 = dir.join("man1");
        std::fs::create_dir_all(&man1)?;
        let mut written: Vec<String> = Vec::new();
        let mut err: Option<String> = None;
        walk(&root, String::new(), &mut |cmd, path| {
            if err.is_some() {
                return;
            }
            let file = man1.join(format!("{}.1", page_name(path)));
            if let Err(e) = std::fs::write(&file, render(cmd, path)) {
                err = Some(format!("{}: {e}", file.display()));
                return;
            }
            written.push(page_name(path));
        });
        if let Some(e) = err {
            return Err(Error::Invalid(e));
        }
        if args.index {
            let idx = dir.join("delonix-man-pages.txt");
            std::fs::write(&idx, written.join("\n") + "\n")?;
        }
        println!(
            "{}",
            po::tf(
                "{n} manual page(s) written to {dir}",
                &[
                    ("n", &written.len().to_string()),
                    ("dir", &man1.display().to_string()),
                ],
            )
        );
        println!(
            "{}",
            po::tf(
                "install them with: sudo cp {dir}/*.1 /usr/local/share/man/man1/ && sudo mandb",
                &[("dir", &man1.display().to_string())],
            )
        );
        return Ok(());
    }
    let cmd = find(&root, &args.command).ok_or_else(|| {
        Error::Invalid(po::tf(
            "no such command: {cmd} (see `delonix --help`)",
            &[("cmd", &args.command.join(" "))],
        ))
    })?;
    // Straight to stdout, unbuffered by line: the intended use is
    // `delonix man container run | man -l -` and a pipe into `man` is the
    // reason SIGPIPE is set to default in `main`.
    let page = render(cmd, &args.command.join(" "));
    std::io::stdout().write_all(page.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapa_a_barra_e_o_hifen() {
        // Um hífen cru vira hífen tipográfico e parte o copiar-colar de uma
        // flag; uma barra crua abre uma sequência de escape do roff.
        assert_eq!(esc("--net a\\b"), "\\-\\-net a\\eb");
    }

    #[test]
    fn uma_linha_iniciada_por_ponto_nao_vira_pedido_roff() {
        // Sem o `\&`, o roff lê a linha como um pedido e ela DESAPARECE da
        // página renderizada — a falha mais silenciosa deste ficheiro.
        assert!(esc(".PP no início").starts_with("\\&."));
        assert!(esc("'quote no início").starts_with("\\&'"));
        // No meio de uma linha, um ponto é só um ponto.
        assert!(!esc("um. dois").starts_with("\\&"));
    }

    #[test]
    fn o_nome_da_pagina_segue_a_convencao_do_docker_e_do_git() {
        assert_eq!(page_name(""), "delonix");
        assert_eq!(page_name("container run"), "delonix-container-run");
        assert_eq!(page_name("net ingress allow"), "delonix-net-ingress-allow");
    }

    /// Uma página tem de trazer as secções que um leitor de `man` procura, e o
    /// `.TH` tem de ser a PRIMEIRA linha (sem ele o `man` não a formata de todo).
    #[test]
    fn a_pagina_tem_a_estrutura_de_uma_manpage() {
        let root = crate::build_command();
        let cmd = find(&root, &["container".into(), "run".into()]).unwrap();
        let p = render(cmd, "container run");
        assert!(p.starts_with(".TH DELONIX\\-CONTAINER\\-RUN 1 "), "{p:.80}");
        for sec in [".SH NAME", ".SH SYNOPSIS", ".SH DESCRIPTION", ".SH OPTIONS"] {
            assert!(p.contains(sec), "falta {sec}");
        }
        // Os exemplos da tabela do manual têm de chegar à página, dentro de um
        // bloco literal `.EX`/`.EE` — é isso que os mantém coláveis.
        assert!(p.contains(".SH EXAMPLES"));
        assert!(p.contains(".EX\ndelonix container run"));
    }

    /// O SYNOPSIS tem de dizer os argumentos, não só o nome.
    ///
    /// Regressão de um defeito REAL e silencioso desta sessão: `render_usage()`
    /// num subcomando levantado do pai devolve o nome nu, e depois o corte pelo
    /// caminho completo não casava com o nome curto que o clap escreve. Nos dois
    /// casos a página gerava-se, formatava-se e ficava com um SYNOPSIS que não
    /// diz nada — nada falha, só se perde a secção mais consultada.
    #[test]
    fn o_synopsis_traz_os_argumentos_e_nao_so_o_nome() {
        let root = crate::build_command();
        for (path, esperado) in [
            (vec!["container", "run"], "<IMAGE>"),
            (vec!["vm", "create"], "<NAME>"),
            (vec!["net", "ingress"], "<COMMAND>"),
        ] {
            let segs: Vec<String> = path.iter().map(|s| s.to_string()).collect();
            let cmd = find(&root, &segs).unwrap();
            let p = render(cmd, &segs.join(" "));
            let syn: String = p
                .lines()
                .skip_while(|l| !l.starts_with(".SH SYNOPSIS"))
                .take_while(|l| !l.starts_with(".SH DESCRIPTION"))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                syn.contains(esperado),
                "SYNOPSIS de `{}` sem `{esperado}`:\n{syn}",
                segs.join(" ")
            );
        }
    }

    /// Nenhuma página pode sair vazia nem perder o cabeçalho — o teste percorre
    /// a árvore inteira em vez de confiar numa amostra.
    #[test]
    fn todas_as_paginas_geram_com_cabecalho() {
        let root = crate::build_command();
        let mut n = 0;
        let mut mal = Vec::new();
        walk(&root, String::new(), &mut |cmd, path| {
            let p = render(cmd, path);
            n += 1;
            if !p.starts_with(".TH ") || !p.contains(".SH NAME") {
                mal.push(path.to_string());
            }
        });
        assert!(n > 200, "só {n} páginas — a árvore não foi percorrida");
        assert!(mal.is_empty(), "páginas mal formadas: {mal:?}");
    }
}
