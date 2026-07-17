//! `delonix kube generate` — gera um manifesto Kubernetes (`kind: Pod`) a partir
//! de um container ou pod já existente no runtime. É o caminho "corri-o local,
//! agora dá-me o YAML para o k8s" (equivalente ao `podman generate kube`).

use clap::Subcommand;
use delonix_runtime_core::{Container, Error, Result, Store};

use super::util::open_stores;

#[derive(Subcommand)]
pub enum KubeCmd {
    /// Gera um `kind: Pod` a partir de um container (ou de todos os membros de
    /// um pod) e imprime-o em stdout.
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
    // Aceita um nome de pod (gera todos os membros) ou um único container.
    let members: Vec<Container> = if store.load(&format!("pod-{name}")).is_ok() {
        all.into_iter().filter(|c| c.pod.as_deref() == Some(name) && !c.name.starts_with("pod-")).collect()
    } else {
        vec![store.load(name)?]
    };
    if members.is_empty() {
        return Err(Error::Invalid(format!("nada a gerar para '{name}'")));
    }
    print!("{}", pod_manifest(name, &members));
    Ok(())
}

/// Função pura: constrói o YAML do `kind: Pod` (testável sem Store).
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

fn quote(s: &str) -> String {
    format!("\"{s}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctr(name: &str, image: &str, cmd: &[&str], cpus: &str, mem: &str) -> Container {
        let mut c = Container::new("id".into(), name.into(), image.into(), cmd.iter().map(|s| s.to_string()).collect(), mem.into());
        c.cpus = cpus.into();
        c
    }

    #[test]
    fn gera_pod_com_recursos_e_args() {
        let c = ctr("web", "nginx:1.27", &["nginx", "-g", "daemon off;"], "0.5", "256M");
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
        assert!(!y.contains("memory:"), "não devia haver limite de memória: {y}");
    }
}
