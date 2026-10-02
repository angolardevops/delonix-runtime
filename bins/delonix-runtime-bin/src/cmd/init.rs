//! `delonix init` — looks at the directory and starts the RIGHT project for it.
//!
//! `stack init` already generates a complete, filled-in project, and `vm init` does the
//! same for a VM. What was missing is the step before those: knowing which one to call, and
//! with which template. That is the whole job here — detect, **say what was detected and
//! why**, and dispatch. It generates nothing of its own.
//!
//! The detection reads the project's own manifests — the dependencies declared in
//! `package.json`, `composer.json`, `pyproject.toml` or `requirements.txt` — because a file
//! NAME does not say which framework a project uses: a `composer.json` is any PHP project,
//! a `package.json` any Node one. When the manifest names no framework a template exists
//! for, the answer is "unknown" and the generic scaffold, with the reason printed, never a
//! template picked because it was the last rule left. [`detect`] is pure over the file
//! names and contents it is given, so every case is testable without a disk.
//!
//! Precedence: an explicit `-t` wins over everything detected, a compose file and a
//! VMfile included (with a warning naming the file); without `-t`, a VMfile goes to
//! `vm init`, and a compose file is already served by `delonix compose up`.

use std::path::Path;

use delonix_model::Result;

/// What the directory looks like, and the evidence for saying so.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Detection {
    /// `stack init --template <t>`; `None` = the generic scaffold.
    pub(crate) template: Option<&'static str>,
    /// A VM project (`VMfile` present) — `vm init`, not `stack init`.
    pub(crate) vm: bool,
    /// The file (and dependency) that decided it. Printed, so the guess is auditable.
    pub(crate) evidence: &'static str,
    /// Set when the right answer is NOT to generate anything.
    pub(crate) already_served: Option<&'static str>,
    /// A manifest was found but names no framework a template exists for.
    pub(crate) unknown: bool,
}

impl Detection {
    fn tpl(template: &'static str, evidence: &'static str) -> Self {
        Self {
            template: Some(template),
            vm: false,
            evidence,
            already_served: None,
            unknown: false,
        }
    }

    fn generic(evidence: &'static str) -> Self {
        Self {
            template: None,
            vm: false,
            evidence,
            already_served: None,
            unknown: false,
        }
    }

    fn unknown(evidence: &'static str) -> Self {
        Self {
            unknown: true,
            ..Self::generic(evidence)
        }
    }
}

/// The manifest contents [`detect`] reads (each `None` when the file is absent).
#[derive(Debug, Default)]
pub(crate) struct Manifests {
    pub(crate) package_json: Option<String>,
    pub(crate) composer_json: Option<String>,
    pub(crate) pyproject: Option<String>,
    pub(crate) requirements: Option<String>,
}

impl Manifests {
    fn read(dir: &Path) -> Self {
        let r = |f: &str| std::fs::read_to_string(dir.join(f)).ok();
        Self {
            package_json: r("package.json"),
            composer_json: r("composer.json"),
            pyproject: r("pyproject.toml"),
            requirements: r("requirements.txt"),
        }
    }
}

/// The dependency names a `package.json` or `composer.json` declares, in any of the
/// dependency maps. `None` when the text is not a JSON object.
fn json_dependencies(text: &str, maps: &[&str]) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let obj = v.as_object()?;
    Some(
        maps.iter()
            .filter_map(|m| obj.get(*m).and_then(|d| d.as_object()))
            .flat_map(|d| d.keys().cloned())
            .collect(),
    )
}

/// The distribution name at the start of a PEP 508 requirement (`Django>=5 ; x` →
/// `django`), normalised the way pip compares names.
fn requirement_name(req: &str) -> Option<String> {
    let name: String = req
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    (!name.is_empty()).then(|| name.to_ascii_lowercase().replace(['_', '.'], "-"))
}

/// Requirement names in a `requirements.txt` (comments, options and blank lines skipped).
fn requirements_txt_names(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty() && !l.starts_with('-'))
        .filter_map(requirement_name)
        .collect()
}

