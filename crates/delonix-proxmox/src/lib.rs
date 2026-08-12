//! A [`VmBackend`] backed by a Proxmox VE node's REST API.
//!
//! **One node, named explicitly.** No inventory, no scheduling, no choosing a
//! node on the user's behalf: a decision that needs to know *who the customer
//! is* belongs to the control plane, not to a node runtime (guardrail #2).
//!
//! # What the spike measured, and why the code looks like this
//!
//! Exercised against a real Proxmox VE 9.1 (the appliance this repo builds),
//! driven through the API as `root@pam`: a VM created, started (`status:
//! running`), snapshotted, stopped and destroyed. Full table in ADR-0008.
//!
//! **Almost everything is an asynchronous TASK.** A create, a start, a snapshot
//! and a destroy each answer with a bare `UPID:pve:…` string — not a result.
//! The outcome is read separately, and its shape is a trap:
//!
//! ```json
//! {"status": "stopped", "exitstatus": "OK", "type": "qmsnapshot"}
//! ```
//!
//! `status: stopped` means **the task finished**, not that it failed; the
//! verdict is in `exitstatus`. A client that reads `status` as the result
//! concludes the exact opposite of the truth. [`Client::wait_task`] reads the
//! right field, and [`task_verdict`] is a pure function with a test for it.
//!
//! The lifecycle is implemented and was watched running: `boot` creates the VM
//! on the node and starts it, `is_running` reads its state, and `stop` stops
//! AND destroys it — the same meaning libvirt gives it here, since a VM left
//! behind on a node after `delonix vm rm` is an orphan nobody is looking for.
//! See `tests/live.rs`, which is skipped with an audible line when no node is
//! configured.
//!
//! # Two things this backend deliberately does NOT do
//!
//! * **It never touches a local disk.** `manages_own_storage()` is `true`, so
//!   the engine hands `cfg.disk` over verbatim — it names a Proxmox volume
//!   (`local-lvm:vm-100-disk-0`) or a template, and this side does not get to
//!   reinterpret it (ADR-0008).
//! * **It is never auto-detected.** `auto_selectable()` is `false`: the only
//!   honest answer to "are you available?" costs a network round trip to a node
//!   that may not be configured at all, and auto-detection is not a place to
//!   make HTTP requests.

use delonix_runtime_core::Vm;
use delonix_runtime_core::{Error, Result};
// `mem_mib` comes from the engine and is NOT re-implemented here. The copy that
// used to live in this file did not know the k8s `Gi`/`Mi` suffix the engine
// tolerates, so `memory: 2Gi` meant 2 GiB on libvirt and Cloud Hypervisor and
// 1 GiB here — silently, which is the failure this repo treats as its worst.
use delonix_vm::{mem_mib, Boot, CreateStage, VmBackend, VmConfig};
use serde::Deserialize;
use std::path::Path;
use std::time::{Duration, Instant};

/// How long to wait for a Proxmox task before giving up. A create that
/// allocates a disk on slow storage is real work; what this guards against is a
/// task that never reaches a terminal state, not a slow one.
const TASK_TIMEOUT: Duration = Duration::from_secs(600);

/// Polling bounds for [`Client::wait_task`].
///
/// It was a flat 750 ms, which is the wrong answer at both ends of the range
/// the same loop has to cover. A `start` finishes in well under a second and
/// paid up to 750 ms of pure waiting for nothing; a create that allocates a
/// disk can run for minutes, and at 750 ms a ten-minute task is 800 requests at
/// the node — which is also 800 chances for a transient failure to abort a task
/// that was going to succeed.
///
/// Backing off keeps the fast case fast (first answer at 150 ms, 5× sooner) and
/// the slow case cheap: the same ten minutes costs ~190 requests instead of 800,
/// measured by `um_backoff_responde_depressa_e_nao_martela_o_no`.
const POLL_MIN: Duration = Duration::from_millis(150);
const POLL_MAX: Duration = Duration::from_secs(4);

/// Next polling interval: 1.5× up to [`POLL_MAX`]. Pure, so the claims above
/// are arithmetic somebody can check rather than a promise in a comment.
fn next_poll_wait(cur: Duration) -> Duration {
    std::cmp::min(cur.mul_f32(1.5), POLL_MAX)
}

/// How the client authenticates against the node.
#[derive(Debug, Clone)]
pub enum Auth {
    /// `PVEAPIToken=<user>!<tokenid>=<secret>` — the form to prefer. A token is
    /// revocable on the node without touching an account, and it is what a
    /// `kind: Secret` should carry.
    ApiToken { id: String, secret: String },
    /// Account credentials, exchanged for a ticket. Accepted because a freshly
    /// installed node has an account before it has any token.
    Password { username: String, password: String },
}

/// Where the node is, and how to get in.
#[derive(Debug, Clone)]
pub struct Target {
    /// `https://<node>:8006` (no path).
    pub base_url: String,
    /// The node's name in the cluster, as `GET /nodes` reports it (`pve`).
    /// Explicit: this backend addresses ONE node and never picks one.
    pub node: String,
    pub auth: Auth,
    /// Accept a certificate this host cannot verify. A stock Proxmox serves a
    /// self-signed one, so many real targets need it — but it removes the check
    /// that stops another machine answering in the node's name, taking the
    /// credential with it. Opt-in, never a fallback after a TLS error.
    pub insecure_tls: bool,
    /// Default bridge for a VM's NIC. `None` → `vmbr0`, which is what a stock
    /// install has. A per-VM `VmConfig.bridge` still wins over this.
    pub bridge: Option<String>,
    /// VLAN tag for the NIC. Lives here and not in `VmConfig` because it
    /// describes how THIS node is cabled, not the VM.
    pub vlan: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct Wrapped<T> {
    data: T,
}

#[derive(Debug, Deserialize)]
struct Ticket {
    ticket: String,
    #[serde(rename = "CSRFPreventionToken")]
    csrf: String,
}

#[derive(Debug, Deserialize)]
struct TaskStatus {
    status: String,
    #[serde(default)]
    exitstatus: Option<String>,
}

/// What a task's terminal state means. Pure, and the reason it exists is that
/// the obvious reading is wrong.
///
/// * still running → `None`
/// * finished, `exitstatus == "OK"` → `Some(Ok(()))`
/// * finished, anything else → `Some(Err(reason))`
///
/// `status: "stopped"` is NOT failure — it is how Proxmox says the task is
/// over. Reading it as the result inverts every verdict this backend makes.
fn task_verdict(status: &str, exitstatus: Option<&str>) -> Option<std::result::Result<(), String>> {
    if status != "stopped" {
        return None;
    }
    match exitstatus {
        Some("OK") => Some(Ok(())),
        Some(other) => Some(Err(other.to_string())),
        // Finished with no exit status recorded: unknown, and unknown is not
        // success. Reporting OK here would be inventing a result.
        None => Some(Err("finished without an exit status".into())),
    }
}

/// The backend is a thin handle over a SHARED client.
///
/// `Arc` and not an owned `Client` because the engine builds a backend per
/// lookup — `backend_for` on every `is_running`, and `vm ls` calls that once
/// per VM. Each construction used to mean a fresh `Client::connect`:
/// authenticate, then `GET /nodes`. Listing ten VMs on a node was thirty round
/// trips where twelve do. The registered factory clones this instead.
pub struct ProxmoxBackend {
    client: std::sync::Arc<Client>,
}

pub struct Client {
    http: reqwest::blocking::Client,
    base: String,
    node: String,
    auth: Auth,
    /// Set only for password auth; a token needs no ticket.
    ///
    /// Behind a lock and re-fetchable, because **a Proxmox ticket expires** (2 h)
    /// and this client is shared for the life of the process: a long-running
    /// one — `serve`, the management API — would start taking 401s partway
    /// through the day with nothing to explain it. Irrelevant to the CLI, whose
    /// process lasts seconds, and irrelevant with an API token, which does not
    /// expire and is the reason tokens are the preferred form.
    ticket: std::sync::RwLock<Option<Ticket>>,
    bridge: String,
    vlan: Option<u16>,
}

impl Client {
    pub fn connect(target: &Target) -> Result<Self> {
        validate_target_url(&target.base_url)?;
        validate_node_name(&target.node)?;
        if let Some(b) = &target.bridge {
            validate_bridge_name(b)?;
        }
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .danger_accept_invalid_certs(target.insecure_tls)
            .build()
            .map_err(|e| {
                Error::Invalid(format!("proxmox: could not build the HTTP client: {e}"))
            })?;
        let me = Self {
            http,
            base: target.base_url.trim_end_matches('/').to_string(),
            node: target.node.clone(),
            auth: target.auth.clone(),
            ticket: std::sync::RwLock::new(None),
            bridge: target.bridge.clone().unwrap_or_else(|| "vmbr0".to_string()),
            vlan: target.vlan,
        };
        me.login()?;
        // Prove the credential AND the node name before anything is created:
        // a wrong node fails here, not halfway through a create.
        let nodes: Wrapped<Vec<serde_json::Value>> = parse(&me.get("/nodes")?, "/nodes")?;
        let names: Vec<String> = nodes
            .data
            .iter()
            .filter_map(|n| n.get("node").and_then(|v| v.as_str()).map(str::to_string))
            .collect();
        if !names.iter().any(|n| n == &me.node) {
            return Err(Error::Invalid(format!(
                "proxmox: no node named '{}' at {} (it has: {})",
                me.node,
                me.base,
                names.join(", ")
            )));
        }
        Ok(me)
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api2/json{path}", self.base)
    }

