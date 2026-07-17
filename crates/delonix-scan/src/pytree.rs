//! Scan de árvores de **módulos Odoo** (Python) — o gate zero-trust do plano
//! Odoo (Bloco D). Três vertentes, todas em funções puras (testáveis sem IO):
//!
//! 1. **Manifesto** — extrai `name`/`version`/`depends` do `__manifest__.py`
//!    (dict literal Python; extração best-effort por análise textual, sem
//!    interpretar Python — nunca EXECUTAMOS código do módulo).
//! 2. **Dependências PyPI** — `requirements.txt`/`external_dependencies` →
//!    pacotes para cruzar com a BD de advisories (ecossistema [`Ecosystem::PyPi`];
//!    o feed OSV `PyPI` é aceite no `advisories_from_osv`).
//! 3. **Lints de risco** — padrões perigosos em `.py` (exec dinâmico, shell,
//!    deserialização insegura, rede crua) com severidade — a base do gate
//!    "Critical/High bloqueiam" (decisão assinada nº 2).

use crate::Severity;
use delonix_runtime_core::{Error, Result};
use serde::Serialize;
use std::path::Path;

/// O manifesto de um módulo Odoo (extração best-effort, sem executar Python).
#[derive(Serialize, Clone, Debug, Default)]
pub struct OdooManifest {
    pub name: Option<String>,
    pub version: Option<String>,
    pub depends: Vec<String>,
}

/// Uma dependência Python declarada (`requirements.txt`).
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct PyDep {
    pub name: String,
    /// Versão fixada (`==`); vazio se não fixada (range/sem pin).
    pub version: String,
}

/// Um achado dos lints de risco.
#[derive(Serialize, Clone, Debug)]
pub struct RiskFinding {
    /// Caminho relativo do ficheiro dentro do módulo.
    pub file: String,
    /// Linha (1-based).
    pub line: usize,
    /// O padrão que disparou (ex.: `os.system(`).
    pub pattern: String,
    pub severity: Severity,
    /// A linha ofensora (aparada, máx. 160 chars).
    pub snippet: String,
}

/// O relatório de UM módulo Odoo.
#[derive(Serialize, Clone, Debug)]
pub struct ModuleReport {
    /// Nome do diretório do módulo.
    pub module: String,
    pub manifest: OdooManifest,
    pub deps: Vec<PyDep>,
    pub risks: Vec<RiskFinding>,
    /// Nº de ficheiros analisados.
    pub files: usize,
}

impl ModuleReport {
    /// A pior severidade dos riscos (`None` = sem riscos).
    pub fn worst(&self) -> Option<Severity> {
        self.risks.iter().map(|r| r.severity).max()
    }
}

/// Extrai um valor string de um dict literal Python: `'chave': 'valor'` ou
/// `"chave": "valor"`. Best-effort textual (chaves do manifesto Odoo são simples).
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

/// Extrai uma lista de strings: `'chave': ['a', 'b']`.
fn dict_list(text: &str, key: &str) -> Vec<String> {
    for quote in ['\'', '"'] {
        let pat = format!("{quote}{key}{quote}");
        let Some(i) = text.find(&pat) else { continue };
        let rest = &text[i + pat.len()..];
        let Some(rest) = rest.trim_start().strip_prefix(':') else { continue };
        let Some(open) = rest.find('[') else { continue };
        let Some(close) = rest[open..].find(']') else { continue };
        let inner = &rest[open + 1..open + close];
        return inner
            .split(',')
            .map(|s| s.trim().trim_matches(|c| c == '\'' || c == '"').to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    Vec::new()
}

/// Analisa o texto de um `__manifest__.py` (sem executar Python).
pub fn parse_odoo_manifest(text: &str) -> OdooManifest {
    OdooManifest {
        name: dict_str(text, "name"),
        version: dict_str(text, "version"),
        depends: dict_list(text, "depends"),
    }
}

/// Analisa um `requirements.txt`: linhas `nome==versão` (pins) e `nome` (sem pin);
/// comentários e flags (`-r`, `--índices`) são ignorados.
pub fn parse_requirements(text: &str) -> Vec<PyDep> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() || line.starts_with('-') {
            continue;
        }
        // corta extras/markers: `pkg[extra]==1.0; python_version…`
        let line = line.split(';').next().unwrap_or(line).trim();
        let (name, version) = match line.split_once("==") {
            Some((n, v)) => (n, v.trim()),
            None => (line.split(&['>', '<', '~', '!'][..]).next().unwrap_or(line), ""),
        };
        let name = name.split('[').next().unwrap_or(name).trim();
        if name.is_empty() {
            continue;
        }
        out.push(PyDep {
            // nomes PyPI são case-insensitive e `-`≡`_` (PEP 503)
            name: name.to_ascii_lowercase().replace('_', "-"),
            version: version.to_string(),
        });
    }
    out
}