/// Requirement names a `pyproject.toml` declares: the string items of
/// `[project] dependencies`, `[project.optional-dependencies]` and `[dependency-groups]`
/// arrays, and the keys of Poetry's dependency tables. A line-level reader, not a TOML
/// parser: it looks only where requirements are declared, so a `description = "a FastAPI
/// service"` never counts as a dependency.
fn pyproject_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut table = String::new();
    let mut in_array = false;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') && !in_array {
            table = line
                .trim_matches(|c| c == '[' || c == ']')
                .trim()
                .to_string();
            continue;
        }
        let poetry = table.starts_with("tool.poetry") && table.ends_with("dependencies");
        if !in_array {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim().trim_matches('"');
            if poetry {
                if let Some(n) = requirement_name(key) {
                    if n != "python" {
                        names.push(n);
                    }
                }
                continue;
            }
            let arrays = key == "dependencies"
                || table == "project.optional-dependencies"
                || table == "dependency-groups";
            if arrays && value.trim_start().starts_with('[') {
                in_array = true;
                scan_quoted(value, &mut names);
                if value.contains(']') {
                    in_array = false;
                }
            }
            continue;
        }
        scan_quoted(line, &mut names);
        if line.contains(']') {
            in_array = false;
        }
    }
    names
}

/// Pushes the requirement name of every quoted string on `line`.
fn scan_quoted(line: &str, names: &mut Vec<String>) {
    for (i, part) in line.split(['"', '\'']).enumerate() {
        if i % 2 == 1 {
            if let Some(n) = requirement_name(part) {
                names.push(n);
            }
        }
    }
}

/// Decides from the file names present and the manifests' declared dependencies. Ordered
/// most-specific first: a Django project also has `.py` files, and a Next.js one also has
/// `package.json` — the broader rule must not win just because it was checked earlier.
pub(crate) fn detect(has: &dyn Fn(&str) -> bool, m: &Manifests) -> Detection {
    // A VMfile is unambiguous and belongs to the other generator entirely.
    if has("VMfile") {
        return Detection {
            vm: true,
            ..Detection::generic("VMfile")
        };
    }
    // Compose already runs natively (`delonix compose up`), so generating a parallel
    // manifest here would leave the project with two sources of truth. Say so instead.
    if has("docker-compose.yml") || has("docker-compose.yaml") || has("compose.yaml") {
        return Detection {
            already_served: Some("delonix compose up"),
            ..Detection::generic("docker-compose.yml")
        };
    }
    if has("__manifest__.py") || has("odoo.conf") {
        return Detection::tpl("odoo", "__manifest__.py/odoo.conf");
    }
    if has("manage.py") {
        return Detection::tpl("django", "manage.py");
    }
    if has("artisan") {
        return Detection::tpl("laravel", "artisan");
    }
    if let Some(text) = &m.composer_json {
        return match json_dependencies(text, &["require", "require-dev"]) {
            Some(deps) if deps.iter().any(|d| d == "laravel/framework") => {
                Detection::tpl("laravel", "composer.json (laravel/framework)")
            }
            Some(_) => Detection::unknown("composer.json (no laravel/framework dependency)"),
            None => Detection::unknown("composer.json (not a JSON object)"),
        };
    }
    if has("go.mod") {
        return Detection::tpl("go", "go.mod");
    }
    if let Some(text) = &m.package_json {
        // Read from the manifest itself, not from a lockfile or a directory name: the
        // dependency is the only thing that actually says which framework this is.
        let Some(deps) = json_dependencies(text, &["dependencies", "devDependencies"]) else {
            return Detection::unknown("package.json (not a JSON object)");
        };
        let dep = |n: &str| deps.iter().any(|d| d == n);
        if dep("next") {
            return Detection::tpl("nextjs", "package.json (next)");
        }
        if dep("@nestjs/core") {
            return Detection::tpl("nestjs", "package.json (@nestjs/core)");
        }
        if dep("fastify") {
            return Detection::tpl("node", "package.json (fastify)");
        }
        return Detection::unknown("package.json (no next, @nestjs/core or fastify dependency)");
    }
    if m.pyproject.is_some() || m.requirements.is_some() {
        let mut names = m
            .pyproject
            .as_deref()
            .map(pyproject_names)
            .unwrap_or_default();
        names.extend(
            m.requirements
                .as_deref()
                .map(requirements_txt_names)
                .unwrap_or_default(),
        );
        if names.iter().any(|n| n == "django") {
            return Detection::tpl("django", "pyproject.toml/requirements.txt (django)");
        }
        if names.iter().any(|n| n == "fastapi") {
            return Detection::tpl("fastapi", "pyproject.toml/requirements.txt (fastapi)");
        }
        return Detection::unknown(
            "pyproject.toml/requirements.txt (no django or fastapi requirement)",
        );
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
        return Detection::generic("Dockerfile/Delonixfile");
    }
    Detection::generic("an empty directory")
}

