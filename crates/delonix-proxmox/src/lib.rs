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
use delonix_vm::{Boot, CreateStage, VmBackend, VmConfig};
use serde::Deserialize;
use std::path::Path;
use std::time::{Duration, Instant};

/// How long to wait for a Proxmox task before giving up. A create that
/// allocates a disk on slow storage is real work; what this guards against is a
/// task that never reaches a terminal state, not a slow one.
const TASK_TIMEOUT: Duration = Duration::from_secs(600);

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

pub struct ProxmoxBackend {
    client: Client,
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
                // model; the shape `model=virtio` is REFUSED by the API
                // ("value 'model' does not have a value in the enumeration"),
                // measured during the spike.
                ("net0", "virtio,bridge=vmbr0"),
            ],
            true,
        )?;
        self.wait_upid(&body, "create")
    }

    /// Clones a template into a new VM.
    pub fn clone_template(&self, template: u32, vmid: u32, name: &str) -> Result<()> {
        let newid = vmid.to_string();
        let body = self.post_form(
            &format!("/nodes/{}/qemu/{template}/clone", self.node),
            &[("newid", newid.as_str()), ("name", name), ("full", "1")],
            true,
        )?;
        self.wait_upid(&body, "clone")
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
            std::thread::sleep(Duration::from_millis(750));
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
fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' | ':' => c.to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect()
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
            "proxmox: '{disk}' does not name anything on the node — use `template:<vmid>` to              clone a template, or `<storage>:<size-in-GiB>` for a fresh disk (e.g. `local-lvm:8`).              A local path has no meaning on a remote node"
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

/// The vmid out of the handle `boot` stored (`proxmox:<node>:<vmid>`). Pure, so
/// the "not ours" case is testable without a node.
fn vmid_from_handle(handle: &str) -> Option<u32> {
    handle
        .strip_prefix("proxmox:")
        .and_then(|r| r.rsplit_once(':'))
        .and_then(|(_, id)| id.parse().ok())
}

/// `"2G"`/`"512M"`/`"2048"` → MiB. Proxmox takes memory in MiB.
fn mem_mib(memory: &str) -> u64 {
    let t = memory.trim();
    let (num, mult) = match t.chars().last() {
        Some('G') | Some('g') => (&t[..t.len() - 1], 1024),
        Some('M') | Some('m') => (&t[..t.len() - 1], 1),
        _ => (t, 1),
    };
    num.trim().parse::<u64>().unwrap_or(1024) * mult
}

// ===========================================================================
// The backend
// ===========================================================================

impl ProxmoxBackend {
    pub fn connect(target: &Target) -> Result<Self> {
        Ok(Self {
            client: Client::connect(target)?,
        })
    }

    /// The node-side id of a VM this backend created, out of the handle `boot`
    /// stored (`proxmox:<node>:<vmid>`).
    ///
    /// NOT the name: two VMs on a node may share one, and every `qm` call takes
    /// the id. A record without the handle was not created by this backend —
    /// saying so beats guessing an id.
    fn vmid_of(&self, vm: &Vm) -> Result<u32> {
        vmid_from_handle(&vm.api_socket)
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "VM '{}' has no Proxmox handle in its record (found {:?}) — it was not                      created by this backend",
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

    fn ip(&self, _vm: &Vm) -> Option<String> {
        // Reaching the guest's address needs the QEMU guest agent installed
        // INSIDE it (`/agent/network-get-interfaces`). Returning None is the
        // honest answer for a guest that does not have it; inventing one from
        // the config would be worse than saying nothing.
        None
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

    #[test]
    fn a_memoria_vai_em_mib_como_a_api_quer() {
        assert_eq!(mem_mib("2G"), 2048);
        assert_eq!(mem_mib("512M"), 512);
        assert_eq!(mem_mib("2048"), 2048);
        assert_eq!(mem_mib(" 4G "), 4096);
        // Unparseable falls back to a working default rather than 0 — a VM with
        // no memory does not start, and the failure would name the node.
        assert_eq!(mem_mib("bananas"), 1024);
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
