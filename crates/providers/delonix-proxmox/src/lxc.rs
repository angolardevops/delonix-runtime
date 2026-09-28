//! A system container on a Proxmox node (ADR-0058, plan 63 slice 3): the
//! `…/lxc` routes, and the [`SystemContainerProvider`] built on them.
//!
//! Every rule here was measured on PVE 9.2.2 before it was written:
//!
//! - **Creating from an OCI archive replaces `entrypoint` and `env` with the
//!   image's** (ADR-0058 T2). So the create sends neither; they are written
//!   with `PUT …/config` afterwards, read back, and compared. A difference
//!   destroys the container: a resource running with a configuration nobody
//!   asked for is worse than no resource.
//! - **`env` is one NUL-separated list and `entrypoint` one line** (the node's
//!   own schema). The line is split on spaces by the node, so an argument that
//!   holds a space cannot be kept as written, and is refused by name.
//! - **A start whose DHCP got no answer ends `WARNINGS: 1`** after about two
//!   minutes, with the container running and no IPv4 on `eth0` (T3). The
//!   start succeeds; the network verdict is `NotReady` with the warning.
//! - **`shutdown` with a deadline fails («container did not stop») for an init
//!   that ignores SIGTERM** — a bare `sleep` as PID 1 does. `stop` follows.
//! - **`DELETE` with `purge` and `destroy-unreferenced-disks`** removes the
//!   config and the rootfs volume.

use crate::error::{Error, Result};
use crate::{parse, Client, Ledger, TaskKind, Wrapped};
use delonix_compute::capability::ProviderReport;
use delonix_compute::system_container::{
    NetworkState, SystemContainerHandle, SystemContainerNet, SystemContainerObservation,
    SystemContainerProvider, SystemContainerSpec,
};
use delonix_compute::vm_provider::{Provider, ProviderId};
use std::path::Path;
use std::sync::Arc;

/// The provider id, as `provider ls` and a record show it.
pub const ID: &str = "proxmox";

impl Client {
    /// `POST /nodes/{node}/lxc`: creates container `vmid`, stopped. `form`
    /// is the create's parameters minus `vmid`.
    pub fn lxc_create(&self, ledger: &Ledger, vmid: u32, form: &[(&str, &str)]) -> Result<()> {
        let id = vmid.to_string();
        let mut all: Vec<(&str, &str)> = vec![("vmid", id.as_str())];
        all.extend_from_slice(form);
        self.task(
            ledger,
            vmid,
            TaskKind::CtCreate,
            || self.post_form(&format!("/nodes/{}/lxc", self.node), &all, true),
            Some(&|| Ok(self.lxc_config(vmid).is_ok())),
        )
    }

    /// `GET /nodes/{node}/lxc/{vmid}/config`.
    pub fn lxc_config(&self, vmid: u32) -> Result<serde_json::Value> {
        let body = self.get(&format!("/nodes/{}/lxc/{vmid}/config", self.node))?;
        let w: Wrapped<serde_json::Value> = parse(&body, "container config")?;
        Ok(w.data)
    }

    /// `PUT /nodes/{node}/lxc/{vmid}/config`. The node applies it inline and
    /// answers `null` (measured); a UPID is waited on.
    pub fn lxc_set_config(&self, ledger: &Ledger, vmid: u32, form: &[(&str, &str)]) -> Result<()> {
        self.task_or_done(
            ledger,
            vmid,
            TaskKind::Configure,
            || self.put_form(&format!("/nodes/{}/lxc/{vmid}/config", self.node), form),
            None,
        )
    }

    /// `GET /nodes/{node}/lxc/{vmid}/status/current` — `running` or `stopped`.
    pub fn lxc_status(&self, vmid: u32) -> Result<String> {
        let body = self.get(&format!("/nodes/{}/lxc/{vmid}/status/current", self.node))?;
        let w: Wrapped<serde_json::Value> = parse(&body, "container status")?;
        Ok(w.data
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string())
    }

