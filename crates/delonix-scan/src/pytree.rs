//! Scanning of **Odoo module** (Python) trees — the zero-trust gate of the Odoo
//! plan (Block D). Three aspects, all in pure functions (testable without IO):
//!
//! 1. **Manifest** — extracts `name`/`version`/`depends` from `__manifest__.py`
//!    (Python literal dict; best-effort extraction via textual analysis, without
//!    interpreting Python — we NEVER EXECUTE the module's code).
//! 2. **PyPI dependencies** — `requirements.txt`/`external_dependencies` →
//!    packages to cross-reference with the advisory DB (ecosystem [`Ecosystem::PyPi`];
//!    the OSV `PyPI` feed is accepted in `advisories_from_osv`).
//! 3. **Risk lints** — dangerous patterns in `.py` (dynamic exec, shell,
//!    unsafe deserialization, raw networking) with severity — the basis of the gate
//!    "Critical/High block" (signed decision no. 2).

use crate::Severity;
use delonix_runtime_core::{Error, Result};
use serde::Serialize;
use std::path::Path;

/// The manifest of an Odoo module (best-effort extraction, without executing Python).
#[derive(Serialize, Clone, Debug, Default)]
pub struct OdooManifest {
    pub name: Option<String>,
    pub version: Option<String>,
    pub depends: Vec<String>,
}

/// A declared Python dependency (`requirements.txt`).
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct PyDep {
    pub name: String,
    /// Pinned version (`==`); empty if not pinned (range/no pin).
    pub version: String,
}

/// A finding from the risk lints.
#[derive(Serialize, Clone, Debug)]
pub struct RiskFinding {
    /// Relative path of the file within the module.
    pub file: String,
    /// Line (1-based).
    pub line: usize,
    /// The pattern that fired (e.g. `os.system(`).
    pub pattern: String,
    pub severity: Severity,
    /// The offending line (trimmed, max 160 chars).
    pub snippet: String,
}

/// The report of ONE Odoo module.
#[derive(Serialize, Clone, Debug)]
pub struct ModuleReport {
    /// Name of the module's directory.
    pub module: String,
    pub manifest: OdooManifest,
    pub deps: Vec<PyDep>,
    pub risks: Vec<RiskFinding>,
    /// Number of files analyzed.
    pub files: usize,
}

impl ModuleReport {
    /// The worst severity among the risks (`None` = no risks).
    pub fn worst(&self) -> Option<Severity> {
        self.risks.iter().map(|r| r.severity).max()
    }
}

/// Extracts a string value from a Python literal dict: `'key': 'value'` or
/// `"key": "value"`. Best-effort textual (Odoo manifest keys are simple).
fn dict_str(text: &str, key: &str) -> Option<String> {
    for quote in ['\'', '"'] {
        let pat = format!("{quote}{key}{quote}");
        let Some(i) = text.find(&pat) else { continue };
        let rest = &text[i + pat.len()..];
        let rest = rest.trim_start().strip_prefix(':')?.trim_start();
        let q = rest.chars().next()?;
        if q != '\'' && q != '"' {
            return None;
        }
        let inner = &rest[1..];
        let end = inner.find(q)?;
        return Some(inner[..end].to_string());
    }
    None
}

/// Extracts a list of strings: `'key': ['a', 'b']`.
fn dict_list(text: &str, key: &str) -> Vec<String> {
    for quote in ['\'', '"'] {
        let pat = format!("{quote}{key}{quote}");
        let Some(i) = text.find(&pat) else { continue };
        let rest = &text[i + pat.len()..];
        let Some(rest) = rest.trim_start().strip_prefix(':') else {
            continue;
        };
        let Some(open) = rest.find('[') else { continue };
        let Some(close) = rest[open..].find(']') else {
            continue;
        };
        let inner = &rest[open + 1..open + close];
        return inner
            .split(',')
            .map(|s| s.trim().trim_matches(|c| c == '\'' || c == '"').to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    Vec::new()
}

/// Parses the text of a `__manifest__.py` (without executing Python).
pub fn parse_odoo_manifest(text: &str) -> OdooManifest {
    OdooManifest {
        name: dict_str(text, "name"),
        version: dict_str(text, "version"),
        depends: dict_list(text, "depends"),
    }
}

/// Parses a `requirements.txt`: `name==version` lines (pins) and `name` (no pin);
/// comments and flags (`-r`, `--indices`) are ignored.
pub fn parse_requirements(text: &str) -> Vec<PyDep> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() || line.starts_with('-') {
            continue;
        }
        // strips extras/markers: `pkg[extra]==1.0; python_version…`
        let line = line.split(';').next().unwrap_or(line).trim();
        let (name, version) = match line.split_once("==") {
            Some((n, v)) => (n, v.trim()),
            None => (
                line.split(&['>', '<', '~', '!'][..]).next().unwrap_or(line),
                "",
            ),
        };
        let name = name.split('[').next().unwrap_or(name).trim();
        if name.is_empty() {
            continue;
        }
        out.push(PyDep {
            // PyPI names are case-insensitive and `-`≡`_` (PEP 503)
            name: name.to_ascii_lowercase().replace('_', "-"),
            version: version.to_string(),
        });
    }
    out
}

