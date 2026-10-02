//! `.dockerignore` — which paths of the build context a `COPY` never sees.
//!
//! The build used to copy every file under a `COPY` source, whatever the
//! context's `.dockerignore` said. Every project template shipped one, so the
//! file read as protection it did not give: a `COPY . .` took `.env`, `.git`,
//! `node_modules` and `vendor/` into a layer, and a secret that lands in a
//! layer stays in the image after the next `RUN rm`.
//!
//! The matching follows Docker's (the `moby/patternmatcher` rules), because the
//! same file is read by both tools and two meanings for one file is how a
//! secret ends up in one image and not in the other:
//!
//! - one pattern per line; blank lines and lines starting with `#` are skipped;
//! - a leading `/` and a trailing `/` are dropped, and the pattern is cleaned
//!   (`./a` → `a`, `a//b` → `a/b`);
//! - `*` matches any run of characters except `/`, `?` one character except
//!   `/`, `[...]` a class (`[!...]`/`[^...]` negated), `\` escapes the next
//!   character, and a whole `**` segment matches any number of directories;
//! - a line starting with `!` re-includes what an earlier line excluded;
//! - the LAST pattern that matches decides;
//! - a path is also excluded when one of its parent directories is (excluding
//!   `node_modules` excludes everything inside it).
//!
//! What is deliberately not here: Docker's per-Dockerfile ignore file
//! (`Dockerfile.dockerignore`) and the build-time exclusion of the Dockerfile
//! and the ignore file themselves — neither is needed for `COPY` to stop
//! leaking what the file names.

use std::path::Path;

/// One cleaned pattern.
#[derive(Debug, Clone, PartialEq)]
struct Rule {
    /// Path segments of the pattern (`**` kept as its own segment).
    segments: Vec<String>,
    /// `!pattern`: re-includes instead of excluding.
    exception: bool,
}

/// The rules of one `.dockerignore`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DockerIgnore {
    rules: Vec<Rule>,
}

impl DockerIgnore {
    /// Parses the text of a `.dockerignore`.
    pub fn parse(text: &str) -> Self {
        let mut rules = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (exception, pat) = match line.strip_prefix('!') {
                Some(rest) => (true, rest.trim()),
                None => (false, line),
            };
            let segments = clean(pat);
            if segments.is_empty() {
                continue;
            }
            rules.push(Rule {
                segments,
                exception,
            });
        }
        Self { rules }
    }

    /// Reads `<context>/.dockerignore`; no file (or an unreadable one) is the
    /// empty rule set, which excludes nothing — the behaviour a context without
    /// the file has always had.
    pub fn load(context: &Path) -> Self {
        std::fs::read_to_string(context.join(".dockerignore"))
            .map(|t| Self::parse(&t))
            .unwrap_or_default()
    }

    /// No rules at all.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// True when some rule re-includes.
    pub fn has_exceptions(&self) -> bool {
        self.rules.iter().any(|r| r.exception)
    }

    /// Could a `!` rule re-include something INSIDE the directory `rel`? A
    /// walker descends into an excluded directory only then. Asking merely
    /// "is there any exception" made one `!.env.example` walk every excluded
    /// tree — a whole `.venv` or `node_modules` — and leave its empty
    /// directory skeleton in the image.
    pub fn may_reinclude_under(&self, rel: &str) -> bool {
        let path = clean(rel);
        self.rules
            .iter()
            .filter(|r| r.exception)
            .any(|r| prefix_matches(&r.segments, &path))
    }

    /// Is `rel` (a path relative to the build context, `/`-separated) excluded?
    /// `""` and `"."` are the context itself and are never excluded.
    pub fn is_excluded(&self, rel: &str) -> bool {
        let path = clean(rel);
        if path.is_empty() {
            return false;
        }
        let mut excluded = false;
        for rule in &self.rules {
            // The path matches, or one of its parent directories does.
            let hit = (1..=path.len()).any(|n| match_segments(&rule.segments, &path[..n]));
            if hit {
                excluded = !rule.exception;
            }
        }
        excluded
    }
}

/// Splits a pattern or a path into its cleaned segments.
fn clean(p: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s.to_string()),
        }
    }
    out
}

/// Can the pattern match some path strictly below `dir`? True when its
/// leading segments match every segment of `dir` and something remains (or a
/// `**` makes the depth open-ended).
fn prefix_matches(pat: &[String], dir: &[String]) -> bool {
    match (pat.first(), dir.first()) {
        (Some(p), _) if p == "**" => true,
        (None, _) => false,
        (Some(_), None) => true,
        (Some(p), Some(d)) => {
            match_segment(p.as_bytes(), d.as_bytes()) && prefix_matches(&pat[1..], &dir[1..])
        }
    }
}

/// Whole-path match of pattern segments against path segments.
fn match_segments(pat: &[String], path: &[String]) -> bool {
    match pat.first() {
        None => path.is_empty(),
        Some(p) if p == "**" => {
            // `**` swallows zero or more whole segments.
            (0..=path.len()).any(|skip| match_segments(&pat[1..], &path[skip..]))
        }
        Some(p) => match path.first() {
            Some(s) if match_segment(p.as_bytes(), s.as_bytes()) => {
                match_segments(&pat[1..], &path[1..])
            }
            _ => false,
        },
    }
}

