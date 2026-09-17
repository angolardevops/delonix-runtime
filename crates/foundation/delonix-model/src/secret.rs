//! The secret MODEL: what a secret is and what a valid name and key look like.
//! Pure — the encrypted store that holds secrets is `delonix-state`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A named secret: a set of `KEY=value` pairs (env).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Secret {
    /// Secret name (reference used by `run --secret <name>`).
    pub name: String,
    /// key→value pairs. The keys are environment variable names.
    #[serde(default)]
    pub data: BTreeMap<String, String>,
    /// Creation/update instant (Unix seconds).
    #[serde(default)]
    pub updated_unix: u64,
}

impl Secret {
    /// The pairs in `KEY=value` format for injection as env.
    pub fn env_pairs(&self) -> Vec<String> {
        self.data.iter().map(|(k, v)| format!("{k}={v}")).collect()
    }
}

/// Is an environment variable name valid? (`[A-Za-z_][A-Za-z0-9_]*`).
pub fn valid_env_key(k: &str) -> bool {
    let mut it = k.chars();
    matches!(it.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && it.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Valid secret name? (`[a-z0-9._-]`, non-empty, ≤ 64).
pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Parses a `.env` file (`KEY=value` lines; ignores empty ones and `#`).
/// Accepts single/double quotes around the value. Returns the valid pairs.
pub fn parse_env_file(content: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            if !valid_env_key(k) {
                continue;
            }
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(v);
            out.insert(k.to_string(), v.to_string());
        }
    }
    out
}