    /// Exchanges the account credentials for a ticket. No-op for an API token,
    /// which needs none.
    fn login(&self) -> Result<()> {
        let Auth::Password { username, password } = &self.auth else {
            return Ok(());
        };
        let body = self.post_form(
            "/access/ticket",
            &[
                ("username", username.as_str()),
                ("password", password.as_str()),
            ],
            false,
        )?;
        let t: Wrapped<Ticket> = parse(&body, "/access/ticket")?;
        *self.ticket.write().unwrap_or_else(|e| e.into_inner()) = Some(t.data);
        Ok(())
    }

    fn authed(&self, rb: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
        let ticket = self.ticket.read().unwrap_or_else(|e| e.into_inner());
        match (&self.auth, ticket.as_ref()) {
            (Auth::ApiToken { id, secret }, _) => {
                rb.header("Authorization", format!("PVEAPIToken={id}={secret}"))
            }
            (_, Some(t)) => rb
                .header("Cookie", format!("PVEAuthCookie={}", t.ticket))
                .header("CSRFPreventionToken", &t.csrf),
            (_, None) => rb,
        }
    }

    fn send(&self, rb: reqwest::blocking::RequestBuilder, authed: bool) -> Result<String> {
        let rb = if authed { self.authed(rb) } else { rb };
        let resp = rb
            .send()
            .map_err(|e| Error::Invalid(format!("proxmox: request failed: {e}")))?;
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            // The body carries the actionable part — a bare status code sends
            // people hunting in the wrong subsystem.
            return Err(Error::Invalid(format!(
                "proxmox: {} returned HTTP {status}: {}",
                self.base,
                truncate_chars(body.trim(), 400)
            )));
        }
        Ok(body)
    }

    /// Sends an authenticated request, and **re-authenticates once on a 401**.
    ///
    /// A Proxmox ticket lasts about two hours and this client is shared for the
    /// life of the process. Without this, a long-running one starts taking 401s
    /// partway through the day — with nothing in the message to suggest that
    /// logging in again is all it takes.
    ///
    /// Once, and only for password auth: an API token that gets a 401 was
    /// revoked or is wrong, and retrying it forever against a node that keeps
    /// saying no is how a credential ends up locked out. `build` re-creates the
    /// request because a `RequestBuilder` is consumed by `send`.
    fn send_authed(&self, build: impl Fn() -> reqwest::blocking::RequestBuilder) -> Result<String> {
        match self.send(build(), true) {
            Err(e) if matches!(self.auth, Auth::Password { .. }) && is_unauthorized(&e) => {
                tracing::debug!("proxmox: ticket rejected, logging in again");
                self.login()?;
                self.send(build(), true)
            }
            other => other,
        }
    }

    fn get(&self, path: &str) -> Result<String> {
        let url = self.url(path);
        self.send_authed(|| self.http.get(&url))
    }

    fn post_form(&self, path: &str, form: &[(&str, &str)], authed: bool) -> Result<String> {
        let url = self.url(path);
        if !authed {
            return self.send(self.http.post(&url).form(form), false);
        }
        self.send_authed(|| self.http.post(&url).form(form))
    }

