//! Registo OCI interno (Distribution `registry:2`) gerido pelo Delonix — Pilar 1, **Bloco E**.
//!
//! Um registo local é o **alvo do *exporter*** dos Cloud Native Buildpacks (rootless, sem
//! daemon, o lifecycle exporta para um registo, não para um socket Docker) e o substrato do
//! **cache** e do **build remoto**. Aqui está só a **lógica pura** (endereço, ref de imagem,
//! argumentos do `delonix run` que sobem o registo); a CLI conduz a execução (`delonix
//! registry up/status/down`), que exige runtime+rede — E2E por validar neste ambiente.

/// Nome do container do registo interno.
pub const NAME: &str = "delonix-registry";
/// Imagem do registo (Distribution).
pub const IMAGE: &str = "registry:2";
/// Volume nomeado para a persistência do registo.
pub const DATA_VOLUME: &str = "delonix-registry-data";
/// Porta por omissão (publicada em loopback).
pub const DEFAULT_PORT: u16 = 5000;

/// Endereço do registo interno (sempre loopback — não exposto à rede).
pub fn addr(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

/// Referência OCI de uma imagem de app no registo interno (`<addr>/<name>:latest`).
pub fn image_ref(addr: &str, name: &str) -> String {
    format!("{addr}/{name}:latest")
}

/// Argumentos do `delonix run` que sobem o registo interno (detached, em loopback, com
/// volume persistente). A porta do host mapeia para a 5000 interna do `registry:2`.
pub fn run_args(port: u16) -> Vec<String> {
    vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        NAME.into(),
        "-p".into(),
        format!("127.0.0.1:{port}:5000"),
        "-v".into(),
        format!("{DATA_VOLUME}:/var/lib/registry"),
        IMAGE.into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addr_and_ref() {
        assert_eq!(addr(5000), "127.0.0.1:5000");
        assert_eq!(image_ref(&addr(5000), "shop"), "127.0.0.1:5000/shop:latest");
    }

    #[test]
    fn run_args_are_loopback_named_and_persistent() {
        let a = run_args(5000);
        assert_eq!(a[0], "run");
        assert!(a.contains(&"-d".to_string()));
        assert!(a.windows(2).any(|w| w == ["--name", NAME]));
        assert!(a.contains(&"127.0.0.1:5000:5000".to_string())); // só loopback
        assert!(a.contains(&format!("{DATA_VOLUME}:/var/lib/registry"))); // persistente
        assert_eq!(a.last().unwrap(), IMAGE);
    }
}