    /// `POST …/status/start`, returning the task's `WARN:` lines (a failed
    /// DHCP is one).
    pub fn lxc_start(&self, ledger: &Ledger, vmid: u32) -> Result<Vec<String>> {
        self.task_collecting_warnings(
            ledger,
            vmid,
            TaskKind::CtStart,
            || {
                self.post_form(
                    &format!("/nodes/{}/lxc/{vmid}/status/start", self.node),
                    &[],
                    true,
                )
            },
            Some(&|| Ok(self.lxc_status(vmid)? == "running")),
            false,
        )
    }

    /// `POST …/status/shutdown` with a deadline in seconds.
    pub fn lxc_shutdown(&self, ledger: &Ledger, vmid: u32, timeout_secs: u32) -> Result<()> {
        let timeout = timeout_secs.to_string();
        self.task(
            ledger,
            vmid,
            TaskKind::CtShutdown,
            || {
                self.post_form(
                    &format!("/nodes/{}/lxc/{vmid}/status/shutdown", self.node),
                    &[("timeout", timeout.as_str())],
                    true,
                )
            },
            Some(&|| Ok(self.lxc_status(vmid)? == "stopped")),
        )
    }

    /// `POST …/status/stop`: kills the container.
    pub fn lxc_stop(&self, ledger: &Ledger, vmid: u32) -> Result<()> {
        self.task(
            ledger,
            vmid,
            TaskKind::CtStop,
            || {
                self.post_form(
                    &format!("/nodes/{}/lxc/{vmid}/status/stop", self.node),
                    &[],
                    true,
                )
            },
            Some(&|| Ok(self.lxc_status(vmid)? == "stopped")),
        )
    }

    /// `DELETE /nodes/{node}/lxc/{vmid}` with `purge` and
    /// `destroy-unreferenced-disks`, so the rootfs volume goes with it.
    pub fn lxc_destroy(&self, ledger: &Ledger, vmid: u32) -> Result<()> {
        self.task(
            ledger,
            vmid,
            TaskKind::CtDestroy,
            || {
                self.delete(&format!(
                    "/nodes/{}/lxc/{vmid}?purge=1&destroy-unreferenced-disks=1",
                    self.node
                ))
            },
            Some(&|| Ok(self.lxc_config(vmid).is_err())),
        )
    }

    /// `GET /nodes/{node}/lxc/{vmid}/interfaces`: the container's interfaces
    /// as the node reads them from inside.
    pub fn lxc_interfaces(&self, vmid: u32) -> Result<serde_json::Value> {
        let body = self.get(&format!("/nodes/{}/lxc/{vmid}/interfaces", self.node))?;
        let w: Wrapped<serde_json::Value> = parse(&body, "container interfaces")?;
        Ok(w.data)
    }
}

/// The node's `entrypoint` line for `args`. The node splits it on spaces, so
/// an argument holding whitespace (or a control character) is refused: it
/// would run as something else than written.
pub(crate) fn entrypoint_line(args: &[String]) -> Result<String> {
    for a in args {
        if a.is_empty() || a.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(Error::InvalidSystemContainer(format!(
                "proxmox: entrypoint argument {a:?} is empty or holds whitespace or a control \
                 character; the node stores the entrypoint as one line split on spaces"
            )));
        }
    }
    Ok(args.join(" "))
}

/// The node's `env` value: `NAME=value` pairs joined by NUL. A name is
/// letters, digits and underscores; a value holds no control character.
pub(crate) fn env_list(env: &[(String, String)]) -> Result<String> {
    let mut out = Vec::with_capacity(env.len());
    for (k, v) in env {
        let name_ok = !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !name_ok {
            return Err(Error::InvalidSystemContainer(format!(
                "proxmox: environment variable name {k:?} is not letters, digits and underscores"
            )));
        }
        if v.chars().any(|c| c.is_control() && c != '\t') {
            return Err(Error::InvalidSystemContainer(format!(
                "proxmox: environment variable {k} holds a control character"
            )));
        }
        out.push(format!("{k}={v}"));
    }
    Ok(out.join("\0"))
}