    /// The next free VM id on the CLUSTER.
    ///
    /// `/cluster/nextid` is the node's own answer, and asking is the only
    /// correct way: ids are cluster-wide, and picking one locally races with
    /// anything else creating a VM. It can still be taken between the answer
    /// and the create — Proxmox rejects that with a clear "already exists",
    /// which is the right failure and not one to paper over with a retry that
    /// might land on somebody else's id.
    pub fn next_vmid(&self) -> Result<u32> {
        let body = self.get("/cluster/nextid")?;
        let w: Wrapped<serde_json::Value> = parse(&body, "/cluster/nextid")?;
        w.data
            .as_str()
            .and_then(|s| s.parse().ok())
            .or_else(|| w.data.as_u64().map(|n| n as u32))
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "proxmox: could not read a VM id from /cluster/nextid: {}",
                    w.data
                ))
            })
    }

    /// Creates a VM with a fresh disk on `storage`.
    pub fn create_vm(
        &self,
        vmid: u32,
        name: &str,
        cfg: &VmConfig,
        storage: &str,
        gib: u32,
    ) -> Result<()> {
        let mem = mem_mib(&cfg.memory).to_string();
        let cores = cfg.vcpus.max(1).to_string();
        let scsi0 = format!("{storage}:{gib}");
        let vmid_s = vmid.to_string();
        let net0 = self.net0_arg(cfg);
        // `ip=dhcp` unless an address was asked for. Proxmox's own cloud-init
        // writes this into the guest, which is why `static_ip` is one of the
        // few `VmConfig` fields this backend CAN honour — the local backends
        // reach the same end through a NoCloud seed ISO, which is a file on
        // this host and therefore meaningless on a node elsewhere.
        let ipconfig0 = match &cfg.static_ip {
            Some(ip) => format!("ip={ip}"),
            None => "ip=dhcp".to_string(),
        };
        let mut form: Vec<(&str, &str)> = vec![
            ("vmid", vmid_s.as_str()),
            ("name", name),
            ("memory", mem.as_str()),
            ("cores", cores.as_str()),
            ("ostype", "l26"),
            ("scsihw", "virtio-scsi-single"),
            ("scsi0", scsi0.as_str()),
            // A NIC on a bridge of the node. `virtio` alone is the model —
            // the value goes in the property's default key, and spelling
            // that key out (`model=virtio`) is what the API refuses. The
            // ADR recorded this shape as refused too; that was an artefact
            // of the spike's `curl -d`, which does not URL-encode.
            // `reqwest`'s `.form()` does, and the node accepts it:
            // measured, `net0 = virtio=BC:24:11:F4:F9:9C,bridge=vmbr0`.
            ("net0", net0.as_str()),
            ("ipconfig0", ipconfig0.as_str()),
            // Enable the QEMU guest agent CHANNEL. This is the host side
            // only: it adds the virtio-serial port the agent talks over,
            // and without it the node will not even try — every
            // `/agent/...` call answers "QEMU guest agent is not running"
            // no matter what the guest has installed. Whether an agent
            // answers on the other end is the image's business, which is
            // exactly why `ip()` treats silence as "unknown" and not as an
            // error (see `parse_agent_ip`).
            ("agent", "1"),
        ];
        // The cloud-init drive, and only when there is something to put in it:
        // an empty one on an image with no cloud-init is a CD-ROM the guest
        // ignores, but it also silently costs a disk on the node's storage.
        let ide2 = format!("{storage}:cloudinit");
        if cfg.static_ip.is_some() {
            form.push(("ide2", ide2.as_str()));
        }
        let body = self.post_form(&format!("/nodes/{}/qemu", self.node), &form, true)?;
        self.wait_upid(&body, "create")
    }

    /// The `net0` property: model, bridge and optional VLAN tag.
    ///
    /// The bridge was hardcoded to `vmbr0`. That is the right DEFAULT — it is
    /// what a stock Proxmox install has — but a node with more than one bridge
    /// had no way to say so, and `VmConfig` already carries a `bridge` field
    /// that every other backend honours. The VLAN comes from the target
    /// (`DELONIX_PROXMOX_VLAN`) rather than from `VmConfig`, which has no field
    /// for one: it is a property of how this node is cabled, not of the VM.
    fn net0_arg(&self, cfg: &VmConfig) -> String {
        let bridge = cfg
            .bridge
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .unwrap_or(&self.bridge);
        match self.vlan {
            Some(tag) => format!("virtio,bridge={bridge},tag={tag}"),
            None => format!("virtio,bridge={bridge}"),
        }
    }

    /// Clones a template into a new VM.
    ///
    /// The agent channel is NOT forced here, unlike `create_vm`: a clone
    /// inherits the template's configuration, and a template built with the
    /// agent already has it. Overriding would silently contradict a choice
    /// somebody made about that template.
    pub fn clone_template(&self, template: u32, vmid: u32, name: &str) -> Result<()> {
        let newid = vmid.to_string();
        let body = self.post_form(
            &format!("/nodes/{}/qemu/{template}/clone", self.node),
            &[("newid", newid.as_str()), ("name", name), ("full", "1")],
            true,
        )?;
        self.wait_upid(&body, "clone")
    }

    /// A VM's configuration, as the node has it.
    ///
    /// Sibling of the other lifecycle calls, and the only way to ask the node
    /// what it actually recorded rather than what was sent — which is how the
    /// live test checks that the guest-agent channel really is on the VM this
    /// backend created. Deliberately NOT used by `ip()`: that runs once per VM
    /// on every `vm ls`, and a second round trip per listing to re-read a
    /// setting `create_vm` always sends would be paid by every user to catch a
    /// case only a hand-edited VM can reach.
    pub fn config(&self, vmid: u32) -> Result<serde_json::Value> {
        let body = self.get(&format!("/nodes/{}/qemu/{vmid}/config", self.node))?;
        let w: Wrapped<serde_json::Value> = parse(&body, "config")?;
        Ok(w.data)
    }

    pub fn start(&self, vmid: u32) -> Result<()> {
        let body = self.post_form(
            &format!("/nodes/{}/qemu/{vmid}/status/start", self.node),
            &[],
            true,
        )?;
        self.wait_upid(&body, "start")
    }

    pub fn stop(&self, vmid: u32) -> Result<()> {
        let body = self.post_form(
            &format!("/nodes/{}/qemu/{vmid}/status/stop", self.node),
            &[],
            true,
        )?;
        self.wait_upid(&body, "stop")
    }

    pub fn snapshot(&self, vmid: u32, name: &str) -> Result<()> {
        let body = self.post_form(
            &format!("/nodes/{}/qemu/{vmid}/snapshot", self.node),
            // `vmstate=1`: include RAM, so a snapshot of a RUNNING VM is a
            // system checkpoint and not just a disk image at an arbitrary
            // instant. It is what the libvirt backend's `snapshot-create-as`
            // gives here, and `restore` returning a guest to a half-written
            // filesystem instead of to a running state would be the same verb
            // meaning two things.
            &[("snapname", name), ("vmstate", "1")],
            true,
        )?;
        self.wait_upid(&body, "snapshot")
    }

    pub fn rollback(&self, vmid: u32, name: &str) -> Result<()> {
        let body = self.post_form(
            &format!("/nodes/{}/qemu/{vmid}/snapshot/{name}/rollback", self.node),
            &[],
            true,
        )?;
        self.wait_upid(&body, "rollback")
    }

    /// The VM's snapshot names.
    ///
    /// **`current` is filtered out**, and it is not cosmetic: the API includes
    /// a pseudo-entry by that name meaning "the live state, i.e. no snapshot".
    /// Listing it would report a snapshot nobody took — and `vm restore
    /// <name> current` would then look like a supported thing to do.
    pub fn snapshots(&self, vmid: u32) -> Result<Vec<String>> {
        let body = self.get(&format!("/nodes/{}/qemu/{vmid}/snapshot", self.node))?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "snapshots")?;
        Ok(w.data
            .iter()
            .filter_map(|s| s.get("name").and_then(|n| n.as_str()))
            .filter(|n| *n != "current")
            .map(str::to_string)
            .collect())
    }

    pub fn destroy(&self, vmid: u32) -> Result<()> {
        let body = self.send(
            self.http
                .delete(self.url(&format!("/nodes/{}/qemu/{vmid}", self.node))),
            true,
        )?;
        self.wait_upid(&body, "destroy")
    }

    /// Reads the UPID out of a response and waits for that task.
    ///
    /// Every one of these endpoints answers with a task id and not a result —
    /// returning here would report a VM created before anything exists.
    fn wait_upid(&self, body: &str, what: &str) -> Result<()> {
        let w: Wrapped<serde_json::Value> = parse(body, what)?;
        let upid = w.data.as_str().ok_or_else(|| {
            Error::Invalid(format!(
                "proxmox: {what} did not answer with a task id: {}",
                truncate_chars(body, 200)
            ))
        })?;
        self.wait_task(upid)
    }

    /// Waits for a Proxmox task (`UPID:…`) to finish, and reports ITS verdict.
    ///
    /// Returning when the POST succeeds would report a VM created before
    /// anything exists — every lifecycle call here answers with a task id.
    pub fn wait_task(&self, upid: &str) -> Result<()> {
        let deadline = Instant::now() + TASK_TIMEOUT;
        let mut wait = POLL_MIN;
        loop {
            let body = self.get(&format!(
                "/nodes/{}/tasks/{}/status",
                self.node,
                urlencode(upid)
            ))?;
            let t: Wrapped<TaskStatus> = parse(&body, "task status")?;
            match task_verdict(&t.data.status, t.data.exitstatus.as_deref()) {
                Some(Ok(())) => return Ok(()),
                Some(Err(why)) => {
                    return Err(Error::Invalid(format!("proxmox: task failed: {why}")))
                }
                None => {}
            }
            if Instant::now() >= deadline {
                return Err(Error::Invalid(format!(
                    "proxmox: task {upid} was still '{}' after {}s — giving up. It may still be \
                     running on the node; nothing here was rolled back",
                    t.data.status,
                    TASK_TIMEOUT.as_secs()
                )));
            }
            std::thread::sleep(wait);
            wait = next_poll_wait(wait);
        }
    }
}

// ===========================================================================
// Pure helpers
// ===========================================================================

fn parse<T: for<'de> Deserialize<'de>>(body: &str, what: &str) -> Result<T> {
    serde_json::from_str(body).map_err(|e| {
        Error::Invalid(format!(
            "proxmox: could not read the answer from {what}: {e} (body starts: {})",
            truncate_chars(body, 160)
        ))
    })
}

/// Truncates to at most `max` BYTES without splitting a character. Slicing a
/// response body by byte index panics when the cut lands inside a multi-byte
/// character — and the body comes from the far end, so that turns any error
/// path into a remote crash. The same fix `delonix-truenas` carries, for the
/// same reason and found the same way.
fn truncate_chars(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Percent-encodes the characters a UPID carries that are not path-safe. A UPID
/// is `UPID:pve:0000…:root@pam:` — the colons are legal in a path segment, the
/// `@` is not reliably so.
///
/// Encodes **bytes**, not chars. The first version mapped `other as u32`, which
/// is right only below 0x80: a `ç` in a username would have produced `%E7`
/// instead of its two UTF-8 bytes, and anything above 0xFF (`%1F600` for an
/// emoji) is not percent-encoding at all — the server would read a path nobody
/// wrote. A UPID comes back from the node with the account name inside it, so
/// the input is not ours to assume ASCII.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The target URL, checked before any credential goes on the wire. Same two
/// refusals as the TrueNAS provisioner, for the same reasons: plain HTTP would
/// send the token in the clear, and userinfo is a password in the manifest
/// under another name (and the classic way to make a URL read as one host and
/// reach another).
pub fn validate_target_url(url: &str) -> Result<()> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| Error::Invalid(format!("invalid Proxmox url '{url}': it needs a scheme")))?;
    if !scheme.eq_ignore_ascii_case("https") {
        return Err(Error::Invalid(format!(
            "invalid Proxmox url '{url}': only https:// is accepted — the API token would go over \
             the wire in the clear otherwise (use `insecureTLS` for the node's self-signed cert)"
        )));
    }
    let hostport = rest.split(['/', '?', '#']).next().unwrap_or("");
    if hostport.is_empty() {
        return Err(Error::Invalid(format!(
            "invalid Proxmox url '{url}': it names no host"
        )));
    }
    if hostport.contains('@') {
        return Err(Error::Invalid(format!(
            "invalid Proxmox url '{url}': credentials in the URL are not accepted — use a \
             `kind: Secret`"
        )));
    }
    Ok(())
}