/// The risk patterns and their severity. The philosophy: module code should
/// NEVER need dynamic exec/shell/unsafe deserialization — when it shows up,
/// it's either malice or an accident waiting to happen.
const RISK_PATTERNS: &[(&str, Severity)] = &[
    // dynamic execution / shell — the direct path to RCE
    ("eval(", Severity::Critical),
    ("exec(", Severity::Critical),
    ("os.system(", Severity::Critical),
    ("os.popen(", Severity::Critical),
    ("__import__(", Severity::High),
    ("subprocess.", Severity::High),
    ("ctypes.", Severity::High),
    // unsafe deserialization — RCE via payload
    ("pickle.loads(", Severity::Critical),
    ("pickle.load(", Severity::High),
    ("marshal.loads(", Severity::Critical),
    ("yaml.load(", Severity::High), // without SafeLoader
    // raw networking from the module (exfiltration/beacon)
    ("socket.socket(", Severity::High),
    // classic obfuscation
    ("base64.b64decode(", Severity::Medium),
    ("codecs.decode(", Severity::Medium),
];

/// Lints the TEXT of a Python file. `file` is the relative path for the report.
pub fn lint_python(file: &str, text: &str) -> Vec<RiskFinding> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let code = line.split('#').next().unwrap_or(""); // ignore comments
        for (pat, sev) in RISK_PATTERNS {
            if code.contains(pat) {
                // exception: `yaml.load(` with SafeLoader is the correct usage
                if *pat == "yaml.load(" && code.contains("SafeLoader") {
                    continue;
                }
                out.push(RiskFinding {
                    file: file.to_string(),
                    line: i + 1,
                    pattern: (*pat).to_string(),
                    severity: *sev,
                    snippet: line.trim().chars().take(160).collect(),
                });
            }
        }
    }
    out
}

/// `true` if the directory IS an Odoo module (has `__manifest__.py`/`__openerp__.py`).
pub fn is_odoo_module(dir: &Path) -> bool {
    dir.join("__manifest__.py").is_file() || dir.join("__openerp__.py").is_file()
}

/// Scans ONE module (directory with `__manifest__.py`).
pub fn scan_module_dir(dir: &Path) -> Result<ModuleReport> {
    let module = dir
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("module")
        .to_string();
    let manifest_text = std::fs::read_to_string(dir.join("__manifest__.py"))
        .or_else(|_| std::fs::read_to_string(dir.join("__openerp__.py")))
        .map_err(|_| {
            Error::Invalid(format!(
                "'{module}' is not an Odoo module (no __manifest__.py)"
            ))
        })?;
    let manifest = parse_odoo_manifest(&manifest_text);
    let mut deps = Vec::new();
    if let Ok(req) = std::fs::read_to_string(dir.join("requirements.txt")) {
        deps = parse_requirements(&req);
    }
    let mut risks = Vec::new();
    let mut files = 0usize;
    walk_py(dir, dir, &mut |rel, text| {
        files += 1;
        risks.extend(lint_python(rel, text));
    })?;
    Ok(ModuleReport {
        module,
        manifest,
        deps,
        risks,
        files,
    })
}

/// Scans a root: the directory itself if it is a module, otherwise each
/// subdirectory that is one (addons repository layout).
pub fn scan_modules_root(dir: &Path) -> Result<Vec<ModuleReport>> {
    if is_odoo_module(dir) {
        return Ok(vec![scan_module_dir(dir)?]);
    }
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)
        .map_err(|e| Error::Runtime {
            context: "module scan",
            message: e.to_string(),
        })?
        .flatten()
    {
        let p = e.path();
        if p.is_dir() && is_odoo_module(&p) {
            out.push(scan_module_dir(&p)?);
        }
    }
    if out.is_empty() {
        return Err(Error::Invalid(
            "no Odoo module found (directories with __manifest__.py)".into(),
        ));
    }
    out.sort_by(|a, b| a.module.cmp(&b.module));
    Ok(out)
}

