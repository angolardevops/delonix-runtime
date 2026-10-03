//! The contract's `label_selector`: the Kubernetes equality grammar
//! (`app=web,tier!=db`), plus the two existence forms (`app`, `!app`).
//!
//! The set-based forms (`env in (a, b)`) are not part of the contract; they
//! are refused by name instead of being read as something else.

use std::collections::BTreeMap;

/// One requirement of a selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requirement {
    Equals(String, String),
    NotEquals(String, String),
    Exists(String),
    NotExists(String),
}

/// A parsed selector: every requirement has to hold. Empty selects everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selector(Vec<Requirement>);

impl Selector {
    /// Parses `text`; the error says which requirement is wrong.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut out = Vec::new();
        for raw in text.split(',').map(str::trim).filter(|r| !r.is_empty()) {
            if raw.contains('(') || raw.contains(')') || raw.contains(' ') {
                return Err(format!(
                    "label selector '{raw}': only the equality grammar is supported (key=value, key!=value, key, !key)"
                ));
            }
            let req = if let Some((k, v)) = raw.split_once("!=") {
                Requirement::NotEquals(key(k, raw)?, v.to_string())
            } else if let Some((k, v)) = raw.split_once("==").or_else(|| raw.split_once('=')) {
                Requirement::Equals(key(k, raw)?, v.to_string())
            } else if let Some(k) = raw.strip_prefix('!') {
                Requirement::NotExists(key(k, raw)?)
            } else {
                Requirement::Exists(key(raw, raw)?)
            };
            out.push(req);
        }
        Ok(Self(out))
    }

    /// Whether `labels` satisfy every requirement. As in Kubernetes, `!=`
    /// also holds for a label that is absent.
    pub fn matches(&self, labels: &BTreeMap<String, String>) -> bool {
        self.0.iter().all(|r| match r {
            Requirement::Equals(k, v) => labels.get(k) == Some(v),
            Requirement::NotEquals(k, v) => labels.get(k) != Some(v),
            Requirement::Exists(k) => labels.contains_key(k),
            Requirement::NotExists(k) => !labels.contains_key(k),
        })
    }
}

fn key(k: &str, raw: &str) -> Result<String, String> {
    if k.is_empty() || k.contains('=') || k.contains('!') {
        return Err(format!(
            "label selector '{raw}': the label key is missing or malformed"
        ));
    }
    Ok(k.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn the_equality_grammar_selects_what_it_says() {
        let web = labels(&[("app", "web"), ("tier", "front")]);
        let db = labels(&[("app", "db")]);
        let s = Selector::parse("app=web,tier!=db").unwrap();
        assert!(s.matches(&web));
        assert!(!s.matches(&db));
        assert!(Selector::parse("app==db").unwrap().matches(&db));
        // `!=` holds for an absent label, as in Kubernetes.
        assert!(Selector::parse("tier!=front").unwrap().matches(&db));
        assert!(Selector::parse("tier").unwrap().matches(&web));
        assert!(!Selector::parse("tier").unwrap().matches(&db));
        assert!(Selector::parse("!tier").unwrap().matches(&db));
        assert!(Selector::parse("").unwrap().matches(&db));
        assert!(Selector::parse(" app=web , tier ").unwrap().matches(&web));
    }

    #[test]
    fn what_is_not_the_equality_grammar_is_refused() {
        for bad in ["env in (a,b)", "=web", "!=web", "a b=c", "!"] {
            assert!(Selector::parse(bad).is_err(), "{bad}");
        }
    }
}
