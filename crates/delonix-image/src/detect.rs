//! Detecção automática de stack (Pilar 1, **Bloco C**) — a fundação do "git push deploy".
//!
//! Dada uma pasta de projecto, descobre a linguagem/framework por **ficheiros-marcador**
//! (`package.json`, `go.mod`, `requirements.txt`, …) e devolve, além da stack, o **template
//! de proxy** (id em `proxy-templates.json`), a **porta** por omissão e o **buildpack
//! Paketo** sugerido — tudo o que o `delonix deploy` precisa para buildar sem Dockerfile.
//!
//! Lógica pura, sem rede: 100% testável. A escolha do builder CNB e a execução do
//! lifecycle são o Bloco B.

use std::path::Path;

/// O resultado da detecção de uma pasta de projecto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    /// Família da stack: `node`, `python`, `go`, `ruby`, `rust`, `java`, `php`, `dotnet`, `static`.
    pub stack: &'static str,
    /// Framework concreto, quando detectável (`express`, `django`, `fastapi`, `rails`, …).
    pub framework: Option<&'static str>,
    /// Id do template de proxy (`proxy-templates.json`): `node-express`, `py-django`, …
    pub proxy_template: &'static str,
    /// Porta por omissão da app (alvo do ingress).
    pub default_port: u16,
    /// Buildpack Paketo sugerido (informativo até o Bloco B correr o lifecycle).
    pub builder: &'static str,
    /// Ficheiro-marcador que disparou a detecção.
    pub marker: &'static str,
}