/// The node's `net0` value for `net`.
pub(crate) fn net0_value(net: &SystemContainerNet) -> Result<String> {
    crate::validate_bridge_name(&net.bridge)?;
    let mut v = format!("name=eth0,bridge={}", net.bridge);
    if let Some(tag) = net.vlan {
        if !(1..=4094).contains(&tag) {
            return Err(Error::InvalidSystemContainer(format!(
                "proxmox: VLAN tag {tag} is outside 1..=4094"
            )));
        }
        v.push_str(&format!(",tag={tag}"));
    }
    v.push_str(if net.dhcp { ",ip=dhcp" } else { ",ip=manual" });
    Ok(v)
}

/// A hostname the node keeps as written: DNS label characters only.
fn valid_hostname(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The IPv4 address (`a.b.c.d`, without its prefix) of interface `name` in a
/// `GET …/interfaces` answer, if it has one.
pub(crate) fn ipv4_of(interfaces: &serde_json::Value, name: &str) -> Option<String> {
    let iface = interfaces
        .as_array()?
        .iter()
        .find(|i| i.get("name").and_then(|n| n.as_str()) == Some(name))?;
    let inet = iface.get("inet")?.as_str()?;
    let addr = inet.split('/').next()?;
    (!addr.is_empty()).then(|| addr.to_string())
}

/// `(node, vmid)` from a `proxmox:<node>:<vmid>` locator.
pub(crate) fn parse_locator(locator: &str) -> Result<(String, u32)> {
    let mut parts = locator.splitn(3, ':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("proxmox"), Some(node), Some(vmid)) if !node.is_empty() => vmid
            .parse()
            .map(|v| (node.to_string(), v))
            .map_err(|_| Error::NoHandle(format!("proxmox: '{locator}' has no VM id"))),
        _ => Err(Error::NoHandle(format!(
            "proxmox: '{locator}' is not a proxmox:<node>:<vmid> locator"
        ))),
    }
}

/// What differs between what was asked and what the node kept: each entry is
/// `field: asked X, node has Y`. Empty when they agree. Pure.
pub(crate) fn config_divergence(
    config: &serde_json::Value,
    entrypoint: Option<&str>,
    env: Option<&str>,
) -> Vec<String> {
    let field = |k: &str| config.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let mut out = Vec::new();
    let unprivileged = config.get("unprivileged").and_then(|v| v.as_u64());
    if unprivileged != Some(1) {
        out.push(format!("unprivileged: asked 1, node has {unprivileged:?}"));
    }
    for (name, want) in [("entrypoint", entrypoint), ("env", env)] {
        if let Some(want) = want {
            let got = field(name);
            if got.as_deref() != Some(want) {
                out.push(format!("{name}: asked {want:?}, node has {got:?}"));
            }
        }
    }
    out
}

/// A [`SystemContainerProvider`] on one Proxmox node.
pub struct ProxmoxSystemContainerProvider {
    client: Arc<Client>,
    /// Where the image archive is uploaded (needs `vztmpl`).
    pub template_storage: String,
    /// Where the rootfs volume is created (needs `rootdir`).
    pub rootfs_storage: String,
    /// The deadline a `shutdown` is given before `stop` follows.
    pub shutdown_timeout_secs: u32,
}

impl ProxmoxSystemContainerProvider {
    pub fn new(client: Arc<Client>, template_storage: &str, rootfs_storage: &str) -> Self {
        ProxmoxSystemContainerProvider {
            client,
            template_storage: template_storage.to_string(),
            rootfs_storage: rootfs_storage.to_string(),
            shutdown_timeout_secs: 30,
        }
    }

    /// The client for the node the locator names.
    fn client_for(&self, node: &str) -> Result<Client> {
        self.client.for_node(node)
    }

    fn refuse(spec: &SystemContainerSpec) -> Result<()> {
        if !spec.unprivileged {
            return Err(Error::InvalidSystemContainer(
                "proxmox: a privileged system container is refused — privilege on a remote node \
                 is a decision of its own (ADR-0058); set unprivileged to true"
                    .to_string(),
            ));
        }
        if !valid_hostname(&spec.name) {
            return Err(Error::InvalidSystemContainer(format!(
                "proxmox: '{}' is not a hostname (letters, digits and '-', up to 63)",
                spec.name
            )));
        }
        if spec.rootfs_gib == 0 || spec.cores == 0 || spec.memory_mib == 0 {
            return Err(Error::InvalidSystemContainer(
                "proxmox: memory, cores and rootfs size must be above zero".to_string(),
            ));
        }
        Ok(())
    }

    fn observe_on(
        client: &Client,
        vmid: u32,
        spec: &SystemContainerSpec,
        warnings: &[String],
    ) -> Result<SystemContainerObservation> {
        let running = client.lxc_status(vmid)? == "running";
        let network = match &spec.network {
            None => NetworkState::NotRequested,
            Some(n) if !n.dhcp => NetworkState::NotRequested,
            Some(_) if !running => NetworkState::Unknown,
            Some(_) => match ipv4_of(&client.lxc_interfaces(vmid)?, "eth0") {
                Some(ipv4) => NetworkState::Ready { ipv4 },
                None => NetworkState::NotReady {
                    reason: if warnings.is_empty() {
                        "eth0 has no IPv4 address".to_string()
                    } else {
                        warnings.join("; ")
                    },
                },
            },
        };
        Ok(SystemContainerObservation { running, network })
    }
}

impl Provider for ProxmoxSystemContainerProvider {
    fn id(&self) -> ProviderId {
        ProviderId(ID)
    }

    fn capabilities(&self) -> ProviderReport {
        crate::capability_report(true)
    }
}

impl SystemContainerProvider for ProxmoxSystemContainerProvider {
    fn create(
        &self,
        dir: &Path,
        spec: &SystemContainerSpec,
    ) -> delonix_model::Result<SystemContainerHandle> {
        Self::refuse(spec)?;
        let entrypoint = (!spec.entrypoint.is_empty())
            .then(|| entrypoint_line(&spec.entrypoint))
            .transpose()?;
        let env = (!spec.env.is_empty())
            .then(|| env_list(&spec.env))
            .transpose()?;
        let net0 = spec.network.as_ref().map(net0_value).transpose()?;

        let client = &self.client;
        let staged =
            client.stage_template(&self.template_storage, &spec.archive, &spec.manifest_digest)?;
        let vmid = client.next_vmid()?;
        let ledger = Ledger::at(dir);
        let (memory, swap, cores) = (
            spec.memory_mib.to_string(),
            spec.swap_mib.to_string(),
            spec.cores.to_string(),
        );
        let rootfs = format!("{}:{}", self.rootfs_storage, spec.rootfs_gib);
        let mut form: Vec<(&str, &str)> = vec![
            ("ostemplate", staged.volid.as_str()),
            ("hostname", spec.name.as_str()),
            ("unprivileged", "1"),
            ("memory", memory.as_str()),
            ("swap", swap.as_str()),
            ("cores", cores.as_str()),
            ("rootfs", rootfs.as_str()),
        ];
        if let Some(net0) = &net0 {
            form.push(("net0", net0.as_str()));
        }
        client.lxc_create(&ledger, vmid, &form)?;

        // T2: the create put the image's entrypoint/env in; write ours, read
        // back, compare. A difference leaves no container behind.
        let result = (|| -> Result<()> {
            let mut set: Vec<(&str, &str)> = Vec::new();
            if let Some(e) = &entrypoint {
                set.push(("entrypoint", e.as_str()));
            }
            if let Some(e) = &env {
                set.push(("env", e.as_str()));
            }
            if !set.is_empty() {
                client.lxc_set_config(&ledger, vmid, &set)?;
            }
            let diverged = config_divergence(
                &client.lxc_config(vmid)?,
                entrypoint.as_deref(),
                env.as_deref(),
            );
            if diverged.is_empty() {
                Ok(())
            } else {
                Err(Error::UnexpectedAnswer(format!(
                    "proxmox: container {vmid} was created with a configuration other than the one \
                     asked for ({}); it was destroyed",
                    diverged.join("; ")
                )))
            }
        })();
        if let Err(e) = result {
            if let Err(d) = client.lxc_destroy(&ledger, vmid) {
                tracing::warn!(vmid, error = %d, "proxmox: could not destroy the container left by a failed configure");
            }
            return Err(e.into());
        }
        Ok(SystemContainerHandle {
            name: spec.name.clone(),
            locator: format!("proxmox:{}:{vmid}", client.node),
        })
    }

    fn start(
        &self,
        dir: &Path,
        h: &SystemContainerHandle,
        spec: &SystemContainerSpec,
    ) -> delonix_model::Result<SystemContainerObservation> {
        let (node, vmid) = parse_locator(&h.locator)?;
        let client = self.client_for(&node)?;
        let ledger = Ledger::at(dir);
        client.settle_pending(&ledger, vmid)?;
        let warnings = client.lxc_start(&ledger, vmid)?;
        Ok(Self::observe_on(&client, vmid, spec, &warnings)?)
    }

    fn stop(&self, dir: &Path, h: &SystemContainerHandle) -> delonix_model::Result<()> {
        let (node, vmid) = parse_locator(&h.locator)?;
        let client = self.client_for(&node)?;
        let ledger = Ledger::at(dir);
        client.settle_pending(&ledger, vmid)?;
        if client.lxc_status(vmid)? != "running" {
            return Ok(());
        }
        if let Err(e) = client.lxc_shutdown(&ledger, vmid, self.shutdown_timeout_secs) {
            tracing::info!(vmid, error = %e, "proxmox: the container did not shut down in time — stopping it");
        }
        if client.lxc_status(vmid)? == "running" {
            client.lxc_stop(&ledger, vmid)?;
        }
        Ok(())
    }

    fn destroy(&self, dir: &Path, h: &SystemContainerHandle) -> delonix_model::Result<()> {
        let (node, vmid) = parse_locator(&h.locator)?;
        let client = self.client_for(&node)?;
        let ledger = Ledger::at(dir);
        client.settle_pending(&ledger, vmid)?;
        if client.lxc_config(vmid).is_err() {
            return Ok(());
        }
        if client.lxc_status(vmid)? == "running" {
            client.lxc_stop(&ledger, vmid)?;
        }
        client.lxc_destroy(&ledger, vmid)?;
        Ok(())
    }

    fn observe(
        &self,
        _dir: &Path,
        h: &SystemContainerHandle,
        spec: &SystemContainerSpec,
    ) -> delonix_model::Result<SystemContainerObservation> {
        let (node, vmid) = parse_locator(&h.locator)?;
        let client = self.client_for(&node)?;
        Ok(Self::observe_on(&client, vmid, spec, &[])?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn an_entrypoint_is_one_line_and_an_argument_with_a_space_is_refused() {
        assert_eq!(
            entrypoint_line(&s(&["/bin/sleep", "3600"])).unwrap(),
            "/bin/sleep 3600"
        );
        for bad in [s(&["/bin/sh", "-c", "echo hi"]), s(&["a\tb"]), s(&[""])] {
            let err = entrypoint_line(&bad).unwrap_err();
            assert_eq!(err.number(), 1540, "{err}");
        }
    }

    #[test]
    fn an_environment_is_nul_separated_and_a_bad_name_is_refused() {
        let env = vec![
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("DLX_TEST".to_string(), "one two".to_string()),
        ];
        assert_eq!(
            env_list(&env).unwrap(),
            "PATH=/usr/bin:/bin\0DLX_TEST=one two"
        );
        for (k, v) in [("A-B", "x"), ("", "x"), ("A", "x\ny")] {
            let err = env_list(&[(k.to_string(), v.to_string())]).unwrap_err();
            assert_eq!(err.number(), 1540, "{k:?}: {err}");
        }
    }

    #[test]
    fn net0_carries_the_bridge_tag_and_dhcp() {
        let net = |vlan, dhcp| SystemContainerNet {
            bridge: "vmbr0".into(),
            vlan,
            dhcp,
        };
        assert_eq!(
            net0_value(&net(None, true)).unwrap(),
            "name=eth0,bridge=vmbr0,ip=dhcp"
        );
        assert_eq!(
            net0_value(&net(Some(20), false)).unwrap(),
            "name=eth0,bridge=vmbr0,tag=20,ip=manual"
        );
        assert!(net0_value(&net(Some(0), true)).is_err());
        assert!(net0_value(&SystemContainerNet {
            bridge: "vmbr0,firewall=1".into(),
            vlan: None,
            dhcp: true
        })
        .is_err());
    }

    /// The `GET …/interfaces` answer captured on PVE 9.2.2 for a container
    /// whose DHCP got no answer: `eth0` has only its link-local IPv6.
    #[test]
    fn the_ipv4_of_eth0_is_read_from_the_interfaces_answer() {
        let no_dhcp: serde_json::Value = serde_json::from_str(r#"[{"name":"lo","inet":"127.0.0.1/8","inet6":"::1/128","hwaddr":"00:00:00:00:00:00"},{"hwaddr":"bc:24:11:8c:bd:ca","inet6":"fe80::be24:11ff:fe8c:bdca/64","name":"eth0"}]"#).unwrap();
        assert_eq!(ipv4_of(&no_dhcp, "eth0"), None);
        assert_eq!(ipv4_of(&no_dhcp, "lo").as_deref(), Some("127.0.0.1"));
        let leased: serde_json::Value =
            serde_json::from_str(r#"[{"name":"eth0","inet":"10.0.0.5/24"}]"#).unwrap();
        assert_eq!(ipv4_of(&leased, "eth0").as_deref(), Some("10.0.0.5"));
    }

    #[test]
    fn a_locator_names_the_node_and_the_vmid() {
        assert_eq!(
            parse_locator("proxmox:pve2:9201").unwrap(),
            ("pve2".to_string(), 9201)
        );
        for bad in ["proxmox:pve", "libvirt:x:1", "proxmox::1", "proxmox:pve:x"] {
            assert!(parse_locator(bad).is_err(), "{bad}");
        }
    }

    /// The config captured after `PUT …/config` on PVE 9.2.2 agrees; a node
    /// that kept the image's entrypoint, or lost `unprivileged`, does not.
    #[test]
    fn a_config_that_differs_from_what_was_asked_is_named_field_by_field() {
        let kept: serde_json::Value = serde_json::json!({
            "entrypoint": "/bin/sleep 3600",
            "env": "PATH=/usr/bin:/bin\u{0}DLX_TEST=one",
            "unprivileged": 1,
        });
        let asked_env = "PATH=/usr/bin:/bin\0DLX_TEST=one";
        assert!(config_divergence(&kept, Some("/bin/sleep 3600"), Some(asked_env)).is_empty());
        assert!(config_divergence(&kept, None, None).is_empty());

        let images: serde_json::Value = serde_json::json!({
            "entrypoint": "/bin/sh",
            "env": "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        });
        let d = config_divergence(&images, Some("/bin/sleep 3600"), Some(asked_env));
        assert_eq!(d.len(), 3, "{d:?}");
        assert!(d[0].starts_with("unprivileged"), "{d:?}");
        assert!(d[1].contains("/bin/sh"), "{d:?}");
    }
}