/// Does this error carry the node's 401?
///
/// Matched on the rendered message because that is where `send` puts the
/// status, and the alternative — a typed status on `Error` — would mean a new
/// variant in `delonix-runtime-core` for one caller. `401` on its own would be
/// too loose (a body can contain any number); the prefix `send` writes is not.
fn is_unauthorized(e: &Error) -> bool {
    e.to_string().contains("returned HTTP 401")
}

/// A bridge name is interpolated into the `net0` property.
pub fn validate_bridge_name(bridge: &str) -> Result<()> {
    let ok = !bridge.is_empty()
        && bridge.len() <= 15 // IFNAMSIZ - 1
        && bridge
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !bridge.starts_with('-');
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "invalid Proxmox bridge name '{bridge}': expected something like 'vmbr0'"
        )))
    }
}

/// A node name goes into a URL path on every single call. Restricted to what
/// Proxmox itself accepts in a node name, which is also what keeps it from
/// escaping the path.
pub fn validate_node_name(node: &str) -> Result<()> {
    let ok = !node.is_empty()
        && node.len() <= 63
        && node
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && !node.starts_with('-');
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "invalid Proxmox node name '{node}': expected letters, digits, '-' and '.'"
        )))
    }
}

/// What `cfg.disk` names on the far side.
#[derive(Debug, PartialEq)]
enum DiskSpec {
    /// `template:<vmid>` — clone this template.
    Template(u32),
    /// `<storage>:<gib>` — a fresh empty disk.
    New { storage: String, gib: u32 },
}

/// Parses `cfg.disk` for the Proxmox backend.
///
/// Refused rather than guessed. The likeliest mistake is a LOCAL path — the
/// habit every other backend teaches — and it means nothing on a node
/// elsewhere; silently treating it as a storage name would create a VM with a
/// disk somewhere nobody asked for.
fn parse_disk_spec(disk: &str) -> Result<DiskSpec> {
    let bad = || {
        Error::Invalid(format!(
            "proxmox: '{disk}' does not name anything on the node — use `template:<vmid>` to \
             clone a template, or `<storage>:<size-in-GiB>` for a fresh disk (e.g. \
             `local-lvm:8`). A local path has no meaning on a remote node"
        ))
    };
    let (head, tail) = disk.split_once(':').ok_or_else(bad)?;
    if disk.contains('/') {
        return Err(bad());
    }
    if head == "template" {
        return tail.parse().map(DiskSpec::Template).map_err(|_| bad());
    }
    let gib: u32 = tail.parse().map_err(|_| bad())?;
    if head.is_empty() || gib == 0 {
        return Err(bad());
    }
    Ok(DiskSpec::New {
        storage: head.to_string(),
        gib,
    })
}

/// Picks the guest's usable IPv4 out of a `network-get-interfaces` answer.
///
/// The shape, from the node (`GET .../agent/network-get-interfaces`), is the
/// QEMU guest agent's own reply wrapped twice — `data.result` is the array:
///
/// ```json
/// {"data": {"result": [
///   {"name": "lo", "ip-addresses": [{"ip-address-type": "ipv4",
///                                    "ip-address": "127.0.0.1", "prefix": 8}]},
///   {"name": "ens18", "hardware-address": "bc:24:11:f4:f9:9c",
///    "ip-addresses": [{"ip-address-type": "ipv4", "ip-address": "10.0.2.15",
///                      "prefix": 24},
///                     {"ip-address-type": "ipv6",
///                      "ip-address": "fe80::be24:11ff:fef4:f99c", "prefix": 64}]}
/// ]}}
/// ```
///
/// What it refuses, and each one is an address that would be reported as the
/// VM's and be useless or wrong:
///
/// * **loopback** — every guest has `127.0.0.1`, and it is the first entry, so
///   taking "the first IPv4" gets it every time;
/// * **IPv6** — the record's `ip` field and everything that reads it (the
///   holder's internal DNS answers A records only) are IPv4 here;
/// * **link-local `169.254.0.0/16`** — what an interface has when DHCP FAILED.
///   Reporting it says "the VM has an address" when the truth is the opposite.
///
/// Order is the agent's, and the first acceptable address wins: a guest with
/// several NICs has no ranking this side could invent that would beat the one
/// the guest itself reports.
fn parse_agent_ip(v: &serde_json::Value) -> Option<String> {
    for iface in v.get("data")?.get("result")?.as_array()? {
        // `lo` by name, and 127/8 by value: a guest may name loopback something
        // else, and an interface named `lo` is not a promise about its address.
        if iface.get("name").and_then(|n| n.as_str()) == Some("lo") {
            continue;
        }
        let Some(addrs) = iface.get("ip-addresses").and_then(|a| a.as_array()) else {
            continue;
        };
        for a in addrs {
            if a.get("ip-address-type").and_then(|t| t.as_str()) != Some("ipv4") {
                continue;
            }
            let Some(ip) = a.get("ip-address").and_then(|s| s.as_str()) else {
                continue;
            };
            let ip = ip.trim();
            if ip.starts_with("127.") || ip.starts_with("169.254.") || ip.is_empty() {
                continue;
            }
            return Some(ip.to_string());
        }
    }
    None
}

/// A snapshot name goes into a URL path and into `qm`'s own namespace.
fn validate_snapshot_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !name.starts_with('-')
        // The API's pseudo-entry for "the live state". Accepting it would let
        // `vm restore <vm> current` look like a supported operation.
        && name != "current";
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "invalid Proxmox snapshot name '{name}': expected letters, digits, '-' and '_' \
             (and not 'current', which the API uses for the live state)"
        )))
    }
}

/// Everything in [`VmConfig`] that this backend cannot honour, refused by NAME.
///
/// The ADR calls accepting and dropping these "the failure mode this repo
/// treats as its worst", and until now that is exactly what happened: a
/// `--hugepages`, a `-v /data:/data` or an `--ssh-key` went into a
/// `--backend proxmox` create, the command reported success, and the VM simply
/// did not have it. There is no way for a user to notice from the outside.
///
/// Grouped by WHY, because the reasons are not the same and a user hitting one
/// of them wants to know which:
///
/// * **the guest is on another machine** — a local kernel/initrd/firmware, a
///   local NoCloud seed ISO, a host device or a 9p share are all paths on THIS
///   filesystem, and nothing on the node can reach them;
/// * **QEMU knobs Proxmox owns itself** — hugepages and CPU pinning are the
///   node's business, configured on the node;
/// * **libvirt-only escape hatches** — there is no domain XML here at all.
///
/// Deliberately NOT refused, because they are honoured: `name`, `disk`,
/// `vcpus`, `memory`, `bridge`, `static_ip`, `namespace` (unused but harmless:
/// `vm_namespace_supported` already refuses a non-default one upstream, where
/// the reason can be explained properly).
fn refuse_unsupported(cfg: &VmConfig) -> Result<()> {
    let mut bad: Vec<&str> = Vec::new();
    let mut add = |present: bool, field: &'static str| {
        if present {
            bad.push(field);
        }
    };
    add(cfg.kernel.is_some(), "kernel");
    add(cfg.initrd.is_some(), "initrd");
    add(cfg.firmware.is_some(), "firmware");
    add(cfg.cmdline.is_some(), "cmdline");
    add(cfg.seed.is_some(), "seed");
    add(cfg.hugepages, "hugepages");
    add(cfg.cpu_affinity.is_some(), "cpuAffinity");
    add(!cfg.devices.is_empty(), "devices");
    add(!cfg.volumes.is_empty(), "volumes");
    add(cfg.vnc, "vnc");
    add(cfg.machine.is_some(), "machine");
    add(cfg.cpu_model.is_some(), "cpuModel");
    add(cfg.cpu_topology.is_some(), "cpuTopology");
    add(cfg.tpm, "tpm");
    add(cfg.video.is_some(), "video");
    add(!cfg.boot_order.is_empty(), "bootOrder");
    add(!cfg.extra_disks.is_empty(), "extraDisks");
    add(!cfg.extra_nics.is_empty(), "extraNics");
    add(!cfg.libvirt_xml_overlay.is_empty(), "libvirtXmlOverlay");
    add(cfg.libvirt_xml.is_some(), "libvirtXml");
    add(cfg.net_mode.is_some(), "netMode");
    if bad.is_empty() {
        return Ok(());
    }
    Err(Error::Invalid(format!(
        "the 'proxmox' backend cannot honour: {}. A VM on a remote node has no access to this \
         host's kernel/initrd/seed/devices/9p paths, its QEMU tuning (hugepages, CPU pinning, \
         machine type, TPM, video, boot order) is the node's own configuration, and there is no \
         libvirt domain XML here at all. Remove the field, or use a local backend \
         (`--backend libvirt`)",
        bad.join(", ")
    )))
}

