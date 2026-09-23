//! A [`VmBackend`] backed by a Proxmox VE node's REST API.
//!
//! **One node, named explicitly.** No inventory, no scheduling, no choosing a
//! node on the user's behalf: a decision that needs to know *who the customer
//! is* belongs to whoever consumes the engine, not to a node runtime (guardrail #2).
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

mod error;

use delonix_compute::Vm;
pub use error::{Error, Result};
// `mem_mib` comes from the engine and is NOT re-implemented here. The copy that
// used to live in this file did not know the k8s `Gi`/`Mi` suffix the engine
// tolerates, so `memory: 2Gi` meant 2 GiB on libvirt and Cloud Hypervisor and
// 1 GiB here — silently, which is the failure this repo treats as its worst.
use delonix_vm::{mem_mib, Boot, CreateStage, VmBackend, VmConfig};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The most of a response body this client reads into memory. Every answer it
/// expects is a few KB (a config, a task status, a snapshot list); a body past
/// this is not one of them, and reading it in full would let a node — or
/// whatever answers in its name — decide how much memory this process takes.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Set to a file path, every request appends `METHOD /path` to it — the
/// numerator of the coverage matrix (`scripts/proxmox_api_inventory.py
/// --trace`), read from what a run actually sent and not from the source.
pub const TRACE_ROUTES_ENV: &str = "DELONIX_PROXMOX_TRACE_ROUTES";

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
///
/// `Debug` is written by hand: the secret and the password are the two values
/// that must never reach a log, a panic message or an error, and a derived
/// `Debug` on this enum is exactly how they would.
#[derive(Clone)]
pub enum Auth {
    /// `PVEAPIToken=<user>!<tokenid>=<secret>` — the form to prefer. A token is
    /// revocable on the node without touching an account, and it is what a
    /// `kind: Secret` should carry.
    ApiToken { id: String, secret: String },
    /// Account credentials, exchanged for a ticket. Accepted because a freshly
    /// installed node has an account before it has any token.
    Password { username: String, password: String },
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::ApiToken { id, .. } => f
                .debug_struct("ApiToken")
                .field("id", id)
                .field("secret", &"<redacted>")
                .finish(),
            Auth::Password { username, .. } => f
                .debug_struct("Password")
                .field("username", username)
                .field("password", &"<redacted>")
                .finish(),
        }
    }
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
    /// A CA certificate (PEM) to trust for this node IN ADDITION to the
    /// system roots — the way to verify a node whose certificate an internal
    /// CA signed, instead of switching verification off with `insecure_tls`.
    pub ca_cert_pem: Option<Vec<u8>>,
}

/// Bounds the client applies to every call. `Default` is what production
/// runs with; a test lowers them to make a hang observable in seconds.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Per-request ceiling (connect + response). Task polling is many short
    /// requests under this, never one long one.
    pub request_timeout: Duration,
    /// How long [`Client::wait_task`] waits for a task to reach a terminal
    /// state before reporting [`Error::TaskTimeout`] — which is NOT proof the
    /// task failed; the task may still be running on the node.
    pub task_timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(120),
            task_timeout: TASK_TIMEOUT,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Wrapped<T> {
    data: T,
}

#[derive(Deserialize)]
struct Ticket {
    ticket: String,
    #[serde(rename = "CSRFPreventionToken")]
    csrf: String,
}

impl std::fmt::Debug for Ticket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ticket")
            .field("ticket", &"<redacted>")
            .field("csrf", &"<redacted>")
            .finish()
    }
}

/// One asynchronous task this client submitted, as the ledger keeps it.
///
/// Written BEFORE the wait starts: a process killed while waiting leaves the
/// UPID on disk, and the next operation on the same VM settles it first
/// ([`Client::settle_pending`]) instead of racing a task it never heard of.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskRecord {
    pub upid: String,
    pub node: String,
    /// What was asked (`create`, `start`, …), in this client's own words.
    pub action: String,
    pub vmid: u32,
    pub started_unix: u64,
    pub state: TaskState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum TaskState {
    /// The node accepted it; the verdict is not in yet.
    Submitted,
    Ok,
    Failed {
        reason: String,
    },
    /// This client stopped waiting; the node may still be running it.
    TimedOut,
}

/// Where a VM's task records live: `<vmdir>/proxmox-tasks.json`, or nowhere.
///
/// Durable state, not process memory (the daemonless rule): every entry is
/// written through before the wait and settled after it. A ledger that cannot
/// be written warns and lets the operation proceed — a `stop` refused because
/// a bookkeeping file is unwritable would be the worse failure.
#[derive(Debug, Clone)]
pub struct Ledger {
    path: Option<PathBuf>,
}

/// Records kept per VM; older ones are dropped, settled first.
const LEDGER_KEEP: usize = 50;

impl Ledger {
    pub fn at(vmdir: &Path) -> Self {
        Self {
            path: Some(vmdir.join("proxmox-tasks.json")),
        }
    }

    /// No persistence — for callers that own no VM directory.
    pub fn none() -> Self {
        Self { path: None }
    }

    pub fn records(&self) -> Vec<TaskRecord> {
        let Some(path) = &self.path else {
            return Vec::new();
        };
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "proxmox: task ledger unreadable, starting a new one");
                Vec::new()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "proxmox: task ledger unreadable, starting a new one");
                Vec::new()
            }
        }
    }

    /// Records whose verdict is not in yet.
    pub fn pending(&self) -> Vec<TaskRecord> {
        self.records()
            .into_iter()
            .filter(|r| r.state == TaskState::Submitted)
            .collect()
    }

    fn record(&self, rec: TaskRecord) {
        self.update(|all| all.push(rec));
    }

    /// Settles the MOST RECENT record with this UPID. A node reuses nothing,
    /// but a mock (or a replayed log) can hand the same id twice, and the
    /// verdict belongs to the task that was just waited on, not to the first
    /// one ever recorded under that id.
    fn settle(&self, upid: &str, state: TaskState) {
        self.update(|all| {
            if let Some(r) = all.iter_mut().rev().find(|r| r.upid == upid) {
                r.state = state;
            }
        });
    }

    fn update(&self, f: impl FnOnce(&mut Vec<TaskRecord>)) {
        let Some(path) = &self.path else {
            return;
        };
        let mut all = self.records();
        f(&mut all);
        if all.len() > LEDGER_KEEP {
            // Drop settled records first, oldest first; a pending one is never
            // dropped to make room.
            let mut settled: Vec<usize> = all
                .iter()
                .enumerate()
                .filter(|(_, r)| r.state != TaskState::Submitted)
                .map(|(i, _)| i)
                .collect();
            let excess = all.len() - LEDGER_KEEP;
            settled.truncate(excess);
            for i in settled.into_iter().rev() {
                all.remove(i);
            }
        }
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(&all).unwrap_or_default())?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = write() {
            tracing::warn!(path = %path.display(), error = %e, "proxmox: could not write the task ledger");
        }
    }
}

/// The kinds of task this client submits, each with the worker type the node
/// lists it under (`GET /nodes/{node}/tasks`, field `type`) — how a task whose
/// answer was lost is found again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskKind {
    Create,
    Clone,
    Start,
    Stop,
    Snapshot,
    Rollback,
    Destroy,
}

impl TaskKind {
    fn action(self) -> &'static str {
        match self {
            TaskKind::Create => "create",
            TaskKind::Clone => "clone",
            TaskKind::Start => "start",
            TaskKind::Stop => "stop",
            TaskKind::Snapshot => "snapshot",
            TaskKind::Rollback => "rollback",
            TaskKind::Destroy => "destroy",
        }
    }

    /// The `type` of the worker Proxmox VE registers for this operation
    /// (the `fork_worker` names of `PVE::API2::Qemu`). `qmcreate`, `qmstart`,
    /// `qmstop`, `qmsnapshot` and `qmdestroy` are the names ADR-0008's spike
    /// saw in a live node's task log; `qmclone` and `qmrollback` follow the
    /// same naming and are exercised by the mock node, not yet by a real one.
    fn worker_type(self) -> &'static str {
        match self {
            TaskKind::Create => "qmcreate",
            TaskKind::Clone => "qmclone",
            TaskKind::Start => "qmstart",
            TaskKind::Stop => "qmstop",
            TaskKind::Snapshot => "qmsnapshot",
            TaskKind::Rollback => "qmrollback",
            TaskKind::Destroy => "qmdestroy",
        }
    }
}

