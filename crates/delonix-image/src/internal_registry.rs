//! Internal OCI registry (Distribution `registry:2`) managed by Delonix — Pillar 1, **Block E**.
//!
//! A local registry is the **target of the *exporter*** of Cloud Native Buildpacks (rootless, no
//! daemon — the lifecycle exports to a registry, not to a Docker socket) and the substrate of the
//! **cache** and the **remote build**. Here lives only the **pure logic** (address, image ref,
//! `delonix run` arguments that bring up the registry); the CLI drives the execution (`delonix
//! registry up/status/down`), which requires runtime+network — E2E yet to be validated in this environment.

/// Name of the internal registry container.
pub const NAME: &str = "delonix-registry";
/// Registry image (Distribution).
pub const IMAGE: &str = "registry:2";
/// Named volume for registry persistence.
pub const DATA_VOLUME: &str = "delonix-registry-data";
/// Default port (published on loopback).
pub const DEFAULT_PORT: u16 = 5000;

/// Address of the internal registry (always loopback — not exposed to the network).
pub fn addr(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

/// OCI reference of an app image in the internal registry (`<addr>/<name>:latest`).
pub fn image_ref(addr: &str, name: &str) -> String {
    format!("{addr}/{name}:latest")
}

/// `delonix run` arguments that bring up the internal registry (detached, on loopback, with
/// a persistent volume). The host port maps to the 5000 internal to `registry:2`.
pub fn run_args(port: u16) -> Vec<String> {
    vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        NAME.into(),
        "-p".into(),
        // The engine's publish is loopback-only BY DEFAULT (DNAT only on the `output` chain,
        // 127.0.0.0/8); external exposure requires DELONIX_PUBLISH_ADDR. So `port:5000`
        // is already restricted to loopback — without needing the `127.0.0.1:` prefix (Docker-style,
        // not yet supported by the `-p` parser).
        format!("{port}:5000"),
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
        assert!(a.contains(&"5000:5000".to_string())); // loopback-only is the publish default
        assert!(a.contains(&format!("{DATA_VOLUME}:/var/lib/registry"))); // persistent
        assert_eq!(a.last().unwrap(), IMAGE);
    }
}