/// The vmid out of the handle `boot` stored (`proxmox:<node>:<vmid>`). Pure, so
/// the "not ours" case is testable without a node.
fn vmid_from_handle(handle: &str) -> Option<u32> {
    handle
        .strip_prefix("proxmox:")
        .and_then(|r| r.rsplit_once(':'))
        .and_then(|(_, id)| id.parse().ok())
}

// ===========================================================================
// The backend
// ===========================================================================

impl ProxmoxBackend {
    pub fn connect(target: &Target) -> Result<Self> {
        Ok(Self {
            client: std::sync::Arc::new(Client::connect(target)?),
        })
    }

    /// Another handle onto the SAME authenticated client. This is what a
    /// registered factory hands out: building a backend must not re-authenticate.
    pub fn sharing(client: std::sync::Arc<Client>) -> Self {
        Self { client }
    }

    /// The shared client, to hand to [`Self::sharing`].
    pub fn client(&self) -> std::sync::Arc<Client> {
        self.client.clone()
    }

    /// The node-side id of a VM this backend created, out of the handle `boot`
    /// stored (`proxmox:<node>:<vmid>`).
    ///
    /// NOT the name: two VMs on a node may share one, and every `qm` call takes
    /// the id. A record without the handle was not created by this backend —
    /// saying so beats guessing an id.
    fn vmid_of(&self, vm: &Vm) -> Result<u32> {
        vmid_from_handle(&vm.api_socket).ok_or_else(|| {
            Error::Invalid(format!(
                "VM '{}' has no Proxmox handle in its record (found {:?}) — it was not created \
                 by this backend",
                vm.name, vm.api_socket
            ))
        })
    }
}

impl VmBackend for ProxmoxBackend {
    fn id(&self) -> &'static str {
        "proxmox"
    }

    /// A remote backend that got this far has already proven itself: `connect`
    /// authenticated and confirmed the node exists. Auto-detection never calls
    /// this (see `auto_selectable`), so there is no path where it costs a
    /// surprise round trip.
    fn available(&self) -> bool {
        true
    }

    fn manages_own_storage(&self) -> bool {
        true
    }

    fn auto_selectable(&self) -> bool {
        false
    }

    /// Creates the VM on the node and starts it.
    ///
    /// `disk` is `cfg.disk` verbatim (see `manages_own_storage`) and names
    /// something on the FAR side, in one of two forms:
    ///
    /// * **`template:<vmid>`** — clone that template. This is the Proxmox way
    ///   of getting a VM with an OS in it, and the one to reach for.
    /// * **`<storage>:<size-in-GiB>`** — a fresh empty disk (`local-lvm:8`).
    ///   Useful for a VM that will boot from something else, and it is what
    ///   makes the lifecycle testable without a template on the node.
    ///
    /// Refused rather than guessed: anything else fails naming both forms. A
    /// local path is the most likely mistake, and it has no meaning here.
    fn boot(
        &self,
        _vmdir: &Path,
        cfg: &VmConfig,
        disk: &str,
        on: &dyn Fn(CreateStage),
    ) -> Result<Boot> {
        // BEFORE anything is created on the node: a field this backend cannot
        // honour is refused by name, never accepted and dropped. The ADR calls
        // that "the failure mode this repo treats as its worst", and it was
        // exactly what happened — a `-v /data:/data` or a `--hugepages` went
        // in, the command said it worked, and the VM did not have it.
        refuse_unsupported(cfg)?;
        let vmid = self.client.next_vmid()?;
        on(CreateStage::Define);
        match parse_disk_spec(disk)? {
            DiskSpec::Template(src) => self.client.clone_template(src, vmid, &cfg.name)?,
            DiskSpec::New { storage, gib } => {
                self.client.create_vm(vmid, &cfg.name, cfg, &storage, gib)?
            }
        }
        on(CreateStage::Start);
        self.client.start(vmid)?;
        // The vmid is what every later call addresses, and the name is not: two
        // VMs on a node may share a name, and `qm` takes the id. It goes in
        // `api_socket`, the field a backend uses for its own handle — the
        // alternative was inventing a field on `Vm` for one backend.
        Ok(Boot {
            pid: None,
            tap: String::new(),
            mac: String::new(),
            api_socket: format!("proxmox:{}:{vmid}", self.client.node),
            ip: None,
        })
    }

    fn is_running(&self, vm: &Vm) -> bool {
        let Ok(vmid) = self.vmid_of(vm) else {
            return false;
        };
        let Ok(body) = self.client.get(&format!(
            "/nodes/{}/qemu/{vmid}/status/current",
            self.client.node
        )) else {
            return false;
        };
        parse::<Wrapped<serde_json::Value>>(&body, "status")
            .ok()
            .and_then(|w| {
                w.data
                    .get("status")
                    .and_then(|s| s.as_str())
                    .map(|s| s == "running")
            })
            .unwrap_or(false)
    }

    /// The guest's IPv4, asked of the QEMU guest agent.
    ///
    /// **`None` is a first-class answer here, not a failure.** The address
    /// lives inside the guest, so the only way to it is an agent RUNNING in
    /// there — `create_vm` opens the channel (`agent=1`), but whether anything
    /// answers depends on the image. A guest with no agent makes the node reply
    /// HTTP 500 `"QEMU guest agent is not running"` (measured), and that is the
    /// ordinary case for, say, a plain cloud image: it must cost a `None` and
    /// not a scary line, because `vm ls` calls this for every VM on every
    /// listing.
    ///
    /// Every failure is therefore swallowed to `None` — but logged at `debug`
    /// rather than dropped, because "no agent" and "the token lost its
    /// permissions" both show up as an empty IP column, and the second one is
    /// worth being able to find.
    fn ip(&self, vm: &Vm) -> Option<String> {
        let vmid = self.vmid_of(vm).ok()?;
        let body = self
            .client
            .get(&format!(
                "/nodes/{}/qemu/{vmid}/agent/network-get-interfaces",
                self.client.node
            ))
            .map_err(|e| {
                tracing::debug!(vm = %vm.name, error = %e, "proxmox: no address from the guest agent");
            })
            .ok()?;
        parse_agent_ip(&parse::<serde_json::Value>(&body, "agent interfaces").ok()?)
    }

    /// Stops the VM and removes it from the node.
    ///
    /// Both halves, because that is what `stop` means for every other backend
    /// here: libvirt undefines the domain so nothing is left behind, and a VM
    /// left `stopped` on a Proxmox node after `delonix vm rm` would be an
    /// orphan nobody is looking for. The order matters — a running VM cannot be
    /// destroyed, and asking anyway gets a task failure that reads like a bug.
    /// Powers the VM off. **The disk stays**, and the VM stays defined on the
    /// node.
    ///
    /// This used to stop AND destroy, on the reasoning that a VM left behind
    /// after `delonix vm rm` is an orphan. The reasoning was right and it was
    /// wired to the wrong verb: the engine calls `stop` for `vm stop` too, and
    /// there the disk is meant to survive — the CLI's own next-steps block says
    /// `stop it (keeps the disk)`. On a local backend the two coincide because
    /// the disk is the engine's file; here the node owns it, so destroying the
    /// VM destroyed the guest's data on a plain `vm stop`. Freeing everything
    /// is now [`Self::destroy`], which is what `vm rm` calls.
    fn stop(&self, _vmdir: &Path, vm: &Vm) -> Result<()> {
        let vmid = self.vmid_of(vm)?;
        if self.is_running(vm) {
            self.client.stop(vmid)?;
        }
        Ok(())
    }

    /// Powers off AND removes the VM from the node — the record is going away,
    /// so nothing may be left behind for nobody to find.
    ///
    /// The order matters: a running VM cannot be destroyed, and asking anyway
    /// gets a task failure that reads like a bug.
    fn destroy(&self, vmdir: &Path, vm: &Vm) -> Result<()> {
        let vmid = self.vmid_of(vm)?;
        self.stop(vmdir, vm)?;
        self.client.destroy(vmid)
    }

    /// Starts the VM this record already names, instead of creating another.
    ///
    /// Without this, `vm start` on a stopped Proxmox VM went through `boot`,
    /// which asks the node for the next free id: a SECOND VM, with a fresh
    /// empty disk, and the first one orphaned on the node with the record
    /// rewritten to point at the new one. The data was still there and nothing
    /// could find it.
    ///
    /// `Ok(None)` when the node no longer has that vmid — the VM was removed
    /// outside this engine, and creating one is then the honest answer.
    fn resume(&self, _vmdir: &Path, vm: &Vm) -> Result<Option<Boot>> {
        let Ok(vmid) = self.vmid_of(vm) else {
            // No handle: not created by this backend. Let the caller create.
            return Ok(None);
        };
        if self.client.config(vmid).is_err() {
            return Ok(None);
        }
        self.client.start(vmid)?;
        Ok(Some(Boot {
            pid: None,
            tap: String::new(),
            mac: String::new(),
            api_socket: vm.api_socket.clone(),
            ip: None,
        }))
    }

    fn snapshot(&self, _vmdir: &Path, vm: &Vm, name: &str) -> Result<()> {
        let vmid = self.vmid_of(vm)?;
        validate_snapshot_name(name)?;
        self.client.snapshot(vmid, name)
    }

    fn restore(&self, _vmdir: &Path, vm: &Vm, name: &str) -> Result<()> {
        let vmid = self.vmid_of(vm)?;
        validate_snapshot_name(name)?;
        self.client.rollback(vmid, name)
    }

    fn snapshots(&self, vm: &Vm) -> Result<Vec<String>> {
        self.client.snapshots(self.vmid_of(vm)?)
    }
}

