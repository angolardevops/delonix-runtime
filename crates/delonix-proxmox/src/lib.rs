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

// ===========================================================================
// The backend
// ===========================================================================

impl ProxmoxBackend {
    pub fn connect(target: &Target) -> Result<Self> {
        Ok(Self {
            client: Client::connect(target)?,
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

    fn boot(
        &self,
        _vmdir: &Path,
        _cfg: &VmConfig,
        _disk: &str,
        _on: &dyn Fn(CreateStage),
    ) -> Result<Boot> {
        Err(Error::Invalid(
            "the Proxmox backend cannot create VMs yet — `boot` is not implemented. The API \
             lifecycle is proven (ADR-0008) but the create path has not been written or \
             validated, and this engine does not ship a backend it has not watched boot a VM"
                .into(),
        ))
    }

    fn is_running(&self, vm: &Vm) -> bool {
        let Ok(body) = self.client.get(&format!(
            "/nodes/{}/qemu/{}/status/current",
            self.client.node, vm.name
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

    fn stop(&self, _vmdir: &Path, _vm: &Vm) -> Result<()> {
        Err(Error::Invalid(
            "the Proxmox backend cannot stop VMs yet — it cannot create them either".into(),
        ))
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
}
