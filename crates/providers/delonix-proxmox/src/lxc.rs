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
    NetworkState, SystemContainerConfig, SystemContainerHandle, SystemContainerNet,
    SystemContainerObservation, SystemContainerProvider, SystemContainerResources,
    SystemContainerSpec,
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

    /// Whether the node has container `vmid`. Only the node's own «does not
    /// exist» answers `false`; any other failure is an error — a transport
    /// error read as «gone» made a destroy report success over a container
    /// it never reached (measured, plan 63 slice 5).
    pub fn lxc_exists(&self, vmid: u32) -> Result<bool> {
        match self.lxc_config(vmid) {
            Ok(_) => Ok(true),
            Err(Error::NodeNotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// The volumes of container `vmid` on `storage`. A container's volumes
    /// are `rootdir` content, not `images`, so the VM listing never sees them.
    pub fn list_ct_volumes(&self, storage: &str, vmid: u32) -> Result<Vec<String>> {
        let body = self.get(&format!(
            "/nodes/{}/storage/{storage}/content?content=rootdir&vmid={vmid}",
            self.node
        ))?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "content")?;
        Ok(w.data
            .iter()
            .filter_map(|b| Some(b.get("volid")?.as_str()?.to_string()))
            .collect())
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
            Some(&|| Ok(!self.lxc_exists(vmid)?)),
        )
    }

    /// `GET /nodes/{node}/lxc/{vmid}/interfaces`: the container's interfaces
    /// as the node reads them from inside.
    pub fn lxc_interfaces(&self, vmid: u32) -> Result<serde_json::Value> {
        let body = self.get(&format!("/nodes/{}/lxc/{vmid}/interfaces", self.node))?;
        let w: Wrapped<serde_json::Value> = parse(&body, "container interfaces")?;
        Ok(w.data)
    }

    /// Waits until the container reads `running`, within the client's task
    /// timeout. A rollback with `start` ends its task before the node answers
    /// for the container again: measured on PVE 9.2.2, `status/current` hung
    /// for about 40 s right after it, and pveproxy answered HTTP 596 to a
    /// client that waited. A failed read here is «not yet», not an answer;
    /// only the deadline is.
    pub fn lxc_wait_running(&self, vmid: u32) -> Result<()> {
        let deadline = std::time::Instant::now() + self.task_timeout;
        loop {
            let last = match self.lxc_status(vmid) {
                Ok(s) if s == "running" => return Ok(()),
                Ok(s) => format!("status {s}"),
                Err(e) => e.to_string(),
            };
            if std::time::Instant::now() >= deadline {
                return Err(Error::TaskTimeout(format!(
                    "proxmox: container {vmid} did not read running within {} s after the \
                     rollback (last: {last})",
                    self.task_timeout.as_secs()
                )));
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    /// `PUT …/lxc/{vmid}/resize` with `disk=rootfs`: grows the root volume
    /// to `gib`. The node forks `resize` (the QEMU worker's name) and refuses
    /// a shrink; the caller refuses it first, by name.
    pub fn lxc_grow_rootfs(&self, ledger: &Ledger, vmid: u32, gib: u32) -> Result<()> {
        let size = format!("{gib}G");
        self.task_or_done(
            ledger,
            vmid,
            TaskKind::Resize,
            || {
                self.put_form(
                    &format!("/nodes/{}/lxc/{vmid}/resize", self.node),
                    &[("disk", "rootfs"), ("size", size.as_str())],
                )
            },
            Some(&|| Ok(config_of(&self.lxc_config(vmid)?).rootfs_gib >= gib)),
        )
    }

    /// `POST /nodes/{node}/vzdump` for one container: `snapshot` mode archives
    /// a running container without stopping it, `stop` stops it for the
    /// length of the archive and starts it again. The probe is a new archive
    /// of this container in `storage`.
    pub fn lxc_backup(&self, ledger: &Ledger, vmid: u32, storage: &str, stop: bool) -> Result<()> {
        let before = self.list_backups(storage, vmid)?.len();
        let id = vmid.to_string();
        let mode = if stop { "stop" } else { "snapshot" };
        self.task(
            ledger,
            vmid,
            TaskKind::Backup,
            || {
                self.post_form(
                    &format!("/nodes/{}/vzdump", self.node),
                    &[
                        ("vmid", id.as_str()),
                        ("storage", storage),
                        ("mode", mode),
                        ("remove", "0"),
                    ],
                    true,
                )
            },
            Some(&|| Ok(self.list_backups(storage, vmid)?.len() > before)),
        )
    }

    /// `POST /nodes/{node}/lxc` with `restore=1` and `force=1`: puts container
    /// `vmid` back from `archive`, over the container of the same id, with its
    /// root volume on `rootfs_storage`. The container must be stopped. No
    /// probe: nothing reads «restored» apart from «not restored».
    pub fn lxc_restore(
        &self,
        ledger: &Ledger,
        vmid: u32,
        archive: &str,
        rootfs_storage: &str,
    ) -> Result<()> {
        let id = vmid.to_string();
        self.task(
            ledger,
            vmid,
            TaskKind::CtRestore,
            || {
                self.post_form(
                    &format!("/nodes/{}/lxc", self.node),
                    &[
                        ("vmid", id.as_str()),
                        ("ostemplate", archive),
                        ("restore", "1"),
                        ("force", "1"),
                        ("storage", rootfs_storage),
                    ],
                    true,
                )
            },
            None,
        )
    }

    /// `POST /nodes/{node}/lxc/{vmid}/clone`: a full copy of container `vmid`
    /// as `newid`, its volumes on `storage`, named `hostname`. The node gives
    /// the copy a new MAC and refuses a full copy of a running container
    /// unless it is taken from `snapname`. The probe is the copy's config.
    pub fn lxc_clone(
        &self,
        ledger: &Ledger,
        vmid: u32,
        newid: u32,
        hostname: &str,
        storage: &str,
        snapname: Option<&str>,
    ) -> Result<()> {
        let new = newid.to_string();
        let mut form: Vec<(&str, &str)> = vec![
            ("newid", new.as_str()),
            ("hostname", hostname),
            ("full", "1"),
            ("storage", storage),
        ];
        if let Some(s) = snapname {
            form.push(("snapname", s));
        }
        self.task(
            ledger,
            vmid,
            TaskKind::CtClone,
            || {
                self.post_form(
                    &format!("/nodes/{}/lxc/{vmid}/clone", self.node),
                    &form,
                    true,
                )
            },
            Some(&|| Ok(self.lxc_config(newid).is_ok())),
        )
    }

    /// `GET …/lxc/{vmid}/snapshot`: the snapshot names, without the API's
    /// `current` pseudo-entry (the live state, not a snapshot anybody took).
    pub fn lxc_snapshots(&self, vmid: u32) -> Result<Vec<String>> {
        let body = self.get(&format!("/nodes/{}/lxc/{vmid}/snapshot", self.node))?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "container snapshots")?;
        Ok(snapshot_names(&w.data))
    }

    /// `POST …/lxc/{vmid}/snapshot`. A name already taken is a CONFLICT,
    /// asked first: the node says so only inside a failed task.
    pub fn lxc_snapshot(&self, ledger: &Ledger, vmid: u32, name: &str) -> Result<()> {
        if self.lxc_snapshots(vmid)?.iter().any(|s| s == name) {
            return Err(taken_ct_snapshot(vmid, name));
        }
        self.task(
            ledger,
            vmid,
            TaskKind::CtSnapshot,
            || {
                self.post_form(
                    &format!("/nodes/{}/lxc/{vmid}/snapshot", self.node),
                    &[("snapname", name)],
                    true,
                )
            },
            Some(&|| Ok(self.lxc_snapshots(vmid)?.iter().any(|s| s == name))),
        )
        .map_err(|e| {
            if e.to_string().contains("already") {
                taken_ct_snapshot(vmid, name)
            } else {
                e
            }
        })
    }

    /// `POST …/lxc/{vmid}/snapshot/{snapname}/rollback`. The node stops a
    /// running container to roll it back and leaves it stopped unless
    /// `start` is sent; `start` says whether to send it. A name the
    /// container does not have is NOT FOUND.
    pub fn lxc_rollback(&self, ledger: &Ledger, vmid: u32, name: &str, start: bool) -> Result<()> {
        if !self.lxc_snapshots(vmid)?.iter().any(|s| s == name) {
            return Err(Error::SnapshotNotFound(format!(
                "snapshot of Proxmox container {vmid}: {name}"
            )));
        }
        let form: &[(&str, &str)] = if start { &[("start", "1")] } else { &[] };
        let snapname = name;
        // No probe: nothing a read shows tells «rolled back» from «not».
        self.task(
            ledger,
            vmid,
            TaskKind::CtRollback,
            || {
                self.post_form(
                    &format!(
                        "/nodes/{}/lxc/{vmid}/snapshot/{snapname}/rollback",
                        self.node
                    ),
                    form,
                    true,
                )
            },
            None,
        )
    }

    /// `DELETE …/lxc/{vmid}/snapshot/{snapname}`; a name the container does
    /// not have is NOT FOUND.
    pub fn lxc_delete_snapshot(&self, ledger: &Ledger, vmid: u32, name: &str) -> Result<()> {
        if !self.lxc_snapshots(vmid)?.iter().any(|s| s == name) {
            return Err(Error::SnapshotNotFound(format!(
                "snapshot of Proxmox container {vmid}: {name}"
            )));
        }
        let snapname = name;
        let path = format!("/nodes/{}/lxc/{vmid}/snapshot/{snapname}", self.node);
        self.task(
            ledger,
            vmid,
            TaskKind::CtDeleteSnapshot,
            || self.delete(&path),
            Some(&|| Ok(!self.lxc_snapshots(vmid)?.iter().any(|s| s == name))),
        )
    }
}

/// The node's `entrypoint` line for `args`. The node splits it on spaces, so
/// an argument holding whitespace (or a control character) is refused: it
/// would run as something else than written.
/// The names of a snapshot listing, without the `current` pseudo-entry.
pub(crate) fn snapshot_names(list: &[serde_json::Value]) -> Vec<String> {
    list.iter()
        .filter_map(|s| s.get("name").and_then(|n| n.as_str()))
        .filter(|n| *n != "current")
        .map(str::to_string)
        .collect()
}

/// A snapshot name the container already has — `Conflict` (exit 5).
fn taken_ct_snapshot(vmid: u32, name: &str) -> Error {
    Error::SnapshotTaken(format!(
        "Proxmox container {vmid} already has a snapshot named '{name}'"
    ))
}

/// The container id an archive name carries (`…/vzdump-lxc-<vmid>-<date>.…`),
/// or `None` for anything that is not a container archive.
pub(crate) fn archive_vmid(archive: &str) -> Option<u32> {
    let file = archive.rsplit('/').next()?;
    let rest = file.strip_prefix("vzdump-lxc-")?;
    rest.split('-').next()?.parse().ok()
}

/// The storage of an archive that belongs to container `vmid`; another
/// container's archive, or a name that is not an archive, is refused before
/// any request.
pub(crate) fn own_archive(archive: &str, vmid: u32) -> Result<&str> {
    let (storage, _) = archive.split_once(':').ok_or_else(|| {
        Error::InvalidSystemContainer(format!(
            "proxmox: '{archive}' is not a node archive id (<storage>:backup/vzdump-lxc-…)"
        ))
    })?;
    if !crate::valid_storage_id(storage) {
        return Err(Error::InvalidSystemContainer(format!(
            "proxmox: '{storage}' is not a storage id"
        )));
    }
    match archive_vmid(archive) {
        Some(v) if v == vmid => Ok(storage),
        Some(v) => Err(Error::InvalidSystemContainer(format!(
            "proxmox: '{archive}' is an archive of container {v}, not of {vmid}"
        ))),
        None => Err(Error::InvalidSystemContainer(format!(
            "proxmox: '{archive}' is not a container archive (vzdump-lxc-…)"
        ))),
    }
}

/// The raw `lxc.*` keys of a container config, as the node answers them
/// (`"lxc": [["lxc.init.cwd", "/"], …]`).
pub(crate) fn raw_lxc_keys(config: &serde_json::Value) -> Vec<(String, String)> {
    config
        .get("lxc")
        .and_then(|l| l.as_array())
        .map(|l| {
            l.iter()
                .filter_map(|kv| {
                    let kv = kv.as_array()?;
                    Some((
                        kv.first()?.as_str()?.to_string(),
                        kv.get(1)?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn missing_archive(vmid: u32, archive: &str) -> Error {
    Error::NodeNotFound(format!("archive {archive} of container {vmid}"))
}

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

/// The container's configuration as the node keeps it: `memory`/`swap`/
/// `cores` as numbers (absent `swap`/`cores` are the node's own defaults, 512
/// and all the node's CPUs, read as 0 here: nobody declared them), the
/// entrypoint split back on the spaces it was joined with (an argument with a
/// space is refused on the way in, so the split is exact), and the env split
/// on NUL then on the first `=`. Pure.
pub(crate) fn config_of(config: &serde_json::Value) -> SystemContainerConfig {
    let number = |k: &str| {
        config
            .get(k)
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(0) as u32
    };
    let entrypoint = config
        .get("entrypoint")
        .and_then(|v| v.as_str())
        .map(|l| {
            l.split(' ')
                .filter(|a| !a.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let env = config
        .get("env")
        .and_then(|v| v.as_str())
        .map(|e| {
            e.split('\0')
                .filter(|p| !p.is_empty())
                .map(|p| match p.split_once('=') {
                    Some((k, v)) => (k.to_string(), v.to_string()),
                    None => (p.to_string(), String::new()),
                })
                .collect()
        })
        .unwrap_or_default();
    SystemContainerConfig {
        memory_mib: number("memory"),
        swap_mib: number("swap"),
        cores: number("cores"),
        entrypoint,
        env,
        rootfs_gib: config
            .get("rootfs")
            .and_then(|v| v.as_str())
            .and_then(rootfs_size_gib)
            .unwrap_or(0),
    }
}

/// The `size=` of a `rootfs` value (`local-lvm:vm-100-disk-0,size=2G`), in
/// GiB. The node writes `G` for what the engine asks, and may write `M` or
/// `T`; a size that is not a whole number of GiB answers `None`, never a
/// rounded number that would read as drift or as none.
pub(crate) fn rootfs_size_gib(value: &str) -> Option<u32> {
    let size = value.split(',').find_map(|p| p.strip_prefix("size="))?;
    let (digits, unit) = size.split_at(size.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = digits.parse().ok()?;
    let gib = match unit {
        "T" => n.checked_mul(1024)?,
        "G" => n,
        "M" if n.is_multiple_of(1024) => n / 1024,
        "K" if n.is_multiple_of(1024 * 1024) => n / (1024 * 1024),
        _ => return None,
    };
    u32::try_from(gib).ok()
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
        if !client.lxc_exists(vmid)? {
            return Ok(());
        }
        if client.lxc_status(vmid)? == "running" {
            client.lxc_stop(&ledger, vmid)?;
        }
        client.lxc_destroy(&ledger, vmid)?;
        Ok(())
    }

    fn configuration(
        &self,
        _dir: &Path,
        h: &SystemContainerHandle,
    ) -> delonix_model::Result<Option<SystemContainerConfig>> {
        let (node, vmid) = parse_locator(&h.locator)?;
        let client = self.client_for(&node)?;
        match client.lxc_config(vmid) {
            Ok(c) => Ok(Some(config_of(&c))),
            Err(Error::NodeNotFound(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn resize(
        &self,
        dir: &Path,
        h: &SystemContainerHandle,
        r: SystemContainerResources,
    ) -> delonix_model::Result<()> {
        if r.memory_mib == 0 || r.cores == 0 {
            return Err(Error::InvalidSystemContainer(
                "proxmox: memory and cores must be above zero".to_string(),
            )
            .into());
        }
        let (node, vmid) = parse_locator(&h.locator)?;
        let client = self.client_for(&node)?;
        let ledger = Ledger::at(dir);
        let (memory, swap, cores) = (
            r.memory_mib.to_string(),
            r.swap_mib.to_string(),
            r.cores.to_string(),
        );
        client.lxc_set_config(
            &ledger,
            vmid,
            &[
                ("memory", memory.as_str()),
                ("swap", swap.as_str()),
                ("cores", cores.as_str()),
            ],
        )?;
        let kept = config_of(&client.lxc_config(vmid)?);
        if (kept.memory_mib, kept.swap_mib, kept.cores) != (r.memory_mib, r.swap_mib, r.cores) {
            return Err(Error::UnexpectedAnswer(format!(
                "proxmox: container {vmid} was asked for memory {} MiB, swap {} MiB, {} core(s) \
                 and the node kept {} MiB, {} MiB, {}",
                r.memory_mib, r.swap_mib, r.cores, kept.memory_mib, kept.swap_mib, kept.cores
            ))
            .into());
        }
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

    fn snapshot(
        &self,
        dir: &Path,
        h: &SystemContainerHandle,
        name: &str,
    ) -> delonix_model::Result<()> {
        crate::validate_snapshot_name(name)?;
        let (node, vmid) = parse_locator(&h.locator)?;
        let client = self.client_for(&node)?;
        let ledger = Ledger::at(dir);
        client.settle_pending(&ledger, vmid)?;
        Ok(client.lxc_snapshot(&ledger, vmid, name)?)
    }

    fn snapshots(
        &self,
        _dir: &Path,
        h: &SystemContainerHandle,
    ) -> delonix_model::Result<Vec<String>> {
        let (node, vmid) = parse_locator(&h.locator)?;
        let client = self.client_for(&node)?;
        Ok(client.lxc_snapshots(vmid)?)
    }

    fn delete_snapshot(
        &self,
        dir: &Path,
        h: &SystemContainerHandle,
        name: &str,
    ) -> delonix_model::Result<()> {
        crate::validate_snapshot_name(name)?;
        let (node, vmid) = parse_locator(&h.locator)?;
        let client = self.client_for(&node)?;
        let ledger = Ledger::at(dir);
        client.settle_pending(&ledger, vmid)?;
        Ok(client.lxc_delete_snapshot(&ledger, vmid, name)?)
    }

    fn grow_rootfs(
        &self,
        dir: &Path,
        h: &SystemContainerHandle,
        gib: u32,
    ) -> delonix_model::Result<()> {
        let (node, vmid) = parse_locator(&h.locator)?;
        let client = self.client_for(&node)?;
        let ledger = Ledger::at(dir);
        client.settle_pending(&ledger, vmid)?;
        let now = config_of(&client.lxc_config(vmid)?).rootfs_gib;
        if gib < now {
            return Err(Error::InvalidSystemContainer(format!(
                "proxmox: container {vmid}'s root volume is {now} GiB and cannot shrink to {gib} \
                 GiB — the node only grows it; a smaller one means recreating the container"
            ))
            .into());
        }
        if gib == now {
            return Ok(());
        }
        client.lxc_grow_rootfs(&ledger, vmid, gib)?;
        let kept = config_of(&client.lxc_config(vmid)?).rootfs_gib;
        if kept != gib {
            return Err(Error::UnexpectedAnswer(format!(
                "proxmox: container {vmid}'s root volume was grown to {gib} GiB and the node \
                 reports {kept} GiB"
            ))
            .into());
        }
        Ok(())
    }

    fn backup(
        &self,
        dir: &Path,
        h: &SystemContainerHandle,
        storage: &str,
        stop: bool,
    ) -> delonix_model::Result<String> {
        crate::valid_storage_id_or_err(storage)?;
        let (node, vmid) = parse_locator(&h.locator)?;
        let client = self.client_for(&node)?;
        let ledger = Ledger::at(dir);
        client.settle_pending(&ledger, vmid)?;
        let before: Vec<String> = client
            .list_backups(storage, vmid)?
            .into_iter()
            .map(|(v, _)| v)
            .collect();
        client.lxc_backup(&ledger, vmid, storage, stop)?;
        let after = client.list_backups(storage, vmid)?;
        after
            .into_iter()
            .map(|(v, _)| v)
            .find(|v| !before.contains(v))
            .ok_or_else(|| {
                Error::UnexpectedAnswer(format!(
                    "proxmox: the backup of container {vmid} ended and {storage} lists no new archive of it"
                ))
                .into()
            })
    }

    fn backups(
        &self,
        _dir: &Path,
        h: &SystemContainerHandle,
        storage: &str,
    ) -> delonix_model::Result<Vec<(String, u64)>> {
        crate::valid_storage_id_or_err(storage)?;
        let (node, vmid) = parse_locator(&h.locator)?;
        let mut all = self.client_for(&node)?.list_backups(storage, vmid)?;
        all.retain(|(v, _)| archive_vmid(v) == Some(vmid));
        all.sort();
        Ok(all)
    }

    fn delete_backup(
        &self,
        dir: &Path,
        h: &SystemContainerHandle,
        archive: &str,
    ) -> delonix_model::Result<()> {
        let (node, vmid) = parse_locator(&h.locator)?;
        let storage = own_archive(archive, vmid)?;
        let client = self.client_for(&node)?;
        if !client
            .list_backups(storage, vmid)?
            .iter()
            .any(|(v, _)| v == archive)
        {
            return Err(missing_archive(vmid, archive).into());
        }
        client.delete_backup(&Ledger::at(dir), vmid, storage, archive)?;
        Ok(())
    }

    fn clone_as(
        &self,
        dir: &Path,
        h: &SystemContainerHandle,
        new_name: &str,
        snapshot: Option<&str>,
    ) -> delonix_model::Result<SystemContainerHandle> {
        if !valid_hostname(new_name) {
            return Err(Error::InvalidSystemContainer(format!(
                "proxmox: '{new_name}' is not a usable container hostname"
            ))
            .into());
        }
        if let Some(s) = snapshot {
            crate::validate_snapshot_name(s)?;
        }
        let (node, vmid) = parse_locator(&h.locator)?;
        let client = self.client_for(&node)?;
        let ledger = Ledger::at(dir);
        client.settle_pending(&ledger, vmid)?;
        // Read in PVE/API2/LXC.pm: a full copy of a RUNNING container is
        // refused unless it comes from a snapshot. Without one named, a
        // temporary snapshot is taken and always deleted afterwards — the full
        // copy does not depend on it.
        let running = client.lxc_status(vmid)? == "running";
        let temp = (running && snapshot.is_none()).then(|| {
            format!(
                "dlxclone{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            )
        });
        if let Some(t) = &temp {
            client.lxc_snapshot(&ledger, vmid, t)?;
        }
        let from = snapshot.or(temp.as_deref());
        let cloned = client.next_vmid().and_then(|newid| {
            client
                .lxc_clone(&ledger, vmid, newid, new_name, &self.rootfs_storage, from)
                .map(|()| newid)
        });
        if let Some(t) = &temp {
            if let Err(e) = client.lxc_delete_snapshot(&ledger, vmid, t) {
                tracing::warn!(vmid, snapshot = %t, error = %e, "proxmox: could not delete the temporary clone snapshot");
            }
        }
        let newid = cloned?;
        Ok(SystemContainerHandle {
            name: new_name.to_string(),
            locator: format!("proxmox:{}:{newid}", client.node),
        })
    }

    fn restore_backup(
        &self,
        dir: &Path,
        h: &SystemContainerHandle,
        archive: &str,
    ) -> delonix_model::Result<Vec<String>> {
        let (node, vmid) = parse_locator(&h.locator)?;
        let storage = own_archive(archive, vmid)?;
        let client = self.client_for(&node)?;
        if !client
            .list_backups(storage, vmid)?
            .iter()
            .any(|(v, _)| v == archive)
        {
            return Err(missing_archive(vmid, archive).into());
        }
        // Measured on PVE 9.2.2: a restore by anyone but root@pam drops the
        // raw `lxc.*` keys (the working directory and halt signal the node
        // wrote from the image) with a warning, and only root@pam may write
        // them back. They are read before and after, so what was lost is
        // named to the caller instead of left in a task log.
        let before = raw_lxc_keys(&client.lxc_config(vmid)?);
        let ledger = Ledger::at(dir);
        client.settle_pending(&ledger, vmid)?;
        // The node overwrites a container only when it is stopped; the state
        // before the call is put back afterwards, as a snapshot restore does.
        let was_running = client.lxc_status(vmid)? == "running";
        if was_running {
            client.lxc_stop(&ledger, vmid)?;
        }
        client.lxc_restore(&ledger, vmid, archive, &self.rootfs_storage)?;
        let after = raw_lxc_keys(&client.lxc_config(vmid)?);
        if was_running {
            client.lxc_start(&ledger, vmid)?;
        }
        Ok(before
            .into_iter()
            .filter(|kv| !after.contains(kv))
            .map(|(k, v)| format!("{k}: {v}"))
            .collect())
    }

    fn restore(
        &self,
        dir: &Path,
        h: &SystemContainerHandle,
        name: &str,
    ) -> delonix_model::Result<()> {
        crate::validate_snapshot_name(name)?;
        let (node, vmid) = parse_locator(&h.locator)?;
        let client = self.client_for(&node)?;
        let ledger = Ledger::at(dir);
        client.settle_pending(&ledger, vmid)?;
        // The node stops a running container to roll it back and leaves it
        // stopped; `start` brings it back to where it was.
        let was_running = client.lxc_status(vmid)? == "running";
        client.lxc_rollback(&ledger, vmid, name, was_running)?;
        if was_running {
            client.lxc_wait_running(vmid)?;
        }
        Ok(())
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

    /// The config captured on PVE 9.2.2 after the configure and a resize.
    #[test]
    fn a_config_reads_back_into_its_fields() {
        let c: serde_json::Value = serde_json::json!({
            "memory": 384, "swap": 128, "cores": 2,
            "entrypoint": "/bin/sleep 3600",
            "env": "PATH=/usr/bin:/bin\u{0}DLX_TEST=one two\u{0}EMPTY=",
            "unprivileged": 1,
        });
        let got = config_of(&c);
        assert_eq!((got.memory_mib, got.swap_mib, got.cores), (384, 128, 2));
        assert_eq!(got.entrypoint, vec!["/bin/sleep", "3600"]);
        assert_eq!(
            got.env,
            vec![
                ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                ("DLX_TEST".to_string(), "one two".to_string()),
                ("EMPTY".to_string(), String::new()),
            ]
        );
        assert_eq!(got.rootfs_gib, 0, "no rootfs in this config");
        let sized = config_of(&serde_json::json!({"rootfs": "local-lvm:vm-100-disk-0,size=2G"}));
        assert_eq!(sized.rootfs_gib, 2);
        assert_eq!(rootfs_size_gib("local-lvm:vm-1-disk-0,size=3072M"), Some(3));
        assert_eq!(rootfs_size_gib("local-lvm:vm-1-disk-0,size=1T"), Some(1024));
        assert_eq!(rootfs_size_gib("local-lvm:vm-1-disk-0,size=1500M"), None);
        assert_eq!(rootfs_size_gib("local-lvm:vm-1-disk-0"), None);
        // A container created without swap/cores/env keeps the node's defaults.
        let bare = config_of(&serde_json::json!({"memory": 256}));
        assert_eq!((bare.swap_mib, bare.cores), (0, 0));
        assert!(bare.entrypoint.is_empty() && bare.env.is_empty());
    }

    #[test]
    fn the_raw_lxc_keys_are_read_from_the_config() {
        let c = serde_json::json!({"lxc": [["lxc.init.cwd", "/"], ["lxc.signal.halt", "SIGTERM"]]});
        assert_eq!(
            raw_lxc_keys(&c),
            vec![
                ("lxc.init.cwd".to_string(), "/".to_string()),
                ("lxc.signal.halt".to_string(), "SIGTERM".to_string())
            ]
        );
        assert!(raw_lxc_keys(&serde_json::json!({"memory": 256})).is_empty());
    }

    #[test]
    fn an_archive_is_owned_by_the_container_its_name_carries() {
        let a = "local:backup/vzdump-lxc-100-2026_09_29-09_00_00.tar.zst";
        assert_eq!(archive_vmid(a), Some(100));
        assert_eq!(own_archive(a, 100).unwrap(), "local");
        assert!(own_archive(a, 101).is_err(), "another container's archive");
        assert!(own_archive("local:backup/vzdump-qemu-100-x.vma.zst", 100).is_err());
        assert!(
            own_archive("vzdump-lxc-100-x.tar", 100).is_err(),
            "no storage"
        );
        assert!(own_archive("../x:backup/vzdump-lxc-100-x.tar", 100).is_err());
        assert_eq!(
            archive_vmid("local:backup/vzdump-lxc-1000-x.tar"),
            Some(1000)
        );
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
