//! `delonix init` — looks at the directory and starts the RIGHT project for it.
//!
//! `stack init` already generates a complete, filled-in project, and `vm init` does the
//! same for a VM. What was missing is the step before those: knowing which one to call, and
//! with which of the eleven templates. That is the whole job here — detect, **say what was
//! detected and why**, and dispatch. It generates nothing of its own.
//!
//! The detection is a pure function ([`detect`]) over the file names present, so it is
//! testable without touching a disk, and it always explains itself: a wrong guess the user
//! can see is a wrong guess the user can override with `-t`, while a silent one just
//! produces a project that does not match the code sitting next to it.

use std::path::Path;

use delonix_runtime_core::Result;

/// What the directory looks like, and the evidence for saying so.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Detection {
    /// `stack init --template <t>`; `None` = the generic scaffold.
    pub(crate) template: Option<&'static str>,
    /// A VM project (`VMfile` present) — `vm init`, not `stack init`.
    pub(crate) vm: bool,
    /// The file that decided it. Printed, so the guess is auditable.
    pub(crate) evidence: &'static str,
    /// Set when the right answer is NOT to generate anything.
    pub(crate) already_served: Option<&'static str>,
}

impl Detection {
    fn tpl(template: &'static str, evidence: &'static str) -> Self {
        Self {
            template: Some(template),
            vm: false,
            evidence,
            already_served: None,
        }
    }
}

/// Decides from the file names present. Ordered most-specific first: a Django project also
/// has `.py` files, and a Next.js one also has `package.json` — the broader rule must not
/// win just because it was checked earlier.
pub(crate) fn detect(has: &dyn Fn(&str) -> bool, pkg_json: Option<&str>) -> Detection {
    // A VMfile is unambiguous and belongs to the other generator entirely.
    if has("VMfile") {
        return Detection {
            template: None,
            vm: true,
            evidence: "VMfile",
            already_served: None,
        };
    }
    // Compose already runs natively (`delonix compose up`), so generating a parallel
    // manifest here would leave the project with two sources of truth. Say so instead.
    if has("docker-compose.yml") || has("docker-compose.yaml") || has("compose.yaml") {
        return Detection {
            template: None,
            vm: false,
            evidence: "docker-compose.yml",
            already_served: Some("delonix compose up"),
        };
    }
    if has("__manifest__.py") || has("odoo.conf") {
        return Detection::tpl("odoo", "__manifest__.py/odoo.conf");
    }
    if has("manage.py") {
        return Detection::tpl("django", "manage.py");
    }
    if has("artisan") || has("composer.json") {
        return Detection::tpl("laravel", "artisan/composer.json");
    }
    if has("go.mod") {
        return Detection::tpl("go", "go.mod");
    }
    if let Some(pkg) = pkg_json {
        // Read from the manifest itself, not from a lockfile or a directory name: the
        // dependency is the only thing that actually says which framework this is.
        if pkg.contains("\"next\"") {
            return Detection::tpl("nextjs", "package.json (next)");
        }
        if pkg.contains("@nestjs/core") {
            return Detection::tpl("nestjs", "package.json (@nestjs/core)");
        }
        return Detection::tpl("node", "package.json");
    }
    if has("pyproject.toml") || has("requirements.txt") {
        return Detection::tpl("python", "pyproject.toml/requirements.txt");
    }
    if has("haproxy.cfg") {
        return Detection::tpl("haproxy", "haproxy.cfg");
    }
    if has("nginx.conf") {
        return Detection::tpl("nginx", "nginx.conf");
    }
    // A Dockerfile is a BUILD, not a language: the generic scaffold wires it up as-is
    // instead of guessing a template that would fight with it.
    if has("Dockerfile") || has("Delonixfile") {
        return Detection {
            template: None,
            vm: false,
            evidence: "Dockerfile/Delonixfile",
            already_served: None,
        };
    }
    Detection {
        template: None,
        vm: false,
        evidence: "an empty directory",
        already_served: None,
    }
}