/// Detecta a stack de um directório de projecto. `None` se nenhum marcador for reconhecido.
///
/// Ordem de prioridade: manifestos de linguagem explícitos primeiro (go.mod, Cargo.toml,
/// pom/gradle, *.csproj), depois `package.json` (Node/SPA), depois Python/Ruby/PHP, e por
/// fim `index.html` (site estático) como *fallback*.
pub fn detect(dir: &Path) -> Option<Detected> {
    let has = |f: &str| dir.join(f).exists();
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap_or_default().to_lowercase();
    let glob1 = |ext: &str| {
        std::fs::read_dir(dir).ok().and_then(|rd| {
            rd.flatten().any(|e| e.path().extension().and_then(|x| x.to_str()) == Some(ext)).then_some(())
        })
    };

    // Go
    if has("go.mod") {
        return Some(Detected { stack: "go", framework: None, proxy_template: "go", default_port: 8080, builder: "paketo-buildpacks/go", marker: "go.mod" });
    }
    // Rust
    if has("Cargo.toml") {
        return Some(Detected { stack: "rust", framework: None, proxy_template: "rust", default_port: 8080, builder: "paketo-buildpacks/rust", marker: "Cargo.toml" });
    }
    // Java (Maven/Gradle) → Spring por omissão
    if has("pom.xml") || has("build.gradle") || has("build.gradle.kts") {
        let marker = if has("pom.xml") { "pom.xml" } else { "build.gradle" };
        return Some(Detected { stack: "java", framework: Some("spring"), proxy_template: "java-spring", default_port: 8080, builder: "paketo-buildpacks/java", marker });
    }
    // .NET
    if has("global.json") || glob1("csproj").is_some() {
        return Some(Detected { stack: "dotnet", framework: None, proxy_template: "dotnet", default_port: 8080, builder: "paketo-buildpacks/dotnet-core", marker: "*.csproj" });
    }
    // Node — sub-detecção SPA vs servidor
    if has("package.json") {
        let pkg = read("package.json");
        let is_spa = ["react", "vue", "@angular", "svelte", "vite"].iter().any(|f| pkg.contains(f))
            && !["express", "fastify", "next", "koa", "nest"].iter().any(|f| pkg.contains(f));
        return Some(if is_spa {
            Detected { stack: "node", framework: Some("spa"), proxy_template: "spa", default_port: 80, builder: "paketo-buildpacks/nodejs", marker: "package.json" }
        } else {
            Detected { stack: "node", framework: Some("express"), proxy_template: "node-express", default_port: 3000, builder: "paketo-buildpacks/nodejs", marker: "package.json" }
        });
    }
    // Python — sub-detecção django/fastapi/flask
    if has("requirements.txt") || has("pyproject.toml") || has("Pipfile") {
        let marker = if has("requirements.txt") { "requirements.txt" } else if has("pyproject.toml") { "pyproject.toml" } else { "Pipfile" };
        let deps = format!("{}{}{}", read("requirements.txt"), read("pyproject.toml"), read("Pipfile"));
        let (framework, template, port) = if deps.contains("django") {
            ("django", "py-django", 8000)
        } else if deps.contains("fastapi") {
            ("fastapi", "py-fastapi", 8000)
        } else if deps.contains("flask") {
            ("flask", "py-flask", 5000)
        } else {
            ("fastapi", "py-fastapi", 8000) // omissão razoável para Python web
        };
        return Some(Detected { stack: "python", framework: Some(framework), proxy_template: template, default_port: port, builder: "paketo-buildpacks/python", marker });
    }
    // Ruby → Rails se o Gemfile o mencionar
    if has("Gemfile") {
        let gem = read("Gemfile");
        let framework = if gem.contains("rails") { Some("rails") } else { None };
        return Some(Detected { stack: "ruby", framework, proxy_template: "ruby-rails", default_port: 3000, builder: "paketo-buildpacks/ruby", marker: "Gemfile" });
    }
    // PHP → Laravel se o composer.json o mencionar; WordPress se houver wp-config
    if has("composer.json") || has("wp-config.php") {
        let comp = read("composer.json");
        let (framework, template) = if has("wp-config.php") {
            (Some("wordpress"), "php-wordpress")
        } else if comp.contains("laravel") {
            (Some("laravel"), "php-laravel")
        } else {
            (None, "php-laravel")
        };
        return Some(Detected { stack: "php", framework, proxy_template: template, default_port: 8080, builder: "paketo-buildpacks/php", marker: "composer.json" });
    }
    // Site estático (fallback) — só se houver index.html e nenhum manifesto acima.
    if has("index.html") {
        return Some(Detected { stack: "static", framework: None, proxy_template: "static", default_port: 80, builder: "paketo-buildpacks/web-servers", marker: "index.html" });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cria um directório temporário com os ficheiros (nome→conteúdo) dados.
    fn scratch(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dlx-detect-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (f, c) in files {
            std::fs::write(dir.join(f), c).unwrap();
        }
        dir
    }

    #[test]
    fn detects_node_express_and_spa() {
        let d = scratch("node-exp", &[("package.json", r#"{"dependencies":{"express":"4"}}"#)]);
        let got = detect(&d).unwrap();
        assert_eq!((got.stack, got.framework, got.proxy_template, got.default_port), ("node", Some("express"), "node-express", 3000));

        let s = scratch("node-spa", &[("package.json", r#"{"dependencies":{"react":"18","vite":"5"}}"#)]);
        let got = detect(&s).unwrap();
        assert_eq!((got.stack, got.framework, got.proxy_template), ("node", Some("spa"), "spa"));
    }

    #[test]
    fn detects_python_frameworks() {
        for (deps, fw, tpl, port) in [
            ("Django==5.0", "django", "py-django", 8000),
            ("fastapi==0.110", "fastapi", "py-fastapi", 8000),
            ("Flask==3.0", "flask", "py-flask", 5000),
            ("requests==2", "fastapi", "py-fastapi", 8000), // sem framework → default
        ] {
            let d = scratch(&format!("py-{fw}"), &[("requirements.txt", deps)]);
            let got = detect(&d).unwrap();
            assert_eq!((got.stack, got.framework.unwrap(), got.proxy_template, got.default_port), ("python", fw, tpl, port));
        }
    }

    #[test]
    fn detects_compiled_and_jvm_and_static() {
        assert_eq!(detect(&scratch("go", &[("go.mod", "module x")])).unwrap().proxy_template, "go");
        assert_eq!(detect(&scratch("rs", &[("Cargo.toml", "[package]")])).unwrap().proxy_template, "rust");
        assert_eq!(detect(&scratch("java", &[("pom.xml", "<project/>")])).unwrap().proxy_template, "java-spring");
        assert_eq!(detect(&scratch("ruby", &[("Gemfile", "gem 'rails'")])).unwrap().framework, Some("rails"));
        assert_eq!(detect(&scratch("static", &[("index.html", "<h1>oi</h1>")])).unwrap().stack, "static");
    }

    #[test]
    fn precedence_compiled_over_static_and_none_when_empty() {
        // go.mod + index.html → Go ganha (manifesto de linguagem tem prioridade).
        let d = scratch("prec", &[("go.mod", "module x"), ("index.html", "<h1/>")]);
        assert_eq!(detect(&d).unwrap().stack, "go");
        // pasta vazia → None.
        assert!(detect(&scratch("empty", &[])).is_none());
    }
}