/// Os padrões de risco e a sua severidade. A filosofia: código de módulo NUNCA
/// devia precisar de exec dinâmico/shell/deserialização insegura — quando
/// aparece, ou é malícia ou é um acidente à espera de acontecer.
const RISK_PATTERNS: &[(&str, Severity)] = &[
    // execução dinâmica / shell — a via direta de RCE
    ("eval(", Severity::Critical),
    ("exec(", Severity::Critical),
    ("os.system(", Severity::Critical),
    ("os.popen(", Severity::Critical),
    ("__import__(", Severity::High),
    ("subprocess.", Severity::High),
    ("ctypes.", Severity::High),
    // deserialização insegura — RCE por payload
    ("pickle.loads(", Severity::Critical),
    ("pickle.load(", Severity::High),
    ("marshal.loads(", Severity::Critical),
    ("yaml.load(", Severity::High), // sem SafeLoader
    // rede crua a partir do módulo (exfiltração/beacon)
    ("socket.socket(", Severity::High),
    // ofuscação clássica
    ("base64.b64decode(", Severity::Medium),
    ("codecs.decode(", Severity::Medium),
];

/// Linta o TEXTO de um ficheiro Python. `file` é o caminho relativo p/ o relatório.
pub fn lint_python(file: &str, text: &str) -> Vec<RiskFinding> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let code = line.split('#').next().unwrap_or(""); // ignora comentários
        for (pat, sev) in RISK_PATTERNS {
            if code.contains(pat) {
                // exceção: `yaml.load(` com SafeLoader é o uso correto
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

/// `true` se o diretório É um módulo Odoo (tem `__manifest__.py`/`__openerp__.py`).
pub fn is_odoo_module(dir: &Path) -> bool {
    dir.join("__manifest__.py").is_file() || dir.join("__openerp__.py").is_file()
}

/// Faz o scan de UM módulo (diretório com `__manifest__.py`).
pub fn scan_module_dir(dir: &Path) -> Result<ModuleReport> {
    let module = dir
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("modulo")
        .to_string();
    let manifest_text = std::fs::read_to_string(dir.join("__manifest__.py"))
        .or_else(|_| std::fs::read_to_string(dir.join("__openerp__.py")))
        .map_err(|_| {
            Error::Invalid(format!(
                "'{module}' não é um módulo Odoo (sem __manifest__.py)"
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
    Ok(ModuleReport { module, manifest, deps, risks, files })
}

/// Faz o scan de uma raiz: o próprio diretório se for um módulo, senão cada
/// subdiretório que o seja (layout de repositório de addons).
pub fn scan_modules_root(dir: &Path) -> Result<Vec<ModuleReport>> {
    if is_odoo_module(dir) {
        return Ok(vec![scan_module_dir(dir)?]);
    }
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)
        .map_err(|e| Error::Runtime { context: "scan módulos", message: e.to_string() })?
        .flatten()
    {
        let p = e.path();
        if p.is_dir() && is_odoo_module(&p) {
            out.push(scan_module_dir(&p)?);
        }
    }
    if out.is_empty() {
        return Err(Error::Invalid(
            "nenhum módulo Odoo encontrado (diretórios com __manifest__.py)".into(),
        ));
    }
    out.sort_by(|a, b| a.module.cmp(&b.module));
    Ok(out)
}

/// Percorre os `.py` de uma árvore chamando `f(caminho_relativo, texto)`.
/// Symlinks NÃO são seguidos (um módulo não devia ter symlinks para fora).
fn walk_py(root: &Path, dir: &Path, f: &mut impl FnMut(&str, &str)) -> Result<()> {
    for e in std::fs::read_dir(dir)
        .map_err(|e| Error::Runtime { context: "scan módulos", message: e.to_string() })?
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
                let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().to_string();
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
        assert_eq!(deps[0], PyDep { name: "requests".into(), version: "2.31.0".into() });
        assert_eq!(deps[1], PyDep { name: "pyyaml".into(), version: String::new() });
        assert_eq!(deps[2], PyDep { name: "pandas".into(), version: "2.2.0".into() });
        assert_eq!(deps[3], PyDep { name: "num2words".into(), version: String::new() });
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
        std::fs::write(m.join("models/mau.py"), "import pickle\npickle.loads(data)\n").unwrap();
        let reports = scan_modules_root(&dir).unwrap();
        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert_eq!(r.module, "meu_modulo");
        assert_eq!(r.deps.len(), 1);
        assert_eq!(r.worst(), Some(Severity::Critical));
        assert_eq!(r.files, 3); // __manifest__.py + models/{ok,mau}.py
        // raiz que É o módulo também funciona
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
