//! The state of the Proxmox cluster's OWN SDN (not `delonix-sdn`), and the
//! global lock over it.
//!
//! [`crate::sdn`]'s doc comment explains the staged model: every SDN write
//! edits a PENDING configuration, and `PUT /cluster/sdn` promotes it to the
//! running one and reloads every node. What that model leaves open is who
//! else is staging at the same time. The pending configuration is ONE per
//! cluster, so an apply pushes whatever anybody staged, not only what the
//! caller did — and an apply after a failed half-change pushes the half.
//!
//! Proxmox VE 9 answers that with a global lock (`POST /cluster/sdn/lock`),
//! and this module is how this client uses it:
//!
//! * [`Client::sdn_transaction`] takes the lock (refusing when someone
//!   else's staged changes are already waiting — [`crate::Error::SdnPendingChanges`]),
//!   runs the caller's changes with the token on every write, and then
//!   either applies with the token (the lock released by the same call) or,
//!   when a change failed, ROLLS BACK with the token — the pending
//!   configuration goes back to the running one, and nothing half-done is
//!   ever applied.
//! * The token travels on the writes WITHOUT a new parameter on every SDN
//!   function: the client remembers it for the thread that opened the
//!   transaction and adds it where the node takes it ([`lock_token_applies`]).
//!
//! # Three facts measured against a live PVE 9.2.2 node (2026-09-27)
//!
//! * **`release-lock` defaults to 1 in the schema and to NOTHING in the
//!   handler.** `PUT /cluster/sdn` and `POST /cluster/sdn/rollback` document
//!   `release-lock` with `default => 1`, and read it with `extract_param`,
//!   which does not apply schema defaults — so a rollback sent with a token
//!   and without `release-lock=1` discards the pending changes and KEEPS the
//!   lock (measured: the next `POST /cluster/sdn/lock` was refused). This
//!   client always sends `release-lock=1` explicitly.
//! * **A fabric DELETE checks the lock although its schema does not list
//!   `lock-token`.** With the lock held, `DELETE /cluster/sdn/fabrics/fabric/{id}`
//!   without a token failed with "invalid lock token provided!", and the same
//!   request with `?lock-token=` in the query succeeded — so the token goes
//!   there too.
//! * **The node's message does not tell "locked" from "wrong token".** A write
//!   with no token while the lock is held and a write with a stale token both
//!   answer "invalid lock token provided!" — hence one error class,
//!   [`crate::Error::SdnLocked`], for both.
//!
//! # What is not a lock
//!
//! The vnet firewall routes (`…/vnets/{vnet}/firewall/*`) and the IPAM
//! reservation routes (`…/vnets/{vnet}/ips`) do not take the token (their
//! schema has no `lock-token`, and the Perl side refuses an unknown
//! parameter): they are not part of the staged configuration, and an apply
//! or a rollback does not touch them. [`Client::sdn_transaction`] does not
//! make them atomic, and says so.

use crate::{parse, Client, Error, Ledger, Result, TaskKind, Wrapped};

/// The token of the cluster's global SDN lock, as `POST /cluster/sdn/lock`
/// answered it. Opaque: the only thing a caller does with it is hand it back.
#[derive(Clone, PartialEq, Eq)]
pub struct SdnLockToken(String);

impl SdnLockToken {
    /// The raw token, for an operator who has to release a lock by hand.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SdnLockToken {
    // Not a secret — any holder of `SDN.Allocate` can force the lock away —
    // but printed whole so a message that names it can be acted on.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SdnLockToken({})", self.0)
    }
}

/// What an apply WOULD change on one node (`GET /cluster/sdn/dry-run`): the
/// unified diff of the node's `/etc/network/interfaces.d/sdn` and of its
/// `/etc/frr/frr.conf` between the running and the pending configuration.
/// `None` is "no difference" — measured: the node answers `null`, not an
/// empty string, for an unchanged file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SdnDryRun {
    pub interfaces_diff: Option<String>,
    pub frr_diff: Option<String>,
}

impl SdnDryRun {
    /// True when an apply would change neither file on that node.
    pub fn is_empty(&self) -> bool {
        self.interfaces_diff.is_none() && self.frr_diff.is_none()
    }
}

/// One object of the staged SDN configuration that differs from the running
/// one — `state` is the node's own word (`new`, `changed`, `deleted`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SdnPendingChange {
    /// `zone`, `vnet`, `subnet`, `controller`, `prefix-list`,
    /// `route-map-entry`, `fabric`, `fabric-node`.
    pub kind: &'static str,
    pub id: String,
    pub state: String,
}

