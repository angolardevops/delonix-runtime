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
mod sdn;
pub use sdn::{
    validate_fabric_id, validate_ip, validate_mac, DhcpRange, FabricProtocol, IpamKind,
    SubnetOptions, ZoneOptions,
};

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

/// The environment variable the CLI reads into [`ClientOptions::trace_routes`]
/// (`DELONIX_PROXMOX_TRACE_ROUTES=<file>`). The library itself never reads the
/// environment; the composition root does, once.
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

/// Poll interval for [`Client::agent_exec_wait`]. Fixed and short, unlike
/// [`POLL_MIN`]/[`POLL_MAX`]'s backoff: a guest-agent exec is not a node
/// task that can run for minutes, it is a process inside the guest that is
/// typically done in well under a second, and this reads no shared ledger —
/// there is nothing here for a backoff to protect against.
const AGENT_EXEC_POLL: Duration = Duration::from_millis(200);

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
    /// A file every request appends `METHOD /path` to — the numerator of the
    /// coverage matrix (`scripts/proxmox_api_inventory.py --trace`), read from
    /// what a run actually sent and not from the source. `None` traces nothing.
    pub trace_routes: Option<PathBuf>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(120),
            task_timeout: TASK_TIMEOUT,
            trace_routes: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Wrapped<T> {
    data: T,
}

/// One cloud-init config key's pending state, as [`Client::cloudinit_pending`]
/// reports it. See that function's doc comment for where this shape comes
/// from and its confirmation status.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CloudInitPendingKey {
    /// The config key this entry is about (`ipconfig0`, `sshkeys`, `citype`,
    /// `ciuser`, `nameserver`, `searchdomain`, …).
    pub key: String,
    /// The value the node currently has applied — absent if the key has
    /// never been set.
    #[serde(default)]
    pub value: Option<String>,
    /// A new value staged but not yet regenerated into the disk image.
    #[serde(default)]
    pub pending: Option<String>,
    /// Set (to `1`) when the key is staged for deletion rather than a new
    /// value.
    #[serde(default)]
    pub delete: Option<i64>,
}

impl CloudInitPendingKey {
    /// Whether [`Client::cloudinit_regenerate`] still has work to do for this
    /// key: a staged value or a staged deletion, neither yet baked into the
    /// disk image.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some() || self.delete.is_some()
    }
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
    /// `POST …/config` — the node's ASYNCHRONOUS config API. It answers a UPID
    /// when it forks a worker and `null` when it applied the change inline, so
    /// this is the one kind [`Client::task_or_done`] is for.
    Configure,
    DeleteSnapshot,
    /// `PUT …/resize` — grows ONE disk of a VM. Grow only: the node refuses a
    /// shrink, and this backend refuses it earlier, by name (see
    /// [`ProxmoxBackend::boot`]).
    Resize,
    /// `POST …/template` — turns a stopped VM into a clone source.
    Template,
    /// `POST …/vzdump` — a crash-consistent backup of the VM's disk to a
    /// storage, taken WITHOUT stopping the guest.
    Backup,
    /// `DELETE …/storage/{storage}/content/{volume}` — removes one backup
    /// archive.
    DeleteBackup,
    /// `POST …/qemu` with `archive=<volid>` instead of the disk parameters —
    /// Proxmox's `qmrestore`, the same route [`Client::create_vm`] calls,
    /// asked a different question.
    Restore,
    /// `POST …/move_disk` — moves one disk to a different storage, or
    /// re-formats it in place.
    MoveDisk,
    /// `PUT …/unlink` — detaches (and by default destroys) one or more disks.
    /// The schema declares `returns: null`, so [`Client::unlink`] goes
    /// through [`Client::task_or_done`] and this worker type may never
    /// actually be seen.
    Unlink,
    /// `PUT …/cloudinit` — regenerates the cloud-init disk image from
    /// whatever is currently staged. Same `returns: null`/`protected: 1`
    /// shape as [`TaskKind::Unlink`]; see [`Client::cloudinit_regenerate`].
    RegenerateCloudInit,
    /// `PUT …/firewall/options` — turns the VM's own (node-side) firewall on
    /// or off. Not this crate's own SDN firewall (`delonix-sdn`, `net
    /// ingress`/`net egress`) — this is Proxmox's NATIVE per-VM firewall,
    /// enforced by the node's `pve-firewall` daemon from rules it stores in
    /// `/etc/pve/firewall/<vmid>.fw`. See [`Client::set_firewall_enabled`].
    FirewallOptions,
    /// `POST …/firewall/rules` — appends a rule to the node's own firewall.
    AddFirewallRule,
    /// `PUT …/firewall/rules/{pos}` — changes one field or more of an
    /// existing rule, addressed by its position.
    UpdateFirewallRule,
    /// `DELETE …/firewall/rules/{pos}`.
    DeleteFirewallRule,
    /// `POST …/firewall/aliases` — names a CIDR or address inside the VM's
    /// own firewall namespace, so a rule can say `+myalias` instead of the
    /// raw value. Scoped to this ONE VM — not `/cluster/firewall/aliases`,
    /// which is cluster-wide and out of scope here (see
    /// `docs/proxmox/matrix-9.2.2.md`, marked `unsupported-by-design`).
    AddFirewallAlias,
    /// `PUT …/firewall/aliases/{name}` — changes the `cidr`/`comment` of an
    /// alias already named. `None` means "leave it as the node already has
    /// it", the same convention [`Self::update_firewall_rule`] uses.
    UpdateFirewallAlias,
    /// `DELETE …/firewall/aliases/{name}`.
    DeleteFirewallAlias,
    /// `POST …/firewall/ipset` — creates a new, empty named IP set inside
    /// the VM's own firewall namespace. Entries are added to it one at a
    /// time afterwards (see [`AddFirewallIpsetCidr`]).
    ///
    /// [`AddFirewallIpsetCidr`]: TaskKind::AddFirewallIpsetCidr
    CreateFirewallIpset,
    /// `DELETE …/firewall/ipset/{name}` — removes the whole set. The node's
    /// own business logic decides whether a set still referenced by a rule
    /// may be removed; not pre-empted here.
    DeleteFirewallIpset,
    /// `POST …/firewall/ipset/{name}` — adds one CIDR/address entry to a set
    /// already created. Note this is the SAME path as
    /// [`CreateFirewallIpset`]'s `GET`/`DELETE` — the set is addressed by
    /// `{name}` in the path, the entry is addressed by `cidr` in the form
    /// body, and only the HTTP method tells the two POSTs (create-the-set vs
    /// add-an-entry) apart from each other by their SHAPE, not their path.
    ///
    /// [`CreateFirewallIpset`]: TaskKind::CreateFirewallIpset
    AddFirewallIpsetCidr,
    /// `PUT …/firewall/ipset/{name}/{cidr}` — changes the `comment`/`nomatch`
    /// of an entry already in the set. The entry's `cidr` itself is the URL
    /// path's own identifier and cannot be renamed in place through this
    /// route; removing and re-adding is the node's own way to do that.
    UpdateFirewallIpsetCidr,
    /// `DELETE …/firewall/ipset/{name}/{cidr}`.
    DeleteFirewallIpsetCidr,
    /// `POST /cluster/sdn/zones` — stages a new SDN zone. Cluster-scoped, not
    /// VM-scoped (see [`sdn::SDN_VMID`]). Writes to the PENDING configuration
    /// only; nothing on any node changes until [`Client::apply_sdn`].
    CreateSdnZone,
    /// `PUT /cluster/sdn/zones/{zone}` — stages a change to an existing
    /// zone's `mtu`. Same PENDING-only caveat as [`TaskKind::CreateSdnZone`].
    UpdateSdnZone,
    /// `DELETE /cluster/sdn/zones/{zone}` — same PENDING-only caveat.
    DeleteSdnZone,
    /// `POST /cluster/sdn/vnets` — stages a new SDN vnet inside a zone. Same
    /// PENDING-only caveat.
    CreateSdnVnet,
    /// `PUT /cluster/sdn/vnets/{vnet}` — stages a change to an existing
    /// vnet's `alias`. Same PENDING-only caveat as [`TaskKind::CreateSdnVnet`].
    UpdateSdnVnet,
    /// `DELETE /cluster/sdn/vnets/{vnet}` — same PENDING-only caveat.
    DeleteSdnVnet,
    /// `POST /cluster/sdn/vnets/{vnet}/subnets` — stages a new subnet inside
    /// a vnet. Same PENDING-only caveat.
    CreateSdnSubnet,
    /// `PUT /cluster/sdn/vnets/{vnet}/subnets/{subnet}` — stages a change to
    /// an existing subnet's `gateway`. Same PENDING-only caveat.
    UpdateSdnSubnet,
    /// `DELETE /cluster/sdn/vnets/{vnet}/subnets/{subnet}` — same
    /// PENDING-only caveat.
    DeleteSdnSubnet,
    /// `POST/PUT/DELETE /cluster/sdn/ipams[/{ipam}]` — an external IPAM
    /// controller entry. STAGED, and VERIFIED by the node against the
    /// controller's URL before it is (see [`Client::create_sdn_ipam`]).
    CreateSdnIpam,
    UpdateSdnIpam,
    DeleteSdnIpam,
    /// `POST/PUT/DELETE /cluster/sdn/dns[/{dns}]` — a DNS controller entry,
    /// same STAGED-and-verified shape as the IPAM ones.
    CreateSdnDns,
    UpdateSdnDns,
    DeleteSdnDns,
    /// `POST/PUT/DELETE /cluster/sdn/fabrics/fabric[/{id}]` — a fabric.
    /// STAGED; measured to answer an empty STRING, never a UPID (see
    /// [`upid_or_done`]).
    CreateSdnFabric,
    UpdateSdnFabric,
    DeleteSdnFabric,
    /// `POST/PUT/DELETE /cluster/sdn/fabrics/node/{fabric_id}[/{node_id}]` —
    /// a node's membership of a fabric. Same shape as the fabric itself.
    CreateSdnFabricNode,
    UpdateSdnFabricNode,
    DeleteSdnFabricNode,
    /// `POST/PUT/DELETE /cluster/sdn/vnets/{vnet}/ips` — an IPAM
    /// reservation. NOT staged: acts on the IPAM database at once, against
    /// the RUNNING subnet (see [`Client::sdn_vnet_ip_add`]).
    AddSdnIp,
    UpdateSdnIp,
    DeleteSdnIp,
    /// `PUT /cluster/sdn` (no body) — reloads the PENDING SDN configuration
    /// onto every node in the cluster. The one SDN call that genuinely forks
    /// a cluster-wide task; every other SDN write above is very likely
    /// synchronous (`null`), which is why they all go through
    /// [`Client::task_or_done`] rather than [`Client::task`] — safe either way.
    ApplySdn,
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
            TaskKind::Configure => "configure",
            TaskKind::DeleteSnapshot => "delete-snapshot",
            TaskKind::Resize => "resize",
            TaskKind::Template => "template",
            TaskKind::Backup => "backup",
            TaskKind::DeleteBackup => "delete-backup",
            TaskKind::Restore => "restore",
            TaskKind::MoveDisk => "move-disk",
            TaskKind::Unlink => "unlink",
            TaskKind::RegenerateCloudInit => "regenerate-cloudinit",
            TaskKind::FirewallOptions => "firewall-options",
            TaskKind::AddFirewallRule => "firewall-add-rule",
            TaskKind::UpdateFirewallRule => "firewall-update-rule",
            TaskKind::DeleteFirewallRule => "firewall-delete-rule",
            TaskKind::AddFirewallAlias => "firewall-add-alias",
            TaskKind::UpdateFirewallAlias => "firewall-update-alias",
            TaskKind::DeleteFirewallAlias => "firewall-delete-alias",
            TaskKind::CreateFirewallIpset => "firewall-create-ipset",
            TaskKind::DeleteFirewallIpset => "firewall-delete-ipset",
            TaskKind::AddFirewallIpsetCidr => "firewall-add-ipset-cidr",
            TaskKind::UpdateFirewallIpsetCidr => "firewall-update-ipset-cidr",
            TaskKind::DeleteFirewallIpsetCidr => "firewall-delete-ipset-cidr",
            TaskKind::CreateSdnZone => "create-sdn-zone",
            TaskKind::UpdateSdnZone => "update-sdn-zone",
            TaskKind::DeleteSdnZone => "delete-sdn-zone",
            TaskKind::CreateSdnVnet => "create-sdn-vnet",
            TaskKind::UpdateSdnVnet => "update-sdn-vnet",
            TaskKind::DeleteSdnVnet => "delete-sdn-vnet",
            TaskKind::CreateSdnSubnet => "create-sdn-subnet",
            TaskKind::UpdateSdnSubnet => "update-sdn-subnet",
            TaskKind::DeleteSdnSubnet => "delete-sdn-subnet",
            TaskKind::CreateSdnIpam => "create-sdn-ipam",
            TaskKind::UpdateSdnIpam => "update-sdn-ipam",
            TaskKind::DeleteSdnIpam => "delete-sdn-ipam",
            TaskKind::CreateSdnDns => "create-sdn-dns",
            TaskKind::UpdateSdnDns => "update-sdn-dns",
            TaskKind::DeleteSdnDns => "delete-sdn-dns",
            TaskKind::CreateSdnFabric => "create-sdn-fabric",
            TaskKind::UpdateSdnFabric => "update-sdn-fabric",
            TaskKind::DeleteSdnFabric => "delete-sdn-fabric",
            TaskKind::CreateSdnFabricNode => "create-sdn-fabric-node",
            TaskKind::UpdateSdnFabricNode => "update-sdn-fabric-node",
            TaskKind::DeleteSdnFabricNode => "delete-sdn-fabric-node",
            TaskKind::AddSdnIp => "add-sdn-ip",
            TaskKind::UpdateSdnIp => "update-sdn-ip",
            TaskKind::DeleteSdnIp => "delete-sdn-ip",
            TaskKind::ApplySdn => "apply-sdn",
        }
    }

    /// The `type` of the worker Proxmox VE registers for this operation
    /// (the `fork_worker` names of `PVE::API2::Qemu`). `qmcreate`, `qmstart`,
    /// `qmstop`, `qmsnapshot` and `qmdestroy` are the names ADR-0008's spike
    /// saw in a live node's task log; `qmclone` and `qmrollback` follow the
    /// same naming and are exercised by the mock node, not yet by a real one.
    ///
    /// **The four firewall names below are an UNCONFIRMED GUESS, likely never
    /// even exercised.** Nothing in this crate has been run against a real
    /// node since they were added — no live suite has reached them yet. Every
    /// firewall write here goes through [`Client::task_or_done`] on the
    /// EXPECTATION, not a measurement, that `pve-firewall` applies a
    /// rule/option change inline (a `null` answer) rather than forking a
    /// worker — it picks up `/etc/pve/firewall/<vmid>.fw` on its own
    /// schedule, which is a different shape from `PVE::API2::Qemu`'s other
    /// config writes. `recover_lost_answer` only ever consults this name
    /// after a transport failure on a firewall write, so a wrong guess here
    /// costs a lost-answer recovery that falls through to its probe instead
    /// of finding a real task — never a wrong answer on the ordinary path.
    /// Correct the name from what `GET /nodes/{node}/tasks` actually shows,
    /// the first time a real node is measured forking a worker for one of
    /// these.
    fn worker_type(self) -> &'static str {
        match self {
            TaskKind::Create => "qmcreate",
            TaskKind::Clone => "qmclone",
            TaskKind::Start => "qmstart",
            TaskKind::Stop => "qmstop",
            TaskKind::Snapshot => "qmsnapshot",
            TaskKind::Rollback => "qmrollback",
            TaskKind::Destroy => "qmdestroy",
            TaskKind::Configure => "qmconfig",
            TaskKind::DeleteSnapshot => "qmdelsnapshot",
            // `PVE::API2::Qemu` forks `resize` (no `qm` prefix) and `qmtemplate`;
            // both names were read back from a live PVE 9.2.2 task log
            // (`docs/proxmox/trace-9.2.2.routes`), not assumed.
            TaskKind::Resize => "resize",
            TaskKind::Template => "qmtemplate",
            // `PVE::API2::VZDump`/`PVE::API2::Storage::Content` fork these under
            // `vzdump` and `imgdel` (`PVE::AbstractConfig::fork_worker` names read
            // from a live PVE 9.2.2 task log, `docs/proxmox/trace-9.2.2.routes`,
            // not assumed).
            TaskKind::Backup => "vzdump",
            TaskKind::DeleteBackup => "imgdel",
            // `PVE::API2::Qemu`'s restore branch of `POST …/qemu` forks
            // `qmrestore` (read from a live PVE 9.2.2 task log,
            // `docs/proxmox/trace-9.2.2.routes`, not assumed).
            TaskKind::Restore => "qmrestore",
            // Read from a live PVE 9.2.2 task log (`docs/proxmox/trace-9.2.2.routes`),
            // not assumed — confirms the original `qmclone`/`qmtemplate`-analogy guess.
            TaskKind::MoveDisk => "qmmove",
            // NEVER OBSERVED on a live node, and that is the confirmed fact: a live
            // run of `unlink` against PVE 9.2.2 forked no task at all (the schema's
            // `returns: null` was right) — `qmdelete` is a guess that may be
            // permanently dead code, kept only so the match stays exhaustive.
            TaskKind::Unlink => "qmdelete",
            // UNCONFIRMED GUESS — no live run has reached this route yet. The
            // schema shares `unlink`'s exact `returns: null`/`protected: 1`
            // shape, and reading the operation's own name (`PVE::API2::Qemu`'s
            // cloud-init regenerate handler locks the VM config and rewrites
            // the drive inline, the same pattern as a plain `POST …/config`)
            // makes an inline `null` answer, forking no task, the likely
            // outcome here too — `qmcloudinit` follows the `qm<verb>` naming
            // every OBSERVED name above uses (`qmcreate`, `qmconfig`,
            // `qmdelsnapshot`, …), kept only so the match stays exhaustive.
            // Correct it from `GET /nodes/{node}/tasks` the first time a real
            // node is measured forking a worker for this route.
            TaskKind::RegenerateCloudInit => "qmcloudinit",
            // NEVER OBSERVED on a live node, and that is the confirmed fact: a live
            // run of all four against PVE 9.2.2 forked no task for any of them
            // (every firewall write applied inline) — `pvefw` is a guess that may
            // be permanently dead code, kept only so the match stays exhaustive.
            TaskKind::FirewallOptions
            | TaskKind::AddFirewallRule
            | TaskKind::UpdateFirewallRule
            | TaskKind::DeleteFirewallRule => "pvefw",
            // NEVER OBSERVED on a live node — and unlike the four siblings just
            // above, that is an ASSUMPTION, not yet a confirmed fact: no live
            // suite has reached the VM firewall's aliases/ipset routes. Assumed
            // by analogy with `FirewallOptions`/`AddFirewallRule`/
            // `UpdateFirewallRule`/`DeleteFirewallRule` (all four measured
            // applying inline against a real PVE 9.2.2 node, see the comment
            // just above) to apply inline too — `pve-firewall` rewrites the
            // whole of `/etc/pve/firewall/<vmid>.fw` on its own schedule for
            // every one of these routes, not per-route, so there is no reason
            // for aliases/ipset to behave differently from rules/options here.
            // Every write below goes through `Client::task_or_done` on that
            // assumption; a wrong guess costs a lost-answer recovery falling
            // through to its probe, never a wrong answer on the ordinary path.
            // `pvefw` (not a distinct name per route) is the same guess the
            // four siblings above use, kept only so the match stays exhaustive.
            TaskKind::AddFirewallAlias
            | TaskKind::UpdateFirewallAlias
            | TaskKind::DeleteFirewallAlias
            | TaskKind::CreateFirewallIpset
            | TaskKind::DeleteFirewallIpset
            | TaskKind::AddFirewallIpsetCidr
            | TaskKind::UpdateFirewallIpsetCidr
            | TaskKind::DeleteFirewallIpsetCidr => "pvefw",
            // NEVER OBSERVED on a live node, and that is the confirmed fact: a
            // live run against PVE 9.2.2 forked no task for any of the four
            // (zone/vnet create/delete all apply inline) — these are guesses
            // that may be permanently dead code, kept only so the match stays
            // exhaustive.
            TaskKind::CreateSdnZone => "sdnzonecreate",
            TaskKind::DeleteSdnZone => "sdnzonedelete",
            TaskKind::CreateSdnVnet => "sdnvnetcreate",
            TaskKind::DeleteSdnVnet => "sdnvnetdelete",
            // UNCONFIRMED GUESS, not yet exercised against a live node — unlike
            // the four zone/vnet names just above, no live run has reached
            // `PUT /cluster/sdn/zones/{zone}`, `PUT /cluster/sdn/vnets/{vnet}`,
            // `POST/PUT/DELETE /cluster/sdn/vnets/{vnet}/subnets[/{subnet}]`
            // yet, so "applies inline" for these five is an ANALOGY to the
            // four confirmed ones, not a measurement of its own. `task_or_done`
            // (not `task`) is what makes the guess safe either way: a `null`
            // answer is read as "applied inline" and a real UPID is still
            // waited on correctly if one of these five turns out to fork a
            // worker after all. Correct these names from what
            // `GET /nodes/{node}/tasks` actually shows the first time a live
            // run reaches one of them (`scripts/proxmox_api_inventory.py
            // --trace`, ADR-0049 D2).
            TaskKind::UpdateSdnZone => "sdnzoneupdate",
            TaskKind::UpdateSdnVnet => "sdnvnetupdate",
            TaskKind::CreateSdnSubnet => "sdnsubnetcreate",
            TaskKind::UpdateSdnSubnet => "sdnsubnetupdate",
            TaskKind::DeleteSdnSubnet => "sdnsubnetdelete",
            // NEVER OBSERVED, and measured NOT to fork: a live run against PVE
            // 9.2.2 (2026-09-25) had every one of these fifteen writes answer
            // inline — the IPAM/DNS/IP ones with `null`, the fabric ones with an
            // empty string — so these names are placeholders that keep the
            // match exhaustive, never something a ledger has recorded.
            TaskKind::CreateSdnIpam => "sdnipamcreate",
            TaskKind::UpdateSdnIpam => "sdnipamupdate",
            TaskKind::DeleteSdnIpam => "sdnipamdelete",
            TaskKind::CreateSdnDns => "sdndnscreate",
            TaskKind::UpdateSdnDns => "sdndnsupdate",
            TaskKind::DeleteSdnDns => "sdndnsdelete",
            TaskKind::CreateSdnFabric => "sdnfabriccreate",
            TaskKind::UpdateSdnFabric => "sdnfabricupdate",
            TaskKind::DeleteSdnFabric => "sdnfabricdelete",
            TaskKind::CreateSdnFabricNode => "sdnfabricnodecreate",
            TaskKind::UpdateSdnFabricNode => "sdnfabricnodeupdate",
            TaskKind::DeleteSdnFabricNode => "sdnfabricnodedelete",
            TaskKind::AddSdnIp => "sdnipadd",
            TaskKind::UpdateSdnIp => "sdnipupdate",
            TaskKind::DeleteSdnIp => "sdnipdelete",
            // Read from a live PVE 9.2.2 task log (`docs/proxmox/trace-9.2.2.routes`),
            // not assumed: `PUT /cluster/sdn` forks `reloadnetworkall`, not the
            // `srvreload` this guess was originally written as.
            TaskKind::ApplySdn => "reloadnetworkall",
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

/// The outcome of a guest command [`Client::agent_exec`] started, as
/// [`Client::agent_exec_status`] reads it.
///
/// Distinct from [`TaskState`] on purpose: a guest exec is not a node task
/// (see the doc comment on [`Client::agent_exec`]) and has no ledger entry,
/// so it does not belong in that enum's `Submitted`/`Ok`/`Failed`/`TimedOut`
/// vocabulary, which a reader would reasonably take as "this went through
/// the ledger".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentExecStatus {
    /// The guest has not reported completion yet.
    Running,
    /// The guest process is over.
    Finished {
        exit_code: i64,
        stdout: String,
        stderr: String,
        /// Set when a signal killed the process instead of it exiting on
        /// its own — `exit_code` is then not meaningful.
        signal: Option<i64>,
    },
}

