//! `--help` as a MANUAL: command map, worked examples and cross-references,
//! attached to the `clap::Command` tree before the parse.
//!
//! # The problem this exists to fix
//!
//! Measured before it existed: **1 of the 234 commands carried an example**
//! (the top-level `SHORTCUTS` block), `-h` and `--help` printed byte-identical
//! output, and a group like `container` answered with 27 subcommands in a flat
//! list ordered by nothing in particular — with descriptions that ran to five
//! lines of unbroken prose. Everything needed to USE the command was there;
//! nothing was arranged so it could be read.
//!
//! # The split: `-h` is a reminder, `--help` is the manual
//!
//! clap already distinguishes them (`about` vs `long_about`, `after_help` vs
//! `after_long_help`) and this repo was defining only the short side, so the
//! long side fell back to it. Here the two are given different jobs:
//!
//! * `-h` — one line of `about`, the usage line, the flags. What you want when
//!   you already know the command and forgot a flag's name.
//! * `--help` — the above plus a COMMAND MAP (for groups), EXAMPLES and
//!   SEE ALSO. What you want the first time.
//!
//! # Why a central table and not `after_long_help` on each derive
//!
//! Same reason `po::translate_help` rewrites the tree instead of the source
//! carrying both languages: the material is UNIFORM (every command wants a map,
//! examples, cross-references), and 234 hand-written `after_long_help`
//! attributes are 234 places for the format to drift. One table is auditable in
//! one read, and it is what lets `todo_o_comando_tem_exemplo` be a real test —
//! a coverage test cannot walk attributes that were never written.
//!
//! The map and the breadcrumb are DERIVED from the `Command` itself, never
//! typed: a subcommand added tomorrow shows up in its parent's map without
//! anyone remembering to add it. Only the editorial material — which category a
//! command belongs to, its examples, what to read next — lives in the table.
//!
//! # i18n
//!
//! The example COMMANDS are the same in every language; their comments are not.
//! Comments go through `po::t` like every other user-facing string, so
//! `--l18n=pt` does not leave an English block in the middle of a Portuguese
//! screen. `see_also` and command names are identifiers — never translated.

use super::po;

/// The editorial material for one command. Everything structural (the tree, the
/// breadcrumb, the usage line) is read off the `clap::Command` instead.
pub struct Entry {
    /// Space-separated path WITHOUT the binary name: `"container run"`, or
    /// `""` for the root command.
    pub path: &'static str,
    /// Which section of the parent's COMMAND MAP this command appears under.
    /// Empty means the parent has no categories (a small group, where a flat
    /// list reads fine and headings would be ceremony).
    pub group: &'static str,
    /// `(comment, command line)`. The comment is translated; the command is
    /// not. Ordered from the most common use to the most specific — someone
    /// reading the first two lines and stopping should still have the answer.
    pub examples: &'static [(&'static str, &'static str)],
    /// Full paths of related commands, for the SEE ALSO line.
    pub see_also: &'static [&'static str],
}

/// Order in which a group's categories are printed. A category absent here
/// sorts last, alphabetically — a new category shows up rather than vanishing.
const GROUP_ORDER: &[&str] = &[
    "Lifecycle",
    "Create",
    "Inspect",
    "Interact",
    "Configure",
    "Networking",
    "Storage",
    "Declarative",
    "Maintenance",
    "Dashboards",
    "Advanced",
];

include!("manual_entries.rs");

/// The entry for a path, if the table has one.
///
/// `pub` for one reason: `cmd::man` renders the SAME examples and
/// cross-references into roff. A manpage that carried its own copy of the
/// editorial material would drift from `--help` the first time either was
/// edited — and nobody re-reads a manpage to check.
pub fn entry(path: &str) -> Option<&'static Entry> {
    ENTRIES.iter().find(|e| e.path == path)
}

/// Category of a command, for its parent's map.
fn group_of(path: &str) -> &'static str {
    entry(path).map(|e| e.group).unwrap_or("")
}

/// Sort key for a category: its index in `GROUP_ORDER`, or the end.
fn group_rank(g: &str) -> usize {
    GROUP_ORDER
        .iter()
        .position(|x| *x == g)
        .unwrap_or(GROUP_ORDER.len())
}