/// Does a write to `path` with `method` carry the SDN lock token?
///
/// Pure, so the rule is a test. Every `/cluster/sdn/…` write takes it except
/// the ones outside the staged configuration (vnet firewall, IPAM
/// reservations), the lock routes themselves, and the two calls that carry it
/// EXPLICITLY (the apply, `PUT /cluster/sdn`, and the rollback) — a token
/// added there behind the caller's back would release the lock without
/// `release-lock=1` having been decided.
pub(crate) fn lock_token_applies(method: &str, path: &str) -> bool {
    if method == "GET" {
        return false;
    }
    let path = path.split('?').next().unwrap_or(path);
    let Some(rest) = path
        .strip_prefix("/cluster/sdn")
        .and_then(|r| r.strip_prefix('/'))
    else {
        return false;
    };
    if rest == "lock" || rest == "rollback" {
        return false;
    }
    if let Some(vnet_rest) = rest.strip_prefix("vnets/") {
        let mut parts = vnet_rest.splitn(3, '/');
        let _vnet = parts.next();
        if matches!(parts.next(), Some("firewall") | Some("ips")) {
            return false;
        }
    }
    true
}

/// The form with `lock-token` appended when there is one (and the caller did
/// not send its own). Owned `&str` pairs, so the caller's slice is untouched.
pub(crate) fn with_lock_token<'a>(
    form: &[(&'a str, &'a str)],
    token: Option<&'a str>,
) -> Vec<(&'a str, &'a str)> {
    let mut out = form.to_vec();
    if let Some(t) = token {
        if !out.iter().any(|(k, _)| *k == "lock-token") {
            out.push(("lock-token", t));
        }
    }
    out
}

/// Clears the client's remembered token when the transaction ends, however
/// it ends — a panic in the caller's closure included. It does NOT roll back
/// on a panic: a network call from a destructor during an unwind is the
/// wrong place for it, and the lock is then left for an operator (its token
/// was never printed, so `force=1` is the way out) — said here rather than
/// discovered.
struct TokenScope<'c> {
    client: &'c Client,
}

impl Drop for TokenScope<'_> {
    fn drop(&mut self) {
        if let Ok(mut g) = self.client.sdn_lock.lock() {
            *g = None;
        }
    }
}

fn items_of(v: &serde_json::Value) -> Vec<serde_json::Value> {
    v.as_array().cloned().unwrap_or_default()
}

/// The pending objects of one list answer (`?pending=1`): every item with a
/// `state`. A NEW object carries its id only inside `pending` (measured on a
/// prefix list), so the id is looked up at the top level first, then there.
pub(crate) fn pending_of(
    kind: &'static str,
    id_key: &str,
    items: &[serde_json::Value],
) -> Vec<SdnPendingChange> {
    items
        .iter()
        .filter_map(|it| {
            let state = it.get("state")?.as_str()?.to_string();
            let id = it
                .get(id_key)
                .or_else(|| it.get("pending").and_then(|p| p.get(id_key)))
                .map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            Some(SdnPendingChange { kind, id, state })
        })
        .collect()
}

impl Client {
    /// The token to add to a write, if a transaction on THIS thread holds the
    /// lock and the route takes it.
    pub(crate) fn sdn_lock_token_for(&self, method: &str, path: &str) -> Option<String> {
        if !lock_token_applies(method, path) {
            return None;
        }
        let g = self.sdn_lock.lock().ok()?;
        match &*g {
            Some((thread, token)) if *thread == std::thread::current().id() => Some(token.clone()),
            _ => None,
        }
    }