/// Walks the `.py` files of a tree, calling `f(relative_path, text)`.
/// Symlinks are NOT followed (a module shouldn't have symlinks pointing outside).
fn walk_py(root: &Path, dir: &Path, f: &mut impl FnMut(&str, &str)) -> Result<()> {
    for e in std::fs::read_dir(dir)
        .map_err(|e| Error::Runtime {
            context: "module scan",
            message: e.to_string(),
        })?
        .flatten()
    {
        let p = e.path();
        if p.is_symlink() {
            continue;
        }
        if p.is_dir() {
            walk_py(root, &p, f)?;
        } else if p.extension().and_then(|x| x.to_str()) == Some("py") {
            if let Ok(text) = std::fs::read_to_string(&p) {
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .to_string();
                f(&rel, &text);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
# -*- coding: utf-8 -*-
{
    'name': "Vendas Angola",
    'version': '17.0.1.0.0',
    'depends': ['base', 'sale', "account"],
    'data': ['views/view.xml'],
}
"#;

    #[test]
    fn parses_manifest_fields() {
        let m = parse_odoo_manifest(MANIFEST);
        assert_eq!(m.name.as_deref(), Some("Vendas Angola"));
        assert_eq!(m.version.as_deref(), Some("17.0.1.0.0"));
        assert_eq!(m.depends, vec!["base", "sale", "account"]);
    }

    #[test]
    fn parses_requirements_with_pins_extras_and_markers() {
        let deps = parse_requirements(
            "requests==2.31.0\nPyYAML>=6\npandas[excel]==2.2.0 ; python_version>'3.9'\n# comentário\n-r outro.txt\nNum2Words\n",
        );
        assert_eq!(
            deps[0],
            PyDep {
                name: "requests".into(),
                version: "2.31.0".into()
            }
        );
        assert_eq!(
            deps[1],
            PyDep {
                name: "pyyaml".into(),
                version: String::new()
            }
        );
        assert_eq!(
            deps[2],
            PyDep {
                name: "pandas".into(),
                version: "2.2.0".into()
            }
        );
        assert_eq!(
            deps[3],
            PyDep {
                name: "num2words".into(),
                version: String::new()
            }
        );
        assert_eq!(deps.len(), 4);
    }

    #[test]
    fn lints_flag_dangerous_python() {
        let code = "import os\nos.system('rm -rf /')\nx = eval(payload)\n# os.system em comentário não conta\nyaml.load(f, Loader=yaml.SafeLoader)\n";
        let risks = lint_python("models/m.py", code);
        assert_eq!(risks.len(), 2);
        assert!(risks.iter().all(|r| r.severity == Severity::Critical));
        assert_eq!(risks[0].line, 2);
        assert_eq!(risks[1].pattern, "eval(");
    }

    #[test]
    fn scans_a_module_tree_end_to_end() {
        let dir = std::env::temp_dir().join(format!("dlx-pytree-{}", std::process::id()));
        let m = dir.join("meu_modulo");
        std::fs::create_dir_all(m.join("models")).unwrap();
        std::fs::write(m.join("__manifest__.py"), MANIFEST).unwrap();
        std::fs::write(m.join("requirements.txt"), "requests==2.31.0\n").unwrap();
        std::fs::write(m.join("models/ok.py"), "def f():\n    return 1\n").unwrap();
        std::fs::write(
            m.join("models/mau.py"),
            "import pickle\npickle.loads(data)\n",
        )
        .unwrap();
        let reports = scan_modules_root(&dir).unwrap();
        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert_eq!(r.module, "meu_modulo");
        assert_eq!(r.deps.len(), 1);
        assert_eq!(r.worst(), Some(Severity::Critical));
        assert_eq!(r.files, 3); // __manifest__.py + models/{ok,mau}.py
                                // a root that IS the module also works
        let direct = scan_modules_root(&m).unwrap();
        assert_eq!(direct.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_non_module_dirs() {
        let dir = std::env::temp_dir().join(format!("dlx-pytree-vazio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(scan_modules_root(&dir).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
