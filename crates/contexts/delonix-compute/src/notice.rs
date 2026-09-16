//! A warning a use case wants the operator to see, returned as data.
//!
//! The context never prints: the interface that owns the terminal renders the
//! template in the operator's language (the CLI through its translation catalog)
//! and substitutes the named arguments. The template is the catalog key, so it is
//! written in English and kept byte for byte.

/// One warning: an English template with `{name}` placeholders, and their values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub template: &'static str,
    pub args: Vec<(&'static str, String)>,
}

impl Notice {
    pub fn new(template: &'static str, args: &[(&'static str, &str)]) -> Self {
        Notice {
            template,
            args: args.iter().map(|(k, v)| (*k, v.to_string())).collect(),
        }
    }

    /// The template with every placeholder substituted — the English rendering,
    /// for an interface without a catalog.
    pub fn render(&self) -> String {
        let mut out = self.template.to_string();
        for (k, v) in &self.args {
            out = out.replace(&format!("{{{k}}}"), v);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_substitutes_every_named_placeholder() {
        let n = Notice::new(
            "volume '{vol}' uses '{medium}'",
            &[("vol", "data"), ("medium", "x")],
        );
        assert_eq!(n.render(), "volume 'data' uses 'x'");
    }
}