    /// The top of the SDN tree (`GET /cluster/sdn`): the names of its
    /// subdirectories (`zones`, `vnets`, `controllers`, `ipams`, `dns`,
    /// `fabrics`, `prefix-lists`, `route-maps`). A directory index, not a
    /// state — "is anything pending" is [`Self::sdn_pending_changes`].
    pub fn sdn_index(&self) -> Result<Vec<serde_json::Value>> {
        let body = self.get("/cluster/sdn")?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "GET /cluster/sdn")?;
        Ok(w.data)
    }

    /// The fabrics subtree index (`GET /cluster/sdn/fabrics`: `fabric`,
    /// `node`, `all`).
    pub fn sdn_fabrics_index(&self) -> Result<Vec<serde_json::Value>> {
        let body = self.get("/cluster/sdn/fabrics")?;
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "GET /cluster/sdn/fabrics")?;
        Ok(w.data)
    }

    /// Every node of every fabric (`GET /cluster/sdn/fabrics/node`), the
    /// PENDING configuration unless `running` — what
    /// [`Self::sdn_fabric_nodes`] answers for one fabric, for all of them.
    pub fn sdn_fabric_nodes_all(&self, running: bool) -> Result<Vec<serde_json::Value>> {
        let body = if running {
            self.get("/cluster/sdn/fabrics/node?running=1")?
        } else {
            self.get("/cluster/sdn/fabrics/node")?
        };
        let w: Wrapped<Vec<serde_json::Value>> = parse(&body, "GET /cluster/sdn/fabrics/node")?;
        Ok(w.data)
    }

    /// What an apply would change on `node` (this client's node when `None`)
    /// — `GET /cluster/sdn/dry-run`. The node renders the pending
    /// configuration and diffs it against its own files; nothing is written.
    ///
    /// Not a substitute for [`Self::sdn_pending_changes`]: a staged object
    /// that renders to nothing on that node (a zone with no vnet, a controller
    /// for another node) is pending and has an empty dry-run.
    pub fn sdn_dry_run(&self, node: Option<&str>) -> Result<SdnDryRun> {
        let node = node.unwrap_or(&self.node);
        crate::validate_node_name(node)?;
        let body = self.get(&format!("/cluster/sdn/dry-run?node={node}"))?;
        let w: Wrapped<serde_json::Value> = parse(&body, "GET /cluster/sdn/dry-run")?;
        let text = |k: &str| {
            w.data
                .get(k)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        Ok(SdnDryRun {
            interfaces_diff: text("interfaces-diff"),
            frr_diff: text("frr-diff"),
        })
    }

    /// Every object of the staged configuration that differs from the running
    /// one: zones, vnets and their subnets, controllers, prefix lists, route-map
    /// entries, fabrics and fabric nodes — the seven kinds a rollback restores
    /// (`PVE::API2::Network::SDN`'s `rollback` handler writes exactly these).
    /// Empty is the node's own "nothing pending", and what a caller reads after
    /// an apply or a rollback.
    pub fn sdn_pending_changes(&self) -> Result<Vec<SdnPendingChange>> {
        let list = |path: &str| -> Result<Vec<serde_json::Value>> {
            let body = self.get(path)?;
            let w: Wrapped<serde_json::Value> = parse(&body, path)?;
            Ok(items_of(&w.data))
        };
        let mut out = Vec::new();
        out.extend(pending_of(
            "zone",
            "zone",
            &list("/cluster/sdn/zones?pending=1")?,
        ));
        let vnets = list("/cluster/sdn/vnets?pending=1")?;
        out.extend(pending_of("vnet", "vnet", &vnets));
        for v in &vnets {
            let Some(vnet) = v
                .get("vnet")
                .or_else(|| v.get("pending").and_then(|p| p.get("vnet")))
                .and_then(|x| x.as_str())
            else {
                continue;
            };
            crate::sdn::validate_sdn_id(vnet)?;
            let path = format!("/cluster/sdn/vnets/{vnet}/subnets?pending=1");
            out.extend(pending_of("subnet", "subnet", &list(&path)?));
        }
        out.extend(pending_of(
            "controller",
            "controller",
            &list("/cluster/sdn/controllers?pending=1")?,
        ));
        out.extend(pending_of(
            "prefix-list",
            "id",
            &list("/cluster/sdn/prefix-lists?pending=1&verbose=1")?,
        ));
        out.extend(pending_of(
            "route-map-entry",
            "route-map-id",
            &list("/cluster/sdn/route-maps/entries?pending=1")?,
        ));
        out.extend(pending_of(
            "fabric",
            "id",
            &list("/cluster/sdn/fabrics/fabric?pending=1")?,
        ));
        out.extend(pending_of(
            "fabric-node",
            "node_id",
            &list("/cluster/sdn/fabrics/node?pending=1")?,
        ));
        Ok(out)
    }

    /// Takes the cluster's global SDN lock (`POST /cluster/sdn/lock`) and
    /// answers its token.
    ///
    /// Without `allow_pending` the node refuses when staged changes are
    /// already waiting ([`Error::SdnPendingChanges`]) — the refusal that keeps
    /// this client from applying someone else's half-finished work. Held by
    /// someone else: [`Error::SdnLocked`].
    ///
    /// Not a task: the node answers the token itself, inline. A LOST answer
    /// here is the one case this client cannot recover — the lock may be
    /// held under a token nobody knows — and the error says what releases it.
    pub fn acquire_sdn_lock(&self, allow_pending: bool) -> Result<SdnLockToken> {
        let form: &[(&str, &str)] = if allow_pending {
            &[("allow-pending", "1")]
        } else {
            &[]
        };
        let body = self
            .post_form("/cluster/sdn/lock", form, true)
            .map_err(|e| match e {
                Error::Request(why) => Error::Request(format!(
                    "{why} — the answer to the SDN lock request was lost, so the lock may now \
                     be held under a token nobody has; `DELETE /cluster/sdn/lock` with \
                     `force=1` releases it once you are sure no one else is using it"
                )),
                other => other,
            })?;
        let w: Wrapped<serde_json::Value> = parse(&body, "POST /cluster/sdn/lock")?;
        match w.data.as_str().filter(|s| !s.is_empty()) {
            Some(t) => Ok(SdnLockToken(t.to_string())),
            None => Err(Error::UnexpectedAnswer(format!(
                "proxmox: the SDN lock request did not answer a token: {}",
                crate::truncate_chars(&body, 200)
            ))),
        }
    }

    /// Releases the lock this client holds (`DELETE /cluster/sdn/lock` with
    /// its token in the query). While ANOTHER holder has the lock, a stale
    /// token is [`Error::SdnLocked`]; measured, releasing a lock nobody holds
    /// succeeds whatever the token — the node checks a token only against a
    /// lock that exists.
    pub fn release_sdn_lock(&self, ledger: &Ledger, token: &SdnLockToken) -> Result<()> {
        let path = format!("/cluster/sdn/lock?lock-token={}", token.0);
        self.task_or_done(
            ledger,
            crate::sdn::SDN_VMID,
            TaskKind::ReleaseSdnLock,
            || self.delete(&path),
            None,
        )
    }

    /// Releases the lock WITHOUT its token (`force=1`) — for a lock whose
    /// holder is gone. It takes the lock away from whoever has it; a caller
    /// that is not sure nobody is using it should not call this.
    pub fn force_release_sdn_lock(&self, ledger: &Ledger) -> Result<()> {
        self.task_or_done(
            ledger,
            crate::sdn::SDN_VMID,
            TaskKind::ReleaseSdnLock,
            || self.delete("/cluster/sdn/lock?force=1"),
            None,
        )
    }

    /// Discards every pending SDN change (`POST /cluster/sdn/rollback`): the
    /// running configuration is written back over the pending one. With a
    /// `token` it is sent, with `release-lock=1` — explicitly, because the
    /// handler does not apply the schema's default (see the module doc).
    ///
    /// The probe after a lost answer is the node's own pending list: nothing
    /// pending is the rollback's effect.
    pub fn rollback_sdn(&self, ledger: &Ledger, token: Option<&SdnLockToken>) -> Result<()> {
        let form: Vec<(&str, &str)> = match token {
            Some(t) => vec![("lock-token", t.0.as_str()), ("release-lock", "1")],
            None => Vec::new(),
        };
        self.task_or_done(
            ledger,
            crate::sdn::SDN_VMID,
            TaskKind::RollbackSdn,
            || self.post_form("/cluster/sdn/rollback", &form, true),
            Some(&|| Ok(self.sdn_pending_changes()?.is_empty())),
        )
    }

    /// [`Self::apply_sdn`] under the lock: `PUT /cluster/sdn` with the token
    /// and `release-lock=1`, so the node commits and frees the lock in the
    /// same call. The reload that follows is the same cluster-wide task, waited
    /// on the same way.
    pub fn apply_sdn_locked(&self, ledger: &Ledger, token: &SdnLockToken) -> Result<()> {
        let form = [("lock-token", token.0.as_str()), ("release-lock", "1")];
        self.task_or_done(
            ledger,
            crate::sdn::SDN_VMID,
            TaskKind::ApplySdn,
            || self.put_form("/cluster/sdn", &form),
            None,
        )
    }

    /// Stages `change` under the cluster's SDN lock and applies it — or, if
    /// `change` fails, discards it.
    ///
    /// 1. Take the lock, refusing when someone else's staged changes are
    ///    already waiting ([`Error::SdnPendingChanges`]) or the lock is held
    ///    ([`Error::SdnLocked`]). Nothing is sent after either refusal.
    /// 2. Run `change` on this thread; every staged SDN write it sends carries
    ///    the token.
    /// 3. `change` failed → roll back with the token (the lock released by the
    ///    same call) and return `change`'s error. If the rollback fails too,
    ///    [`Error::SdnRollbackFailed`] names both failures and the token.
    /// 4. `change` succeeded → [`Self::apply_sdn_locked`]. If the apply request
    ///    itself fails before the node committed, the lock is released with
    ///    the token before the error is returned.
    ///
    /// `change` must not call [`Self::apply_sdn`] itself: the apply is this
    /// function's job, and an apply without the token is refused by the node
    /// while the lock is held. The vnet firewall and IPAM reservation writes
    /// are not part of the staged configuration — a rollback does not undo
    /// them (see the module doc).
    pub fn sdn_transaction<T>(
        &self,
        ledger: &Ledger,
        change: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let token = self.acquire_sdn_lock(false)?;
        let outcome = {
            if let Ok(mut g) = self.sdn_lock.lock() {
                *g = Some((std::thread::current().id(), token.0.clone()));
            }
            let _scope = TokenScope { client: self };
            change()
        };
        match outcome {
            Err(e) => match self.rollback_sdn(ledger, Some(&token)) {
                Ok(()) => Err(e),
                Err(rb) => Err(Error::SdnRollbackFailed(format!(
                    "proxmox: the SDN change failed ({e}) and discarding it failed too ({rb}); \
                     pending changes and the SDN lock (token {}) may be left on the cluster",
                    token.0
                ))),
            },
            Ok(v) => match self.apply_sdn_locked(ledger, &token) {
                Ok(()) => Ok(v),
                Err(e) => {
                    // The commit happens before the reload task is forked: a
                    // failed REQUEST may have left the lock held, a failed
                    // TASK has not (the node released it at commit). Release
                    // it if it is still ours; a refusal here only means it
                    // was already gone.
                    if let Err(rel) = self.release_sdn_lock(ledger, &token) {
                        tracing::debug!(error = %rel, "proxmox: the SDN lock was already released after a failed apply");
                    }
                    Err(e)
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{lock_token_applies, pending_of, with_lock_token};

    #[test]
    fn the_lock_token_goes_on_staged_writes_only() {
        for (m, p) in [
            ("POST", "/cluster/sdn/zones"),
            ("PUT", "/cluster/sdn/zones/z1"),
            ("DELETE", "/cluster/sdn/vnets/v1"),
            ("POST", "/cluster/sdn/vnets/v1/subnets"),
            ("POST", "/cluster/sdn/controllers"),
            ("DELETE", "/cluster/sdn/prefix-lists/pl1/entries/5"),
            ("POST", "/cluster/sdn/route-maps/entries"),
            // Measured: the fabric DELETE checks the lock although its schema
            // does not list the parameter.
            ("DELETE", "/cluster/sdn/fabrics/fabric/f1"),
            ("DELETE", "/cluster/sdn/fabrics/node/f1/pve?x=1"),
            ("POST", "/cluster/sdn/ipams"),
        ] {
            assert!(lock_token_applies(m, p), "{m} {p} should carry the token");
        }
        for (m, p) in [
            ("GET", "/cluster/sdn/zones"),
            ("PUT", "/cluster/sdn"),
            ("POST", "/cluster/sdn/lock"),
            ("DELETE", "/cluster/sdn/lock"),
            ("POST", "/cluster/sdn/rollback"),
            ("POST", "/cluster/sdn/vnets/v1/firewall/rules"),
            ("PUT", "/cluster/sdn/vnets/v1/firewall/options"),
            ("POST", "/cluster/sdn/vnets/v1/ips"),
            ("DELETE", "/cluster/sdn/vnets/v1/ips?ip=10.0.0.5"),
            ("POST", "/nodes/pve/qemu"),
            ("PUT", "/cluster/firewall/options"),
        ] {
            assert!(
                !lock_token_applies(m, p),
                "{m} {p} must not carry the token"
            );
        }
    }

    #[test]
    fn a_token_is_appended_once_and_never_over_the_callers_own() {
        let form = [("zone", "z1"), ("type", "simple")];
        assert_eq!(with_lock_token(&form, None), form.to_vec());
        let with = with_lock_token(&form, Some("t-1"));
        assert_eq!(with.last(), Some(&("lock-token", "t-1")));
        let own = [("lock-token", "mine")];
        assert_eq!(with_lock_token(&own, Some("t-1")), own.to_vec());
    }

    #[test]
    fn a_new_objects_id_is_read_from_inside_pending() {
        let items: Vec<serde_json::Value> = serde_json::from_str(
            r#"[{"state":"new","type":"prefix-list","pending":{"id":"pl1"}},
                {"id":"pl2","state":"deleted"},
                {"id":"pl3"}]"#,
        )
        .unwrap();
        let p = pending_of("prefix-list", "id", &items);
        assert_eq!(p.len(), 2, "an item without a state is not pending: {p:?}");
        assert_eq!((p[0].id.as_str(), p[0].state.as_str()), ("pl1", "new"));
        assert_eq!((p[1].id.as_str(), p[1].state.as_str()), ("pl2", "deleted"));
    }
}
