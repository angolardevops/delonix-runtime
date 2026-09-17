//! `delonix explain DX-4201` and `delonix explain codes` — the dictionary of
//! numbered codes (ADR-0043), for a person.
//!
//! The table lives in `delonix_model::codes`; this module only prints it. The
//! English texts are the msgids of the translation catalogue, like every other
//! user-facing string.

use super::output::{Describe, Table};
use super::po::{t, tf};
use delonix_model::codes::{self, Code};
use delonix_model::{Error, Result};

/// One entry, `kubectl describe` style.
pub fn explain(number: u16, json: bool) -> Result<()> {
    let Some(c) = codes::lookup(number) else {
        return Err(Error::NotFound(tf(
            "code {code} (the dictionary: `delonix explain codes`)",
            &[("code", &codes::label(number))],
        )));
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&entry_json(c))?);
    } else {
        describe(c).print();
    }
    Ok(())
}

/// One entry as JSON, its texts in the chosen language. The shape the generated
/// documentation page reads, and a script's way to look a code up.
fn entry_json(c: &Code) -> serde_json::Value {
    serde_json::json!({
        "code": c.label(),
        "number": c.number,
        "id": c.id,
        "class": t(c.class.name()),
        "domain": t(c.domain.name()),
        "exit": c.exit,
        "message": t(c.message),
        "meaning": t(c.meaning),
        "remedy": t(c.remedy),
    })
}

fn describe(c: &Code) -> Describe {
    let mut d = Describe::new();
    d.field(t("Code"), c.label())
        .field(t("Id"), c.id)
        .field(
            t("Class"),
            tf(
                "{class} (exit {exit})",
                &[("class", t(c.class.name())), ("exit", &c.exit.to_string())],
            ),
        )
        .field(t("Domain"), t(c.domain.name()))
        .field(t("Message"), t(c.message))
        .field(t("Meaning"), t(c.meaning))
        .field(t("What to do"), t(c.remedy));
    d
}

/// The whole dictionary, one line per code.
pub fn list(json: bool) -> Result<()> {
    if json {
        let all: Vec<_> = codes::CATALOG.iter().map(entry_json).collect();
        println!("{}", serde_json::to_string_pretty(&all)?);
        return Ok(());
    }
    let mut table = Table::new(&[t("CODE"), t("CLASS"), t("DOMAIN"), t("EXIT"), t("MESSAGE")]);
    for c in codes::CATALOG {
        table.row(vec![
            c.label(),
            t(c.class.name()).to_string(),
            t(c.domain.name()).to_string(),
            c.exit.to_string(),
            t(c.message).to_string(),
        ]);
    }
    table.print();
    Ok(())
}

#[cfg(test)]
mod tests {
    use delonix_model::codes::{Class, Domain, CATALOG};

    /// Every text of the dictionary reaches a Portuguese reader. A code added
    /// without its translation fails here, not in front of an operator.
    #[test]
    fn every_dictionary_text_has_a_portuguese_translation() {
        let mut missing = Vec::new();
        for c in CATALOG {
            for s in [c.message, c.meaning, c.remedy] {
                if !super::super::po::has_pt_translation(s) {
                    missing.push(format!("{}: {s}", c.label()));
                }
            }
        }
        for s in Class::ALL
            .iter()
            .map(|c| c.name())
            .chain(Domain::ALL.iter().map(|d| d.name()))
        {
            if !super::super::po::has_pt_translation(s) {
                missing.push(s.to_string());
            }
        }
        assert!(
            missing.is_empty(),
            "no PT translation for:\n{}",
            missing.join("\n")
        );
    }
}