/// What [`Client::recover_lost_answer`] found on the node after a request
/// whose answer never arrived.
enum Recovered {
    /// The task is running (or ran) on the node: wait on THIS one.
    Task(String),
    /// No task in flight, and the effect is already there.
    Done,
    /// Nothing on the node says the request was received.
    Nothing,
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
    task_timeout: Duration,
}

impl Client {
    pub fn connect(target: &Target) -> Result<Self> {
        Self::connect_with(target, ClientOptions::default())
    }

    pub fn connect_with(target: &Target, opts: ClientOptions) -> Result<Self> {
        validate_target_url(&target.base_url)?;
        validate_node_name(&target.node)?;
        if let Some(b) = &target.bridge {
            validate_bridge_name(b)?;
        }
        let mut builder = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(opts.request_timeout)
            // One node, a handful of sequential calls: a pool this small is
            // all it ever uses, and a bound on it is a bound on sockets held
            // open against the node by a long-running process.
            .pool_max_idle_per_host(4)
            .danger_accept_invalid_certs(target.insecure_tls);
        if let Some(pem) = &target.ca_cert_pem {
            let cert = reqwest::Certificate::from_pem(pem).map_err(|e| {
                Error::ClientBuild(format!(
                    "proxmox: the CA certificate given for {} is not a PEM certificate: {e}",
                    target.base_url
                ))
            })?;
            builder = builder.add_root_certificate(cert);
        }
        let http = builder.build().map_err(|e| {
            Error::ClientBuild(format!("proxmox: could not build the HTTP client: {e}"))
        })?;
        let me = Self {
            http,
            base: target.base_url.trim_end_matches('/').to_string(),
            node: target.node.clone(),
            auth: target.auth.clone(),
            ticket: std::sync::RwLock::new(None),
            bridge: target.bridge.clone().unwrap_or_else(|| "vmbr0".to_string()),
            vlan: target.vlan,
            task_timeout: opts.task_timeout,
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
            return Err(Error::NoSuchNode(format!(
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

    fn send(
        &self,
        method: &str,
        path: &str,
        rb: reqwest::blocking::RequestBuilder,
        authed: bool,
    ) -> Result<String> {
        let rb = if authed { self.authed(rb) } else { rb };
        trace_route(method, path);
        let resp = rb
            .send()
            .map_err(|e| Error::Request(format!("proxmox: request failed: {e}")))?;
        let status = resp.status();
        // Read errors used to be swallowed into an empty body, which then
        // failed to parse as "could not read the answer" — a truncated
        // connection reported as a malformed node. They are transport now.
        let body = read_bounded(resp, path)?;
        if !status.is_success() {
            return Err(classify_status(status, &self.base, path, &body));
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
    fn send_authed(
        &self,
        method: &str,
        path: &str,
        build: impl Fn() -> reqwest::blocking::RequestBuilder,
    ) -> Result<String> {
        match self.send(method, path, build(), true) {
            Err(e) if matches!(self.auth, Auth::Password { .. }) && is_unauthorized(&e) => {
                tracing::debug!("proxmox: ticket rejected, logging in again");
                self.login()?;
                self.send(method, path, build(), true)
            }
            other => other,
        }
    }

    fn get(&self, path: &str) -> Result<String> {
        let url = self.url(path);
        self.send_authed("GET", path, || self.http.get(&url))
    }

    fn delete(&self, path: &str) -> Result<String> {
        let url = self.url(path);
        self.send_authed("DELETE", path, || self.http.delete(&url))
    }

    fn post_form(&self, path: &str, form: &[(&str, &str)], authed: bool) -> Result<String> {
        let url = self.url(path);
        if !authed {
            return self.send("POST", path, self.http.post(&url).form(form), false);
        }
        self.send_authed("POST", path, || self.http.post(&url).form(form))
    }

    /// The VM's `status` as the node reports it (`running`, `stopped`, …).
    pub fn status_current(&self, vmid: u32) -> Result<String> {
        let body = self.get(&format!("/nodes/{}/qemu/{vmid}/status/current", self.node))?;
        let w: Wrapped<serde_json::Value> = parse(&body, "status")?;
        w.data
            .get("status")
            .and_then(|s| s.as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                Error::UnexpectedAnswer(format!(
                    "proxmox: status/current of VM {vmid} carries no `status`: {}",
                    truncate_chars(&body, 200)
                ))
            })
    }

    /// The tasks the node is running right now for this VM: `(upid, type)`.
    pub fn active_tasks(&self, vmid: u32) -> Result<Vec<(String, String)>> {
        let body = self.get(&format!(
            "/nodes/{}/tasks?vmid={vmid}&source=active",
            self.node
        ))?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "tasks")?;
        Ok(w.data
            .iter()
            .filter_map(|t| {
                let upid = t.get("upid")?.as_str()?;
                let kind = t.get("type")?.as_str()?;
                Some((upid.to_string(), kind.to_string()))
            })
            .collect())
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
                Error::UnexpectedAnswer(format!(
                    "proxmox: could not read a VM id from /cluster/nextid: {}",
                    w.data
                ))
            })
    }

    /// Creates a VM with a fresh disk on `storage`.
    pub fn create_vm(
        &self,
        ledger: &Ledger,
        vmid: u32,
        name: &str,
        cfg: &VmConfig,
        storage: &str,
        gib: u32,
    ) -> Result<()> {
        let form = create_form(vmid, name, cfg, storage, gib, &self.net0_arg(cfg));
        let form: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
        self.task(
            ledger,
            vmid,
            TaskKind::Create,
            || self.post_form(&format!("/nodes/{}/qemu", self.node), &form, true),
            Some(&|| self.vm_exists(vmid)),
        )
    }

    /// Does the node have a VM with this id? `NotFound` is the answer `false`,
    /// not a failure.
    pub fn vm_exists(&self, vmid: u32) -> Result<bool> {
        match self.config(vmid) {
            Ok(_) => Ok(true),
            Err(Error::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Applies to an already-cloned VM the configuration the clone did not carry.
    ///
    /// **A clone inherits the TEMPLATE's settings, not the caller's**, and until
    /// now nothing put the caller's back: `boot` cloned and started, so a VM
    /// asked for with 4 vCPU and 8 GiB came up with whatever the template had,
    /// on the template's bridge, with no address configured and no SSH key. It
    /// is the path a DKS node takes (a golden image registered as a template),
    /// i.e. exactly the case where a node nobody can log into is useless.
    ///
    /// The agent channel is deliberately NOT forced here — that part of the
    /// original reasoning holds: a template built with the agent already has it,
    /// and overriding would contradict a choice somebody made about it.
    pub fn configure_clone(&self, vmid: u32, cfg: &VmConfig) -> Result<()> {
        let mem = mem_mib(&cfg.memory).to_string();
        let cores = cfg.vcpus.max(1).to_string();
        let net0 = self.net0_arg(cfg);
        let ci = cloud_init_form(cfg);
        let mut form: Vec<(&str, &str)> = vec![
            ("memory", mem.as_str()),
            ("cores", cores.as_str()),
            ("net0", net0.as_str()),
        ];
        form.extend(ci.iter().map(|(k, v)| (*k, v.as_str())));
        // No `ide2` here: a cloud-init drive cannot be added to a VM that has
        // one, and a template meant for cloud-init carries it already. Saying
        // so beats a failure whose message is about a busy device slot.
        // `authed: true` — this is NOT the login call. The first version passed
        // `false` and the node answered 401 straight after a clone that had
        // just succeeded, leaving a VM on the node with the template's CPU,
        // memory and no key: the exact half-configured state this function
        // exists to prevent. Measured against a live PVE 9.2.
        // A config change is applied synchronously and answers `data: null` —
        // there is no UPID to wait on, unlike clone/start/stop. It takes the
        // same config lock, though, so it goes through the same retry.
        with_lock_retry("configure", || {
            self.post_form(
                &format!("/nodes/{}/qemu/{vmid}/config", self.node),
                &form,
                true,
            )
            .map(|_| ())
        })
    }

    /// The `net0` property: model, bridge and optional VLAN tag.
    ///
    /// (See [`cloud_init_form`] for the cloud-init half of the same idea.)
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
    pub fn clone_template(
        &self,
        ledger: &Ledger,
        template: u32,
        vmid: u32,
        name: &str,
    ) -> Result<()> {
        let newid = vmid.to_string();
        self.task(
            ledger,
            vmid,
            TaskKind::Clone,
            || {
                self.post_form(
                    &format!("/nodes/{}/qemu/{template}/clone", self.node),
                    &[("newid", newid.as_str()), ("name", name), ("full", "1")],
                    true,
                )
            },
            Some(&|| self.vm_exists(vmid)),
        )
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

    pub fn start(&self, ledger: &Ledger, vmid: u32) -> Result<()> {
        self.task(
            ledger,
            vmid,
            TaskKind::Start,
            || {
                self.post_form(
                    &format!("/nodes/{}/qemu/{vmid}/status/start", self.node),
                    &[],
                    true,
                )
            },
            Some(&|| Ok(self.status_current(vmid)? == "running")),
        )
    }

    pub fn stop(&self, ledger: &Ledger, vmid: u32) -> Result<()> {
        self.task(
            ledger,
            vmid,
            TaskKind::Stop,
            || {
                self.post_form(
                    &format!("/nodes/{}/qemu/{vmid}/status/stop", self.node),
                    &[],
                    true,
                )
            },
            Some(&|| Ok(self.status_current(vmid)? == "stopped")),
        )
    }

    /// Takes a snapshot, refusing a name that is already taken as a CONFLICT.
    ///
    /// Asked first rather than read out of the task's failure: the node says
    /// `snapshot name 's1' already used` inside a task that came back as a
    /// generic failure (exit 1), where libvirt's backend answers the same case
    /// with exit 5. A script telling «pick another name» from «something broke»
    /// cannot parse the message. The node's own refusal is still mapped, for
    /// the window between the question and the create.
    pub fn snapshot(&self, ledger: &Ledger, vmid: u32, name: &str) -> Result<()> {
        if self.snapshots(vmid)?.iter().any(|s| s == name) {
            return Err(taken_snapshot(vmid, name));
        }
        self.task(
            ledger,
            vmid,
            TaskKind::Snapshot,
            || {
                self.post_form(
                    &format!("/nodes/{}/qemu/{vmid}/snapshot", self.node),
                    // `vmstate=1`: include RAM, so a snapshot of a RUNNING VM is a
                    // system checkpoint and not just a disk image at an arbitrary
                    // instant. It is what the libvirt backend's `snapshot-create-as`
                    // gives here, and `restore` returning a guest to a half-written
                    // filesystem instead of to a running state would be the same
                    // verb meaning two things.
                    &[("snapname", name), ("vmstate", "1")],
                    true,
                )
            },
            Some(&|| Ok(self.snapshots(vmid)?.iter().any(|s| s == name))),
        )
        .map_err(|e| {
            if e.to_string().contains("already used") {
                taken_snapshot(vmid, name)
            } else {
                e
            }
        })
    }

    /// Reverts to a snapshot; a name the VM does not have is NOT FOUND (exit 4),
    /// as on libvirt, and not whatever shape the node's refusal takes.
    pub fn rollback(&self, ledger: &Ledger, vmid: u32, name: &str) -> Result<()> {
        if !self.snapshots(vmid)?.iter().any(|s| s == name) {
            return Err(Error::SnapshotNotFound(format!(
                "snapshot of Proxmox VM {vmid}: {name}"
            )));
        }
        // No probe: a rollback leaves nothing a read can tell apart from
        // "not rolled back". A lost answer with no task in flight is an error.
        self.task(
            ledger,
            vmid,
            TaskKind::Rollback,
            || {
                self.post_form(
                    &format!("/nodes/{}/qemu/{vmid}/snapshot/{name}/rollback", self.node),
                    &[],
                    true,
                )
            },
            None,
        )
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

    pub fn destroy(&self, ledger: &Ledger, vmid: u32) -> Result<()> {
        // `purge` drops the VM from backup jobs, replication and HA, and
        // `destroy-unreferenced-disks` removes disks the config no longer
        // names — without them a destroy freed the VM and left storage behind,
        // which is exactly what `vm destroy` exists to give back.
        let path = format!(
            "/nodes/{}/qemu/{vmid}?purge=1&destroy-unreferenced-disks=1",
            self.node
        );
        self.task(
            ledger,
            vmid,
            TaskKind::Destroy,
            || self.delete(&path),
            Some(&|| Ok(!self.vm_exists(vmid)?)),
        )
    }

    /// Issues a task-creating request and waits for the task — **again, while
    /// the node answers that the VM's config lock is busy**.
    ///
    /// The lock is not held by anything this backend does. Measured on a PVE
    /// 9.2, in the node's journal: when a QEMU process exits (a `qmstop`, or the
    /// restart inside a `qmrollback` of a snapshot with RAM), `qmeventd` runs
    /// `qm cleanup`, which takes `lock-<vmid>.conf` and — if the VM is already
    /// running again by then — sits on it until it gives up:
    ///
    /// ```text
    /// qmeventd: Starting cleanup for 100 / trying to acquire lock... OK
    /// qmeventd: VM cleanup: QEMU process 2031 for VM 100 still running (or newly started)
    /// qmeventd: aborting cleanup, VM is still running after 30 seconds
    /// ```
    ///
    /// Every task that needs the lock in that window waits 10 s and fails with
    /// `can't lock file '/var/lock/qemu-server/lock-100.conf' - got timeout`.
    /// So a `vm restart` (stop, start) passed and the NEXT one failed, every
    /// time; so did anything right after a `vm snapshot restore`. Nothing waits
    /// for the cleanup to be over, and the API has no way to ask.
    ///
    /// Retrying is safe for exactly this failure: the lock is taken before the
    /// task does anything, so a task that could not get it changed nothing.
    ///
    /// **A lost answer is not a lost request.** A `create` whose connection
    /// dropped after the node accepted it is running on the node with nobody
    /// waiting; sending it again would make a second VM. So a transport
    /// failure on the submission is followed by a look at the node
    /// ([`Client::recover_lost_answer`]): a task of this kind in flight for
    /// this VM is waited on instead, an effect already there is accepted, and
    /// only when the node shows neither does the transport error stand.
    fn task(
        &self,
        ledger: &Ledger,
        vmid: u32,
        kind: TaskKind,
        issue: impl Fn() -> Result<String>,
        probe: Option<&dyn Fn() -> Result<bool>>,
    ) -> Result<()> {
        let what = kind.action();
        with_lock_retry(what, || {
            let upid = match issue() {
                Ok(body) => upid_of(&body, what)?,
                Err(Error::Request(why)) => match self.recover_lost_answer(vmid, kind, probe) {
                    Recovered::Task(upid) => upid,
                    Recovered::Done => return Ok(()),
                    Recovered::Nothing => return Err(Error::Request(why)),
                },
                Err(e) => return Err(e),
            };
            ledger.record(TaskRecord {
                upid: upid.clone(),
                node: self.node.clone(),
                action: what.to_string(),
                vmid,
                started_unix: now_unix(),
                state: TaskState::Submitted,
            });
            let verdict = self.wait_task(&upid);
            ledger.settle(&upid, state_of(&verdict));
            verdict
        })
    }

    /// After a request whose answer never came: what does the node say?
    fn recover_lost_answer(
        &self,
        vmid: u32,
        kind: TaskKind,
        probe: Option<&dyn Fn() -> Result<bool>>,
    ) -> Recovered {
        match self.active_tasks(vmid) {
            Ok(tasks) => {
                if let Some((upid, _)) = tasks.into_iter().find(|(_, t)| t == kind.worker_type()) {
                    tracing::warn!(
                        vmid,
                        what = kind.action(),
                        %upid,
                        "proxmox: the answer to the request was lost but the node is running the task — waiting on it"
                    );
                    return Recovered::Task(upid);
                }
            }
            Err(e) => {
                tracing::debug!(vmid, error = %e, "proxmox: could not list the node's tasks after a lost answer");
                return Recovered::Nothing;
            }
        }
        match probe.map(|p| p()) {
            Some(Ok(true)) => {
                tracing::warn!(
                    vmid,
                    what = kind.action(),
                    "proxmox: the answer to the request was lost, and the node already shows its effect — not resending"
                );
                Recovered::Done
            }
            _ => Recovered::Nothing,
        }
    }

    /// One look at a task: `None` while it runs, else its verdict.
    fn task_status(&self, upid: &str) -> Result<(String, Option<std::result::Result<(), String>>)> {
        let body = self.get(&format!(
            "/nodes/{}/tasks/{}/status",
            self.node,
            urlencode(upid)
        ))?;
        let t: Wrapped<TaskStatus> = parse(&body, "task status")?;
        let verdict = task_verdict(&t.data.status, t.data.exitstatus.as_deref());
        Ok((t.data.status, verdict))
    }

    /// Settles what the ledger still lists as submitted, WITHOUT waiting: one
    /// status read per pending task. Returns the ones still running.
    pub fn reconcile(&self, ledger: &Ledger) -> Result<Vec<TaskRecord>> {
        let mut running = Vec::new();
        for rec in ledger.pending() {
            let (_, verdict) = self.task_status(&rec.upid)?;
            match verdict {
                Some(v) => ledger.settle(
                    &rec.upid,
                    state_of(
                        &v.map_err(|why| Error::TaskFailed(format!("proxmox: task failed: {why}"))),
                    ),
                ),
                None => running.push(rec),
            }
        }
        Ok(running)
    }

    /// Before touching a VM: settle its ledger, and WAIT for any task of its
    /// own still in flight — a `stop` issued on top of a running `start` is a
    /// race the node decides, not this client.
    pub fn settle_pending(&self, ledger: &Ledger, vmid: u32) -> Result<()> {
        for rec in self.reconcile(ledger)? {
            if rec.vmid != vmid {
                continue;
            }
            tracing::info!(vmid, action = %rec.action, upid = %rec.upid, "proxmox: a task from an earlier run is still in flight — waiting for it first");
            let verdict = self.wait_task(&rec.upid);
            ledger.settle(&rec.upid, state_of(&verdict));
            verdict?;
        }
        Ok(())
    }

    /// Waits for a Proxmox task (`UPID:…`) to finish, and reports ITS verdict.
    ///
    /// Returning when the POST succeeds would report a VM created before
    /// anything exists — every lifecycle call here answers with a task id.
    pub fn wait_task(&self, upid: &str) -> Result<()> {
        let deadline = Instant::now() + self.task_timeout;
        let mut wait = POLL_MIN;
        loop {
            let (status, verdict) = self.task_status(upid)?;
            match verdict {
                Some(Ok(())) => return Ok(()),
                Some(Err(why)) => {
                    return Err(Error::TaskFailed(format!("proxmox: task failed: {why}")))
                }
                None => {}
            }
            if Instant::now() >= deadline {
                return Err(Error::TaskTimeout(format!(
                    "proxmox: task {upid} was still '{status}' after {}s — giving up. It may still be \
                     running on the node; nothing here was rolled back",
                    self.task_timeout.as_secs()
                )));
            }
            std::thread::sleep(wait);
            wait = next_poll_wait(wait);
        }
    }
}

/// Reads the UPID out of a task-producing answer.
///
/// Every one of these endpoints answers with a task id and not a result —
/// returning it as a result would report a VM created before anything exists.
fn upid_of(body: &str, what: &str) -> Result<String> {
    let w: Wrapped<serde_json::Value> = parse(body, what)?;
    w.data
        .as_str()
        .filter(|s| s.starts_with("UPID:"))
        .map(str::to_string)
        .ok_or_else(|| {
            Error::UnexpectedAnswer(format!(
                "proxmox: {what} did not answer with a task id: {}",
                truncate_chars(body, 200)
            ))
        })
}

fn state_of(verdict: &Result<()>) -> TaskState {
    match verdict {
        Ok(()) => TaskState::Ok,
        Err(Error::TaskTimeout(_)) => TaskState::TimedOut,
        Err(e) => TaskState::Failed {
            reason: e.to_string(),
        },
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reads at most [`MAX_RESPONSE_BYTES`]; one byte more is a refusal, never a
/// silently cut body handed to a parser.
fn read_bounded(resp: reqwest::blocking::Response, path: &str) -> Result<String> {
    use std::io::Read;
    let mut buf = Vec::new();
    resp.take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| {
            Error::Request(format!(
                "proxmox: reading the answer from {path} failed: {e}"
            ))
        })?;
    if buf.len() > MAX_RESPONSE_BYTES {
        return Err(Error::ResponseTooLarge(format!(
            "proxmox: the answer from {path} exceeded {} MiB — refusing to read it",
            MAX_RESPONSE_BYTES / (1024 * 1024)
        )));
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// The typed failure for a non-2xx answer. The message keeps the shape the
/// CLI always printed (`proxmox: <base> returned HTTP <status>: <body>`);
/// what changes is the CLASS, which is what an exit code and a reconciler
/// read.
///
/// Proxmox VE says most application errors with HTTP 500 — a missing VM
/// ("Configuration file … does not exist") and a taken id ("already exists")
/// included. Those two are read from the body, the way `is_lock_timeout`
/// already reads the lock verdict; everything else under 500 stays generic.
fn classify_status(status: reqwest::StatusCode, base: &str, path: &str, body: &str) -> Error {
    let text = format!(
        "proxmox: {base} returned HTTP {status}: {}",
        truncate_chars(body.trim(), 400)
    );
    let path_only = path.split('?').next().unwrap_or(path);
    match status.as_u16() {
        401 => Error::Unauthorized(text),
        403 => Error::Forbidden(text),
        404 => Error::NotFound(format!("Proxmox resource at {path_only}: {text}")),
        409 => Error::Conflict(text),
        400 | 422 => Error::BadRequest(text),
        502..=504 => Error::Unavailable(text),
        500 if body.contains("does not exist") => {
            Error::NotFound(format!("Proxmox resource at {path_only}: {text}"))
        }
        500 if body.contains("already exists") => Error::Conflict(text),
        _ => Error::HttpStatus(text),
    }
}

/// Appends `METHOD /path` to the file named by [`TRACE_ROUTES_ENV`], if set.
/// The query is dropped: the matrix is keyed by route, not by arguments.
fn trace_route(method: &str, path: &str) {
    let Ok(file) = std::env::var(TRACE_ROUTES_ENV) else {
        return;
    };
    if file.is_empty() {
        return;
    }
    use std::io::Write;
    let route = path.split('?').next().unwrap_or(path);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
    {
        let _ = writeln!(f, "{method} {route}");
    }
}

// ===========================================================================
// Pure helpers
// ===========================================================================

/// How long to keep retrying an operation the node refuses because the VM's
/// config lock is busy. `qmeventd` holds it for at most 30 s after a QEMU exit
/// (see [`Client::task`]) and every attempt already spends the node's own 10 s
/// waiting for it, so 90 s covers the cleanup with room for one more — and
/// still ends: a lock held by something that never lets go must surface as an
/// error, not as a command that hangs.
const LOCK_RETRY_WINDOW: Duration = Duration::from_secs(90);
const LOCK_RETRY_PAUSE: Duration = Duration::from_secs(1);

/// Is this the node refusing because the VM's config lock is taken?
///
/// Matched on the message, like [`is_unauthorized`], and on BOTH halves: a
/// `can't lock file` without `got timeout` is a permission or filesystem
/// problem that no amount of waiting fixes.
fn is_lock_timeout(e: &Error) -> bool {
    let m = e.to_string();
    m.contains("can't lock file") && m.contains("got timeout")
}

fn with_lock_retry<T>(what: &str, op: impl FnMut() -> Result<T>) -> Result<T> {
    retry_on_lock(what, LOCK_RETRY_WINDOW, LOCK_RETRY_PAUSE, op)
}

/// Runs `op` again while it fails on a busy config lock, until `window` has
/// passed. Any other outcome — success or a different failure — returns at
/// once. Durations are parameters so the ceiling is a test, not a comment.
fn retry_on_lock<T>(
    what: &str,
    window: Duration,
    pause: Duration,
    mut op: impl FnMut() -> Result<T>,
) -> Result<T> {
    let started = Instant::now();
    let mut attempt = 1u32;
    loop {
        match op() {
            Err(e) if is_lock_timeout(&e) => {
                if started.elapsed() >= window {
                    return Err(Error::LockTimeout(format!(
                        "{e} — the VM's config lock stayed busy for {}s over {attempt} attempts \
                         of '{what}'; something on the node is still holding it (check the \
                         node's task log for this VM)",
                        started.elapsed().as_secs()
                    )));
                }
                tracing::info!(
                    what,
                    attempt,
                    "proxmox: the VM's config lock is busy on the node, retrying"
                );
                std::thread::sleep(pause);
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// A snapshot name the VM already has — `Conflict` (exit 5), the same class
/// libvirt's backend gives: the next move is «pick another name or remove that
/// one», not «something broke».
fn taken_snapshot(vmid: u32, name: &str) -> Error {
    Error::SnapshotTaken(format!(
        "Proxmox VM {vmid} already has a snapshot named '{name}'"
    ))
}

fn parse<T: for<'de> Deserialize<'de>>(body: &str, what: &str) -> Result<T> {
    serde_json::from_str(body).map_err(|e| {
        Error::Decode(format!(
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
    let (scheme, rest) = url.split_once("://").ok_or_else(|| {
        Error::InvalidUrl(format!("invalid Proxmox url '{url}': it needs a scheme"))
    })?;
    if !scheme.eq_ignore_ascii_case("https") {
        return Err(Error::InvalidUrl(format!(
            "invalid Proxmox url '{url}': only https:// is accepted — the API token would go over \
             the wire in the clear otherwise (use `insecureTLS` for the node's self-signed cert)"
        )));
    }
    let hostport = rest.split(['/', '?', '#']).next().unwrap_or("");
    if hostport.is_empty() {
        return Err(Error::InvalidUrl(format!(
            "invalid Proxmox url '{url}': it names no host"
        )));
    }
    if hostport.contains('@') {
        return Err(Error::CredentialInUrl(format!(
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
/// variant in the shared `Error` of `delonix-model` for one caller. `401` on its own would be
/// too loose (a body can contain any number); the prefix `send` writes is not.
fn is_unauthorized(e: &Error) -> bool {
    matches!(e, Error::Unauthorized(_))
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
        Err(Error::InvalidBridgeName(format!(
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
        Err(Error::InvalidNodeName(format!(
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
        Error::InvalidDiskSpec(format!(
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
        Err(Error::InvalidSnapshotName(format!(
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
    Err(Error::UnsupportedField(format!(
        "the 'proxmox' backend cannot honour: {}. A VM on a remote node has no access to this \
         host's kernel/initrd/seed/devices/9p paths, its QEMU tuning (hugepages, CPU pinning, \
         machine type, TPM, video, boot order) is the node's own configuration, and there is no \
         libvirt domain XML here at all. Remove the field, or use a local backend \
         (`--backend libvirt`)",
        bad.join(", ")
    )))
}

/// Translates the cloud-init INTENT of a `VmConfig` into the node's own
/// cloud-init parameters. **This is the whole point of the intent fields.**
///
/// The local backends realize the same intent as a NoCloud seed ISO — a file on
/// the host, which is why `seed` stays refused here and always will be. Proxmox
/// has cloud-init of its own and speaks these four keys, so the same manifest
/// produces the same guest on either side without the caller knowing which.
///
/// Empty when there is nothing to deliver, and the caller uses that to decide
/// whether the VM needs a cloud-init drive at all: an empty drive on an image
/// that ignores it is a CD-ROM nobody reads that still costs storage on the node.
///
/// **`sshkeys` is percent-encoded BY US, on top of the form encoding.** This
/// comment used to say the opposite — that `.form()` encoding it once was
/// enough and that encoding it ourselves would be a double-encoding bug. The
/// node says otherwise, and it says it plainly:
///
/// ```text
/// 400 Bad Request {"errors":{"sshkeys":"invalid format - invalid urlencoded
///   string: ssh-ed25519 AAAAC3Nza... validacao-proxmox\n"}}
/// ```
///
/// So the VALUE of this parameter is itself defined as a urlencoded string —
/// the transport encoding is a separate layer, and the node decodes twice on
/// purpose. It is the one parameter of this API with that shape, which is
/// exactly why it is worth a comment; reasoning about it from the outside got
/// it backwards, and only a live node settled it.
fn cloud_init_form(cfg: &VmConfig) -> Vec<(&'static str, String)> {
    if cfg.cloud_init == Some(false) {
        return Vec::new();
    }
    let mut out: Vec<(&'static str, String)> = Vec::new();
    // `ip=dhcp` unless an address was asked for. Proxmox's own cloud-init writes
    // this into the guest — the local backends reach the same end through the
    // seed's `network-config`.
    out.push((
        "ipconfig0",
        match &cfg.static_ip {
            Some(ip) => format!("ip={ip}"),
            None => "ip=dhcp".to_string(),
        },
    ));
    // The hostname travels as `name`, which Proxmox's cloud-init writes into the
    // guest — so an explicit `hostname` that differs from the VM name is the
    // only case that needs saying, and `searchdomain` is not it.
    if let Some(h) = cfg
        .hostname
        .as_deref()
        .filter(|h| !h.is_empty() && *h != cfg.name)
    {
        out.push(("name", h.to_string()));
    }
    if !cfg.ssh_keys.is_empty() {
        out.push((
            "ciuser",
            cfg.ci_user
                .as_deref()
                .filter(|u| !u.is_empty())
                .unwrap_or(delonix_vm::cloudinit::DEFAULT_CI_USER)
                .to_string(),
        ));
        out.push(("sshkeys", urlencode(&cfg.ssh_keys.join("\n"))));
    }
    out
}

/// The body of `POST /nodes/<node>/qemu`. Pure, so that "each key goes ONCE"
/// is a test and not a hope.
///
/// It was not once. `ipconfig0` sat in the fixed list here AND came back from
/// [`cloud_init_form`] (added there by `23af3c45`), so the node got two values
/// and refused every create with `400 ipconfig0: type check ('string') failed -
/// got ARRAY` — measured against a PVE 9.2. The unit test only asked whether
/// the key was *present*, which a duplicate satisfies. The network config now
/// lives only in `cloud_init_form`, next to the drive that carries it: an
/// appliance (`cloud_init: false`) gets neither.
fn create_form(
    vmid: u32,
    name: &str,
    cfg: &VmConfig,
    storage: &str,
    gib: u32,
    net0: &str,
) -> Vec<(&'static str, String)> {
    let ci = cloud_init_form(cfg);
    let mut form: Vec<(&'static str, String)> = vec![
        ("vmid", vmid.to_string()),
        ("name", name.to_string()),
        ("memory", mem_mib(&cfg.memory).to_string()),
        ("cores", cfg.vcpus.max(1).to_string()),
        ("ostype", "l26".into()),
        ("scsihw", "virtio-scsi-single".into()),
        ("scsi0", format!("{storage}:{gib}")),
        // A NIC on a bridge of the node. `virtio` alone is the model — the
        // value goes in the property's default key, and spelling that key out
        // (`model=virtio`) is what the API refuses. The ADR recorded this shape
        // as refused too; that was an artefact of the spike's `curl -d`, which
        // does not URL-encode. `reqwest`'s `.form()` does, and the node accepts
        // it: measured, `net0 = virtio=BC:24:11:F4:F9:9C,bridge=vmbr0`.
        ("net0", net0.to_string()),
        // Enable the QEMU guest agent CHANNEL. This is the host side only: it
        // adds the virtio-serial port the agent talks over, and without it the
        // node will not even try — every `/agent/...` call answers "QEMU guest
        // agent is not running" no matter what the guest has installed.
        // Whether an agent answers on the other end is the image's business,
        // which is exactly why `ip()` treats silence as "unknown" and not as an
        // error (see `parse_agent_ip`).
        ("agent", "1".into()),
    ];
    // The cloud-init drive, and only when there is something to put in it: an
    // empty one on an image with no cloud-init is a CD-ROM the guest ignores,
    // but it also silently costs a disk on the node's storage.
    let has_ci = !ci.is_empty();
    // `name` is the one key both halves can carry: `cloud_init_form` sends an
    // explicit `hostname` as `name`, because the node's cloud-init reads the VM
    // name as the hostname — the same thing `configure_clone` does. The more
    // specific value wins, by name and not by a generic de-duplication, which
    // would make the test below pass whatever got sent twice.
    if ci.iter().any(|(k, _)| *k == "name") {
        form.retain(|(k, _)| *k != "name");
    }
    form.extend(ci);
    if has_ci {
        form.push(("ide2", format!("{storage}:cloudinit")));
    }
    form
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
            Error::NoHandle(format!(
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
        vmdir: &Path,
        cfg: &VmConfig,
        disk: &str,
        on: &dyn Fn(CreateStage),
    ) -> delonix_model::Result<Boot> {
        // BEFORE anything is created on the node: a field this backend cannot
        // honour is refused by name, never accepted and dropped. The ADR calls
        // that "the failure mode this repo treats as its worst", and it was
        // exactly what happened — a `-v /data:/data` or a `--hugepages` went
        // in, the command said it worked, and the VM did not have it.
        refuse_unsupported(cfg)?;
        let ledger = Ledger::at(vmdir);
        let vmid = self.client.next_vmid()?;
        on(CreateStage::Define);
        // Everything after the VM EXISTS on the node has to undo it on failure.
        // `create_with` cannot: its cleanup removes a local overlay, and for a
        // backend that owns its storage it deliberately touches nothing — the
        // only thing it holds is `cfg.disk`, which here names the TEMPLATE. The
        // vmid is known only in here.
        //
        // Measured, and it is why this exists: a `configure_clone` that failed
        // (401, in the first version) left VMID 100 on the node with the
        // template's CPU and no key, invisible to `vm ls` because no record was
        // ever written, and removable only by hand through the node's own UI.
        // One per attempt, and the likeliest next move is to retry the command
        // that just failed.
        let undo = |e: Error| -> Error {
            // Best-effort by nature: if the node is unreachable this fails too,
            // and the ORIGINAL error is the one worth reporting — a cleanup
            // error on top would name the symptom instead of the cause.
            if let Err(e2) = self.client.destroy(&ledger, vmid) {
                tracing::warn!(vmid, error = %e2, "proxmox: could not remove the VM left by a failed create");
            }
            e
        };
        match parse_disk_spec(disk)? {
            DiskSpec::Template(src) => {
                self.client.clone_template(&ledger, src, vmid, &cfg.name)?;
                // A clone carries the TEMPLATE's CPU, memory, NIC and cloud-init
                // — never the caller's. Applying them is a separate call by the
                // API's own shape (clone takes `newid`/`name`/`full` and nothing
                // else), and skipping it was how a DKS node came up with the
                // golden's defaults and no key on it.
                self.client.configure_clone(vmid, cfg).map_err(undo)?;
            }
            DiskSpec::New { storage, gib } => self
                .client
                .create_vm(&ledger, vmid, &cfg.name, cfg, &storage, gib)?,
        }
        on(CreateStage::Start);
        self.client.start(&ledger, vmid).map_err(undo)?;
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
            lease_floor: None,
        })
    }

    fn is_running(&self, vm: &Vm) -> bool {
        let Ok(vmid) = self.vmid_of(vm) else {
            return false;
        };
        self.client
            .status_current(vmid)
            .map(|s| s == "running")
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
    fn stop(&self, vmdir: &Path, vm: &Vm) -> delonix_model::Result<()> {
        let vmid = self.vmid_of(vm)?;
        let ledger = Ledger::at(vmdir);
        self.client.settle_pending(&ledger, vmid)?;
        if self.is_running(vm) {
            self.client.stop(&ledger, vmid)?;
        }
        Ok(())
    }

    /// Powers off AND removes the VM from the node — the record is going away,
    /// so nothing may be left behind for nobody to find.
    ///
    /// The order matters: a running VM cannot be destroyed, and asking anyway
    /// gets a task failure that reads like a bug.
    fn destroy(&self, vmdir: &Path, vm: &Vm) -> delonix_model::Result<()> {
        let vmid = self.vmid_of(vm)?;
        self.stop(vmdir, vm)?;
        Ok(self.client.destroy(&Ledger::at(vmdir), vmid)?)
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
    fn resume(&self, vmdir: &Path, vm: &Vm) -> delonix_model::Result<Option<Boot>> {
        let Ok(vmid) = self.vmid_of(vm) else {
            // No handle: not created by this backend. Let the caller create.
            return Ok(None);
        };
        let ledger = Ledger::at(vmdir);
        self.client.settle_pending(&ledger, vmid)?;
        if !self.client.vm_exists(vmid)? {
            return Ok(None);
        }
        self.client.start(&ledger, vmid)?;
        Ok(Some(Boot {
            pid: None,
            tap: String::new(),
            mac: String::new(),
            api_socket: vm.api_socket.clone(),
            ip: None,
            lease_floor: None,
        }))
    }

    fn snapshot(&self, vmdir: &Path, vm: &Vm, name: &str) -> delonix_model::Result<()> {
        let vmid = self.vmid_of(vm)?;
        validate_snapshot_name(name)?;
        let ledger = Ledger::at(vmdir);
        self.client.settle_pending(&ledger, vmid)?;
        Ok(self.client.snapshot(&ledger, vmid, name)?)
    }

    fn restore(&self, vmdir: &Path, vm: &Vm, name: &str) -> delonix_model::Result<()> {
        let vmid = self.vmid_of(vm)?;
        validate_snapshot_name(name)?;
        let ledger = Ledger::at(vmdir);
        self.client.settle_pending(&ledger, vmid)?;
        Ok(self.client.rollback(&ledger, vmid, name)?)
    }

    // `_vmdir` porque o Proxmox não tem disco local nosso: os instantâneos
    // vivem no lado do servidor, indexados pelo `vmid`. O parâmetro entrou no
    // trait quando os verbos passaram a servir uma VM PARADA (v0.52.0), e serve
    // os backends que leem o overlay em disco — este não é um deles.
    fn snapshots(&self, _vmdir: &Path, vm: &Vm) -> delonix_model::Result<Vec<String>> {
        Ok(self.client.snapshots(self.vmid_of(vm)?)?)
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
pub fn register(target: Target) -> delonix_model::Result<()> {
    // Fail on a malformed target HERE, at registration, rather than at the
    // first `vm create`: the operator is looking at the configuration now.
    validate_target_url(&target.base_url)?;
    validate_node_name(&target.node)?;

    let shared: std::sync::Mutex<Option<std::sync::Arc<Client>>> = std::sync::Mutex::new(None);
    Ok(delonix_vm::register_backend(
        delonix_vm::BackendRegistration {
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
                let c = std::sync::Arc::new(
                    Client::connect(&target)
                        .map_err(|e| delonix_vm::Error::Engine(delonix_model::Error::from(e)))?,
                );
                *slot = Some(c.clone());
                Ok(Box::new(ProxmoxBackend::sharing(c)))
            }),
        },
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_com(ssh: &[&str]) -> VmConfig {
        VmConfig {
            name: "no1".into(),
            vcpus: 2,
            memory: "1G".into(),
            ssh_keys: ssh.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    /// O `sshkeys` vai PERCENT-ENCODED por nós, por cima do encoding do
    /// formulário. Medido contra um PVE 9.2 ao vivo: o valor cru é recusado com
    /// `400 … "invalid format - invalid urlencoded string"`. Escrevi primeiro o
    /// contrário, com um comentário confiante a explicar porquê — este teste
    /// existe para a próxima pessoa não repetir o raciocínio.
    #[test]
    fn as_chaves_ssh_vao_percent_encoded() {
        let f = cloud_init_form(&cfg_com(&["ssh-ed25519 AAAA+B/c teste"]));
        let sk = f
            .iter()
            .find(|(k, _)| *k == "sshkeys")
            .expect("sshkeys em falta")
            .1
            .clone();
        assert!(!sk.contains(' '), "o espaço tem de ir escapado: {sk}");
        assert!(sk.contains("%20"), "{sk}");
        assert!(
            sk.contains("%2B"),
            "o '+' tem de ir escapado, senão vira espaço: {sk}"
        );
        assert!(sk.contains("%2F"), "{sk}");
        // Duas chaves separam-se por newline, também escapada.
        let f = cloud_init_form(&cfg_com(&["ssh-ed25519 AAA a", "ssh-rsa BBB b"]));
        let sk = f.iter().find(|(k, _)| *k == "sshkeys").unwrap().1.clone();
        assert!(sk.contains("%0A"), "{sk}");
    }

    /// Sem chaves não se manda `ciuser` nem `sshkeys` — mas o `ipconfig0` vai
    /// sempre, e é ele que decide se o `ide2` de cloud-init é preciso.
    #[test]
    fn sem_chaves_so_vai_a_configuracao_de_rede() {
        let f = cloud_init_form(&cfg_com(&[]));
        assert!(
            f.iter().any(|(k, v)| *k == "ipconfig0" && v == "ip=dhcp"),
            "{f:?}"
        );
        assert!(
            !f.iter().any(|(k, _)| *k == "sshkeys" || *k == "ciuser"),
            "{f:?}"
        );
    }

    /// The node's REAL message (PVE 9.2, task log) is lock contention; half of
    /// it is not.
    #[test]
    fn recognises_the_nodes_lock_contention() {
        let real = Error::TaskFailed(
            "proxmox: task failed: can't lock file '/var/lock/qemu-server/lock-100.conf' - got \
             timeout"
                .into(),
        );
        assert!(is_lock_timeout(&real));
        assert!(!is_lock_timeout(&Error::TaskFailed(
            "proxmox: task failed: can't lock file '/x': Permission denied".into()
        )));
        assert!(!is_lock_timeout(&Error::TaskFailed(
            "proxmox: task failed: snapshot name 's1' already used".into()
        )));
    }

    fn lock_err() -> Error {
        Error::TaskFailed("task failed: can't lock file 'lock-100.conf' - got timeout".into())
    }

    /// Retries while the lock is busy, and stops as soon as it clears.
    #[test]
    fn retries_on_lock_contention_until_it_clears() {
        let mut n = 0;
        let r = retry_on_lock("stop", Duration::from_secs(5), Duration::ZERO, || {
            n += 1;
            if n < 3 {
                Err(lock_err())
            } else {
                Ok(n)
            }
        });
        assert_eq!(r.unwrap(), 3);
    }

    /// Any OTHER failure returns at once: retrying a 400 is hammering the node.
    #[test]
    fn other_failures_are_not_retried() {
        let mut n = 0;
        let r: Result<()> = retry_on_lock("stop", Duration::from_secs(5), Duration::ZERO, || {
            n += 1;
            Err(Error::HttpStatus("proxmox: returned HTTP 400".into()))
        });
        assert!(r.is_err());
        assert_eq!(n, 1);
    }

    /// Bounded: a lock that never clears becomes an error, not a hung command —
    /// and the error says it waited and how many times it tried.
    #[test]
    fn lock_contention_has_a_ceiling() {
        let mut n = 0;
        // A ceiling of 40 backoffs, not 6: with 30ms against 5ms a parallel test
        // run could spend the whole budget in the first attempt and never retry —
        // the assertion below then failed intermittently for a reason that has
        // nothing to do with the retry logic.
        let r: Result<()> = retry_on_lock(
            "stop",
            Duration::from_millis(200),
            Duration::from_millis(5),
            || {
                n += 1;
                Err(lock_err())
            },
        );
        let e = r.unwrap_err().to_string();
        assert!(n > 1, "it must have retried at least once");
        assert!(e.contains("got timeout") && e.contains("attempts"), "{e}");
    }

    /// A snapshot name already taken is a conflict (5), as on libvirt.
    #[test]
    fn a_taken_snapshot_name_is_a_conflict() {
        assert!(taken_snapshot(100, "s1").is_conflict());
    }

    /// Every key goes ONCE in the create body. `ipconfig0` went twice (fixed
    /// list + `cloud_init_form`) and the node refused EVERY create with `400
    /// ipconfig0: type check ('string') failed - got ARRAY` — measured on a PVE
    /// 9.2. The test above only asked whether the key was there, and a
    /// duplicate is.
    #[test]
    fn the_create_sends_each_key_once() {
        let cases = [
            cfg_com(&[]),
            cfg_com(&["ssh-ed25519 AAAA x"]),
            VmConfig {
                static_ip: Some("10.0.0.5/24,gw=10.0.0.1".into()),
                hostname: Some("other".into()),
                ..cfg_com(&["ssh-ed25519 AAAA x"])
            },
            VmConfig {
                cloud_init: Some(false),
                ..cfg_com(&[])
            },
        ];
        for cfg in &cases {
            let f = create_form(100, &cfg.name, cfg, "local-lvm", 8, "virtio,bridge=vmbr0");
            let mut keys: Vec<&str> = f.iter().map(|(k, _)| *k).collect();
            keys.sort_unstable();
            let n = keys.len();
            keys.dedup();
            assert_eq!(n, keys.len(), "key repeated in the create: {f:?}");
        }
        // An explicit hostname wins over the VM name, as in configure_clone.
        let f = create_form(100, "no1", &cases[2], "local-lvm", 8, "virtio");
        assert!(f.contains(&("name", "other".into())), "{f:?}");
        assert!(
            f.contains(&("ipconfig0", "ip=10.0.0.5/24,gw=10.0.0.1".into())),
            "{f:?}"
        );
        // An appliance gets neither cloud-init network config nor a drive.
        let f = create_form(100, "no1", &cases[3], "local-lvm", 8, "virtio");
        assert!(
            !f.iter().any(|(k, _)| *k == "ipconfig0" || *k == "ide2"),
            "{f:?}"
        );
    }

    /// Um appliance não leva cloud-init nenhum — e a lista VAZIA é o que faz o
    /// chamador não lhe anexar um drive de cloud-init que ninguém lê.
    #[test]
    fn um_appliance_nao_leva_cloud_init_nenhum() {
        let cfg = VmConfig {
            cloud_init: Some(false),
            ..cfg_com(&["ssh-ed25519 AAAA x"])
        };
        assert!(cloud_init_form(&cfg).is_empty());
    }

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
            ) -> delonix_model::Result<Boot> {
                unreachable!()
            }
            fn is_running(&self, _: &Vm) -> bool {
                false
            }
            fn ip(&self, _: &Vm) -> Option<String> {
                None
            }
            fn stop(&self, _: &Path, _: &Vm) -> delonix_model::Result<()> {
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
            task_timeout: TASK_TIMEOUT,
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
        // Built the way `send` builds them, so the test reads the real path.
        let st = |code: u16, body: &str| {
            classify_status(
                reqwest::StatusCode::from_u16(code).unwrap(),
                "https://pve",
                "/nodes/pve/qemu/100/config",
                body,
            )
        };
        assert!(is_unauthorized(&st(401, "bad ticket")));
        assert!(!is_unauthorized(&st(
            500,
            "QEMU guest agent is not running"
        )));
        // A body that merely mentions the number is not a 401.
        assert!(!is_unauthorized(&st(500, "disk 401 is missing")));
        assert!(!is_unauthorized(&st(403, "permission denied")));
    }

    /// The CLASS of a failure is what an exit code and a reconciler read; the
    /// text keeps the shape the CLI always printed.
    #[test]
    fn every_http_status_lands_in_its_class() {
        let st = |code: u16, body: &str| {
            classify_status(
                reqwest::StatusCode::from_u16(code).unwrap(),
                "https://pve",
                "/nodes/pve/qemu/100/config?x=1",
                body,
            )
        };
        assert!(matches!(st(401, ""), Error::Unauthorized(_)));
        assert!(matches!(st(403, ""), Error::Forbidden(_)));
        assert!(matches!(st(404, ""), Error::NotFound(_)));
        assert!(matches!(st(409, ""), Error::Conflict(_)));
        assert!(matches!(st(400, ""), Error::BadRequest(_)));
        assert!(matches!(st(422, ""), Error::BadRequest(_)));
        assert!(matches!(st(502, ""), Error::Unavailable(_)));
        assert!(matches!(st(503, ""), Error::Unavailable(_)));
        assert!(matches!(st(504, ""), Error::Unavailable(_)));
        // Proxmox says these two with a 500; the body carries the verdict.
        let missing = st(
            500,
            "Configuration file 'nodes/pve/qemu-server/100.conf' does not exist\n",
        );
        assert!(matches!(missing, Error::NotFound(_)), "{missing}");
        assert!(
            missing.to_string().contains("/nodes/pve/qemu/100/config"),
            "names the route"
        );
        assert!(!missing.to_string().contains("?x=1"), "without the query");
        assert!(matches!(
            st(500, "VM 100 already exists"),
            Error::Conflict(_)
        ));
        let generic = st(500, "unable to open file");
        assert!(matches!(generic, Error::HttpStatus(_)));
        assert_eq!(
            generic.to_string(),
            "proxmox: https://pve returned HTTP 500 Internal Server Error: unable to open file"
        );
        // Through the shared class: exit 4 for a missing VM, 5 for a taken id.
        assert!(delonix_model::Error::from(st(404, "")).is_not_found());
        assert!(delonix_model::Error::from(st(409, "")).is_conflict());
    }

    #[test]
    fn a_secret_never_reaches_debug_output() {
        let t = Target {
            base_url: "https://pve:8006".into(),
            node: "pve".into(),
            auth: Auth::ApiToken {
                id: "root@pam!delonix".into(),
                secret: "s3cr3t-token-value".into(),
            },
            insecure_tls: false,
            bridge: None,
            vlan: None,
            ca_cert_pem: None,
        };
        let shown = format!("{t:?}");
        assert!(!shown.contains("s3cr3t"), "{shown}");
        assert!(
            shown.contains("root@pam!delonix"),
            "the id is not a secret: {shown}"
        );
        let p = Auth::Password {
            username: "root@pam".into(),
            password: "hunter2-pass".into(),
        };
        assert!(!format!("{p:?}").contains("hunter2"));
        let ticket = Ticket {
            ticket: "PVE:root@pam:ABC".into(),
            csrf: "csrf-value".into(),
        };
        let shown = format!("{ticket:?}");
        assert!(
            !shown.contains("ABC") && !shown.contains("csrf-value"),
            "{shown}"
        );
    }

    #[test]
    fn a_task_answer_is_a_upid_or_it_is_not_an_answer() {
        assert_eq!(
            upid_of(
                r#"{"data":"UPID:pve:0000ABCD:00001234:5F3E:qmstart:100:root@pam:"}"#,
                "start"
            )
            .unwrap(),
            "UPID:pve:0000ABCD:00001234:5F3E:qmstart:100:root@pam:"
        );
        // A string that is not a task id, a number, and no `data` at all.
        assert!(matches!(
            upid_of(r#"{"data":"ok"}"#, "start"),
            Err(Error::UnexpectedAnswer(_))
        ));
        assert!(matches!(
            upid_of(r#"{"data":100}"#, "start"),
            Err(Error::UnexpectedAnswer(_))
        ));
        assert!(matches!(
            upid_of(r#"{"data":"#, "start"),
            Err(Error::Decode(_))
        ));
    }

    #[test]
    fn the_ledger_is_written_before_the_wait_and_settled_after() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path());
        assert!(ledger.records().is_empty());
        let rec = |n: u32| TaskRecord {
            upid: format!("UPID:pve:{n:08X}:00000001:0:qmstart:{n}:root@pam:"),
            node: "pve".into(),
            action: "start".into(),
            vmid: n,
            started_unix: 1,
            state: TaskState::Submitted,
        };
        ledger.record(rec(100));
        assert_eq!(ledger.pending().len(), 1, "written before any verdict");
        ledger.settle(&rec(100).upid, TaskState::Ok);
        assert!(ledger.pending().is_empty());
        assert_eq!(ledger.records()[0].state, TaskState::Ok);
        // A settle on an unknown UPID changes nothing and does not panic.
        ledger.settle("UPID:nope", TaskState::Ok);
        assert_eq!(ledger.records().len(), 1);
        // No path: nothing persisted, nothing pending.
        let none = Ledger::none();
        none.record(rec(1));
        assert!(none.records().is_empty());
    }

    #[test]
    fn the_ledger_drops_settled_records_first_and_never_a_pending_one() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path());
        for n in 0..(LEDGER_KEEP as u32 + 10) {
            let state = if n == 3 {
                TaskState::Submitted
            } else {
                TaskState::Ok
            };
            ledger.record(TaskRecord {
                upid: format!("UPID:pve:{n:08X}::qmstart:{n}:root@pam:"),
                node: "pve".into(),
                action: "start".into(),
                vmid: n,
                started_unix: n as u64,
                state,
            });
        }
        let all = ledger.records();
        assert_eq!(all.len(), LEDGER_KEEP);
        assert!(
            all.iter()
                .any(|r| r.vmid == 3 && r.state == TaskState::Submitted),
            "the pending one survived"
        );
        assert!(
            !all.iter().any(|r| r.vmid == 0),
            "the oldest settled one went first"
        );
    }

    #[test]
    fn a_timeout_is_recorded_as_timed_out_not_as_failed() {
        assert_eq!(state_of(&Ok(())), TaskState::Ok);
        assert_eq!(
            state_of(&Err(Error::TaskTimeout("x".into()))),
            TaskState::TimedOut
        );
        assert_eq!(
            state_of(&Err(Error::TaskFailed("proxmox: task failed: y".into()))),
            TaskState::Failed {
                reason: "proxmox: task failed: y".into()
            }
        );
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