/// Wildcard match of one segment (`*`, `?`, `[...]`, `\`).
fn match_segment(pat: &[u8], s: &[u8]) -> bool {
    match pat.first() {
        None => s.is_empty(),
        Some(b'*') => (0..=s.len()).any(|k| match_segment(&pat[1..], &s[k..])),
        Some(b'?') => !s.is_empty() && match_segment(&pat[1..], &s[1..]),
        Some(b'[') => {
            let Some(c) = s.first() else {
                return false;
            };
            match match_class(&pat[1..], *c) {
                Some((ok, rest)) => ok && match_segment(rest, &s[1..]),
                // An unterminated class is a literal `[`.
                None => *c == b'[' && match_segment(&pat[1..], &s[1..]),
            }
        }
        Some(b'\\') if pat.len() > 1 => {
            s.first() == Some(&pat[1]) && match_segment(&pat[2..], &s[1..])
        }
        Some(p) => s.first() == Some(p) && match_segment(&pat[1..], &s[1..]),
    }
}

/// `[...]` after the `[`: whether `c` is in the class, and the pattern after
/// the closing `]`. `None` when the class never closes.
fn match_class(pat: &[u8], c: u8) -> Option<(bool, &[u8])> {
    let (negate, mut i) = match pat.first() {
        Some(b'!') | Some(b'^') => (true, 1),
        _ => (false, 0),
    };
    let mut found = false;
    let mut first = true;
    while i < pat.len() {
        if pat[i] == b']' && !first {
            return Some((found != negate, &pat[i + 1..]));
        }
        first = false;
        let lo = pat[i];
        if i + 2 < pat.len() && pat[i + 1] == b'-' && pat[i + 2] != b']' {
            if lo <= c && c <= pat[i + 2] {
                found = true;
            }
            i += 3;
        } else {
            if lo == c {
                found = true;
            }
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::DockerIgnore;

    fn ig(text: &str) -> DockerIgnore {
        DockerIgnore::parse(text)
    }

    #[test]
    fn a_plain_name_excludes_the_entry_and_everything_under_it() {
        let d = ig("node_modules\n.env\n");
        assert!(d.is_excluded("node_modules"));
        assert!(d.is_excluded("node_modules/fastify/package.json"));
        assert!(d.is_excluded(".env"));
        assert!(!d.is_excluded(".env.example"));
        assert!(!d.is_excluded("src/app.ts"));
        // Docker's patterns are anchored at the context root: `.env` does not
        // reach a nested `.env`.
        assert!(!d.is_excluded("config/.env"));
    }

    #[test]
    fn a_double_star_reaches_any_depth() {
        let d = ig("**/__pycache__\n**/*.pyc\n");
        assert!(d.is_excluded("__pycache__"));
        assert!(d.is_excluded("src/app/__pycache__/x.cpython-312.pyc"));
        assert!(d.is_excluded("a/b/c.pyc"));
        assert!(!d.is_excluded("a/b/c.py"));
    }

    #[test]
    fn the_last_matching_rule_wins_and_an_exception_reincludes() {
        let d = ig("*.md\n!README.md\n");
        assert!(d.is_excluded("CHANGELOG.md"));
        assert!(!d.is_excluded("README.md"));
        let d = ig("!README.md\n*.md\n");
        assert!(d.is_excluded("README.md"), "a later exclusion wins again");
        assert!(ig("*.md\n!README.md\n").has_exceptions());
        assert!(!ig("*.md\n").has_exceptions());
    }

    /// One `!` rule must not make every excluded directory worth walking.
    #[test]
    fn an_exception_opens_only_the_directories_it_can_reach() {
        let d = ig(".venv\nnode_modules\n.env.*\n!.env.example\ndocs\n!docs/api/openapi.yaml\n");
        assert!(!d.may_reinclude_under(".venv"));
        assert!(!d.may_reinclude_under("node_modules/pkg"));
        assert!(d.may_reinclude_under("docs"));
        assert!(d.may_reinclude_under("docs/api"));
        assert!(!d.may_reinclude_under("docs/adr"));
        assert!(ig("build\n!**/keep.txt\n").may_reinclude_under("build/deep"));
    }

    #[test]
    fn slashes_dots_comments_and_blanks_are_cleaned() {
        let d = ig("# a comment\n\n/dist/\n./coverage\nvendor//x\n");
        assert!(d.is_excluded("dist/main.js"));
        assert!(d.is_excluded("coverage"));
        assert!(d.is_excluded("vendor/x/y"));
        assert!(!d.is_excluded("# a comment"));
        assert!(!d.is_excluded("."));
        assert!(!d.is_excluded(""));
    }

    #[test]
    fn wildcards_classes_and_escapes() {
        let d = ig("*.lo?\nfile[0-9].txt\n[!a]x\n\\*literal\n");
        assert!(d.is_excluded("x.log"));
        assert!(!d.is_excluded("x.logs"));
        assert!(d.is_excluded("file7.txt"));
        assert!(!d.is_excluded("fileA.txt"));
        assert!(d.is_excluded("bx"));
        assert!(!d.is_excluded("ax"));
        assert!(d.is_excluded("*literal"));
        assert!(!d.is_excluded("xliteral"));
        // `*` never crosses a `/`.
        assert!(!ig("*.txt\n").is_excluded("dir/a.txt"));
    }

    #[test]
    fn an_empty_file_excludes_nothing() {
        let d = ig("");
        assert!(d.is_empty());
        assert!(!d.is_excluded(".env"));
    }
}
