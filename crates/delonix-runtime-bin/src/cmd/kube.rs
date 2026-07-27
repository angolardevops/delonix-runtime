//! `delonix kube generate` — generates a Kubernetes manifest (`kind: Pod`) from
//! a container or pod already running in the engine. It's the "ran it locally,
//! now give me the YAML for k8s" path (equivalent to `podman generate kube`).

use clap::Subcommand;
use delonix_runtime_core::{Container, Error, Result, Store};

use super::util::open_stores;

#[derive(Subcommand)]
pub enum KubeCmd {
    /// Generates a `kind: Pod` from a container (or from every member of a
    /// pod) and prints it to stdout.
    Generate {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::containers))]
        name: String,
    },
}

pub fn run(action: KubeCmd) -> Result<()> {
    let (_images, store) = open_stores()?;
    match action {
        KubeCmd::Generate { name } => cmd_generate(&store, &name),
    }
}

fn cmd_generate(store: &Store, name: &str) -> Result<()> {
    let all = store.list()?;
    // Accepts a pod name (generates every member) or a single container.
    let members: Vec<Container> = if store.load(&format!("pod-{name}")).is_ok() {
        all.into_iter()
            .filter(|c| c.pod.as_deref() == Some(name) && !c.name.starts_with("pod-"))
            .collect()
    } else {
        vec![store.load(name)?]
    };
    if members.is_empty() {
        return Err(Error::Invalid(super::po::tf(
            "nothing to generate for '{name}'",
            &[("name", name)],
        )));
    }
    print!("{}", pod_manifest(name, &members));
    Ok(())
}

/// Pure function: builds the `kind: Pod` YAML (testable without a Store).
fn pod_manifest(name: &str, members: &[Container]) -> String {
    let mut y = String::new();
    y.push_str("apiVersion: v1\n");
    y.push_str("kind: Pod\n");
    y.push_str("metadata:\n");
    y.push_str(&format!("  name: {name}\n"));
    y.push_str("  labels:\n");
    y.push_str(&format!("    app: {name}\n"));
    y.push_str("spec:\n");
    y.push_str("  containers:\n");
    for c in members {
        y.push_str(&format!("    - name: {}\n", c.name));
        y.push_str(&format!("      image: {}\n", c.image));
        if !c.command.is_empty() {
            y.push_str(&format!("      command: [{}]\n", quote(&c.command[0])));
            if c.command.len() > 1 {
                let args: Vec<String> = c.command[1..].iter().map(|a| quote(a)).collect();
                y.push_str(&format!("      args: [{}]\n", args.join(", ")));
            }
        }
        y.push_str("      resources:\n");
        y.push_str("        limits:\n");
        // cpus (cores) → millicores do k8s; memória tal e qual (ex.: `64M` → `64Mi` aprox).
        if let Ok(cpus) = c.cpus.parse::<f64>() {
            y.push_str(&format!("          cpu: \"{}m\"\n", (cpus * 1000.0) as i64));
        }
        if c.memory_max != "max" {
            y.push_str(&format!("          memory: \"{}i\"\n", c.memory_max));
        }
    }
    y
}

/// YAML double-quoted-scalar escaping.
///
/// BUG FOUND: this used to be a bare `format!("\"{s}\"")` with no escaping
/// at all. `command`/`args` come from the container's own record (whatever
/// the user or an image entrypoint originally set), and this manifest's
/// whole point is to be piped into `kubectl apply -f -`. A command
/// containing an embedded `"` (`sh -c 'echo "hi"'`) produced `["echo
/// "hi""]` — invalid YAML, `kubectl` rejects it outright. A command
/// containing a raw newline would be worse: it injects a literal line
/// break into the document, potentially adding sibling keys the emitter
/// never intended. Proper YAML double-quoted scalars escape `\`/`"` and
/// control characters — this covers the realistic ones without pulling in
/// a full YAML emitter for one call site.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctr(name: &str, image: &str, cmd: &[&str], cpus: &str, mem: &str) -> Container {
        let mut c = Container::new(
            "id".into(),
            name.into(),
            image.into(),
            cmd.iter().map(|s| s.to_string()).collect(),
            mem.into(),
        );
        c.cpus = cpus.into();
        c
    }

    #[test]
    fn gera_pod_com_recursos_e_args() {
        let c = ctr(
            "web",
            "nginx:1.27",
            &["nginx", "-g", "daemon off;"],
            "0.5",
            "256M",
        );
        let y = pod_manifest("web", &[c]);
        assert!(y.contains("kind: Pod"));
        assert!(y.contains("name: web"));
        assert!(y.contains("image: nginx:1.27"));
        assert!(y.contains("command: [\"nginx\"]"));
        assert!(y.contains("args: [\"-g\", \"daemon off;\"]"));
        assert!(y.contains("cpu: \"500m\""));
        assert!(y.contains("memory: \"256Mi\""));
    }

    #[test]
    fn memoria_max_nao_vira_limite() {
        // `memory_max = "max"` (sem teto) não deve gerar um `memory: "maxi"` inválido.
        let c = ctr("x", "alpine", &["sh"], "1", "max");
        let y = pod_manifest("x", &[c]);
        assert!(
            !y.contains("memory:"),
            "não devia haver limite de memória: {y}"
        );
    }

    #[test]
    fn quote_escapa_aspas_e_barra_invertida() {
        // BUG regression guard: `quote` used to be a bare `"{s}"` with no
        // escaping — `sh -c 'echo "hi"'` produced `["echo "hi""]`, invalid
        // YAML that `kubectl apply -f -` rejects outright.
        assert_eq!(quote(r#"echo "hi""#), r#""echo \"hi\"""#);
        assert_eq!(quote(r"C:\path"), r#""C:\\path""#);
    }

    #[test]
    fn quote_escapa_quebras_de_linha_em_vez_de_as_injectar() {
        // A raw embedded newline would inject a literal line break into the
        // emitted document — potentially adding sibling keys the emitter
        // never intended. Must come out as the two-character escape `\n`,
        // not an actual newline byte.
        let q = quote("line1\nline2");
        assert_eq!(q, r#""line1\nline2""#);
        assert!(
            !q.contains('\n'),
            "não pode conter uma quebra de linha real"
        );
    }

    #[test]
    fn gera_pod_com_comando_contendo_aspas_produz_yaml_valido() {
        let c = ctr("web", "alpine", &["sh", "-c", r#"echo "hi""#], "0.1", "64M");
        let y = pod_manifest("web", &[c]);
        assert!(y.contains(r#""echo \"hi\"""#), "{y}");
        // A quick sanity check that the offending pattern from the bug
        // report (unescaped adjacent quotes) is gone.
        assert!(!y.contains(r#""echo "hi"""#));
    }
}