/// Detects and dispatches. `template` overrides the detection entirely — the guess is a
/// convenience, never something the user has to fight.
pub fn run(
    dir: Option<std::path::PathBuf>,
    name: Option<String>,
    template: Option<String>,
    template_version: Option<String>,
    force: bool,
    edge: super::scaffold::EdgeArgs,
) -> Result<()> {
    let dir = dir.unwrap_or_else(|| std::path::PathBuf::from("."));
    let d = &dir;
    let has = |f: &str| Path::new(d).join(f).exists();
    let det = detect(&has, &Manifests::read(&dir));

    if let Some(t) = &template {
        // Explicit wins. What detection would have done is still said, because a compose
        // file or a VMfile next to the new manifest is a second source of truth the user
        // should know about.
        if det.vm || det.already_served.is_some() {
            super::output::warn(&super::po::tf(
                "found {evidence}, but -t {t} was given: generating the {t} template anyway \
                 — the project now has two descriptions to keep in step",
                &[("evidence", det.evidence), ("t", t)],
            ));
        }
        println!(
            "{}",
            super::po::tf("using -t {t} → stack init --template {t}", &[("t", t)])
        );
        return super::stack::init_for(
            super::scaffold::Target::Stack,
            dir,
            name,
            None,
            force,
            template,
            template_version,
            false,
            edge.clone(),
        );
    }

    if let Some(cmd) = det.already_served {
        super::output::warn(&super::po::tf(
            "found {evidence} — this project already runs natively with `{cmd}`; \
             generating a second manifest would give it two sources of truth",
            &[("evidence", det.evidence), ("cmd", cmd)],
        ));
        return Ok(());
    }
    if det.unknown {
        super::output::warn(&super::po::tf(
            "found {evidence} — no template exists for this project's framework; \
             writing the generic scaffold (pass -t <template> to choose one)",
            &[("evidence", det.evidence)],
        ));
    }
    let chosen = det.template.map(String::from);
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
            name,
            None,
            force,
            chosen,
            template_version,
            false,
        );
    }
    super::stack::init_for(
        super::scaffold::Target::Stack,
        dir,
        name,
        None,
        force,
        chosen,
        template_version,
        false,
        edge,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det(files: &[&'static str], m: Manifests) -> Detection {
        let owned: Vec<String> = files.iter().map(|s| s.to_string()).collect();
        detect(&|f: &str| owned.iter().any(|x| x == f), &m)
    }

    fn pkg(json: &str) -> Manifests {
        Manifests {
            package_json: Some(json.into()),
            ..Default::default()
        }
    }

    /// The order is the requirement, not the individual rules: a Django project also has
    /// `.py` files and a Next.js one also has `package.json`, so a broader rule that ran
    /// first would silently win and generate the wrong project next to the right code.
    #[test]
    fn the_most_specific_rule_wins_over_the_broader_one() {
        let dj = Manifests {
            requirements: Some("fastapi\n".into()),
            ..Default::default()
        };
        assert_eq!(
            det(&["manage.py", "requirements.txt"], dj).template,
            Some("django")
        );
        assert_eq!(
            det(
                &["package.json"],
                pkg(r#"{"dependencies":{"next":"16","fastify":"5"}}"#)
            )
            .template,
            Some("nextjs")
        );
        assert_eq!(
            det(
                &["package.json"],
                pkg(r#"{"dependencies":{"@nestjs/core":"12"}}"#)
            )
            .template,
            Some("nestjs")
        );
        assert_eq!(
            det(
                &["package.json"],
                pkg(r#"{"devDependencies":{"fastify":"5"}}"#)
            )
            .template,
            Some("node")
        );
        assert_eq!(det(&["go.mod"], Manifests::default()).template, Some("go"));
    }

    /// Before this, `package.json` with any content meant Fastify, `composer.json` meant
    /// Laravel and any Python manifest meant FastAPI. The manifest must NAME the framework;
    /// otherwise the answer is "unknown", not the last template left.
    #[test]
    fn a_manifest_without_a_known_framework_is_unknown() {
        let express = det(
            &["package.json"],
            pkg(r#"{"dependencies":{"express":"4"}}"#),
        );
        assert!(express.unknown && express.template.is_none());
        // The substring the old matcher looked for, in a place that is not a dependency.
        let textual = det(
            &["package.json"],
            pkg(r#"{"description":"not \"next\" at all"}"#),
        );
        assert!(textual.unknown, "a textual match must not decide");
        assert!(det(&["package.json"], pkg("not json")).unknown);

        let symfony = Manifests {
            composer_json: Some(r#"{"require":{"symfony/framework-bundle":"7"}}"#.into()),
            ..Default::default()
        };
        assert!(det(&["composer.json"], symfony).unknown);
        let laravel = Manifests {
            composer_json: Some(r#"{"require":{"laravel/framework":"^13.0"}}"#.into()),
            ..Default::default()
        };
        assert_eq!(det(&["composer.json"], laravel).template, Some("laravel"));

        let flask = Manifests {
            pyproject: Some("[project]\nname = \"x\"\ndescription = \"a fastapi-like thing\"\ndependencies = [\"flask>=3\"]\n".into()),
            ..Default::default()
        };
        assert!(
            det(&["pyproject.toml"], flask).unknown,
            "the description is not a dependency"
        );
    }

    #[test]
    fn python_manifests_are_read_where_requirements_live() {
        let names = pyproject_names(
            "[project]\ndependencies = [\n  \"Django==5.2.*\",  # web\n  'uvicorn[standard]>=0.3',\n]\n\
             [dependency-groups]\ndev = [\"pytest>=8\"]\n\
             [tool.poetry.dependencies]\npython = \"^3.12\"\nFastAPI = \"^0.115\"\n",
        );
        assert_eq!(names, vec!["django", "uvicorn", "pytest", "fastapi"]);
        assert_eq!(
            requirements_txt_names(
                "# pins\n-r base.txt\nFastAPI==0.115.0\nhttpx ; python_version>'3'\n"
            ),
            vec!["fastapi", "httpx"]
        );
    }

    /// A `VMfile` is the other generator entirely, and a compose file is already served —
    /// generating next to it would leave two sources of truth. (An explicit `-t` still
    /// wins in `run`, with a warning.)
    #[test]
    fn a_vmfile_and_compose_do_not_fall_into_the_generic_scaffold() {
        let vm = det(&["VMfile", "go.mod"], Manifests::default());
        assert!(vm.vm, "the VMfile must win even with a go.mod next to it");
        let compose = det(&["docker-compose.yml", "package.json"], pkg("{}"));
        assert_eq!(compose.already_served, Some("delonix compose up"));
        assert!(
            compose.template.is_none(),
            "nothing is generated over compose"
        );
    }

    /// A Dockerfile says how to BUILD, not which language template to impose.
    #[test]
    fn a_dockerfile_alone_uses_the_generic_scaffold() {
        let d = det(&["Dockerfile"], Manifests::default());
        assert!(d.template.is_none() && !d.vm && !d.unknown);
        assert_eq!(d.evidence, "Dockerfile/Delonixfile");
        assert_eq!(
            det(&[], Manifests::default()).evidence,
            "an empty directory"
        );
    }
}