/// Wraps at `width` columns, preserving paragraph breaks and never splitting a
/// word. Deliberately counts CHARS and not display width: help text is prose,
/// where the two only diverge on CJK — and this repo already has
/// `output::display_width` for the tables where the difference bites.
fn wrap(text: &str, width: usize, indent: &str) -> String {
    let mut out = String::new();
    for (i, para) in text.split("\n\n").enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let mut line = String::new();
        for word in para.split_whitespace() {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
                out.push_str(indent);
                out.push_str(&line);
                out.push('\n');
                line.clear();
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.is_empty() {
            out.push_str(indent);
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// The COMMAND MAP of a group: its children arranged by category, derived from
/// the live `Command` so it cannot omit a subcommand that exists.
///
/// Returns `None` for a leaf — a command with no children has no map, and an
/// empty heading is worse than no heading.
fn command_map(cmd: &clap::Command, path: &str) -> Option<String> {
    let kids: Vec<&clap::Command> = cmd
        .get_subcommands()
        .filter(|s| s.get_name() != "help" && !s.is_hide_set())
        .collect();
    if kids.is_empty() {
        return None;
    }
    let child_path = |name: &str| {
        if path.is_empty() {
            name.to_string()
        } else {
            format!("{path} {name}")
        }
    };
    // Group name -> the child names under it, in declaration order (which is
    // the order the author chose, and reads as a workflow: run before stop).
    let mut cats: Vec<(&'static str, Vec<String>)> = Vec::new();
    for k in &kids {
        let g = group_of(&child_path(k.get_name()));
        let name = k.get_name().to_string();
        match cats.iter_mut().find(|(c, _)| *c == g) {
            Some((_, v)) => v.push(name),
            None => cats.push((g, vec![name])),
        }
    }
    // A group where nothing was categorised gets no map: the flat `Commands:`
    // list clap already prints says the same thing without a second copy.
    if cats.iter().all(|(g, _)| g.is_empty()) {
        return None;
    }
    cats.sort_by_key(|(g, _)| group_rank(g));
    let label_w = cats
        .iter()
        .map(|(g, _)| po::t(g).chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (g, names) in cats {
        // The uncategorised leftovers of a categorised group are still real
        // commands — printing them under a heading beats dropping them.
        let label = if g.is_empty() {
            po::t("Other")
        } else {
            po::t(g)
        };
        out.push_str(&format!(
            "  {label:<label_w$}  {}\n",
            names.join(" · "),
            label_w = label_w
        ));
    }
    Some(out)
}

/// Assembles the `--help` tail of one command: map, examples, cross-references
/// and the breadcrumb saying where in the tree this command lives.
fn tail(cmd: &clap::Command, path: &str) -> Option<String> {
    let e = entry(path);
    let map = command_map(cmd, path);
    // Nothing editorial and no map to derive: leave clap's own `after_help`
    // (the root's SHORTCUTS block) alone rather than overwriting it with a
    // breadcrumb nobody asked for.
    e?;
    let e = e.unwrap();
    let mut out = String::new();
    if let Some(map) = map {
        out.push_str(po::t("COMMAND MAP"));
        out.push_str(":\n");
        out.push_str(&map);
        out.push('\n');
    }
    if !e.examples.is_empty() {
        out.push_str(po::t("EXAMPLES"));
        out.push_str(":\n");
        for (i, (comment, line)) in e.examples.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            // The comment is prose and can be long in either language; the
            // command line is code and is NEVER wrapped — a wrapped command is
            // a command that does not run when pasted.
            out.push_str(&wrap(po::t(comment), 74, "  # "));
            out.push_str(&format!("  {line}\n"));
        }
        out.push('\n');
    }
    if !e.see_also.is_empty() {
        out.push_str(po::t("SEE ALSO"));
        out.push_str(":\n");
        let refs: Vec<String> = e.see_also.iter().map(|r| format!("delonix {r}")).collect();
        out.push_str(&wrap(&refs.join(" · "), 76, "  "));
        out.push('\n');
    }
    // The breadcrumb closes every page: it is the answer to "what is this a
    // part of", which a deep tree makes easy to lose.
    if !path.is_empty() {
        let crumb: Vec<&str> = std::iter::once("delonix").chain(path.split(' ')).collect();
        out.push_str(&format!("  {}\n", crumb.join(" › ")));
    }
    Some(out.trim_end().to_string())
}

/// Attaches the manual to the whole tree, recursively. Called once, before the
/// parse, next to `po::translate_help` — and AFTER it, so a map built here
/// carries the already-translated category labels.
pub fn apply(cmd: clap::Command) -> clap::Command {
    apply_at(cmd, String::new())
}

fn apply_at(mut cmd: clap::Command, path: String) -> clap::Command {
    if let Some(t) = tail(&cmd, &path) {
        // `after_long_help` and NOT `after_help`: this is the manual, and the
        // whole point of the split is that `-h` stays short.
        cmd = cmd.after_long_help(t);
    }
    // Categories also decide the ORDER of clap's own `Commands:` list, so the
    // short `-h` gets the same arrangement without a second rendering of it.
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_string())
        .collect();
    for name in names {
        let child = if path.is_empty() {
            name.clone()
        } else {
            format!("{path} {name}")
        };
        let rank = group_rank(group_of(&child));
        cmd = cmd.mut_subcommand(&name, |s| {
            // `help` is clap's own and has no place in a category.
            let s = if s.get_name() == "help" {
                s
            } else {
                s.display_order(rank)
            };
            apply_at(s, child)
        });
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quebra_o_texto_sem_cortar_palavras() {
        let w = wrap("um dois tres quatro cinco", 12, "  ");
        for line in w.lines() {
            assert!(line.chars().count() <= 14, "linha larga demais: {line:?}");
        }
        // Nenhuma palavra pode ter sido partida a meio.
        let junto: String = w.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(junto, "um dois tres quatro cinco");
    }

    #[test]
    fn uma_palavra_maior_que_a_largura_nao_entra_em_ciclo() {
        // A guarda que falta aqui é a que transforma um wrap em loop infinito:
        // uma palavra sozinha maior que a largura tem de sair NUMA linha, não
        // ser partida nem repetida.
        let w = wrap("aaaaaaaaaaaaaaaaaaaaaaaa fim", 5, "");
        assert_eq!(w.lines().count(), 2);
        assert_eq!(w.lines().next().unwrap(), "aaaaaaaaaaaaaaaaaaaaaaaa");
    }

    /// Um caminho na tabela que não existe na CLI é uma entrada que NUNCA será
    /// mostrada — e não há sintoma: o `--help` desse comando fica sem exemplos
    /// e a tabela continua a parecer completa. Só um teste que confronta as
    /// duas árvores o apanha.
    #[test]
    fn nenhuma_entrada_aponta_para_um_comando_inexistente() {
        use clap::CommandFactory;
        let root = crate::Cli::command();
        let mut faltam = Vec::new();
        for e in ENTRIES {
            if e.path.is_empty() {
                continue;
            }
            let mut cur = &root;
            let mut ok = true;
            for seg in e.path.split(' ') {
                match cur.get_subcommands().find(|s| s.get_name() == seg) {
                    Some(s) => cur = s,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                faltam.push(e.path);
            }
        }
        assert!(
            faltam.is_empty(),
            "entradas do manual para comandos que não existem: {faltam:?}"
        );
    }

    /// O inverso: um comando sem entrada é um `--help` sem exemplo. ESTRITO —
    /// um `<=` deixaria a dívida instalada a ler-se como verde, que é
    /// exactamente o defeito que o ratchet do i18n existe para não repetir.
    #[test]
    fn todo_o_comando_tem_entrada_no_manual() {
        use clap::CommandFactory;
        fn walk(cmd: &clap::Command, path: String, out: &mut Vec<String>) {
            // Present-but-empty is the failure this test exists to catch: the
            // skeleton was generated for all 234 commands at once, so "has an
            // entry" is true from the first minute and says nothing. What has
            // to be true is that someone WROTE an example.
            if !path.is_empty() && entry(&path).is_none_or(|e| e.examples.is_empty()) {
                out.push(path.clone());
            }
            for s in cmd.get_subcommands() {
                if s.get_name() == "help" || s.is_hide_set() {
                    continue;
                }
                let child = if path.is_empty() {
                    s.get_name().to_string()
                } else {
                    format!("{path} {}", s.get_name())
                };
                walk(s, child, out);
            }
        }
        let mut sem = Vec::new();
        walk(&crate::Cli::command(), String::new(), &mut sem);
        assert!(
            sem.is_empty(),
            "{} comando(s) sem entrada em manual_entries.rs:\n  {}",
            sem.len(),
            sem.join("\n  ")
        );
    }

    /// O `about` é o que aparece ao LADO do nome na lista do comando pai; um
    /// parágrafo inteiro ali empurra tudo o resto para fora do ecrã e a lista
    /// deixa de se poder percorrer com os olhos.
    ///
    /// RATCHET e não tolerância: medido em 74 de 236 comandos antes desta
    /// sessão (o pior com 447 caracteres). O teste falha se o número SUBIR (um
    /// comando novo com doc-comment sem quebra) e também se DESCER — encurta-se
    /// e baixa-se a constante no mesmo commit. Um `<=` deixava a dívida a
    /// ler-se como verde, que é o defeito que o ratchet do i18n já existe para
    /// não repetir.
    ///
    /// A correcção é sempre a mesma e não apaga nada: uma linha `///` vazia
    /// depois da primeira frase manda o resto para o `long_about`, que é o que
    /// o `--help` mostra.
    #[test]
    fn a_descricao_curta_cabe_numa_linha() {
        use clap::CommandFactory;
        const LIMITE: usize = 110;
        const LONGOS_PENDENTES: usize = 0;
        fn walk(cmd: &clap::Command, path: String, out: &mut Vec<(usize, String)>) {
            if let Some(a) = cmd.get_about() {
                let n = a.to_string().chars().count();
                if !path.is_empty() && n > LIMITE {
                    out.push((n, path.clone()));
                }
            }
            for s in cmd.get_subcommands() {
                if s.get_name() == "help" || s.is_hide_set() {
                    continue;
                }
                let child = if path.is_empty() {
                    s.get_name().to_string()
                } else {
                    format!("{path} {}", s.get_name())
                };
                walk(s, child, out);
            }
        }
        let mut longos = Vec::new();
        walk(&crate::Cli::command(), String::new(), &mut longos);
        longos.sort_by_key(|(n, _)| std::cmp::Reverse(*n));
        let lista: Vec<String> = longos
            .iter()
            .map(|(n, p)| format!("{p} ({n} caracteres)"))
            .collect();
        assert_eq!(
            longos.len(),
            LONGOS_PENDENTES,
            "descrições curtas acima de {LIMITE} caracteres: {} (esperado \
             {LONGOS_PENDENTES}). Parte o doc-comment com uma linha `///` \
             vazia depois da primeira frase — nada se apaga, o resto passa a \
             long_about.\n  {}",
            longos.len(),
            lista.join("\n  ")
        );
    }

    /// `image build` só se invoca como `image --vm build`.
    ///
    /// Os cinco aliases de topo do grupo `image` (build/init/convert/import/
    /// ls-remote) exigem o `--vm`, que **não** é global no clap: `delonix image
    /// build` sozinho não corre. A alternativa era escrever o exemplo sem a
    /// flag para satisfazer a regra e publicar uma linha que falha ao ser
    /// colada — que é o defeito que esta bateria existe para apanhar.
    fn invoca_por_alias(path: &str, line: &str) -> bool {
        let segs: Vec<&str> = path.split(' ').collect();
        segs.len() == 2 && segs[0] == "image" && line.contains(&format!("image --vm {}", segs[1]))
    }

    /// Um exemplo tem de começar pelo comando que documenta — senão documenta
    /// outro. Apanha o erro de copiar-colar entre entradas vizinhas, que é
    /// invisível a olho numa tabela de 234 linhas.
    #[test]
    fn os_exemplos_invocam_o_comando_que_documentam() {
        let mut mal = Vec::new();
        for e in ENTRIES {
            if e.path.is_empty() {
                continue;
            }
            for (_, line) in e.examples {
                // Um exemplo pode legitimamente ser um pipeline ou trazer um
                // prefixo de ambiente (`DELONIX_ROOT=… delonix …`); o que se
                // exige é que o caminho do comando APAREÇA nele.
                if !line.contains(e.path) && !invoca_por_alias(e.path, line) {
                    mal.push(format!("{}: {line}", e.path));
                }
            }
        }
        assert!(
            mal.is_empty(),
            "exemplo(s) que não invocam o próprio comando:\n  {}",
            mal.join("\n  ")
        );
    }
}