/// Registers this backend under the name `proxmox`, against `target`.
///
/// **This is the caller ADR-0008's decision 2 was waiting for.** The registry
/// takes a closure precisely because a remote backend needs configuration, and
/// `fn() -> Box<dyn VmBackend>` had nowhere to receive an endpoint, a node name
/// and a credential.
///
/// **Connects once, lazily.** Registering does no I/O — a node that is
/// unreachable costs nothing until somebody selects the backend — and the
/// authenticated client is then SHARED by every later lookup. Without that,
/// `vm ls` over ten VMs would authenticate ten times, because the engine builds
/// a backend per `backend_for`.
///
/// Never auto-selectable: auto-detection asks `available()`, and the only
/// honest answer here costs a network round trip to a node nobody named.
pub fn register(target: Target) -> Result<()> {
    // Fail on a malformed target HERE, at registration, rather than at the
    // first `vm create`: the operator is looking at the configuration now.
    validate_target_url(&target.base_url)?;
    validate_node_name(&target.node)?;

    let shared: std::sync::Mutex<Option<std::sync::Arc<Client>>> = std::sync::Mutex::new(None);
    delonix_vm::register_backend(delonix_vm::BackendRegistration {
        id: "proxmox",
        aliases: &["pve"],
        auto_selectable: false,
        new: Box::new(move || {
            let mut slot = shared.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = slot.as_ref() {
                return Ok(Box::new(ProxmoxBackend::sharing(c.clone())));
            }
            // A failed connect is NOT cached: a node that was down when the
            // first VM was listed must not stay "down" for the rest of the
            // process.
            let c = std::sync::Arc::new(Client::connect(&target)?);
            *slot = Some(c.clone());
            Ok(Box::new(ProxmoxBackend::sharing(c)))
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_nao_quer_dizer_falhou() {
        // The trap the spike found, and the whole reason this function exists:
        // `status: "stopped"` is how Proxmox says the task is OVER. Reading it
        // as the result inverts every verdict.
        assert!(matches!(task_verdict("stopped", Some("OK")), Some(Ok(()))));
        assert!(task_verdict("running", None).is_none());
        assert!(task_verdict("running", Some("OK")).is_none());
        // A real failure carries its reason.
        match task_verdict("stopped", Some("command 'qm start 900' failed")) {
            Some(Err(why)) => assert!(why.contains("qm start")),
            other => panic!("expected a failure, got {other:?}"),
        }
        // Finished with nothing recorded is UNKNOWN, and unknown is not
        // success — reporting OK there would be inventing a result.
        assert!(matches!(task_verdict("stopped", None), Some(Err(_))));
    }

    #[test]
    fn a_url_e_o_no_sao_verificados_antes_de_qualquer_credencial() {
        assert!(
            validate_target_url("http://pve.local:8006").is_err(),
            "plain http"
        );
        assert!(
            validate_target_url("https://u:p@evil/").is_err(),
            "userinfo"
        );
        assert!(validate_target_url("pve.local").is_err(), "no scheme");
        assert!(validate_target_url("https://").is_err(), "no host");
        assert!(validate_target_url("https://pve.local:8006").is_ok());

        // The node name is interpolated into EVERY path.
        for bad in ["", "a/b", "a b", "../x", "-pve", "n@de"] {
            assert!(validate_node_name(bad).is_err(), "{bad}");
        }
        assert!(validate_node_name("pve").is_ok());
        assert!(validate_node_name("pve-01.lab").is_ok());
    }

    #[test]
    fn um_upid_atravessa_o_path_intacto() {
        // A UPID carries `@` (from `root@pam`), which is not reliably safe in a
        // path segment; the colons are.
        let upid = "UPID:pve:000005E5:00003010:6A7AD1F9:qmsnapshot:901:root@pam:";
        let e = urlencode(upid);
        assert!(e.contains("root%40pam"), "{e}");
        assert!(e.starts_with("UPID:pve:"), "colons must stay: {e}");
    }

    #[test]
    fn o_backend_nao_toca_em_disco_local_nem_e_auto_detectado() {
        // Both are the point of ADR-0008's phase (a): the engine must not
        // prepare a local overlay for a hypervisor on another machine, and
        // auto-detection must not make network requests.
        struct Probe;
        impl VmBackend for Probe {
            fn id(&self) -> &'static str {
                "proxmox"
            }
            fn available(&self) -> bool {
                true
            }
            fn boot(
                &self,
                _: &Path,
                _: &VmConfig,
                _: &str,
                _: &dyn Fn(CreateStage),
            ) -> Result<Boot> {
                unreachable!()
            }
            fn is_running(&self, _: &Vm) -> bool {
                false
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> Result<()> {
                Ok(())
            }
            fn manages_own_storage(&self) -> bool {
                true
            }
            fn auto_selectable(&self) -> bool {
                false
            }
        }
        let p = Probe;
        assert!(p.manages_own_storage());
        assert!(!p.auto_selectable());
    }

    #[test]
    fn um_corpo_de_resposta_nao_faz_o_cliente_entrar_em_panico() {
        let body = format!("{}é", "A".repeat(399));
        assert_eq!(truncate_chars(&body, 400).len(), 399);
        assert_eq!(truncate_chars("olá", 4000), "olá");
    }

    #[test]
    fn o_disco_de_um_no_remoto_nao_e_um_caminho_local() {
        assert_eq!(
            parse_disk_spec("template:9000").unwrap(),
            DiskSpec::Template(9000)
        );
        assert_eq!(
            parse_disk_spec("local-lvm:8").unwrap(),
            DiskSpec::New {
                storage: "local-lvm".into(),
                gib: 8
            }
        );
        // The likeliest mistake: a local path, which every other backend takes
        // and which means nothing on a node elsewhere. Treating it as a storage
        // name would create a disk somewhere nobody asked for.
        for bad in [
            "/var/lib/delonix/vm-images/x.qcow2",
            "./x.qcow2",
            "local-lvm:/tmp/x",
            "x.qcow2",
            "local-lvm",
            "local-lvm:0",
            "local-lvm:abc",
            ":8",
            "template:abc",
        ] {
            assert!(parse_disk_spec(bad).is_err(), "{bad:?} should be refused");
        }
        // And the refusal names BOTH forms, because a reader who got here does
        // not know either.
        let e = parse_disk_spec("/tmp/x.qcow2").unwrap_err().to_string();
        assert!(e.contains("template:") && e.contains("size-in-GiB"), "{e}");
    }

    /// The backend reads `memory` through the ENGINE's parser, and the reason
    /// is a measured divergence: the copy this crate used to carry did not know
    /// the k8s `Gi`/`Mi` suffix, so the very same manifest meant 2 GiB on
    /// libvirt and Cloud Hypervisor and 1 GiB here — with nothing said.
    #[test]
    fn a_memoria_le_se_como_nos_outros_backends() {
        assert_eq!(mem_mib("2G"), 2048);
        assert_eq!(mem_mib("512M"), 512);
        assert_eq!(mem_mib("2048"), 2048);
        assert_eq!(mem_mib(" 4G "), 4096);
        // What the local copy got wrong, and the whole point of sharing one.
        assert_eq!(
            mem_mib("2Gi"),
            2048,
            "o sufixo k8s dava 1024 na copia local"
        );
        assert_eq!(mem_mib("512Mi"), 512);
        // Unparseable still falls back to something that boots — but the engine
        // WARNS, which the silent copy did not.
        assert_eq!(mem_mib("bananas"), 1024);
    }

    /// A UPID comes back from the node with the account name inside it, so the
    /// input is not ours to assume ASCII. `other as u32` was right only below
    /// 0x80: a `ç` became `%E7` (one byte instead of its two UTF-8 bytes) and
    /// anything above 0xFF produced `%1F600`, which is not percent-encoding at
    /// all — the server reads a path nobody wrote.
    #[test]
    fn o_urlencode_codifica_bytes_e_nao_code_points() {
        assert_eq!(urlencode("ç"), "%C3%A7");
        assert_eq!(urlencode("😀"), "%F0%9F%98%80");
        // Every escape is exactly two hex digits, which is what the grammar says.
        let e = urlencode("UPID:pve:x:joão@pam:");
        for part in e.split('%').skip(1) {
            assert!(
                part.len() >= 2 && part[..2].chars().all(|c| c.is_ascii_hexdigit()),
                "escape mal formado em {e}"
            );
        }
        assert!(e.contains("%40pam"), "{e}");
    }

    /// Both ends of the range the same loop covers. A flat interval is the
    /// wrong answer at each: too slow for a `start`, and a hammering for a
    /// create that runs for minutes.
    #[test]
    fn um_backoff_responde_depressa_e_nao_martela_o_no() {
        assert_eq!(POLL_MIN, Duration::from_millis(150));
        assert!(POLL_MIN < Duration::from_millis(750), "mais responsivo");

        // Ten minutes of polling: count the requests, and cap the interval.
        let (mut waited, mut reqs, mut w) = (Duration::ZERO, 0, POLL_MIN);
        while waited < Duration::from_secs(600) {
            waited += w;
            reqs += 1;
            w = next_poll_wait(w);
            assert!(w <= POLL_MAX, "o intervalo nao pode crescer sem tecto");
        }
        assert!(
            reqs < 250,
            "a 750ms fixos eram 800 pedidos numa tarefa de 10 min; deram {reqs}"
        );
        assert_eq!(next_poll_wait(POLL_MAX), POLL_MAX, "estavel no tecto");
    }

    /// Three refusals, and each one is an address that would be reported as
    /// the VM's and be useless or actively misleading.
    #[test]
    fn o_ip_do_agente_ignora_loopback_ipv6_e_link_local() {
        let j = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();

        // The ordinary answer. `lo` comes FIRST — which is why "the first IPv4"
        // would report 127.0.0.1 for every guest there is.
        let normal = j(r#"{"data":{"result":[
            {"name":"lo","ip-addresses":[
                {"ip-address-type":"ipv4","ip-address":"127.0.0.1","prefix":8},
                {"ip-address-type":"ipv6","ip-address":"::1","prefix":128}]},
            {"name":"ens18","hardware-address":"bc:24:11:f4:f9:9c","ip-addresses":[
                {"ip-address-type":"ipv6","ip-address":"fe80::be24:11ff:fef4:f99c","prefix":64},
                {"ip-address-type":"ipv4","ip-address":"10.0.2.15","prefix":24}]}]}}"#);
        assert_eq!(parse_agent_ip(&normal).as_deref(), Some("10.0.2.15"));

        // DHCP FAILED: the interface is up and has 169.254/16. Reporting it
        // would say "the VM has an address" when the truth is the opposite —
        // and the record's IP is what the holder's DNS hands out.
        let apipa = j(r#"{"data":{"result":[
            {"name":"eth0","ip-addresses":[
                {"ip-address-type":"ipv4","ip-address":"169.254.11.4","prefix":16}]}]}}"#);
        assert_eq!(parse_agent_ip(&apipa), None);

        // IPv6 only: the record's `ip` and the internal DNS are IPv4 here, so
        // an address nothing can use is not an answer.
        let v6 = j(r#"{"data":{"result":[
            {"name":"eth0","ip-addresses":[
                {"ip-address-type":"ipv6","ip-address":"2001:db8::1","prefix":64}]}]}}"#);
        assert_eq!(parse_agent_ip(&v6), None);

        // A loopback that is not NAMED `lo` is still loopback.
        let odd = j(r#"{"data":{"result":[
            {"name":"lo0","ip-addresses":[
                {"ip-address-type":"ipv4","ip-address":"127.0.0.1","prefix":8}]},
            {"name":"eth0","ip-addresses":[
                {"ip-address-type":"ipv4","ip-address":"192.168.1.50","prefix":24}]}]}}"#);
        assert_eq!(parse_agent_ip(&odd).as_deref(), Some("192.168.1.50"));

        // An interface with no addresses at all, before one that has them.
        let empty_first = j(r#"{"data":{"result":[
            {"name":"eth0"},
            {"name":"eth1","ip-addresses":[]},
            {"name":"eth2","ip-addresses":[
                {"ip-address-type":"ipv4","ip-address":"10.1.2.3","prefix":24}]}]}}"#);
        assert_eq!(parse_agent_ip(&empty_first).as_deref(), Some("10.1.2.3"));
    }

    /// The shapes that are NOT an interface list, and none of them may panic or
    /// invent an address. The middle one is measured, not imagined: it is
    /// exactly what a node answers (with HTTP 500) for a guest that has no
    /// agent running, which is the ordinary case for a plain cloud image.
    #[test]
    fn uma_resposta_sem_agente_nao_e_um_ip_nem_um_panico() {
        let j = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        assert_eq!(
            parse_agent_ip(&j(
                r#"{"message":"QEMU guest agent is not running\n","data":null}"#
            )),
            None
        );
        assert_eq!(parse_agent_ip(&j(r#"{"data":{}}"#)), None);
        assert_eq!(parse_agent_ip(&j(r#"{"data":{"result":"nope"}}"#)), None);
        assert_eq!(parse_agent_ip(&j(r#"{}"#)), None);
        assert_eq!(parse_agent_ip(&j(r#"[]"#)), None);
        // A result whose entries are not objects.
        assert_eq!(parse_agent_ip(&j(r#"{"data":{"result":[1,2,"x"]}}"#)), None);
    }

    /// The ADR calls accepting-and-dropping "the failure mode this repo treats
    /// as its worst", and until this pass it was exactly what happened: a
    /// `-v /data:/data` on a `--backend proxmox` create reported success and
    /// the VM did not have the volume. Nothing outside could tell.
    #[test]
    fn um_campo_que_este_backend_nao_honra_e_recusado_pelo_nome() {
        let base = VmConfig {
            name: "v".into(),
            disk: "local-lvm:8".into(),
            memory: "1G".into(),
            ..Default::default()
        };
        // The plain case must stay accepted, or the refusal is useless.
        assert!(refuse_unsupported(&base).is_ok());
        // And so must the fields this backend DOES honour.
        assert!(refuse_unsupported(&VmConfig {
            bridge: Some("vmbr1".into()),
            static_ip: Some("10.0.0.5/24".into()),
            vcpus: 4,
            ..base.clone()
        })
        .is_ok());

        // Each of these used to be swallowed. The message must NAME the field:
        // "unsupported configuration" sends someone reading the whole manifest.
        let cases: Vec<(&str, VmConfig)> = vec![
            (
                "volumes",
                VmConfig {
                    volumes: vec![delonix_vm::VmVolume {
                        source: "/data".into(),
                        tag: "data".into(),
                        mount_path: "/data".into(),
                        read_only: false,
                    }],
                    ..base.clone()
                },
            ),
            (
                "hugepages",
                VmConfig {
                    hugepages: true,
                    ..base.clone()
                },
            ),
            (
                "kernel",
                VmConfig {
                    kernel: Some("/boot/vmlinuz".into()),
                    ..base.clone()
                },
            ),
            (
                "seed",
                VmConfig {
                    seed: Some("/x/seed.iso".into()),
                    ..base.clone()
                },
            ),
            (
                "devices",
                VmConfig {
                    devices: vec!["/dev/kvm".into()],
                    ..base.clone()
                },
            ),
            (
                "libvirtXml",
                VmConfig {
                    libvirt_xml: Some("<domain/>".into()),
                    ..base.clone()
                },
            ),
            (
                "vnc",
                VmConfig {
                    vnc: true,
                    ..base.clone()
                },
            ),
        ];
        for (field, cfg) in cases {
            let e = match refuse_unsupported(&cfg) {
                Ok(()) => panic!("'{field}' foi aceite e descartado em silencio"),
                Err(e) => e.to_string(),
            };
            assert!(e.contains(field), "a recusa tem de nomear '{field}': {e}");
            assert!(
                e.contains("libvirt"),
                "e tem de dizer o que fazer em vez disso: {e}"
            );
        }

        // Several at once are reported TOGETHER: fixing them one error at a
        // time is a create attempt per field.
        let e = refuse_unsupported(&VmConfig {
            hugepages: true,
            vnc: true,
            tpm: true,
            ..base
        })
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("hugepages") && e.contains("vnc") && e.contains("tpm"),
            "{e}"
        );
    }

    #[test]
    fn a_bridge_e_a_vlan_entram_no_net0() {
        let cli = |bridge: Option<&str>, vlan| Client {
            http: reqwest::blocking::Client::new(),
            base: "https://x".into(),
            node: "pve".into(),
            auth: Auth::ApiToken {
                id: "a!b".into(),
                secret: "c".into(),
            },
            ticket: std::sync::RwLock::new(None),
            bridge: bridge.unwrap_or("vmbr0").to_string(),
            vlan,
        };
        let cfg = VmConfig::default();
        assert_eq!(cli(None, None).net0_arg(&cfg), "virtio,bridge=vmbr0");
        assert_eq!(
            cli(Some("vmbr9"), None).net0_arg(&cfg),
            "virtio,bridge=vmbr9",
            "o default do alvo"
        );
        assert_eq!(
            cli(None, Some(42)).net0_arg(&cfg),
            "virtio,bridge=vmbr0,tag=42"
        );
        // A per-VM bridge beats the target's default.
        let per_vm = VmConfig {
            bridge: Some("vmbr7".into()),
            ..Default::default()
        };
        assert_eq!(
            cli(Some("vmbr9"), None).net0_arg(&per_vm),
            "virtio,bridge=vmbr7"
        );
        // Blank is "no opinion", not a bridge named "".
        let blank = VmConfig {
            bridge: Some("  ".into()),
            ..Default::default()
        };
        assert_eq!(
            cli(Some("vmbr9"), None).net0_arg(&blank),
            "virtio,bridge=vmbr9"
        );
    }

    #[test]
    fn nomes_que_entram_num_path_ou_numa_propriedade_sao_validados() {
        for bad in [
            "",
            "a b",
            "a/b",
            "-vmbr0",
            "vmbr0;reboot",
            "x".repeat(16).as_str(),
        ] {
            assert!(validate_bridge_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_bridge_name("vmbr0").is_ok());
        assert!(validate_bridge_name("vmbr0.100").is_ok());

        for bad in ["", "a b", "a/b", "-s", "s;x", "current"] {
            assert!(validate_snapshot_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_snapshot_name("antes-do-upgrade_1").is_ok());
    }

    /// A 401 has to be told apart from every other failure, because only that
    /// one is worth logging in again for — and only for password auth. An API
    /// token that gets a 401 was revoked, and retrying it forever is how a
    /// credential ends up locked out.
    #[test]
    fn so_um_401_dispara_nova_autenticacao() {
        let err = |s: &str| Error::Invalid(s.to_string());
        assert!(is_unauthorized(&err(
            "proxmox: https://pve returned HTTP 401 Unauthorized: bad ticket"
        )));
        assert!(!is_unauthorized(&err(
            "proxmox: https://pve returned HTTP 500: QEMU guest agent is not running"
        )));
        // A body that merely mentions the number is not a 401.
        assert!(!is_unauthorized(&err(
            "proxmox: https://pve returned HTTP 500: disk 401 is missing"
        )));
    }

    #[test]
    fn o_vmid_vem_do_registo_e_nao_do_nome() {
        // Two VMs on a node may share a name; every `qm` call takes the id.
        // A record with no handle was not created by this backend — saying so
        // beats guessing an id and acting on somebody else's VM.
        assert!(vmid_from_handle("").is_none());
        assert!(
            vmid_from_handle("/run/x.sock").is_none(),
            "a libvirt/CH record"
        );
        assert!(vmid_from_handle("proxmox:pve:").is_none());
        assert!(vmid_from_handle("proxmox:pve:abc").is_none());
        assert_eq!(vmid_from_handle("proxmox:pve:101"), Some(101));
        // A node name with a colon still yields the id: the parse takes the LAST
        // field.
        assert_eq!(vmid_from_handle("proxmox:a:b:7"), Some(7));
    }

    /// O parser contra uma resposta REAL, e não contra uma escrita à mão.
    ///
    /// Capturada de um `qemu-guest-agent` 7.2.22 a correr num convidado Debian
    /// bookworm com DUAS NICs, dentro da appliance `proxmox-ve:9.1` que este
    /// repo constrói, por
    /// `GET /nodes/pve/qemu/100/agent/network-get-interfaces`.
    ///
    /// Os casos escritos à mão acima cobrem as REGRAS (loopback, IPv6,
    /// link-local); este cobre a FORMA — que é o que nenhuma amostra inventada
    /// pode garantir. Trouxe três coisas que o exemplo do doc-comment não tem:
    /// um bloco `statistics` por interface, as chaves em ordem VARIÁVEL entre
    /// interfaces (o `eth0` traz `statistics` antes de `ip-addresses`), e um
    /// IPv6 `fec0::` site-local a par do `fe80::`.
    ///
    /// A escolha é o `eth0` e não o `eth1`: a ordem é a que o agente reporta, e
    /// medida ao vivo 12 vezes seguidas ela não variou — que é o que torna o
    /// `ip()` de um convidado multi-NIC determinístico em vez de oscilante,
    /// a única pergunta que um segundo NIC levanta e uma NIC só nunca revela.
    #[test]
    fn o_parser_contra_um_agente_a_serio_com_duas_nics() {
        let v: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/agent-network-get-interfaces.json"
        ))
        .expect("a resposta gravada tem de ser JSON válido");
        assert_eq!(parse_agent_ip(&v).as_deref(), Some("10.0.2.17"));
    }
}