/// Detects and dispatches. `template` overrides the detection entirely — the guess is a
/// convenience, never something the user has to fight.
pub fn run(dir: Option<std::path::PathBuf>, template: Option<String>, force: bool) -> Result<()> {
    let dir = dir.unwrap_or_else(|| std::path::PathBuf::from("."));
    let d = &dir;
    let has = |f: &str| Path::new(d).join(f).exists();
    let pkg = std::fs::read_to_string(dir.join("package.json")).ok();
    let det = detect(&has, pkg.as_deref());

    if let Some(cmd) = det.already_served {
        super::output::warn(&super::po::tf(
            "found {evidence} — this project already runs natively with `{cmd}`; \
             generating a second manifest would give it two sources of truth",
            &[("evidence", det.evidence), ("cmd", cmd)],
        ));
        return Ok(());
    }
    let chosen = template.clone().or_else(|| det.template.map(String::from));
    println!(
        "{}",
        super::po::tf(
            "detected {evidence} → {what}",
            &[
                ("evidence", det.evidence),
                (
                    "what",
                    &if det.vm {
                        "vm init".to_string()
                    } else {
                        match &chosen {
                            Some(t) => format!("stack init --template {t}"),
                            None => "stack init (generic scaffold)".to_string(),
                        }
                    }
                ),
            ],
        )
    );
    // Dispatches to the SAME generator the explicit commands use — this module decides
    // which one, it does not generate anything of its own.
    if det.vm {
        // `vm init` has its OWN generator (the two have the same signature but are not the
        // same function) — dispatching to the stack one would quietly produce a different
        // project than `vm init` does.
        return super::vm::init_for(
            super::scaffold::Target::Vm,
            dir,
            None,
            None,
            force,
            chosen,
            false,
        );
    }
    super::stack::init_for(
        super::scaffold::Target::Stack,
        dir,
        None,
        None,
        force,
        chosen,
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det(files: &[&'static str], pkg: Option<&str>) -> Detection {
        let owned: Vec<String> = files.iter().map(|s| s.to_string()).collect();
        detect(&|f: &str| owned.iter().any(|x| x == f), pkg)
    }

    /// The order is the requirement, not the individual rules: a Django project also has
    /// `.py` files and a Next.js one also has `package.json`, so a broader rule that ran
    /// first would silently win and generate the wrong project next to the right code.
    #[test]
    fn a_regra_mais_especifica_ganha_a_mais_larga() {
        assert_eq!(
            det(&["manage.py", "requirements.txt"], None).template,
            Some("django"),
            "manage.py tem de ganhar ao requirements.txt"
        );
        assert_eq!(
            det(&["package.json"], Some(r#"{"dependencies":{"next":"14"}}"#)).template,
            Some("nextjs")
        );
        assert_eq!(
            det(
                &["package.json"],
                Some(r#"{"dependencies":{"@nestjs/core":"10"}}"#)
            )
            .template,
            Some("nestjs")
        );
        assert_eq!(
            det(
                &["package.json"],
                Some(r#"{"dependencies":{"express":"4"}}"#)
            )
            .template,
            Some("node")
        );
        assert_eq!(det(&["go.mod"], None).template, Some("go"));
    }

    /// A `VMfile` is the other generator entirely, and a compose file is already served —
    /// generating next to it would leave two sources of truth.
    #[test]
    fn o_vmfile_e_o_compose_nao_caem_no_scaffold_generico() {
        let vm = det(&["VMfile", "go.mod"], None);
        assert!(vm.vm, "o VMfile tem de ganhar mesmo com um go.mod ao lado");
        let compose = det(&["docker-compose.yml", "package.json"], Some("{}"));
        assert_eq!(compose.already_served, Some("delonix compose up"));
        assert!(
            compose.template.is_none(),
            "nao gera nada por cima do compose"
        );
    }

    /// A Dockerfile says how to BUILD, not which language template to impose.
    #[test]
    fn um_dockerfile_sozinho_usa_o_scaffold_generico() {
        let d = det(&["Dockerfile"], None);
        assert!(d.template.is_none() && !d.vm);
        assert_eq!(d.evidence, "Dockerfile/Delonixfile");
        assert_eq!(det(&[], None).evidence, "an empty directory");
    }
}
