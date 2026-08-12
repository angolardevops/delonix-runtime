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
    ticket: Option<Ticket>,
}

impl Client {
    pub fn connect(target: &Target) -> Result<Self> {
        validate_target_url(&target.base_url)?;
        validate_node_name(&target.node)?;
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .danger_accept_invalid_certs(target.insecure_tls)
            .build()
            .map_err(|e| {
                Error::Invalid(format!("proxmox: could not build the HTTP client: {e}"))
            })?;
        let mut me = Self {
            http,
            base: target.base_url.trim_end_matches('/').to_string(),
            node: target.node.clone(),
            auth: target.auth.clone(),
            ticket: None,
        };
        if let Auth::Password { username, password } = &me.auth {
            let body = me.post_form(
                "/access/ticket",
                &[
                    ("username", username.as_str()),
                    ("password", password.as_str()),
                ],
                false,
            )?;
            let t: Wrapped<Ticket> = parse(&body, "/access/ticket")?;
            me.ticket = Some(t.data);
        }
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

    fn authed(&self, rb: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
        match (&self.auth, &self.ticket) {
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

    fn get(&self, path: &str) -> Result<String> {
        self.send(self.http.get(self.url(path)), true)
    }

    fn post_form(&self, path: &str, form: &[(&str, &str)], authed: bool) -> Result<String> {
        self.send(self.http.post(self.url(path)).form(form), authed)
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
        let body = self.post_form(
            &format!("/nodes/{}/qemu", self.node),
            &[
                ("vmid", vmid_s.as_str()),
                ("name", name),
                ("memory", mem.as_str()),
                ("cores", cores.as_str()),
                ("ostype", "l26"),
                ("scsihw", "virtio-scsi-single"),
                ("scsi0", scsi0.as_str()),
                // A NIC on the node's default bridge. `virtio` alone is the
                // model — the value goes in the property's default key, and
                // spelling that key out (`model=virtio`) is what the API
                // refuses. The ADR recorded this shape as refused too; that was
                // an artefact of the spike's `curl -d`, which does not
                // URL-encode. `reqwest`'s `.form()` does, and the node accepts
                // it: measured, `net0 = virtio=BC:24:11:F4:F9:9C,bridge=vmbr0`
                // in the resulting config.
                ("net0", "virtio,bridge=vmbr0"),
                // Enable the QEMU guest agent CHANNEL. This is the host side
                // only: it adds the virtio-serial port the agent talks over,
                // and without it the node will not even try — every
                // `/agent/...` call answers "QEMU guest agent is not running"
                // no matter what the guest has installed. Whether an agent
                // answers on the other end is the image's business, which is
                // exactly why `ip()` treats silence as "unknown" and not as an
                // error (see `parse_agent_ip`).
                ("agent", "1"),
            ],
            true,
        )?;
        self.wait_upid(&body, "create")
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
    fn stop(&self, _vmdir: &Path, vm: &Vm) -> Result<()> {
        let vmid = self.vmid_of(vm)?;
        if self.is_running(vm) {
            self.client.stop(vmid)?;
        }
        self.client.destroy(vmid)
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
}