/// The raw shape of `GET …/agent/exec-status`.
///
/// `exitcode`/`out-data`/`err-data` are absent, not zero/empty, while the
/// guest process is still running (`exited == 0`) — `Option`, not a default
/// that would read as "finished with nothing to say".
#[derive(Debug, Deserialize)]
struct AgentExecStatusBody {
    exited: i64,
    #[serde(default, rename = "exitcode")]
    exit_code: Option<i64>,
    #[serde(default, rename = "out-data")]
    out_data: Option<String>,
    #[serde(default, rename = "err-data")]
    err_data: Option<String>,
    #[serde(default)]
    signal: Option<i64>,
}

/// Turns the raw exec-status shape into the outcome [`Client::agent_exec_status`]
/// reports. Pure, so the running/finished boundary — `exited == 0` means
/// running, anything else means the fields below are meaningful — is a fact
/// a test can check directly, the same discipline [`task_verdict`] applies
/// to a node task's `status`/`exitstatus`.
fn agent_exec_status_of(body: AgentExecStatusBody) -> AgentExecStatus {
    if body.exited == 0 {
        return AgentExecStatus::Running;
    }
    AgentExecStatus::Finished {
        exit_code: body.exit_code.unwrap_or(0),
        stdout: body.out_data.unwrap_or_default(),
        stderr: body.err_data.unwrap_or_default(),
        signal: body.signal,
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
    trace_routes: Option<PathBuf>,
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
            trace_routes: opts.trace_routes,
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
        trace_route(self.trace_routes.as_deref(), method, path);
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

    fn put_form(&self, path: &str, form: &[(&str, &str)]) -> Result<String> {
        let url = self.url(path);
        self.send_authed("PUT", path, || self.http.put(&url).form(form))
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

    /// Whether the QEMU guest agent answers, for VM `vmid`.
    ///
    /// `Ok(false)` — never an [`Err`] — for the node's own "QEMU guest agent
    /// is not running": that is the ordinary state of any guest that has not
    /// booted an agent yet (a plain cloud image, or one still coming up), the
    /// same case [`ProxmoxBackend::ip`]'s doc comment already treats as a
    /// first-class, non-failure answer — `vm ls` cannot afford a scary line
    /// for every guest without one. Any OTHER failure (the node unreachable,
    /// the credential revoked) still propagates: those are "could not ask",
    /// not "no agent".
    ///
    /// **Not a node task** (see `allowed_outside_task` in the test module):
    /// the agent answers inline, there is no UPID to wait on.
    pub fn agent_ping(&self, vmid: u32) -> Result<bool> {
        match self.post_form(
            &format!("/nodes/{}/qemu/{vmid}/agent/ping", self.node),
            &[],
            true,
        ) {
            Ok(_) => Ok(true),
            Err(e) if is_agent_not_running(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Runs `argv[0] argv[1] …` inside the guest through the QEMU agent, and
    /// returns the pid [`Client::agent_exec_status`] polls for the outcome.
    ///
    /// **Not a node task.** The work runs inside the GUEST — the agent
    /// answers with a pid inline, and there is nothing on the node for a
    /// worker to do (see `allowed_outside_task` in the test module).
    ///
    /// `command` is a REPEATED form field, one per argv element: the agent
    /// takes an argv array, never a shell string, so `["ls", "-la", "/tmp"]`
    /// sends three separate `command=` pairs. Joining them into one
    /// `"ls -la /tmp"` string would hand the agent a single argument that
    /// happens to contain spaces, not three arguments.
    pub fn agent_exec(&self, vmid: u32, argv: &[&str]) -> Result<u32> {
        let form: Vec<(&str, &str)> = argv.iter().map(|a| ("command", *a)).collect();
        let body = self.post_form(
            &format!("/nodes/{}/qemu/{vmid}/agent/exec", self.node),
            &form,
            true,
        )?;
        let w: Wrapped<serde_json::Value> = parse(&body, "agent exec")?;
        w.data
            .get("pid")
            .and_then(|p| p.as_u64())
            .map(|p| p as u32)
            .ok_or_else(|| {
                Error::UnexpectedAnswer(format!(
                    "proxmox: agent exec on VM {vmid} did not answer with a pid: {}",
                    truncate_chars(&body, 200)
                ))
            })
    }

    /// Reads the outcome of a pid [`Client::agent_exec`] started.
    ///
    /// `exited` is `0` while the guest process is still running —
    /// `exitcode`/`out-data`/`err-data` are meaningless (and often absent)
    /// until it is `1`. Reading them regardless would report a stale or
    /// zero exit code as the real one.
    pub fn agent_exec_status(&self, vmid: u32, pid: u32) -> Result<AgentExecStatus> {
        let body = self.get(&format!(
            "/nodes/{}/qemu/{vmid}/agent/exec-status?pid={pid}",
            self.node
        ))?;
        let w: Wrapped<AgentExecStatusBody> = parse(&body, "agent exec-status")?;
        Ok(agent_exec_status_of(w.data))
    }

    /// Runs `argv` in the guest and polls until it finishes or `timeout`
    /// elapses.
    ///
    /// Shaped like [`Client::wait_task`] — issue once, poll on a short
    /// interval, give up at a hard ceiling — but it is not the same loop: a
    /// guest exec is not a node task, has no ledger entry, and nothing here
    /// is ever retried or recorded. A caller that wants those guarantees is
    /// asking the wrong primitive.
    pub fn agent_exec_wait(
        &self,
        vmid: u32,
        argv: &[&str],
        timeout: Duration,
    ) -> Result<AgentExecStatus> {
        let pid = self.agent_exec(vmid, argv)?;
        let deadline = Instant::now() + timeout;
        loop {
            match self.agent_exec_status(vmid, pid)? {
                AgentExecStatus::Running => {}
                done => return Ok(done),
            }
            if Instant::now() >= deadline {
                return Err(Error::TaskTimeout(format!(
                    "proxmox: guest command (pid {pid}) on VM {vmid} was still running after {}s \
                     — giving up. It may still be running in the guest; nothing here was rolled back",
                    timeout.as_secs()
                )));
            }
            std::thread::sleep(AGENT_EXEC_POLL);
        }
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

    /// Restores a backup archive into a fresh VM — Proxmox's `qmrestore`,
    /// the same route [`Client::create_vm`] calls (`POST …/qemu`), asked a
    /// different question: `archive=<volid>` in place of the disk
    /// parameters. The node reads ostype, disks and every other setting
    /// back out of the archive's own saved config — nothing about the
    /// original VM is passed here beyond the archive itself.
    ///
    /// `vmid` MUST be free — this call never sets `force`. A restore that
    /// silently overwrote a live VM would be the worst possible way to
    /// lose one; replacing an existing vmid is the operator's decision to
    /// make explicitly, never this primitive's to assume on their behalf.
    ///
    /// `storage`: the target storage for the restored disk(s), which may
    /// differ from wherever the original VM's disk lived.
    ///
    /// The effect probe is [`Self::vm_exists`] — the same one `create_vm`
    /// uses, because a restore that lands is indistinguishable from a
    /// create that lands: both answer with a VM the node now has.
    pub fn restore_vm(
        &self,
        ledger: &Ledger,
        vmid: u32,
        archive: &str,
        storage: &str,
    ) -> Result<()> {
        let vmid_s = vmid.to_string();
        self.task(
            ledger,
            vmid,
            TaskKind::Restore,
            || {
                self.post_form(
                    &format!("/nodes/{}/qemu", self.node),
                    &[
                        ("vmid", vmid_s.as_str()),
                        ("archive", archive),
                        ("storage", storage),
                    ],
                    true,
                )
            },
            Some(&|| self.vm_exists(vmid)),
        )
    }

    /// Does the node have a VM with this id? `NotFound` is the answer `false`,
    /// not a failure.
    pub fn vm_exists(&self, vmid: u32) -> Result<bool> {
        match self.config(vmid) {
            Ok(_) => Ok(true),
            Err(Error::NodeNotFound(_)) => Ok(false),
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
    pub fn configure_clone(&self, ledger: &Ledger, vmid: u32, cfg: &VmConfig) -> Result<()> {
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
        // `POST …/config` is the node's ASYNCHRONOUS config API (the schema
        // says `returns: string`): it answers a UPID when it forks a worker —
        // a disk change does — and `null` when it applied the change inline.
        // The first version of this call read `null` as the only answer and
        // never waited, which is the "HTTP 200 reported as done" the ADR-0049
        // rules forbid; now a UPID is waited on like every other write, and
        // only a `null` is taken as done. Same config lock, same retry.
        self.task_or_done(
            ledger,
            vmid,
            TaskKind::Configure,
            || {
                self.post_form(
                    &format!("/nodes/{}/qemu/{vmid}/config", self.node),
                    &form,
                    true,
                )
            },
            None,
        )
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

    /// The VM's boot disk as the node has it: `(key, bytes)` — `scsi0` and
    /// the bytes its `size=` says. Read from [`Self::config`], so it answers
    /// what the node RECORDED, which is the only size a resize can be judged
    /// against. A config with no sized boot disk is an unexpected answer, not
    /// a zero: the caller compares against it, and a zero would make every
    /// request look like a grow.
    pub fn boot_disk(&self, vmid: u32) -> Result<(String, u64)> {
        let cfg = self.config(vmid)?;
        boot_disk_of(&cfg).ok_or_else(|| {
            Error::UnexpectedAnswer(format!(
                "proxmox: VM {vmid} has no boot disk with a size in its config: {}",
                truncate_chars(&cfg.to_string(), 300)
            ))
        })
    }

    /// Grows `disk` of VM `vmid` to `gib` GiB (`PUT …/resize`).
    ///
    /// Grow only. The node refuses a shrink («shrinking disks is not
    /// supported») inside a task that comes back as a generic failure; this
    /// backend refuses it BEFORE anything is created, by name and with both
    /// numbers ([`ProxmoxBackend::boot`]), so this is only ever called to grow.
    /// A UPID, like every write; the effect probe reads the size back from
    /// the config, never from what the call said.
    pub fn resize_disk(&self, ledger: &Ledger, vmid: u32, disk: &str, gib: u32) -> Result<()> {
        let size = format!("{gib}G");
        let want = u64::from(gib) * GIB;
        self.task(
            ledger,
            vmid,
            TaskKind::Resize,
            || {
                self.put_form(
                    &format!("/nodes/{}/qemu/{vmid}/resize", self.node),
                    &[("disk", disk), ("size", size.as_str())],
                )
            },
            Some(&|| Ok(disk_size_of(&self.config(vmid)?, disk).is_some_and(|b| b >= want))),
        )
    }

    /// Turns a STOPPED VM into a template (`POST …/template`): the clone
    /// source `disk: template:<vmid>` names. The effect probe is the config's
    /// own `template` flag.
    pub fn mark_template(&self, ledger: &Ledger, vmid: u32) -> Result<()> {
        self.task(
            ledger,
            vmid,
            TaskKind::Template,
            || {
                self.post_form(
                    &format!("/nodes/{}/qemu/{vmid}/template", self.node),
                    &[],
                    true,
                )
            },
            Some(&|| Ok(self.config(vmid)?.get("template").and_then(|t| t.as_u64()) == Some(1))),
        )
    }

    /// The backups `storage` holds for VM `vmid` — `(volid, bytes)`, read from
    /// `GET …/storage/{storage}/content?content=backup&vmid=<vmid>`.
    ///
    /// The `vmid` filter is the NODE'S own: it answers only with archives that
    /// belong to this VM, so a second VM's backups on the same storage never
    /// show up here.
    pub fn list_backups(&self, storage: &str, vmid: u32) -> Result<Vec<(String, u64)>> {
        let body = self.get(&format!(
            "/nodes/{}/storage/{storage}/content?content=backup&vmid={vmid}",
            self.node
        ))?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "content")?;
        Ok(w.data
            .iter()
            .filter_map(|b| {
                let volid = b.get("volid")?.as_str()?.to_string();
                let size = b.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
                Some((volid, size))
            })
            .collect())
    }

    /// Backs up VM `vmid`'s disk to `storage` (`POST …/vzdump`) WITHOUT
    /// stopping it — `mode=snapshot`, the same "nothing has to stop" rule
    /// `rbackup.rs` states for the local backends, now answered by the node's
    /// own mechanism instead of a local overlay copy this backend has no
    /// disk to make (`manages_own_storage`: `write_vm_archive` refuses a
    /// Proxmox VM today, because `vm.overlay` names something on the far
    /// node, never a local file).
    ///
    /// Without a guest agent the filesystem is NOT frozen — an ordinary
    /// crash-consistent snapshot, the same guarantee vzdump gives any VM that
    /// has none. `VmBackupQuiesced` stays unclaimed until a guest with an
    /// agent proves the freeze, which this call does not attempt to force.
    ///
    /// `remove=0`: this call is a primitive, not a policy — retention is the
    /// caller's job (the same split `rbackup.rs`'s own `prune` already makes
    /// for the local kinds), and a node default that silently deleted the
    /// PREVIOUS backup would make that job impossible to do from here.
    ///
    /// The effect probe is the archive count for this vmid going up, read
    /// BEFORE the request is sent — a lost answer is reconciled against that
    /// baseline, never against a bare "does one exist" that a second run
    /// could satisfy by accident.
    pub fn backup_vm(&self, ledger: &Ledger, vmid: u32, storage: &str) -> Result<()> {
        let before = self.list_backups(storage, vmid)?.len();
        let vmid_s = vmid.to_string();
        self.task(
            ledger,
            vmid,
            TaskKind::Backup,
            || {
                self.post_form(
                    &format!("/nodes/{}/vzdump", self.node),
                    &[
                        ("vmid", vmid_s.as_str()),
                        ("storage", storage),
                        ("mode", "snapshot"),
                        ("remove", "0"),
                    ],
                    true,
                )
            },
            Some(&|| Ok(self.list_backups(storage, vmid)?.len() > before)),
        )
    }

    /// Removes one backup archive (`DELETE …/storage/{storage}/content/{volume}`).
    ///
    /// `volid` goes through the same [`urlencode`] a form value does: it is a
    /// single path segment (`{volume}`) that itself contains a `/`
    /// (`local:backup/vzdump-qemu-100-…`), and an unescaped one would be read
    /// as a second path level, not part of the id.
    pub fn delete_backup(
        &self,
        ledger: &Ledger,
        vmid: u32,
        storage: &str,
        volid: &str,
    ) -> Result<()> {
        let owned_volid = volid.to_string();
        let path = format!(
            "/nodes/{}/storage/{storage}/content/{}",
            self.node,
            urlencode(volid)
        );
        self.task(
            ledger,
            vmid,
            TaskKind::DeleteBackup,
            || self.delete(&path),
            Some(&|| {
                Ok(!self
                    .list_backups(storage, vmid)?
                    .iter()
                    .any(|(v, _)| *v == owned_volid))
            }),
        )
    }

    /// Moves `disk` (a drive key, `scsi0`) to `storage`, or re-formats it in
    /// place when `storage` is `None` (`POST …/move_disk`).
    ///
    /// `delete_source` drops the SOURCE disk reference once the move is done —
    /// without it the disk survives on the OLD storage as an unreferenced one,
    /// which is the node's own default and not what "move" means to a caller
    /// asking for a clean move with nothing left behind.
    ///
    /// A UPID, like every write here: the effect probe reads the moved disk's
    /// `storage=` back from [`Client::config`], never from what the call said.
    /// With `storage: None` there is nothing this side can tell apart from "not
    /// moved yet" (the format changed but the storage did not), so there is no
    /// probe in that case — a lost answer with no task in flight then surfaces
    /// as the transport error it is, same as [`Client::rollback`].
    pub fn move_disk(
        &self,
        ledger: &Ledger,
        vmid: u32,
        disk: &str,
        storage: Option<&str>,
        delete_source: bool,
        format: Option<&str>,
    ) -> Result<()> {
        let delete_val = if delete_source { "1" } else { "0" };
        let mut form: Vec<(&str, &str)> = vec![("disk", disk), ("delete", delete_val)];
        if let Some(s) = storage {
            form.push(("storage", s));
        }
        if let Some(f) = format {
            form.push(("format", f));
        }
        // The probe only exists when a target storage was asked for: with
        // `storage: None` (a format-only reformat) nothing this side can read
        // back distinguishes "moved" from "not yet" — the storage name does
        // not change — so a lost answer with no matching task in flight then
        // surfaces as the transport error it is, the same as `rollback`.
        let want_storage: Option<Box<dyn Fn() -> Result<bool>>> = storage.map(|s| {
            let want = s.to_string();
            let disk = disk.to_string();
            Box::new(move || {
                Ok(disk_storage_of(&self.config(vmid)?, &disk).as_deref() == Some(want.as_str()))
            }) as Box<dyn Fn() -> Result<bool>>
        });
        self.task(
            ledger,
            vmid,
            TaskKind::MoveDisk,
            || {
                self.post_form(
                    &format!("/nodes/{}/qemu/{vmid}/move_disk", self.node),
                    &form,
                    true,
                )
            },
            want_storage.as_deref(),
        )
    }

    /// Detaches one or more disks from a VM (`PUT …/unlink`), and by default
    /// destroys them — `force` removes a disk even if something else still
    /// references it (a boot order entry, another device slot).
    ///
    /// The schema declares this route's `returns: null` rather than `string`,
    /// which is the shape [`Client::task_or_done`] exists for: most calls
    /// apply inline, and a UPID — should the node ever fork one — is still
    /// waited on rather than assumed absent. The effect probe is the disk
    /// keys no longer present in [`Client::config`].
    pub fn unlink(&self, ledger: &Ledger, vmid: u32, idlist: &[&str], force: bool) -> Result<()> {
        let ids = idlist.join(",");
        let force_val = if force { "1" } else { "0" };
        let form: Vec<(&str, &str)> = vec![("idlist", ids.as_str()), ("force", force_val)];
        let keys: Vec<String> = idlist.iter().map(|s| s.to_string()).collect();
        self.task_or_done(
            ledger,
            vmid,
            TaskKind::Unlink,
            || self.put_form(&format!("/nodes/{}/qemu/{vmid}/unlink", self.node), &form),
            Some(&|| {
                let cfg = self.config(vmid)?;
                Ok(keys.iter().all(|k| cfg.get(k).is_none()))
            }),
        )
    }

    /// The RENDERED cloud-init file the node would inject at the guest's next
    /// boot (`GET …/cloudinit/dump?type=<user|network|meta>`).
    ///
    /// Genuinely different from [`Client::config`]/[`Client::configure_clone`],
    /// which only read or write the cloud-init INPUTS (`ipconfig0`, `sshkeys`,
    /// …) — nothing else in this crate reads the OUTPUT cloud-init would
    /// actually write into the guest. Not a task: the schema's `returns:
    /// string` here is the rendered file itself, read like any other plain
    /// GET (`status_current`, `config`).
    ///
    /// `kind` is refused by name when it is not one of the three the route
    /// accepts — never sent to the node as-is, which would just be a 400 with
    /// no more information than refusing it here already gives.
    pub fn cloudinit_dump(&self, vmid: u32, kind: &str) -> Result<String> {
        if !matches!(kind, "user" | "network" | "meta") {
            return Err(Error::InvalidCloudInitKind(format!(
                "proxmox: cloud-init dump type '{kind}' is not one of 'user', 'network', 'meta'"
            )));
        }
        let body = self.get(&format!(
            "/nodes/{}/qemu/{vmid}/cloudinit/dump?type={kind}",
            self.node
        ))?;
        let w: Wrapped<String> = parse(&body, "cloudinit dump")?;
        Ok(w.data)
    }

    /// The cloud-init keys the node still has PENDING — staged by
    /// [`Client::config`]/[`Client::configure_clone`] (`ipconfig0`, `sshkeys`,
    /// `citype`, …) but not yet baked into the disk image a booting guest
    /// would actually read (`GET …/qemu/{vmid}/cloudinit`).
    ///
    /// A plain read, like [`Client::cloudinit_dump`]: the schema's `returns:
    /// array` here is a list, never a UPID — nothing forks for a GET.
    ///
    /// **Measured against a live PVE 9.2.2 node, and the result is not what
    /// the route's name suggests.** A direct `ipconfig0` write through
    /// [`Client::config`] — VM stopped, then again with it running — never
    /// makes this list non-empty, before OR after [`Client::cloudinit_regenerate`]
    /// runs: [`Client::cloudinit_dump`] confirms the new address DOES reach
    /// the rendered file, so the write and the regenerate both work, but
    /// this route reports nothing about either step for that key. The
    /// general `GET …/qemu/{vmid}/pending` route shows the same picture —
    /// `ipconfig0` there carries only `value`, never a separate `pending`
    /// field, because a network config change applies immediately and
    /// never enters PVE's pending-vs-current split at all. Whatever this
    /// route DOES populate for — a `cicustom`-sourced snippet's own drift is
    /// the most likely candidate, going by the route's docs, but that is a
    /// guess, not a measurement — remains unconfirmed. [`CloudInitPendingKey`]'s
    /// shape is UNCHANGED from a guess (this crate's schema extract has no
    /// item shape for any route to confirm it against), so a live answer
    /// that is genuinely non-empty may still not deserialize as expected;
    /// what IS confirmed is that the common case — right after an
    /// `ipconfig0` write — is an empty list, not an error.
    pub fn cloudinit_pending(&self, vmid: u32) -> Result<Vec<CloudInitPendingKey>> {
        let body = self.get(&format!("/nodes/{}/qemu/{vmid}/cloudinit", self.node))?;
        let w: Wrapped<Vec<CloudInitPendingKey>> = parse(&body, "cloudinit pending")?;
        Ok(w.data)
    }

    /// Regenerates the cloud-init disk image from whatever is currently
    /// staged (`PUT …/qemu/{vmid}/cloudinit`) — the step that makes a
    /// [`Client::config`] cloud-init write ([`Client::cloudinit_pending`]'s
    /// `pending`/`delete` fields) actually reach the guest, at its next boot.
    /// Takes no body: the node re-derives the drive from the VM's own config,
    /// so there is nothing for a caller to pass beyond which VM.
    ///
    /// The schema declares `returns: null` and `protected: 1` — the exact
    /// combination [`Client::unlink`] has, and a live PVE 9.2.2 run of THAT
    /// route confirmed it applies inline and forks no task at all. This goes
    /// through [`Client::task_or_done`] on the same expectation, not yet
    /// measured for this specific route — see [`TaskKind::worker_type`] for
    /// the guessed name, marked there as unconfirmed.
    ///
    /// No probe: measured against a live node, [`Client::cloudinit_pending`]
    /// stays empty whether or not a regenerate ever ran (see that function's
    /// doc comment) — reading it back would ALWAYS say "nothing pending",
    /// telling a lost-answer recovery the effect already happened even when
    /// it never did. A lost answer with no task in flight is a plain
    /// transport error here, the same choice [`Client::rollback`] makes for
    /// the same reason.
    pub fn cloudinit_regenerate(&self, ledger: &Ledger, vmid: u32) -> Result<()> {
        self.task_or_done(
            ledger,
            vmid,
            TaskKind::RegenerateCloudInit,
            || self.put_form(&format!("/nodes/{}/qemu/{vmid}/cloudinit", self.node), &[]),
            None,
        )
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

    /// Deletes a snapshot — the disk state and the RAM it may carry, on the
    /// node. `DELETE …/snapshot/{snapname}` answers a UPID (`qmdelsnapshot`),
    /// so it is a task like every other write; a name the VM does not have is
    /// NOT FOUND (exit 4), as on libvirt.
    pub fn delete_snapshot(&self, ledger: &Ledger, vmid: u32, name: &str) -> Result<()> {
        if !self.snapshots(vmid)?.iter().any(|s| s == name) {
            return Err(Error::SnapshotNotFound(format!(
                "snapshot of Proxmox VM {vmid}: {name}"
            )));
        }
        let snapname = name;
        let path = format!("/nodes/{}/qemu/{vmid}/snapshot/{snapname}", self.node);
        self.task(
            ledger,
            vmid,
            TaskKind::DeleteSnapshot,
            || self.delete(&path),
            // A lost answer whose effect is already there: the name is gone
            // from the node's list, and that is what `delete` promised.
            Some(&|| Ok(!self.snapshots(vmid)?.iter().any(|s| s == name))),
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

    // =======================================================================
    // The node's own (native) per-VM firewall — NOT this crate's SDN firewall
    // =======================================================================
    //
    // This engine already has its own firewall, for its OWN containers and
    // VMs: `delonix-sdn`, nftables-based, `net ingress`/`net egress`. What
    // follows is a DIFFERENT thing entirely — Proxmox VE's own, NATIVE
    // per-VM firewall, enforced by the node's own `pve-firewall` daemon from
    // rules the node stores in `/etc/pve/firewall/<vmid>.fw`. It only applies
    // to a VM running ON a Proxmox node, through this backend, and has no
    // relationship whatsoever to this engine's SDN firewall — the two never
    // see each other's rules, and turning one on or off does nothing to the
    // other. Every doc comment below says "the node's own firewall" for
    // exactly this reason: plain "firewall" in this crate is ambiguous.

    /// The VM's own-firewall OPTION set, as the node has it — an object whose
    /// most important field is `enable` (`0`/`1`, or absent).
    ///
    /// **The node's own firewall does NOTHING at all unless `enable` is `1`
    /// here — even with rules already added.** This is the single most likely
    /// way this API surface confuses somebody: rules go in with
    /// [`Self::add_firewall_rule`], nothing about the VM changes, and the
    /// reason is always the same — the firewall itself was never turned on.
    /// See [`Self::set_firewall_enabled`].
    ///
    /// A raw value and not a typed struct: the option set is a handful of
    /// scalars (`enable`, `dhcp`, `ndp`, `radv`, `ipfilter`, `log_level_in`,
    /// …) that nothing here composes into a decision — [`Self::config`] and
    /// [`Self::boot_disk`] draw the same line for the same reason.
    pub fn firewall_options(&self, vmid: u32) -> Result<serde_json::Value> {
        let body = self.get(&format!(
            "/nodes/{}/qemu/{vmid}/firewall/options",
            self.node
        ))?;
        let w: Wrapped<serde_json::Value> = parse(&body, "firewall options")?;
        Ok(w.data)
    }

    /// Turns the VM's own firewall on or off (`PUT …/firewall/options`,
    /// `enable=0|1`) — the switch [`Self::firewall_options`]'s doc comment
    /// warns about. Rules survive being turned off; they simply stop being
    /// enforced until this is `true` again.
    pub fn set_firewall_enabled(&self, ledger: &Ledger, vmid: u32, enabled: bool) -> Result<()> {
        let value = if enabled { "1" } else { "0" };
        self.task_or_done(
            ledger,
            vmid,
            TaskKind::FirewallOptions,
            || {
                self.put_form(
                    &format!("/nodes/{}/qemu/{vmid}/firewall/options", self.node),
                    &[("enable", value)],
                )
            },
            Some(&|| {
                Ok(self
                    .firewall_options(vmid)?
                    .get("enable")
                    .and_then(|v| v.as_u64())
                    == Some(u64::from(enabled)))
            }),
        )
    }

    /// The VM's own-firewall rules, in the node's own order (`pos` is the
    /// rule's position, and the only handle [`Self::firewall_rule`],
    /// [`Self::update_firewall_rule`] and [`Self::delete_firewall_rule`]
    /// address one by — never a name).
    pub fn firewall_rules(&self, vmid: u32) -> Result<Vec<serde_json::Value>> {
        let body = self.get(&format!("/nodes/{}/qemu/{vmid}/firewall/rules", self.node))?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "firewall rules")?;
        Ok(w.data)
    }

    /// One rule of the VM's own firewall, at position `pos`.
    pub fn firewall_rule(&self, vmid: u32, pos: u32) -> Result<serde_json::Value> {
        let body = self.get(&format!(
            "/nodes/{}/qemu/{vmid}/firewall/rules/{pos}",
            self.node
        ))?;
        let w: Wrapped<serde_json::Value> = parse(&body, "firewall rule")?;
        Ok(w.data)
    }

    /// Appends a rule to the VM's own firewall (`POST …/firewall/rules`).
    ///
    /// `rule_type` (`in`/`out`) and `action` are required by the node and
    /// validated BEFORE anything reaches the wire — `action` accepts only the
    /// three plain verdicts (`ACCEPT`/`DROP`/`REJECT`); a firewall GROUP's
    /// name (`+groupname`) is a different kind of value this backend has no
    /// validation for and does not accept.
    ///
    /// **`enable` is always sent explicitly, defaulting to `true`** — never
    /// left to the node's own default. A rule created accidentally disabled
    /// would still show up in [`Self::firewall_rules`] looking present while
    /// doing nothing, which is exactly the silently-half-honoured shape this
    /// crate's doctrine treats as its worst failure (see `refuse_unsupported`
    /// for the same posture applied to `VmConfig`).
    ///
    /// No probe: a rule appended by position has nothing a read can reliably
    /// tell apart from "not appended" without racing whatever else is adding
    /// rules to the same VM at the same time — the same reasoning
    /// [`Self::rollback`] gives for having none. A lost answer with no
    /// firewall task in flight on the node is therefore an error, not a
    /// second guess.
    pub fn add_firewall_rule(
        &self,
        ledger: &Ledger,
        vmid: u32,
        rule_type: &str,
        action: &str,
        opts: &FirewallRuleOpts,
    ) -> Result<()> {
        validate_firewall_direction(rule_type)?;
        validate_firewall_action(action)?;
        let enable = if opts.enable.unwrap_or(true) {
            "1"
        } else {
            "0"
        };
        let mut form: Vec<(&str, String)> = vec![
            ("type", rule_type.to_string()),
            ("action", action.to_string()),
            ("enable", enable.to_string()),
        ];
        form.extend(firewall_rule_common_fields(opts));
        let form: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
        self.task_or_done(
            ledger,
            vmid,
            TaskKind::AddFirewallRule,
            || {
                self.post_form(
                    &format!("/nodes/{}/qemu/{vmid}/firewall/rules", self.node),
                    &form,
                    true,
                )
            },
            None,
        )
    }

    /// Changes one field or more of a rule already at position `pos`
    /// (`PUT …/firewall/rules/{pos}`) — only the fields given in `opts` are
    /// sent, and `None` means "leave it as the node already has it", not
    /// "clear it". Unlike [`Self::add_firewall_rule`], `type`/`action` are
    /// optional here too (`opts.rule_type`/`opts.action`): an update may
    /// change either, neither, or both.
    ///
    /// **No `digest` (optimistic-concurrency) parameter is sent.** The node
    /// accepts a config's current digest to refuse an update that raced
    /// another writer; this client does not read or carry one, so two
    /// concurrent updates to the same rule can still overwrite each other.
    /// Not implemented, not silently worked around.
    ///
    /// No probe, for the same reason [`Self::add_firewall_rule`] has none: an
    /// arbitrary set of changed fields has no single read this could compare
    /// against without assuming which ones were asked for.
    pub fn update_firewall_rule(
        &self,
        ledger: &Ledger,
        vmid: u32,
        pos: u32,
        opts: &FirewallRuleOpts,
    ) -> Result<()> {
        if let Some(t) = opts.rule_type {
            validate_firewall_direction(t)?;
        }
        if let Some(a) = opts.action {
            validate_firewall_action(a)?;
        }
        let mut form = firewall_rule_common_fields(opts);
        if let Some(t) = opts.rule_type {
            form.push(("type", t.to_string()));
        }
        if let Some(a) = opts.action {
            form.push(("action", a.to_string()));
        }
        if let Some(enabled) = opts.enable {
            form.push(("enable", if enabled { "1" } else { "0" }.to_string()));
        }
        let form: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
        self.task_or_done(
            ledger,
            vmid,
            TaskKind::UpdateFirewallRule,
            || {
                self.put_form(
                    &format!("/nodes/{}/qemu/{vmid}/firewall/rules/{pos}", self.node),
                    &form,
                )
            },
            None,
        )
    }

    /// Removes a rule of the VM's own firewall at position `pos`
    /// (`DELETE …/firewall/rules/{pos}`). The probe is the node's own list, as
    /// on [`Self::delete_snapshot`]: no entry left at that position, never
    /// taken from what the call said.
    pub fn delete_firewall_rule(&self, ledger: &Ledger, vmid: u32, pos: u32) -> Result<()> {
        let path = format!("/nodes/{}/qemu/{vmid}/firewall/rules/{pos}", self.node);
        self.task_or_done(
            ledger,
            vmid,
            TaskKind::DeleteFirewallRule,
            || self.delete(&path),
            Some(&|| {
                Ok(!self
                    .firewall_rules(vmid)?
                    .iter()
                    .any(|r| r.get("pos").and_then(|p| p.as_u64()) == Some(u64::from(pos))))
            }),
        )
    }

    /// The VM's own firewall as a directory of its sub-resources
    /// (`GET …/firewall`, `returns: array`).
    ///
    /// **Believed to be a plain directory index, not implemented as
    /// anything more than a raw pass-through — this is an INFERENCE, not a
    /// measurement.** This repo's own extracted route inventory
    /// (`docs/proxmox/api-9.2.2.routes.json`) carries no field-level schema
    /// (method/path/perm/returns only), so the exact shape of each entry is
    /// not known from it. What IS known: every sibling route below this one
    /// (`rules`, `aliases`, `ipset`, `options`, `log`, `refs`) is its own,
    /// separately useful call, and Proxmox's API is built throughout on a
    /// parent path answering a directory listing of its children when GET'd
    /// with nothing more specific asked for — the same shape
    /// `GET /cluster/sdn` and `GET /nodes/{node}/qemu/{vmid}` themselves
    /// take. On that basis this is not given a second, composed meaning: it
    /// is not "independently useful" the way the routes below it are, since
    /// everything it could tell a caller is already answered, in more
    /// specific form, by calling one of them directly. Kept as a raw
    /// pass-through — like [`Self::firewall_options`] and
    /// [`Self::sdn_zones`] before it — for the one case it IS useful: a
    /// caller that only wants to confirm the sub-path exists at all before
    /// touching anything under it.
    pub fn firewall_index(&self, vmid: u32) -> Result<Vec<serde_json::Value>> {
        let body = self.get(&format!("/nodes/{}/qemu/{vmid}/firewall", self.node))?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "firewall index")?;
        Ok(w.data)
    }

    /// Every alias of the VM's own firewall (`GET …/firewall/aliases`) — a
    /// name for a CIDR or address, usable in a rule's `source`/`dest` as
    /// `+name` instead of the raw value.
    ///
    /// Scoped to this ONE VM. `/cluster/firewall/aliases` is cluster-wide
    /// administration, out of scope here (`docs/proxmox/matrix-9.2.2.md`
    /// marks it `unsupported-by-design`), and so is
    /// `/nodes/{node}/lxc/{vmid}/firewall/aliases` — see the same file, ADR-0049 D4.
    pub fn firewall_aliases(&self, vmid: u32) -> Result<Vec<serde_json::Value>> {
        let body = self.get(&format!(
            "/nodes/{}/qemu/{vmid}/firewall/aliases",
            self.node
        ))?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "firewall aliases")?;
        Ok(w.data)
    }

    /// One alias of the VM's own firewall, by name
    /// (`GET …/firewall/aliases/{name}`).
    pub fn firewall_alias(&self, vmid: u32, name: &str) -> Result<serde_json::Value> {
        validate_firewall_object_name(name)?;
        let body = self.get(&format!(
            "/nodes/{}/qemu/{vmid}/firewall/aliases/{name}",
            self.node
        ))?;
        let w: Wrapped<serde_json::Value> = parse(&body, "firewall alias")?;
        Ok(w.data)
    }

    /// Names a CIDR or address in the VM's own firewall namespace
    /// (`POST …/firewall/aliases`).
    ///
    /// `name` and `cidr` are both checked before anything reaches the wire
    /// — see [`validate_firewall_object_name`] and
    /// [`validate_firewall_cidr`], the same posture every other input this
    /// crate cannot follow through blindly is held to.
    pub fn add_firewall_alias(
        &self,
        ledger: &Ledger,
        vmid: u32,
        name: &str,
        cidr: &str,
        comment: Option<&str>,
    ) -> Result<()> {
        validate_firewall_object_name(name)?;
        validate_firewall_cidr(cidr)?;
        let mut form: Vec<(&str, &str)> = vec![("name", name), ("cidr", cidr)];
        if let Some(c) = comment {
            form.push(("comment", c));
        }
        self.task_or_done(
            ledger,
            vmid,
            TaskKind::AddFirewallAlias,
            || {
                self.post_form(
                    &format!("/nodes/{}/qemu/{vmid}/firewall/aliases", self.node),
                    &form,
                    true,
                )
            },
            Some(&|| {
                Ok(self
                    .firewall_aliases(vmid)?
                    .iter()
                    .any(|a| a.get("name").and_then(|v| v.as_str()) == Some(name)))
            }),
        )
    }

    /// Changes the `cidr`/`comment` of an alias already named
    /// (`PUT …/firewall/aliases/{name}`). `None` is meant to mean "leave it
    /// as the node already has it", the same convention
    /// [`Self::update_firewall_rule`] uses for a rule — **but measured
    /// against a live node, this route does NOT honour that convention on
    /// its own.** A live run updated only `cidr` on an alias that already
    /// carried a `comment`, and the node came back with the comment GONE:
    /// PVE replaces the whole alias here rather than patching named fields,
    /// unlike the rules route, where the same live run confirmed the
    /// opposite. To give a caller of THIS function the "unspecified means
    /// unchanged" guarantee its doc comment promises, the current alias is
    /// read first ([`Self::firewall_alias`]) and whichever of `cidr`/
    /// `comment` is `None` is re-sent from what is already there — nothing
    /// carried forward for a field the alias never had. No probe: the merge
    /// read already reaches the node once, and re-reading straight after a
    /// write to confirm it stuck is the same "trust what it reports, not
    /// what the call claimed" the merge step itself already practises.
    pub fn update_firewall_alias(
        &self,
        ledger: &Ledger,
        vmid: u32,
        name: &str,
        cidr: Option<&str>,
        comment: Option<&str>,
    ) -> Result<()> {
        validate_firewall_object_name(name)?;
        if let Some(c) = cidr {
            validate_firewall_cidr(c)?;
        }
        let current = self.firewall_alias(vmid, name)?;
        let cidr = cidr.or_else(|| current.get("cidr").and_then(|v| v.as_str()));
        let comment = comment.or_else(|| current.get("comment").and_then(|v| v.as_str()));
        let mut form: Vec<(&str, &str)> = Vec::new();
        if let Some(c) = cidr {
            form.push(("cidr", c));
        }
        if let Some(c) = comment {
            form.push(("comment", c));
        }
        self.task_or_done(
            ledger,
            vmid,
            TaskKind::UpdateFirewallAlias,
            || {
                self.put_form(
                    &format!("/nodes/{}/qemu/{vmid}/firewall/aliases/{name}", self.node),
                    &form,
                )
            },
            None,
        )
    }

    /// Removes an alias (`DELETE …/firewall/aliases/{name}`). The probe is
    /// the node's own list, as on [`Self::delete_firewall_rule`]: no entry
    /// left under that name, never taken from what the call said.
    pub fn delete_firewall_alias(&self, ledger: &Ledger, vmid: u32, name: &str) -> Result<()> {
        validate_firewall_object_name(name)?;
        let path = format!("/nodes/{}/qemu/{vmid}/firewall/aliases/{name}", self.node);
        self.task_or_done(
            ledger,
            vmid,
            TaskKind::DeleteFirewallAlias,
            || self.delete(&path),
            Some(&|| {
                Ok(!self
                    .firewall_aliases(vmid)?
                    .iter()
                    .any(|a| a.get("name").and_then(|v| v.as_str()) == Some(name)))
            }),
        )
    }

    /// Every named IP set of the VM's own firewall (`GET …/firewall/ipset`)
    /// — the SET names themselves, not the CIDR entries inside any one of
    /// them (see [`Self::firewall_ipset_entries`] for that).
    pub fn firewall_ipsets(&self, vmid: u32) -> Result<Vec<serde_json::Value>> {
        let body = self.get(&format!("/nodes/{}/qemu/{vmid}/firewall/ipset", self.node))?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "firewall ipsets")?;
        Ok(w.data)
    }

    /// Creates a new, empty named IP set (`POST …/firewall/ipset`). Entries
    /// are added to it one at a time afterwards, with
    /// [`Self::add_firewall_ipset_cidr`].
    pub fn create_firewall_ipset(
        &self,
        ledger: &Ledger,
        vmid: u32,
        name: &str,
        comment: Option<&str>,
    ) -> Result<()> {
        validate_firewall_object_name(name)?;
        let mut form: Vec<(&str, &str)> = vec![("name", name)];
        if let Some(c) = comment {
            form.push(("comment", c));
        }
        self.task_or_done(
            ledger,
            vmid,
            TaskKind::CreateFirewallIpset,
            || {
                self.post_form(
                    &format!("/nodes/{}/qemu/{vmid}/firewall/ipset", self.node),
                    &form,
                    true,
                )
            },
            Some(&|| {
                Ok(self
                    .firewall_ipsets(vmid)?
                    .iter()
                    .any(|s| s.get("name").and_then(|v| v.as_str()) == Some(name)))
            }),
        )
    }

    /// Removes a whole named IP set (`DELETE …/firewall/ipset/{name}`). The
    /// node refuses this while a rule still references the set — its own
    /// business logic, surfaced as an ordinary node error and not pre-empted
    /// here, the same posture [`Self::delete_sdn_zone`] takes for a zone
    /// still referenced by a vnet. [`Self::firewall_refs`] is how a caller
    /// checks that BEFORE trying.
    pub fn delete_firewall_ipset(&self, ledger: &Ledger, vmid: u32, name: &str) -> Result<()> {
        validate_firewall_object_name(name)?;
        let path = format!("/nodes/{}/qemu/{vmid}/firewall/ipset/{name}", self.node);
        self.task_or_done(
            ledger,
            vmid,
            TaskKind::DeleteFirewallIpset,
            || self.delete(&path),
            Some(&|| {
                Ok(!self
                    .firewall_ipsets(vmid)?
                    .iter()
                    .any(|s| s.get("name").and_then(|v| v.as_str()) == Some(name)))
            }),
        )
    }

    /// Every CIDR/address entry of one named IP set
    /// (`GET …/firewall/ipset/{name}`) — NOT the set names themselves (see
    /// [`Self::firewall_ipsets`] for that).
    pub fn firewall_ipset_entries(&self, vmid: u32, name: &str) -> Result<Vec<serde_json::Value>> {
        validate_firewall_object_name(name)?;
        let body = self.get(&format!(
            "/nodes/{}/qemu/{vmid}/firewall/ipset/{name}",
            self.node
        ))?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "firewall ipset entries")?;
        Ok(w.data)
    }

    /// Adds one CIDR/address entry to a set already created
    /// (`POST …/firewall/ipset/{name}`).
    ///
    /// **Same path as [`Self::firewall_ipset_entries`]'s `GET` and
    /// [`Self::delete_firewall_ipset`]'s `DELETE`** — the set is addressed by
    /// `{name}` in the URL, and this is a DIFFERENT operation from
    /// [`Self::create_firewall_ipset`] (which is also a `POST`, to
    /// `…/firewall/ipset` with no `{name}` in the path — creating the set
    /// itself, not an entry inside one). Reading the two `TaskKind` doc
    /// comments side by side is the fastest way to tell them apart.
    pub fn add_firewall_ipset_cidr(
        &self,
        ledger: &Ledger,
        vmid: u32,
        name: &str,
        cidr: &str,
        opts: &IpsetCidrOpts,
    ) -> Result<()> {
        validate_firewall_object_name(name)?;
        validate_firewall_cidr(cidr)?;
        let extra = ipset_cidr_fields(opts);
        let mut form: Vec<(&str, &str)> = vec![("cidr", cidr)];
        form.extend(extra.iter().map(|(k, v)| (*k, v.as_str())));
        self.task_or_done(
            ledger,
            vmid,
            TaskKind::AddFirewallIpsetCidr,
            || {
                self.post_form(
                    &format!("/nodes/{}/qemu/{vmid}/firewall/ipset/{name}", self.node),
                    &form,
                    true,
                )
            },
            Some(&|| {
                Ok(self
                    .firewall_ipset_entries(vmid, name)?
                    .iter()
                    .any(|e| e.get("cidr").and_then(|v| v.as_str()) == Some(cidr)))
            }),
        )
    }

    /// One CIDR/address entry of a set, by its own value
    /// (`GET …/firewall/ipset/{name}/{cidr}`).
    ///
    /// `cidr` is percent-encoded before it goes into the URL — it is a
    /// second path segment (`{cidr}`) that itself contains a `/`
    /// (`10.0.0.0/8`), the same trap [`Self::delete_backup`]'s doc comment
    /// warns about for a volume id.
    pub fn firewall_ipset_cidr(
        &self,
        vmid: u32,
        name: &str,
        cidr: &str,
    ) -> Result<serde_json::Value> {
        validate_firewall_object_name(name)?;
        validate_firewall_cidr(cidr)?;
        let body = self.get(&format!(
            "/nodes/{}/qemu/{vmid}/firewall/ipset/{name}/{}",
            self.node,
            urlencode(cidr)
        ))?;
        let w: Wrapped<serde_json::Value> = parse(&body, "firewall ipset entry")?;
        Ok(w.data)
    }

    /// Changes the `comment`/`nomatch` of an entry already in the set
    /// (`PUT …/firewall/ipset/{name}/{cidr}`). The entry's `cidr` itself is
    /// the URL path's own identifier and is not one of the fields this sends
    /// — removing and re-adding is the node's own way to change it.
    ///
    /// **Reads the entry first and carries forward what `opts` leaves out —
    /// measured against a live node that this route does NOT do the
    /// merge itself.** [`Self::update_firewall_rule`]'s doc comment states
    /// "`None` means leave it as the node already has it, not clear it", and
    /// a live run confirmed that IS how the rules route behaves. This ipset
    /// entry route was assumed to work the same way and does not: a live run
    /// updated only `nomatch` on an entry that already carried a `comment`,
    /// and the node came back with the comment GONE, not preserved — PVE
    /// replaces the whole entry here rather than patching named fields. To
    /// give a caller of THIS function the same "unspecified means unchanged"
    /// guarantee the rules route gives for free, the current entry is read
    /// ([`Self::firewall_ipset_cidr`]) and any field `opts` leaves as `None`
    /// is re-sent from what is already there — nothing carried forward for a
    /// field the entry never had, so a first-ever update does not invent an
    /// empty `comment=` out of nothing. No probe: the merge read already
    /// requires reaching the node once, and re-reading straight after a
    /// write to confirm it stuck is the same "trust what it reports, not
    /// what the call claimed" the merge step itself already practises.
    pub fn update_firewall_ipset_cidr(
        &self,
        ledger: &Ledger,
        vmid: u32,
        name: &str,
        cidr: &str,
        opts: &IpsetCidrOpts,
    ) -> Result<()> {
        validate_firewall_object_name(name)?;
        validate_firewall_cidr(cidr)?;
        let current = self.firewall_ipset_cidr(vmid, name, cidr)?;
        let merged = IpsetCidrOpts {
            comment: opts
                .comment
                .or_else(|| current.get("comment").and_then(|v| v.as_str())),
            nomatch: opts.nomatch.or_else(|| {
                current
                    .get("nomatch")
                    .and_then(|v| {
                        v.as_i64()
                            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    })
                    .map(|n| n != 0)
            }),
        };
        let extra = ipset_cidr_fields(&merged);
        let form: Vec<(&str, &str)> = extra.iter().map(|(k, v)| (*k, v.as_str())).collect();
        self.task_or_done(
            ledger,
            vmid,
            TaskKind::UpdateFirewallIpsetCidr,
            || {
                self.put_form(
                    &format!(
                        "/nodes/{}/qemu/{vmid}/firewall/ipset/{name}/{}",
                        self.node,
                        urlencode(cidr)
                    ),
                    &form,
                )
            },
            None,
        )
    }

    /// Removes one CIDR/address entry from a set
    /// (`DELETE …/firewall/ipset/{name}/{cidr}`). The probe is the set's own
    /// remaining entries, as on [`Self::delete_firewall_alias`].
    pub fn delete_firewall_ipset_cidr(
        &self,
        ledger: &Ledger,
        vmid: u32,
        name: &str,
        cidr: &str,
    ) -> Result<()> {
        validate_firewall_object_name(name)?;
        validate_firewall_cidr(cidr)?;
        let path = format!(
            "/nodes/{}/qemu/{vmid}/firewall/ipset/{name}/{}",
            self.node,
            urlencode(cidr)
        );
        self.task_or_done(
            ledger,
            vmid,
            TaskKind::DeleteFirewallIpsetCidr,
            || self.delete(&path),
            Some(&|| {
                Ok(!self
                    .firewall_ipset_entries(vmid, name)?
                    .iter()
                    .any(|e| e.get("cidr").and_then(|v| v.as_str()) == Some(cidr)))
            }),
        )
    }

    /// The tail of the VM's own firewall log (`GET …/firewall/log`) — what
    /// `pve-firewall` itself has logged for this VM, raw. No `start`/`limit`
    /// paging parameter is sent; only the node's own default page is asked
    /// for, the same "no complexity beyond what is proven" restraint
    /// [`Self::firewall_options`]'s raw-`Value` return already applies.
    pub fn firewall_log(&self, vmid: u32) -> Result<Vec<serde_json::Value>> {
        let body = self.get(&format!("/nodes/{}/qemu/{vmid}/firewall/log", self.node))?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "firewall log")?;
        Ok(w.data)
    }

    /// What refers to this VM's own aliases/ipsets (`GET …/firewall/refs`)
    /// — read BEFORE deleting an alias or a set, to avoid breaking a rule
    /// that names it. [`Self::delete_firewall_ipset`]'s doc comment points
    /// here for exactly that reason.
    pub fn firewall_refs(&self, vmid: u32) -> Result<Vec<serde_json::Value>> {
        let body = self.get(&format!("/nodes/{}/qemu/{vmid}/firewall/refs", self.node))?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "firewall refs")?;
        Ok(w.data)
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
        self.task_inner(ledger, vmid, kind, issue, probe, false)
    }

    /// [`Self::task`] for a route the node may answer WITHOUT a task: a
    /// `null` is taken as "applied inline", a UPID is waited on. `POST
    /// …/config` is measured behaving like that; every node's-own-firewall
    /// write (`set_firewall_enabled`/`add_firewall_rule`/
    /// `update_firewall_rule`/`delete_firewall_rule`) goes through this same
    /// path too, on the expectation — NOT yet measured against a real node,
    /// see [`TaskKind::worker_type`]'s doc comment — that `pve-firewall`
    /// applies a rule/option change inline rather than forking a worker.
    /// Every other write answers a UPID or it is an unexpected answer.
    fn task_or_done(
        &self,
        ledger: &Ledger,
        vmid: u32,
        kind: TaskKind,
        issue: impl Fn() -> Result<String>,
        probe: Option<&dyn Fn() -> Result<bool>>,
    ) -> Result<()> {
        self.task_inner(ledger, vmid, kind, issue, probe, true)
    }

    fn task_inner(
        &self,
        ledger: &Ledger,
        vmid: u32,
        kind: TaskKind,
        issue: impl Fn() -> Result<String>,
        probe: Option<&dyn Fn() -> Result<bool>>,
        null_is_done: bool,
    ) -> Result<()> {
        let what = kind.action();
        with_lock_retry(what, || {
            let upid = match issue() {
                Ok(body) => match upid_or_done(&body, what, null_is_done)? {
                    Some(upid) => upid,
                    None => return Ok(()),
                },
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
/// The task id in a write's answer; `Ok(None)` for a `data: null` when the
/// route is one that may apply inline (`null_is_done`). Any other shape —
/// a string that is not a UPID, an object, a `null` where a task was the
/// only honest answer — is an unexpected answer, never a success.
fn upid_or_done(body: &str, what: &str, null_is_done: bool) -> Result<Option<String>> {
    let w: Wrapped<serde_json::Value> = parse(body, what)?;
    if let Some(s) = w.data.as_str().filter(|s| s.starts_with("UPID:")) {
        return Ok(Some(s.to_string()));
    }
    // `null` is how the Perl-side SDN/config routes say "applied inline"; the
    // Rust-side fabrics API (`/cluster/sdn/fabrics/*`, PVE 9) says the same
    // with an EMPTY STRING — measured against a live 9.2.2 node, `{"data":""}`
    // on every fabric write. Both mean the same thing to a caller: done, no
    // task to wait on.
    if null_is_done && (w.data.is_null() || w.data.as_str() == Some("")) {
        return Ok(None);
    }
    Err(Error::UnexpectedAnswer(format!(
        "proxmox: {what} did not answer with a task id: {}",
        truncate_chars(body, 200)
    )))
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
        404 => Error::NodeNotFound(format!("Proxmox resource at {path_only}: {text}")),
        409 => Error::NodeConflict(text),
        400 | 422 => Error::BadRequest(text),
        502..=504 => Error::NodeUnavailable(text),
        500 if body.contains("does not exist") => {
            Error::NodeNotFound(format!("Proxmox resource at {path_only}: {text}"))
        }
        500 if body.contains("already exists") => Error::NodeConflict(text),
        _ => Error::HttpStatus(text),
    }
}

/// Appends `METHOD /path` to the trace file, if one was given. The query is
/// dropped: the matrix is keyed by route, not by arguments.
fn trace_route(file: Option<&Path>, method: &str, path: &str) {
    let Some(file) = file else {
        return;
    };
    use std::io::Write;
    let route = path.split('?').next().unwrap_or(path);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
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
pub(crate) fn urlencode(s: &str) -> String {
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

/// Does this failure say the node's ANSWER, not the request, was "no
/// agent"?
///
/// Matched on the message, the same way [`is_unauthorized`]/[`is_lock_timeout`]
/// are: Proxmox reports this specific condition as an HTTP 500 whose body is
/// the literal string `"QEMU guest agent is not running"` — [`classify_status`]
/// has no dedicated variant for it, so it falls through to [`Error::HttpStatus`]
/// like any other 500 the node has no typed reason for. Matching the status
/// code alone would also catch every OTHER 500 (a bad guest command, storage
/// full), and those are real failures that must propagate, not read as "no
/// agent".
fn is_agent_not_running(e: &Error) -> bool {
    matches!(e, Error::HttpStatus(m) if m.contains("QEMU guest agent is not running"))
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

const GIB: u64 = 1024 * 1024 * 1024;

/// What `disk_size_gib` means next to a FRESH disk (`<storage>:<gib>`): the
/// same size said twice, or a refusal. Two numbers for one disk is two
/// answers to the same question — picking either silently is how a VM comes
/// up with a disk nobody asked for. Pure.
fn check_fresh_disk_size(spec: &DiskSpec, asked: Option<u32>) -> Result<()> {
    match (spec, asked) {
        (DiskSpec::New { storage, gib }, Some(asked)) if asked != *gib => {
            Err(Error::InvalidDiskSpec(format!(
                "proxmox: the disk is sized twice — `disk: {storage}:{gib}` says {gib} GiB and \
                 `diskSize` says {asked} GiB. Say it once: `{storage}:{asked}`, or drop `diskSize`"
            )))
        }
        _ => Ok(()),
    }
}

/// Grow `cur` bytes of `key` to `asked` GiB: `Some((key, gib))` when there is
/// something to grow, `None` when the size already matches, and a refusal —
/// by name, with both numbers — for a shrink, because the node cannot do one
/// and its own refusal arrives inside a failed task with no hint. Pure.
fn disk_grow_plan(
    template: u32,
    key: &str,
    cur: u64,
    asked: Option<u32>,
) -> Result<Option<(String, u32)>> {
    let Some(gib) = asked else {
        return Ok(None);
    };
    let want = u64::from(gib) * GIB;
    if want < cur {
        return Err(Error::InvalidDiskSpec(format!(
            "proxmox: `diskSize` {gib} GiB is smaller than template {template}'s `{key}` \
             ({}): a disk on the node grows and never shrinks. Ask for at least that, or \
             drop `diskSize` to keep the template's size",
            fmt_gib(cur)
        )));
    }
    if want == cur {
        return Ok(None);
    }
    Ok(Some((key.to_string(), gib)))
}

fn fmt_gib(bytes: u64) -> String {
    if bytes.is_multiple_of(GIB) {
        format!("{} GiB", bytes / GIB)
    } else {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    }
}

/// The boot disk of a node-side VM config: `(key, bytes)`.
///
/// `boot: order=scsi0;ide2;net0` names the order, and the first entry that is a
/// disk with a `size=` is the one; without an order, `bootdisk` (older
/// configs), then the lowest-numbered `scsi`/`virtio`/`sata`/`ide` drive that
/// has a size. A cloud-init drive and a CD-ROM never qualify — they carry no
/// `size=` a resize could apply to. Pure.
fn boot_disk_of(cfg: &serde_json::Value) -> Option<(String, u64)> {
    let sized = |key: &str| disk_size_of(cfg, key).map(|b| (key.to_string(), b));
    if let Some(order) = cfg.get("boot").and_then(|b| b.as_str()) {
        let order = order
            .split(',')
            .find_map(|kv| kv.strip_prefix("order="))
            .unwrap_or("");
        if let Some(found) = order.split(';').find_map(sized) {
            return Some(found);
        }
    }
    if let Some(found) = cfg.get("bootdisk").and_then(|b| b.as_str()).and_then(sized) {
        return Some(found);
    }
    let mut keys: Vec<(usize, u32, String)> = cfg
        .as_object()?
        .keys()
        .filter_map(|k| {
            let bus = ["scsi", "virtio", "sata", "ide"]
                .iter()
                .position(|b| k.starts_with(b))?;
            let idx: u32 = k[["scsi", "virtio", "sata", "ide"][bus].len()..]
                .parse()
                .ok()?;
            Some((bus, idx, k.clone()))
        })
        .collect();
    keys.sort();
    keys.into_iter().find_map(|(_, _, k)| sized(&k))
}

/// The bytes of drive `key` in a config, or `None` when the key is absent, is
/// not a sized disk (CD-ROM, cloud-init drive) or has no readable `size=`.
fn disk_size_of(cfg: &serde_json::Value, key: &str) -> Option<u64> {
    drive_size_bytes(cfg.get(key)?.as_str()?)
}

/// The `<storage>` component of drive `key` in a config
/// (`local-lvm:vm-100-disk-0,size=1G` → `local-lvm`) — what
/// [`Client::move_disk`]'s effect probe reads back, because the storage name
/// is the one thing a move is asked to change that a caller can confirm from
/// outside. `None` when the key is absent or carries no `:` at all (`none`,
/// the value an empty CD-ROM slot has).
fn disk_storage_of(cfg: &serde_json::Value, key: &str) -> Option<String> {
    cfg.get(key)?
        .as_str()?
        .split_once(':')
        .map(|(storage, _)| storage.to_string())
}

/// `size=` of a drive property value (`local-lvm:vm-100-disk-0,size=32G`), in
/// bytes. The node writes `<n>[KMGT]` in binary units, or bare bytes.
fn drive_size_bytes(value: &str) -> Option<u64> {
    if value.contains("media=cdrom") || value.contains(":cloudinit") {
        return None;
    }
    parse_size(value.split(',').find_map(|kv| kv.strip_prefix("size="))?)
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let last = s.chars().last()?;
    let (num, mult) = match last {
        'K' | 'k' => (&s[..s.len() - 1], 1u64 << 10),
        'M' | 'm' => (&s[..s.len() - 1], 1u64 << 20),
        'G' | 'g' => (&s[..s.len() - 1], 1u64 << 30),
        'T' | 't' => (&s[..s.len() - 1], 1u64 << 40),
        c if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    if let Ok(n) = num.parse::<u64>() {
        return n.checked_mul(mult);
    }
    // `1.5G` — the node writes a fraction when the bytes are not a whole unit.
    let n: f64 = num.parse().ok()?;
    let bytes = n * mult as f64;
    // `as u64` saturates: a number past the range would read as u64::MAX, which
    // is a size no disk has, not «could not read».
    (n.is_finite() && n >= 0.0 && bytes < u64::MAX as f64).then_some(bytes as u64)
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

/// The optional fields of a rule of the VM's own (node-side) firewall —
/// [`Client::add_firewall_rule`] and [`Client::update_firewall_rule`] — as the
/// node names them on the wire.
///
/// **`enable` behaves differently in the two callers, and each names it where
/// it is used**: `add_firewall_rule` sends it explicitly, defaulting to
/// enabled when `None`, because a rule that LOOKS present in
/// [`Client::firewall_rules`] but was silently created disabled is exactly
/// the trap this crate's doctrine warns against; `update_firewall_rule` sends
/// it only when `Some`, because there `None` means "leave it as the node
/// already has it", not "disable it".
///
/// **`rule_type`/`action` are read by `update_firewall_rule` only.**
/// `add_firewall_rule` takes the direction and the verdict as its own
/// required parameters instead (the node requires both on create, and
/// refusing an unsupported `action` needs to happen before anything reaches
/// the wire); on an update, unlike a create, either may be left unchanged, so
/// they belong here as optional fields like everything else.
#[derive(Debug, Clone, Default)]
pub struct FirewallRuleOpts<'a> {
    pub enable: Option<bool>,
    pub comment: Option<&'a str>,
    pub source: Option<&'a str>,
    pub dest: Option<&'a str>,
    pub proto: Option<&'a str>,
    pub dport: Option<&'a str>,
    pub sport: Option<&'a str>,
    pub iface: Option<&'a str>,
    /// A named service macro (`ssh`, `http`, …) — the node's `macro`
    /// property. Renamed here because `macro` is a Rust keyword.
    pub macro_name: Option<&'a str>,
    /// Only read by [`Client::update_firewall_rule`] — see the struct's doc
    /// comment for why `add_firewall_rule` does not read it from here.
    pub rule_type: Option<&'a str>,
    /// Only read by [`Client::update_firewall_rule`], for the same reason.
    pub action: Option<&'a str>,
}

/// The fields of [`FirewallRuleOpts`] that both `add_firewall_rule` and
/// `update_firewall_rule` forward verbatim — everything except `enable`,
/// `rule_type` and `action`, which each caller handles on its own (see the
/// struct's doc comment). Pure, so "each key at most once" is a test.
fn firewall_rule_common_fields(opts: &FirewallRuleOpts) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = Vec::new();
    let mut push = |key: &'static str, val: Option<&str>| {
        if let Some(v) = val {
            out.push((key, v.to_string()));
        }
    };
    push("comment", opts.comment);
    push("source", opts.source);
    push("dest", opts.dest);
    push("proto", opts.proto);
    push("dport", opts.dport);
    push("sport", opts.sport);
    push("iface", opts.iface);
    push("macro", opts.macro_name);
    out
}

/// The only three plain verdicts a rule of the node's own firewall can carry
/// through this client — never a firewall GROUP's name (`+groupname`), which
/// chains to a set of rules kept elsewhere on the node and would need
/// validation of its own this backend does not have. Refused by value,
/// before anything reaches the wire, the same posture every other input this
/// crate cannot follow through blindly is held to (`refuse_unsupported`,
/// `validate_snapshot_name`, `parse_disk_spec`).
fn validate_firewall_action(action: &str) -> Result<()> {
    if matches!(action, "ACCEPT" | "DROP" | "REJECT") {
        Ok(())
    } else {
        Err(Error::InvalidFirewallRule(format!(
            "invalid Proxmox firewall rule action '{action}': expected ACCEPT, DROP or REJECT \
             (a firewall group's name is not accepted here)"
        )))
    }
}

/// `in`/`out` are the only two directions a rule of the node's own firewall
/// can take.
fn validate_firewall_direction(rule_type: &str) -> Result<()> {
    if matches!(rule_type, "in" | "out") {
        Ok(())
    } else {
        Err(Error::InvalidFirewallRule(format!(
            "invalid Proxmox firewall rule type '{rule_type}': expected 'in' or 'out'"
        )))
    }
}

/// The optional fields of one entry of a named IP set —
/// [`Client::add_firewall_ipset_cidr`] and
/// [`Client::update_firewall_ipset_cidr`], as the node names them on the
/// wire. `cidr` itself is not here: [`Client::add_firewall_ipset_cidr`]
/// takes it as its own required parameter (an entry cannot be created
/// without one), and [`Client::update_firewall_ipset_cidr`] addresses it
/// through the URL path, where it is the identifier being updated rather
/// than a field being changed.
#[derive(Debug, Clone, Copy, Default)]
pub struct IpsetCidrOpts<'a> {
    pub comment: Option<&'a str>,
    /// The node's own `nomatch` — an entry marked this way EXCLUDES its
    /// `cidr` from the set instead of including it (a "deny within an
    /// allow" carve-out). `None` here is sent as nothing at all, which the
    /// node defaults to `false`/included — unlike
    /// [`FirewallRuleOpts::enable`], there is no equivalent "created but
    /// silently doing the opposite of what it looks like" trap for this
    /// field to guard against: an entry with no `nomatch` sent behaves
    /// exactly as an entry with `nomatch=0` sent, on the node's own default.
    pub nomatch: Option<bool>,
}

/// The fields of [`IpsetCidrOpts`] that both
/// [`Client::add_firewall_ipset_cidr`] and
/// [`Client::update_firewall_ipset_cidr`] forward verbatim — the whole
/// struct, since unlike [`FirewallRuleOpts`] neither caller here has a field
/// it handles specially itself. Pure, so "each key at most once, nothing for
/// an all-`None` set" is a test, the same as
/// [`firewall_rule_common_fields`]'s own sibling test already holds it to.
fn ipset_cidr_fields(opts: &IpsetCidrOpts) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = Vec::new();
    if let Some(c) = opts.comment {
        out.push(("comment", c.to_string()));
    }
    if let Some(nm) = opts.nomatch {
        out.push(("nomatch", if nm { "1" } else { "0" }.to_string()));
    }
    out
}

/// A firewall alias or IP-set name — Proxmox's own `pve-fw-alias-name`/
/// `pve-fw-ipset-name` formats, which this repo's extracted route inventory
/// (`docs/proxmox/api-9.2.2.routes.json`) does not carry a copy of (it has
/// method/path/perm/returns only, never a field's regex) — so, like
/// [`crate::sdn::validate_sdn_id`]'s comment says of the SDN id format, this
/// is INFERRED from Proxmox's own published `PVE::JSONSchema`/
/// `PVE::Firewall` format registration, not extracted from a live response.
/// Both formats share the exact same shape in the upstream schema: a letter,
/// then one or more letters/digits/`-`/`_` (minimum length 2). Refused
/// before it reaches a URL path or the node's own namespace, the same
/// discipline [`crate::sdn::validate_sdn_id`] and
/// [`crate::validate_bridge_name`] already apply to what goes into a path —
/// worth confirming against a live node's actual 400 message the first time
/// one is available, the same note [`TaskKind::worker_type`] carries for its
/// own guesses.
fn validate_firewall_object_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let first_ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic());
    let rest_ok = chars.clone().count() >= 1
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if first_ok && rest_ok {
        Ok(())
    } else {
        Err(Error::InvalidFirewallObjectName(format!(
            "invalid Proxmox firewall alias/ipset name '{name}': expected a letter, then one \
             or more letters, digits, '-' or '_' (at least 2 characters total)"
        )))
    }
}

/// A CIDR or bare address for an alias's `cidr` field, or one entry of a
/// named IP set — checked for BASIC shape before it goes into a URL path
/// segment (`ipset/{name}/{cidr}`, percent-encoded by [`urlencode`]) and
/// into the node's own address parser.
///
/// Deliberately shallow: an address (`std::net::IpAddr`, so both IPv4 and
/// IPv6 — Proxmox's own alias/ipset entries accept either) with an optional
/// `/<prefix>` whose value fits the address family's own bit width. This is
/// NOT a full re-implementation of the node's own `pve-fw-addr-spec`
/// grammar (which also accepts DNS-style ranges the node resolves itself);
/// it exists only to keep a value with the wrong general SHAPE — a
/// hostname, a stray path separator, empty text — from ever reaching the
/// wire, the same "refuse what is clearly wrong, let the node be the
/// authority on the rest" restraint [`validate_firewall_action`] applies to
/// a rule's verdict.
fn validate_firewall_cidr(value: &str) -> Result<()> {
    let bad = || {
        Error::InvalidFirewallAddress(format!(
            "invalid Proxmox firewall address '{value}': expected an IPv4/IPv6 address, \
             optionally with a '/<prefix>'"
        ))
    };
    let (addr, prefix) = match value.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (value, None),
    };
    let ip: std::net::IpAddr = addr.parse().map_err(|_| bad())?;
    if let Some(p) = prefix {
        let max = if ip.is_ipv4() { 32 } else { 128 };
        let n: u32 = p.parse().map_err(|_| bad())?;
        if n > max {
            return Err(bad());
        }
    }
    Ok(())
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
///
/// `network` is refused when it NAMES a network: an engine SDN network lives
/// on this host, inside the holder, and a VM on a remote node cannot join it —
/// its NIC is `net0` on the node's bridge (`bridge`). The default value the
/// CLI and the manifest fill in (`ingress`, the engine's default network) is
/// accepted, because the caller did not ask for anything. Measured before
/// this (`docs/discovery/58_…`): `--network lab-net` was accepted, the record
/// said `Network: lab-net`, and the VM was on `vmbr0` — the ADR-0044 D1 gap.
fn refuse_unsupported(cfg: &VmConfig) -> Result<()> {
    let mut bad: Vec<&str> = Vec::new();
    let mut add = |present: bool, field: &'static str| {
        if present {
            bad.push(field);
        }
    };
    add(names_an_engine_network(&cfg.network), "network");
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
         host's kernel/initrd/seed/devices/9p paths or its SDN networks (the NIC is `net0` on the \
         node's bridge — see `bridge`), its QEMU tuning (hugepages, CPU pinning, machine type, \
         TPM, video, boot order) is the node's own configuration, and there is no libvirt domain \
         XML here at all. Remove the field, or use a local backend (`--backend libvirt`)",
        bad.join(", ")
    )))
}

/// Whether `VmConfig.network` names an engine SDN network, as opposed to the
/// values that mean "the default network" (the same set the Cloud Hypervisor
/// backend treats as the ingress bridge). Pure.
fn names_an_engine_network(network: &str) -> bool {
    !matches!(network, "" | "ingress" | "bridge" | "default")
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
        // `disk_size_gib` was neither read nor refused here — the class ADR-0044
        // D1 names (used by the local backends, silently ignored by this one):
        // a `diskSize: 40` on a template clone came up with the template's
        // disk and the command said it worked. Now it is decided BEFORE
        // anything exists on the node: next to a fresh disk it is the same
        // size said twice or a refusal; on a clone it is a grow after the
        // clone, and a shrink is refused here with both numbers, because the
        // node's own refusal arrives inside a failed task.
        let spec = parse_disk_spec(disk)?;
        check_fresh_disk_size(&spec, cfg.disk_size_gib)?;
        // The template's config is read only when there is a size to judge:
        // a clone without `diskSize` costs no extra round trip.
        let grow = match (&spec, cfg.disk_size_gib) {
            (DiskSpec::Template(src), Some(_)) => {
                let (key, cur) = self.client.boot_disk(*src)?;
                disk_grow_plan(*src, &key, cur, cfg.disk_size_gib)?
            }
            _ => None,
        };
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
        match spec {
            DiskSpec::Template(src) => {
                self.client.clone_template(&ledger, src, vmid, &cfg.name)?;
                // A clone carries the TEMPLATE's CPU, memory, NIC and cloud-init
                // — never the caller's. Applying them is a separate call by the
                // API's own shape (clone takes `newid`/`name`/`full` and nothing
                // else), and skipping it was how a DKS node came up with the
                // golden's defaults and no key on it.
                self.client
                    .configure_clone(&ledger, vmid, cfg)
                    .map_err(undo)?;
                // And the clone carries the template's DISK SIZE, which is the
                // golden's floor, never the size the tenant pays for. Same
                // reason `disk_size_gib` exists for the local overlays.
                if let Some((key, gib)) = grow {
                    self.client
                        .resize_disk(&ledger, vmid, &key, gib)
                        .map_err(undo)?;
                }
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

    fn delete_snapshot(&self, vmdir: &Path, vm: &Vm, name: &str) -> delonix_model::Result<()> {
        let vmid = self.vmid_of(vm)?;
        validate_snapshot_name(name)?;
        let ledger = Ledger::at(vmdir);
        self.client.settle_pending(&ledger, vmid)?;
        Ok(self.client.delete_snapshot(&ledger, vmid, name)?)
    }

    /// The address is OBSERVED: it comes from the guest agent
    /// (`agent/network-get-interfaces`), and a guest without one answers
    /// `None`, never a computed address. Written out rather than inherited so
    /// the trait's default is not this backend's answer by accident — the
    /// prediction/observation distinction is what `vm create --wait` decides
    /// by, and a backend that leaves it to a default has not decided.
    fn ip_is_predicted(&self) -> bool {
        false
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
    register_with(target, ClientOptions::default())
}

/// [`register`] with the client's bounds and trace chosen by the caller — the
/// composition root, which is where the environment is read.
pub fn register_with(target: Target, opts: ClientOptions) -> delonix_model::Result<()> {
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
            report: Box::new(|| capability_report(true)),
            new: Box::new(move || {
                let mut slot = shared.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(c) = slot.as_ref() {
                    return Ok(Box::new(ProxmoxBackend::sharing(c.clone())));
                }
                // A failed connect is NOT cached: a node that was down when the
                // first VM was listed must not stay "down" for the rest of the
                // process.
                let c = std::sync::Arc::new(
                    Client::connect_with(&target, opts.clone())
                        .map_err(|e| delonix_vm::Error::Engine(delonix_model::Error::from(e)))?,
                );
                *slot = Some(c.clone());
                Ok(Box::new(ProxmoxBackend::sharing(c)))
            }),
        },
    )?)
}

/// What the Proxmox backend says about the capability catalog (ADR-0050).
///
/// Declared, never probed: building this report contacts nothing — a remote
/// provider authenticates on construction, and `provider ls` must cost zero
/// round trips. The health therefore says so (`Unknown`/`NotProbed`) instead
/// of guessing either way. `configured` is whether a target is registered
/// in this process; without one nothing here is selectable.
pub fn capability_report(configured: bool) -> delonix_compute::capability::ProviderReport {
    use delonix_compute::capability::{
        Capability as C, CapabilityState as S, HealthStatus, ProviderHealth, ProviderKind,
        ProviderReport,
    };
    let health = if configured {
        ProviderHealth {
            status: HealthStatus::Unknown,
            reason: "NotProbed",
            message: "a remote provider is not contacted by `provider ls`; the first VM operation authenticates".to_string(),
        }
    } else {
        ProviderHealth {
            status: HealthStatus::Unavailable,
            reason: "NotConfigured",
            message: "set DELONIX_PROXMOX_URL/_NODE and a credential to register a target"
                .to_string(),
        }
    };
    ProviderReport::build("proxmox", ProviderKind::Compute, configured, health, |c| {
        match c {
        C::ProviderAvailability => S::Partial { detail: "`available()` is always true once configured; reachability is learned on the first call" },
        C::ResourceReadback => S::Supported { evidence: "live:crates/providers/delonix-proxmox/tests/live.rs::cria_arranca_e_destroi_contra_um_no_real" },
        C::Events => S::NotImplemented,
        C::AsyncOperations => S::Partial { detail: "every write waits on its UPID and is written to a per-VM task ledger before the wait (ADR-0049 slice 1); the engine exposes no job handle yet" },
        C::VmCreate => S::Supported { evidence: "live:crates/providers/delonix-proxmox/tests/live.rs::cria_arranca_e_destroi_contra_um_no_real" },
        C::VmStart => S::Supported { evidence: "live:crates/providers/delonix-proxmox/tests/live.rs::cria_arranca_e_destroi_contra_um_no_real" },
        C::VmStop => S::Supported { evidence: "live:crates/providers/delonix-proxmox/tests/live.rs::cria_arranca_e_destroi_contra_um_no_real" },
        C::VmDestroy => S::Supported { evidence: "live:crates/providers/delonix-proxmox/tests/live.rs::cria_arranca_e_destroi_contra_um_no_real" },
        C::VmRestart => S::Supported { evidence: "live:crates/providers/delonix-proxmox/tests/live.rs::cria_arranca_e_destroi_contra_um_no_real" },
        C::VmPause => S::UnsupportedByProvider { reason: "`…/status/suspend` is not called; refused by name (`unsupported_pause`)" },
        C::VmResume => S::UnsupportedByProvider { reason: "`…/status/resume` is not called; refused by name" },
        C::VmResumeSameIdentity => S::Supported { evidence: "live:crates/providers/delonix-proxmox/tests/live.rs::cria_arranca_e_destroi_contra_um_no_real" },
        C::VmClone => S::Supported { evidence: "live:crates/providers/delonix-proxmox/tests/live.rs::a_template_clone_gets_the_disk_size_asked_for" },
        C::VmTemplate => S::Partial { detail: "`POST …/template` is a client call (`mark_template`) the live case uses to make its clone source; no engine verb turns a VM into a template" },
        C::VmResizeCold => S::NotImplemented,
        C::VmHotplug => S::NotImplemented,
        C::VmExtraDisks => S::UnsupportedByProvider { reason: "refused by name (`refuse_unsupported`); ADR-0049 slice 2 maps disks beyond `config`" },
        C::VmExtraNics => S::UnsupportedByProvider { reason: "refused by name; one `net0` on the target's bridge/VLAN" },
        C::VmDiskResize => S::Partial { detail: "`PUT …/resize` grows a template clone's boot disk to `diskSize` at create (live case); a shrink is refused by name; no engine verb resizes an existing VM" },
        C::VmPciPassthrough => S::UnsupportedByProvider { reason: "`devices` refused by name: the guest is on another machine" },
        C::VmTpm => S::UnsupportedByProvider { reason: "refused by name: the node owns the QEMU knobs" },
        C::VmCpuModel => S::UnsupportedByProvider { reason: "refused by name: the node owns the QEMU knobs" },
        C::VmCpuPinning => S::UnsupportedByProvider { reason: "refused by name" },
        C::VmHugepages => S::UnsupportedByProvider { reason: "refused by name" },
        C::VmCloudInit => S::Partial { detail: "hostname/user/ssh keys map to the node's cloud-init keys through `config`; a `seed` file is refused" },
        C::VmRestartPolicyNative => S::UnsupportedByProvider { reason: "the engine's supervisor is not on the node; no policy is set there" },
        C::VmNamespaceIsolation => S::UnsupportedByProvider { reason: "refused before any API call (`vm_namespace_supported`)" },
        C::VmAntispoof => S::RequiresExternalComponent { component: "the node's firewall (`…/firewall`), excluded as administration (ADR-0049 D3)" },
        C::VmRawDefinition => S::UnsupportedByProvider { reason: "no raw config passthrough (ADR-0049 D6)" },
        C::ContainerLifecycle | C::ContainerExec | C::ContainerLogs | C::ContainerHotReconfigure
        | C::ContainerResourceLimits | C::ContainerGpuCdi | C::ContainerSeccompCustomProfile
        | C::ContainerOomDetection | C::PodSharedNetwork | C::PodSharedIpcUts | C::PodSharedPid
        | C::ContainerImages | C::ContainerBackupRestore => {
            S::UnsupportedByProvider { reason: "LXC needs its own boundary decision (ADR-0049 D4); containers are the Linux provider's" }
        }
        C::VmNetworkNat => S::UnsupportedByProvider { reason: "the node has no NAT network of the engine's; `net0` is bridged" },
        C::VmNetworkBridge => S::Supported { evidence: "live:crates/providers/delonix-proxmox/tests/live.rs::cria_arranca_e_destroi_contra_um_no_real" },
        C::VmNetworkSdn => S::UnsupportedByProvider { reason: "the engine's SDN is on this host; a `network` that names one is refused by name (`refuse_unsupported`), the NIC is `net0` on the node's bridge" },
        C::VmStaticIp => S::NotImplemented,
        C::StoragePools => S::Partial { detail: "`disk: <storage>:<gib>` names a node storage for a fresh disk; pools are not listed (ADR-0049: `storage` missing, not excluded)" },
        C::VmSnapshotDisk => S::Supported { evidence: "live:crates/providers/delonix-proxmox/tests/live.rs::cria_arranca_e_destroi_contra_um_no_real" },
        C::VmSnapshotMemory => S::Partial { detail: "`vmstate=1` on every snapshot of a running VM; the live case snapshots once, state not asserted" },
        C::VmSnapshotRestore => S::Supported { evidence: "live:crates/providers/delonix-proxmox/tests/live.rs::cria_arranca_e_destroi_contra_um_no_real" },
        C::VmSnapshotDelete => S::Supported { evidence: "live:crates/providers/delonix-proxmox/tests/live.rs::cria_arranca_e_destroi_contra_um_no_real" },
        C::VmSnapshotPersistent => S::Partial { detail: "snapshots live on the node; the live case lists `live1` back from the node right after taking it, but deletes it BEFORE the stop, so nothing asserts a snapshot is still there after a stop/start" },
        C::VmBackupDisk => S::Supported { evidence: "live:crates/providers/delonix-proxmox/tests/live.rs::a_backup_lands_on_the_storage_and_comes_off_it" },
        C::VmBackupQuiesced => S::NotImplemented,
        C::VmBackupRestore => S::Supported { evidence: "live:crates/providers/delonix-proxmox/tests/live.rs::a_deleted_vm_comes_back_from_its_own_backup" },
        C::VmMigrationCold => S::NotImplemented,
        C::VmMigrationLive => S::RequiresExternalComponent { component: "a Proxmox cluster with shared storage; the engine addresses ONE node (ADR-0008) and never picks the target" },
        C::VmReplication => S::RequiresExternalComponent { component: "cluster replication jobs (ADR-0049 D3: excluded as administration)" },
        C::VmHighAvailability => S::RequiresExternalComponent { component: "cluster HA policy (ADR-0049 D3: excluded as administration)" },
        C::VmConsoleSerial => S::NotImplemented,
        C::VmConsoleVnc => S::NotImplemented,
        C::VmGuestAgent => S::Partial { detail: "`agent=1` is set and `agent/network-get-interfaces` read; a guest without the agent answers `None`" },
        C::VmIpObserved => S::Supported { evidence: "live:crates/providers/delonix-proxmox/tests/live.rs::o_ip_vem_do_agente_de_um_convidado_a_serio" },
        C::MetricsPrometheus => S::NotImplemented,
        C::MetricsPerWorkloadNetwork => S::NotImplemented,
        C::HostHealth => S::NotImplemented,
        C::HostCapacity => S::NotImplemented,
        C::TransportVerified => S::Partial { detail: "TLS verified by default (webpki roots, or `ca_cert_pem`/`DELONIX_PROXMOX_CA_FILE` for an internal CA); `insecure_tls` is an explicit opt-out; 16 MiB response bound; `Debug` redacts the credential (ADR-0049 slice 1)" },
        C::CredentialInVault => S::Partial { detail: "`DELONIX_PROXMOX_SECRET` names a `kind: Secret`; the env-var form keeps the token in the environment" },
        C::NetBridge | C::NetMacvlanIpvlan | C::NetVlan | C::NetOverlayVxlan | C::NetOverlayEncrypted | C::NetIpam | C::NetStaticIp | C::NetDns | C::NetPublishPorts | C::NetRoutesBetweenNetworks | C::NetNamespaceIsolation | C::NetTunnelEgress | C::NetRateLimit | C::NetPacketCapture | C::NetL7Proxy | C::NetIpv6 | C::VolumeLocal | C::VolumeBind | C::VolumeNfs | C::VolumeCifs | C::VolumeWebdav | C::VolumeQuota | C::VolumeSnapshot | C::VolumeProvisionNas | C::StorageLvmThin | C::StorageZfsBtrfs | C::StorageCeph | C::FirewallPerWorkload | C::FirewallDefaultDeny | C::FirewallSourceFiltering | C::FirewallEgressPolicy => {
            S::UnsupportedByProvider { reason: "not a compute capability: answered by the network/storage provider" }
        }
    }
    })
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

    /// `diskSize` next to a FRESH disk is the same number said twice, or a
    /// refusal that names both — never one of them picked in silence.
    #[test]
    fn a_fresh_disk_sized_twice_is_refused_unless_the_two_agree() {
        let spec = parse_disk_spec("local-lvm:8").unwrap();
        assert!(check_fresh_disk_size(&spec, None).is_ok());
        assert!(check_fresh_disk_size(&spec, Some(8)).is_ok());
        let e = check_fresh_disk_size(&spec, Some(40)).unwrap_err();
        assert!(e.is_invalid_argument(), "{e}");
        let e = e.to_string();
        assert!(
            e.contains("local-lvm:8") && e.contains("40") && e.contains("local-lvm:40"),
            "{e}"
        );
        // A template has no size of its own here: nothing to disagree with.
        assert!(
            check_fresh_disk_size(&parse_disk_spec("template:9000").unwrap(), Some(40)).is_ok()
        );
    }

    /// Grow, keep, or refuse a shrink with both numbers — decided before the
    /// clone exists, because the node's own «shrinking disks is not supported»
    /// arrives inside a failed task after a VM has been created.
    #[test]
    fn a_clone_grows_keeps_or_refuses_by_name() {
        assert_eq!(disk_grow_plan(9000, "scsi0", 2 * GIB, None).unwrap(), None);
        assert_eq!(
            disk_grow_plan(9000, "scsi0", 2 * GIB, Some(2)).unwrap(),
            None
        );
        assert_eq!(
            disk_grow_plan(9000, "scsi0", 2 * GIB, Some(40)).unwrap(),
            Some(("scsi0".into(), 40))
        );
        let e = disk_grow_plan(9000, "virtio0", 3 * GIB + GIB / 2, Some(2)).unwrap_err();
        assert!(e.is_invalid_argument(), "{e}");
        let e = e.to_string();
        assert!(
            e.contains("9000")
                && e.contains("virtio0")
                && e.contains("3.50 GiB")
                && e.contains("2 GiB"),
            "{e}"
        );
    }

    /// The boot disk is read from the config the node RECORDED: the `boot`
    /// order first, then `bootdisk`, then the lowest-numbered sized drive —
    /// and a CD-ROM or a cloud-init drive is never it.
    #[test]
    fn the_boot_disk_comes_from_the_recorded_config() {
        let cfg = serde_json::json!({
            "boot": "order=ide2;scsi0;net0",
            "ide2": "local:iso/x.iso,media=cdrom,size=700M",
            "scsi0": "local-lvm:vm-100-disk-0,size=32G",
            "scsi1": "local-lvm:vm-100-disk-1,size=100G",
            "ide0": "local-lvm:vm-100-cloudinit,media=cdrom"
        });
        assert_eq!(boot_disk_of(&cfg), Some(("scsi0".into(), 32 * GIB)));
        let cfg = serde_json::json!({
            "bootdisk": "virtio0",
            "scsi0": "local-lvm:vm-100-disk-0,size=8G",
            "virtio0": "local-lvm:vm-100-disk-1,size=1G"
        });
        assert_eq!(boot_disk_of(&cfg), Some(("virtio0".into(), GIB)));
        let cfg = serde_json::json!({
            "ide2": "local-lvm:vm-100-cloudinit,media=cdrom",
            "sata1": "local-lvm:vm-100-disk-2,size=3G",
            "scsi1": "local-lvm:vm-100-disk-1,size=2G"
        });
        assert_eq!(boot_disk_of(&cfg), Some(("scsi1".into(), 2 * GIB)));
        assert_eq!(
            boot_disk_of(&serde_json::json!({"ide2": "none,media=cdrom"})),
            None
        );
        assert_eq!(boot_disk_of(&serde_json::json!({"memory": 512})), None);
    }

    /// `size=` as the node writes it: binary units, a fraction when the bytes
    /// are not a whole unit, bare bytes, and nothing for a drive with none.
    #[test]
    fn a_drive_size_reads_as_the_node_writes_it() {
        assert_eq!(
            drive_size_bytes("local-lvm:vm-1-disk-0,size=32G"),
            Some(32 * GIB)
        );
        assert_eq!(
            drive_size_bytes("local-lvm:vm-1-disk-0,size=1.5G,ssd=1"),
            Some(GIB + GIB / 2)
        );
        assert_eq!(
            drive_size_bytes("local:1/vm-1-disk-0.qcow2,size=500M"),
            Some(500 << 20)
        );
        assert_eq!(drive_size_bytes("x,size=2T"), Some(2u64 << 40));
        assert_eq!(drive_size_bytes("x,size=4096"), Some(4096));
        assert_eq!(
            drive_size_bytes("local-lvm:vm-1-cloudinit,media=cdrom"),
            None
        );
        assert_eq!(drive_size_bytes("none,media=cdrom"), None);
        assert_eq!(drive_size_bytes("local-lvm:vm-1-disk-0"), None);
        assert_eq!(parse_size("32X"), None);
        assert_eq!(parse_size(""), None);
        assert_eq!(
            parse_size("99999999999999999999G"),
            None,
            "overflow is not a size"
        );
    }

    /// What [`Client::move_disk`]'s effect probe reads back: the storage NAME
    /// only, up to the first `:` — never the size, which a move is not asked
    /// to change and `disk_size_of`/`drive_size_bytes` already own.
    #[test]
    fn the_storage_of_a_drive_reads_up_to_the_first_colon() {
        let cfg = serde_json::json!({
            "scsi0": "local-lvm:vm-100-disk-0,size=1G",
            "ide2": "local:iso/x.iso,media=cdrom,size=700M",
            "sata0": "none,media=cdrom",
        });
        assert_eq!(disk_storage_of(&cfg, "scsi0").as_deref(), Some("local-lvm"));
        assert_eq!(disk_storage_of(&cfg, "ide2").as_deref(), Some("local"));
        assert_eq!(
            disk_storage_of(&cfg, "sata0"),
            None,
            "an empty CD-ROM slot ('none,media=cdrom') has no colon at all, so it is `None` \
             here just like a missing key — a caller comparing it against a real storage name \
             never sees a false match"
        );
        assert_eq!(disk_storage_of(&cfg, "missing"), None);
    }

    /// The check runs BEFORE any request: an unknown `type` never reaches the
    /// node, unlike a `disk`/`storage`/`idlist` typo, which the node itself
    /// would refuse with a 400 this crate maps into [`Error::BadRequest`].
    #[test]
    fn cloudinit_dump_refuses_an_unknown_type_before_any_request() {
        let cli = Client {
            http: reqwest::blocking::Client::new(),
            base: "https://x".into(),
            node: "pve".into(),
            auth: Auth::ApiToken {
                id: "a!b".into(),
                secret: "c".into(),
            },
            ticket: std::sync::RwLock::new(None),
            bridge: "vmbr0".into(),
            vlan: None,
            task_timeout: TASK_TIMEOUT,
            trace_routes: None,
        };
        let e = cli.cloudinit_dump(100, "bogus").unwrap_err();
        assert!(e.is_invalid_argument(), "{e}");
        assert!(e.to_string().contains("bogus"), "{e}");
        assert!(
            matches!(e, Error::InvalidCloudInitKind(_)),
            "must be the typed refusal, not a request failure: {e}"
        );
    }

    /// [`CloudInitPendingKey`] deserializes the shape its doc comment
    /// describes — a key with only a current `value`, one with a staged
    /// `pending` value not yet baked in, one staged for deletion, and one
    /// present with neither current nor pending (a key the node lists but
    /// nothing has ever set).
    #[test]
    fn cloudinit_pending_key_reads_value_pending_and_delete_independently() {
        let key = |s: &str| serde_json::from_str::<CloudInitPendingKey>(s).unwrap();

        let applied = key(r#"{"key":"citype","value":"nocloud"}"#);
        assert_eq!(applied.value.as_deref(), Some("nocloud"));
        assert_eq!(applied.pending, None);
        assert!(
            !applied.is_pending(),
            "a plain applied value is not pending"
        );

        let staged = key(r#"{"key":"ipconfig0","value":"ip=dhcp","pending":"ip=192.168.1.50/24"}"#);
        assert_eq!(staged.value.as_deref(), Some("ip=dhcp"));
        assert_eq!(staged.pending.as_deref(), Some("ip=192.168.1.50/24"));
        assert!(
            staged.is_pending(),
            "a staged value still needs a regenerate"
        );

        let deleted = key(r#"{"key":"sshkeys","value":"ssh-ed25519 x","delete":1}"#);
        assert_eq!(deleted.pending, None);
        assert_eq!(deleted.delete, Some(1));
        assert!(
            deleted.is_pending(),
            "a staged deletion still needs a regenerate"
        );

        let untouched = key(r#"{"key":"searchdomain"}"#);
        assert_eq!(untouched.value, None);
        assert!(!untouched.is_pending());
    }

    /// [`Client::cloudinit_pending`] unwraps the `{"data": [...]}` envelope
    /// every other GET in this crate goes through ([`Wrapped`]), and the
    /// common case — nothing staged — is an empty list, not an error.
    #[test]
    fn cloudinit_pending_unwraps_the_data_envelope_and_an_empty_list_is_not_an_error() {
        let w: Wrapped<Vec<CloudInitPendingKey>> = parse(
            r#"{"data":[{"key":"citype","value":"nocloud"},{"key":"ipconfig0","pending":"ip=dhcp"}]}"#,
            "cloudinit pending",
        )
        .unwrap();
        assert_eq!(w.data.len(), 2);
        assert!(w.data[1].is_pending());

        let empty: Wrapped<Vec<CloudInitPendingKey>> =
            parse(r#"{"data":[]}"#, "cloudinit pending").unwrap();
        assert!(empty.data.is_empty());
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

    /// `exited == 0` is the WHOLE boundary — not the presence of the other
    /// fields, which the node also omits while the guest is still running.
    /// Getting this backwards would report a stale or absent exit code as a
    /// real result while the command is still going.
    #[test]
    fn agent_exec_status_of_reads_exited_as_the_only_boundary() {
        let body = |s: &str| serde_json::from_str::<AgentExecStatusBody>(s).unwrap();
        assert_eq!(
            agent_exec_status_of(body(r#"{"exited":0}"#)),
            AgentExecStatus::Running
        );
        // The measured shape of a finished, successful command.
        assert_eq!(
            agent_exec_status_of(body(
                r#"{"exited":1,"exitcode":0,"out-data":"hi\n","err-data":""}"#
            )),
            AgentExecStatus::Finished {
                exit_code: 0,
                stdout: "hi\n".into(),
                stderr: String::new(),
                signal: None,
            }
        );
        // A non-zero exit and a killed-by-signal case, exercised together
        // because the two fields are meaningless together (a real node
        // answers one or the other), and the type has to carry both.
        assert_eq!(
            agent_exec_status_of(body(
                r#"{"exited":1,"exitcode":137,"out-data":"","err-data":"boom"}"#
            )),
            AgentExecStatus::Finished {
                exit_code: 137,
                stdout: String::new(),
                stderr: "boom".into(),
                signal: None,
            }
        );
        assert_eq!(
            agent_exec_status_of(body(r#"{"exited":1,"signal":9}"#)),
            AgentExecStatus::Finished {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
                signal: Some(9),
            }
        );
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

    /// `--network lab-net` on a Proxmox create was accepted and dropped: the
    /// record said `Network: lab-net` and the VM sat on `vmbr0` (measured,
    /// `docs/discovery/58_…`). The default the CLI fills in has to stay
    /// accepted, or every plain create is refused.
    #[test]
    fn a_named_engine_network_is_refused_and_the_default_is_not() {
        for default in ["", "ingress", "bridge", "default"] {
            assert!(
                refuse_unsupported(&VmConfig {
                    name: "v".into(),
                    disk: "local-lvm:8".into(),
                    memory: "1G".into(),
                    network: default.into(),
                    ..Default::default()
                })
                .is_ok(),
                "the default network {default:?} must be accepted"
            );
        }
        let e = refuse_unsupported(&VmConfig {
            name: "v".into(),
            disk: "local-lvm:8".into(),
            memory: "1G".into(),
            network: "lab-net".into(),
            ..Default::default()
        })
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("network"),
            "the refusal must name the field: {e}"
        );
        assert!(
            e.contains("bridge"),
            "and point at what the node honours: {e}"
        );
    }

    /// Every write this crate sends to the node goes through [`Client::task`]
    /// (or [`Client::task_or_done`] for the one route that may apply inline).
    /// ADR-0049 D5 rule 1 lived only as prose until this test read the
    /// source: a new write added outside the task path passed CI, and a
    /// `POST …/config` HAD been added that way. The login is the one write
    /// that is not a node operation — it exchanges a credential for a
    /// ticket and changes nothing on the node — and it is named here with
    /// that reason, not skipped by pattern.
    ///
    /// Reads `sdn.rs` too, not just this file: `impl Client` is split across
    /// the two (the SDN zone/vnet/apply calls live in `sdn.rs`), and a gate
    /// that only reads `lib.rs` would wave through a write added there —
    /// exactly the blind spot this test exists to close.
    #[test]
    fn every_write_to_the_node_goes_through_the_task_path() {
        let sources = [include_str!("lib.rs"), include_str!("sdn.rs")];
        // Writes are what these helpers send; `fn post_form`/`fn delete`
        // themselves are definitions, not call sites.
        let write_calls = ["self.post_form(", "self.put_form(", "self.delete("];
        let allowed_outside_task: &[(&str, &str)] = &[
            (
                "login",
                "exchanges the credential for a ticket; it writes nothing on the node",
            ),
            (
                "agent_ping",
                "a guest-agent command answers inline (the agent itself, not a node worker) \
                 — there is no UPID to wait on",
            ),
            (
                "agent_exec",
                "starts a process inside the guest and answers its pid inline; the pid is \
                 polled by agent_exec_status, which is a plain read and not a write at all",
            ),
        ];
        let mut checked = 0;
        for src in sources {
            let src = src.split("#[cfg(test)]").next().unwrap();
            for needle in write_calls {
                let mut from = 0;
                while let Some(off) = src[from..].find(needle) {
                    let at = from + off;
                    from = at + needle.len();
                    checked += 1;
                    // The enclosing `fn`: the last `fn <name>(` before the call.
                    let head = &src[..at];
                    // The LATER of the two forms: an `rfind` of `fn ` alone would
                    // stop at a private fn defined before the enclosing `pub fn`.
                    let fn_at = [head.rfind("\n    fn "), head.rfind("\n    pub fn ")]
                        .into_iter()
                        .flatten()
                        .max()
                        .expect("a call site inside a fn");
                    let fn_name: String = head[fn_at..]
                        .trim_start_matches('\n')
                        .trim_start()
                        .trim_start_matches("pub ")
                        .trim_start_matches("fn ")
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if let Some((_, why)) = allowed_outside_task.iter().find(|(f, _)| *f == fn_name)
                    {
                        assert!(!why.is_empty());
                        continue;
                    }
                    // Inside the task path: a `self.task(`/`self.task_or_done(`
                    // opened in this fn whose argument list is still open here.
                    let body = &head[fn_at..];
                    let opened = ["self.task(", "self.task_or_done("]
                        .iter()
                        .filter_map(|t| body.rfind(t).map(|i| i + t.len()))
                        .max();
                    let inside = opened.is_some_and(|i| {
                        let mut depth = 1i32;
                        for c in body[i..].chars() {
                            match c {
                                '(' => depth += 1,
                                ')' => depth -= 1,
                                _ => {}
                            }
                            if depth == 0 {
                                return false;
                            }
                        }
                        true
                    });
                    assert!(
                    inside,
                    "`{needle}` in `fn {fn_name}` is a write outside the task path: route it through \
                     `self.task(...)` so its UPID is waited on and recorded in the ledger, or name \
                     it in `allowed_outside_task` with the reason"
                );
                }
            }
        }
        assert!(
            checked >= 10,
            "the scan found only {checked} write sites — is the source split right?"
        );
    }

    /// `POST …/config` answers a UPID or `null`; `null` is done ONLY there.
    #[test]
    fn a_null_answer_is_done_only_where_the_route_may_apply_inline() {
        assert_eq!(
            upid_or_done(r#"{"data":null}"#, "configure", true).unwrap(),
            None
        );
        assert!(upid_or_done(r#"{"data":null}"#, "start", false).is_err());
        assert_eq!(
            upid_or_done(
                r#"{"data":"UPID:pve:1:2:3:qmconfig:100:root@pam:"}"#,
                "configure",
                true
            )
            .unwrap()
            .as_deref(),
            Some("UPID:pve:1:2:3:qmconfig:100:root@pam:")
        );
        assert!(upid_or_done(r#"{"data":"done"}"#, "configure", true).is_err());
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
            trace_routes: None,
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

    /// Only the three plain verdicts go through — never a firewall group's
    /// name, which this client has no validation for at all.
    #[test]
    fn only_the_three_plain_verdicts_are_accepted_as_action() {
        for ok in ["ACCEPT", "DROP", "REJECT"] {
            assert!(validate_firewall_action(ok).is_ok(), "{ok:?}");
        }
        for bad in ["accept", "Drop", "+mygroup", "", "ALLOW"] {
            let e = validate_firewall_action(bad).unwrap_err().to_string();
            assert!(e.contains(bad), "the refusal must name the value: {e}");
        }
    }

    #[test]
    fn only_in_and_out_are_accepted_as_type() {
        for ok in ["in", "out"] {
            assert!(validate_firewall_direction(ok).is_ok(), "{ok:?}");
        }
        for bad in ["IN", "both", "", "forward"] {
            let e = validate_firewall_direction(bad).unwrap_err().to_string();
            assert!(e.contains(bad), "the refusal must name the value: {e}");
        }
    }

    /// Each optional field lands under the node's own name exactly once, and
    /// an all-`None` set sends nothing at all — the shape `create_form`'s own
    /// sibling test already holds `cloud_init_form` to.
    #[test]
    fn each_optional_field_of_the_rule_lands_once_under_the_nodes_own_name() {
        assert_eq!(
            firewall_rule_common_fields(&FirewallRuleOpts::default()),
            []
        );

        let opts = FirewallRuleOpts {
            comment: Some("web"),
            source: Some("10.0.0.0/8"),
            dest: Some("192.168.1.5"),
            proto: Some("tcp"),
            dport: Some("443"),
            sport: Some("1024:65535"),
            iface: Some("net0"),
            macro_name: Some("ssh"),
            // Read only by `update_firewall_rule`, never by this helper.
            enable: Some(true),
            rule_type: Some("in"),
            action: Some("ACCEPT"),
        };
        let fields = firewall_rule_common_fields(&opts);
        for (key, want) in [
            ("comment", "web"),
            ("source", "10.0.0.0/8"),
            ("dest", "192.168.1.5"),
            ("proto", "tcp"),
            ("dport", "443"),
            ("sport", "1024:65535"),
            ("iface", "net0"),
            ("macro", "ssh"),
        ] {
            assert_eq!(
                fields.iter().filter(|(k, _)| *k == key).count(),
                1,
                "{key} must appear exactly once: {fields:?}"
            );
            assert_eq!(
                fields
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.as_str()),
                Some(want)
            );
        }
        // `enable`/`type`/`action` are NOT emitted by this helper — each
        // caller of `add_firewall_rule`/`update_firewall_rule` handles them
        // on its own, per the struct's doc comment.
        for absent in ["enable", "type", "action"] {
            assert!(
                !fields.iter().any(|(k, _)| *k == absent),
                "{absent} must not come from this helper: {fields:?}"
            );
        }
    }

    /// The shape Proxmox's own `pve-fw-alias-name`/`pve-fw-ipset-name`
    /// formats accept: a letter, then one or more letters/digits/`-`/`_`.
    #[test]
    fn firewall_object_names_need_a_leading_letter_and_a_second_character() {
        for ok in ["a1", "Web", "my-set_1", "z9"] {
            assert!(validate_firewall_object_name(ok).is_ok(), "{ok:?}");
        }
        for bad in ["", "a", "1abc", "-abc", "_abc", "a b", "a/b", "a."] {
            let e = validate_firewall_object_name(bad).unwrap_err().to_string();
            assert!(e.contains(bad), "the refusal must name the value: {e}");
        }
    }

    /// An address with a `/<prefix>` past the address family's own bit width
    /// is refused, and so is anything that does not parse as an address at
    /// all — but the shallow validator does not try to be the node's own
    /// `pve-fw-addr-spec` grammar (DNS-style ranges included).
    #[test]
    fn firewall_addresses_are_checked_by_shape_not_reimplemented() {
        for ok in [
            "10.0.0.0/8",
            "192.168.1.5",
            "::1",
            "2001:db8::/32",
            "0.0.0.0/0",
        ] {
            assert!(validate_firewall_cidr(ok).is_ok(), "{ok:?}");
        }
        for bad in [
            "",
            "not-an-address",
            "10.0.0.0/33",
            "::1/129",
            "10.0.0.0/",
            "10.0.0.0/-1",
        ] {
            let e = validate_firewall_cidr(bad).unwrap_err().to_string();
            assert!(e.contains(bad), "the refusal must name the value: {e}");
        }
    }

    /// `comment` and `nomatch` each land under the node's own name exactly
    /// once, and an all-`None` set sends nothing — the same shape
    /// `firewall_rule_common_fields`'s own sibling test already holds its
    /// counterpart to.
    #[test]
    fn ipset_cidr_fields_default_sends_nothing_and_each_key_lands_once() {
        assert_eq!(ipset_cidr_fields(&IpsetCidrOpts::default()), []);

        let fields = ipset_cidr_fields(&IpsetCidrOpts {
            comment: Some("web"),
            nomatch: Some(true),
        });
        assert_eq!(
            fields.iter().filter(|(k, _)| *k == "comment").count(),
            1,
            "{fields:?}"
        );
        assert_eq!(
            fields
                .iter()
                .find(|(k, _)| *k == "comment")
                .map(|(_, v)| v.as_str()),
            Some("web")
        );
        assert_eq!(
            fields.iter().filter(|(k, _)| *k == "nomatch").count(),
            1,
            "{fields:?}"
        );
        assert_eq!(
            fields
                .iter()
                .find(|(k, _)| *k == "nomatch")
                .map(|(_, v)| v.as_str()),
            Some("1")
        );

        let fields = ipset_cidr_fields(&IpsetCidrOpts {
            comment: None,
            nomatch: Some(false),
        });
        assert_eq!(
            fields
                .iter()
                .find(|(k, _)| *k == "nomatch")
                .map(|(_, v)| v.as_str()),
            Some("0")
        );
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

    /// [`Client::agent_ping`]'s whole reason to exist: this ONE 500 has to
    /// read as "no agent, not a failure" while every other 500 — and every
    /// other status — reads as a real error that `?` must propagate.
    #[test]
    fn is_agent_not_running_so_matches_the_nodes_own_wording() {
        let st = |code: u16, body: &str| {
            classify_status(
                reqwest::StatusCode::from_u16(code).unwrap(),
                "https://pve",
                "/nodes/pve/qemu/100/agent/ping",
                body,
            )
        };
        assert!(is_agent_not_running(&st(
            500,
            "QEMU guest agent is not running\n"
        )));
        // Same status, different reason: a real failure, not "no agent".
        assert!(!is_agent_not_running(&st(500, "unable to open file")));
        assert!(!is_agent_not_running(&st(401, "bad ticket")));
        assert!(!is_agent_not_running(&st(403, "permission denied")));
        assert!(!is_agent_not_running(&Error::Request(
            "proxmox: request failed: connection refused".into()
        )));
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
        assert!(matches!(st(404, ""), Error::NodeNotFound(_)));
        assert!(matches!(st(409, ""), Error::NodeConflict(_)));
        assert!(matches!(st(400, ""), Error::BadRequest(_)));
        assert!(matches!(st(422, ""), Error::BadRequest(_)));
        assert!(matches!(st(502, ""), Error::NodeUnavailable(_)));
        assert!(matches!(st(503, ""), Error::NodeUnavailable(_)));
        assert!(matches!(st(504, ""), Error::NodeUnavailable(_)));
        // Proxmox says these two with a 500; the body carries the verdict.
        let missing = st(
            500,
            "Configuration file 'nodes/pve/qemu-server/100.conf' does not exist\n",
        );
        assert!(matches!(missing, Error::NodeNotFound(_)), "{missing}");
        assert!(
            missing.to_string().contains("/nodes/pve/qemu/100/config"),
            "names the route"
        );
        assert!(!missing.to_string().contains("?x=1"), "without the query");
        assert!(matches!(
            st(500, "VM 100 already exists"),
            Error::NodeConflict(_)
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
            upid_or_done(
                r#"{"data":"UPID:pve:0000ABCD:00001234:5F3E:qmstart:100:root@pam:"}"#,
                "start",
                false
            )
            .unwrap()
            .as_deref(),
            Some("UPID:pve:0000ABCD:00001234:5F3E:qmstart:100:root@pam:")
        );
        // A string that is not a task id, a number, and no `data` at all.
        assert!(matches!(
            upid_or_done(r#"{"data":"ok"}"#, "start", false),
            Err(Error::UnexpectedAnswer(_))
        ));
        assert!(matches!(
            upid_or_done(r#"{"data":100}"#, "start", false),
            Err(Error::UnexpectedAnswer(_))
        ));
        assert!(matches!(
            upid_or_done(r#"{"data":"#, "start", false),
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
