//! The NAT role on an OPNsense appliance (ADR-0059 F5): source NAT
//! (`firewall/source_nat`) and destination NAT (`firewall/d_nat`).
//!
//! Measured on 26.1.2_5 before any of this was written:
//!
//! * **The two controllers are two models.** `source_nat` is flat
//!   (`source_net`, `description`, `enabled`, `categories` as uuids), like
//!   the filter. `d_nat` is the older shape: nested `source`/`destination`,
//!   `descr`, `disabled`, and the mark goes in `category` BY NAME — written
//!   that way, a read answers the category's uuid under `categories`, as the
//!   other tables do. `categories` written directly is ignored.
//! * **Any apply pushes everything.** A source NAT rule added and not
//!   applied was loaded by `firewall/filter/apply`; `source_nat/apply`
//!   loaded two destination NAT rules that were only staged. So a commit
//!   here checks the filter's and the aliases' pending changes too.
//! * **A NAT rule has no label in pf.** The filter's rules carry their uuid
//!   as a label; these do not. A rule is recognized as loaded by the text of
//!   its line (`diagnostics/firewall/pf_statistics/rules`, section
//!   `nat rules`):
//!
//!   ```text
//!   nat on vtnet0 inet from 10.77.0.0/24 to any -> (vtnet0:0) port 1024:65535
//!   rdr on vtnet0 inet proto tcp from any to (vtnet0:1) port = 8443 -> 10.77.0.10 port 443
//!   ```
//!
//!   The interface address is `<interface>ip` in the configuration (`lanip`)
//!   and `(vtnet0:N)` in pf.
//!
//! What this does NOT see, said here rather than implied by a green commit:
//! a NAT rule someone else DELETED and did not apply (nothing in the
//! configuration names it any more, and the appliance's own automatic lines
//! are not told apart from it), and a staged rule whose line cannot be
//! derived (a source that is an alias, a target that is not an address).
//! Two rules with the same source network, or the same protocol, target and
//! target port, read as one line.

use crate::{
    is_uuid, owner_of_row, row_uuid, str_field, truncate_chars, Client, Error, PendingChange,
    Result, StagedOp,
};
use delonix_networking::gateway::EnsureOutcome;
use delonix_networking::nat::{NatKind, NatObserved, NatProvider, NatRule, INTERFACE_ADDRESS};
use delonix_networking::ownership::{Owner, OwnerMark, RemoveOutcome};
use serde_json::Value;

/// The two tables, and what differs between their routes and field names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Table {
    Source,
    Destination,
}

impl Table {
    const BOTH: [Table; 2] = [Table::Source, Table::Destination];

    fn of(kind: NatKind) -> Self {
        match kind {
            NatKind::Source => Table::Source,
            NatKind::Destination => Table::Destination,
        }
    }

    fn controller(self) -> &'static str {
        match self {
            Table::Source => "firewall/source_nat",
            Table::Destination => "firewall/d_nat",
        }
    }

    fn description_field(self) -> &'static str {
        match self {
            Table::Source => "description",
            Table::Destination => "descr",
        }
    }

    fn enabled(self, row: &Value) -> bool {
        match self {
            Table::Source => str_field(row, "enabled") != "0",
            Table::Destination => str_field(row, "disabled") != "1",
        }
    }
}

/// How a rule's line reads in pf — what tells a loaded rule from a staged
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Signature {
    /// `nat on … from <source> to …`
    Source { source: String },
    /// `rdr on … proto <protocol> … -> <target> port <port>`
    Destination {
        protocol: String,
        target: String,
        port: String,
    },
}

/// pf prints a `/32` as the bare address.
fn printed_net(cidr: &str) -> String {
    cidr.strip_suffix("/32").unwrap_or(cidr).to_string()
}

fn is_ipv4(s: &str) -> bool {
    s.parse::<std::net::Ipv4Addr>().is_ok()
}

impl Signature {
    fn of_rule(rule: &NatRule) -> Self {
        match rule.kind {
            NatKind::Source => Signature::Source {
                source: printed_net(&rule.source),
            },
            NatKind::Destination => Signature::Destination {
                protocol: rule.protocol.clone().unwrap_or_default(),
                target: rule.target.clone(),
                port: rule.target_port.map(|p| p.to_string()).unwrap_or_default(),
            },
        }
    }

    /// The signature of a configured row, or `None` when its line cannot be
    /// derived (see the module doc).
    fn of_row(table: Table, row: &Value) -> Option<Self> {
        match table {
            Table::Source => {
                let source = str_field(row, "source_net");
                let (addr, _) = source.split_once('/').unwrap_or((&source, ""));
                is_ipv4(addr).then(|| Signature::Source {
                    source: printed_net(&source),
                })
            }
            Table::Destination => {
                let protocol = str_field(row, "protocol").to_ascii_lowercase();
                let target = str_field(row, "target");
                let port = str_field(row, "local-port");
                (matches!(protocol.as_str(), "tcp" | "udp")
                    && is_ipv4(&target)
                    && port.parse::<u16>().is_ok())
                .then_some(Signature::Destination {
                    protocol,
                    target,
                    port,
                })
            }
        }
    }

    fn loaded_in(&self, lines: &[String]) -> bool {
        lines.iter().any(|l| match self {
            Signature::Source { source } => {
                l.starts_with("nat on ") && l.contains(&format!(" from {source} to "))
            }
            Signature::Destination {
                protocol,
                target,
                port,
            } => {
                l.starts_with("rdr on ")
                    && l.contains(&format!(" proto {protocol} "))
                    && l.ends_with(&format!(" -> {target} port {port}"))
            }
        })
    }
}

/// The NAT lines pf has loaded, without their `@N ` position. An answer
/// without the `nat rules` section is refused rather than read as "nothing
/// loaded", which would call every configured rule unapplied.
fn running_nat_lines(answer: &Value) -> Result<Vec<String>> {
    let rules = answer
        .get("rules")
        .and_then(|r| r.get("nat rules"))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            Error::Decode(format!(
                "diagnostics/firewall/pf_statistics/rules answered without its nat rules: {}",
                truncate_chars(&answer.to_string(), 200)
            ))
        })?;
    Ok(rules
        .keys()
        .map(|line| match line.split_once(' ') {
            Some((at, rest)) if at.starts_with('@') => rest.to_string(),
            _ => line.clone(),
        })
        .collect())
}

/// A configured row as the port's rule. `None` for a row this client could
/// not have written (a destination that is not the interface's address, a
/// port that is not a number): it is not compared, and never matched.
fn rule_of_row(table: Table, row: &Value) -> Option<NatRule> {
    let interface = str_field(row, "interface");
    let description = str_field(row, table.description_field());
    match table {
        Table::Source => {
            let target = str_field(row, "target");
            Some(NatRule {
                description,
                kind: NatKind::Source,
                source: str_field(row, "source_net"),
                protocol: None,
                port: None,
                target: if target == format!("{interface}ip") {
                    INTERFACE_ADDRESS.to_string()
                } else {
                    target
                },
                target_port: None,
                interface,
            })
        }
        Table::Destination => {
            if str_field(row, "destination.network") != format!("{interface}ip") {
                return None;
            }
            let source = str_field(row, "source.network");
            Some(NatRule {
                description,
                kind: NatKind::Destination,
                source: if source.is_empty() {
                    "any".into()
                } else {
                    source
                },
                protocol: Some(str_field(row, "protocol").to_ascii_lowercase()),
                port: Some(str_field(row, "destination.port").parse().ok()?),
                target: str_field(row, "target"),
                target_port: Some(str_field(row, "local-port").parse().ok()?),
                interface,
            })
        }
    }
}

/// The write body of a rule. `label` is the owner category's NAME and
/// `category` its uuid: the two tables take the mark differently.
fn write_body(rule: &NatRule, label: &str, category: &str) -> Value {
    match rule.kind {
        NatKind::Source => serde_json::json!({ "rule": {
            "enabled": "1",
            "interface": rule.interface,
            "ipprotocol": "inet",
            "protocol": "any",
            "source_net": rule.source,
            "destination_net": "any",
            "target": if rule.target == INTERFACE_ADDRESS {
                format!("{}ip", rule.interface)
            } else {
                rule.target.clone()
            },
            "description": rule.description,
            "categories": category,
        }}),
        NatKind::Destination => serde_json::json!({ "rule": {
            "disabled": "0",
            "interface": rule.interface,
            "ipprotocol": "inet",
            "protocol": rule.protocol.clone().unwrap_or_default(),
            "source": { "network": rule.source },
            "destination": {
                "network": format!("{}ip", rule.interface),
                "port": rule.port.map(|p| p.to_string()).unwrap_or_default(),
            },
            "target": rule.target,
            "local-port": rule.target_port.map(|p| p.to_string()).unwrap_or_default(),
            "descr": rule.description,
            "category": label,
        }}),
    }
}

/// One NAT write this value staged and has not applied.
#[derive(Debug, Clone)]
struct StagedNat {
    table: Table,
    uuid: String,
    description: String,
    signature: Signature,
    op: StagedOp,
}

/// What ONE caller staged through one [`OpnsenseNatProvider`].
#[derive(Debug, Default)]
struct NatStaging(std::sync::Mutex<Vec<StagedNat>>);

impl NatStaging {
    fn record(&self, change: StagedNat) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(change);
    }

    fn all(&self) -> Vec<StagedNat> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn covers(&self, pending: &PendingChange) -> bool {
        pending.kind == "nat" && self.all().iter().any(|c| c.uuid == pending.id)
    }

    fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

impl Client {
    fn nat_rows(&self, table: Table) -> Result<Vec<Value>> {
        Ok(self
            .search_rows(&format!("{}/search_rule", table.controller()))?
            .into_iter()
            // The appliance's own anti-lockout lines are listed with the
            // user's rules, under ids that are not uuids.
            .filter(|r| r.get("uuid").and_then(Value::as_str).is_some_and(is_uuid))
            .collect())
    }

    fn nat_lines(&self) -> Result<Vec<String>> {
        running_nat_lines(&self.request(
            reqwest::Method::GET,
            "diagnostics/firewall/pf_statistics/rules",
            None,
        )?)
    }

    /// The NAT rows configured and not running, or running and disabled —
    /// for the rows whose pf line can be derived (see the module doc).
    fn nat_pending(&self) -> Result<Vec<PendingChange>> {
        self.nat_pending_where(|_| true)
    }

    /// The pending NAT rows that do not carry `owner`'s mark (all of them
    /// when there is no owner) — what a filter commit would push for
    /// somebody else.
    pub(crate) fn nat_pending_not_owned_by(
        &self,
        owner: Option<&OwnerMark>,
    ) -> Result<Vec<PendingChange>> {
        let labels = match owner {
            Some(_) => self.owner_categories()?,
            None => Vec::new(),
        };
        self.nat_pending_where(|row| match owner {
            Some(o) => owner_of_row(row, o, &labels) != Owner::Ours,
            None => true,
        })
    }

    fn nat_pending_where(&self, keep: impl Fn(&Value) -> bool) -> Result<Vec<PendingChange>> {
        let mut rows = Vec::new();
        for table in Table::BOTH {
            rows.extend(self.nat_rows(table)?.into_iter().map(|r| (table, r)));
        }
        // No NAT row configured: nothing can be staged, and the running
        // state need not be read (one request fewer on every filter commit
        // of an appliance that has no NAT).
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let lines = self.nat_lines()?;
        let mut out = Vec::new();
        for (table, row) in rows {
            let Some(sig) = Signature::of_row(table, &row) else {
                continue;
            };
            if !keep(&row) {
                continue;
            }
            let what = match (table.enabled(&row), sig.loaded_in(&lines)) {
                (true, false) => "created or enabled, not applied",
                (false, true) => "disabled, not applied",
                _ => continue,
            };
            out.push(PendingChange {
                kind: "nat",
                id: str_field(&row, "uuid"),
                label: str_field(&row, table.description_field()),
                what,
            });
        }
        Ok(out)
    }

    /// Everything staged on the appliance that `staging` did not stage: NAT
    /// rows, and the filter's and the aliases' pending changes, which the
    /// same apply would push.
    fn nat_foreign_pending(&self, staging: &NatStaging) -> Result<Vec<PendingChange>> {
        let mut foreign: Vec<PendingChange> = self
            .nat_pending()?
            .into_iter()
            .filter(|p| !staging.covers(p))
            .collect();
        foreign.extend(self.pending_changes()?);
        Ok(foreign)
    }

    /// The pre-check: staged NAT rows that are not `staging`'s. The filter
    /// and the aliases are left to the commit, so a caller that staged
    /// those itself (a gateway document) is not refused for its own work.
    fn refuse_nat_rows_foreign(&self, staging: &NatStaging) -> Result<()> {
        let foreign: Vec<PendingChange> = self
            .nat_pending()?
            .into_iter()
            .filter(|p| !staging.covers(p))
            .collect();
        Self::refuse_listed(foreign)
    }

    fn refuse_nat_foreign(&self, staging: &NatStaging) -> Result<()> {
        Self::refuse_listed(self.nat_foreign_pending(staging)?)
    }

    /// The NAT rows carrying `owner`'s mark that are staged and not loaded,
    /// taken into `staging` as created by it.
    fn adopt_nat_pending(&self, owner: &OwnerMark, staging: &NatStaging) -> Result<Vec<String>> {
        let lines = self.nat_lines()?;
        let labels = self.owner_categories()?;
        let mut adopted = Vec::new();
        for table in Table::BOTH {
            for row in self.nat_rows(table)? {
                let Some(signature) = Signature::of_row(table, &row) else {
                    continue;
                };
                if !table.enabled(&row)
                    || signature.loaded_in(&lines)
                    || owner_of_row(&row, owner, &labels) != Owner::Ours
                    || staging
                        .all()
                        .iter()
                        .any(|c| c.uuid == str_field(&row, "uuid"))
                {
                    continue;
                }
                let description = str_field(&row, table.description_field());
                adopted.push(format!("nat rule '{description}'"));
                staging.record(StagedNat {
                    table,
                    uuid: row_uuid(&row, table.controller())?,
                    description,
                    signature,
                    op: StagedOp::Created,
                });
            }
        }
        Ok(adopted)
    }

    fn refuse_listed(foreign: Vec<PendingChange>) -> Result<()> {
        if foreign.is_empty() {
            return Ok(());
        }
        let list: Vec<String> = foreign.iter().map(PendingChange::to_string).collect();
        Err(Error::ForeignPending(format!(
            "the appliance has {} staged change(s) that are not this engine's: {} — its apply \
             pushes everything staged, so nothing was applied; have them applied or reverted \
             on the appliance first",
            foreign.len(),
            list.join("; ")
        )))
    }

    fn ensure_nat(
        &self,
        rule: &NatRule,
        owner: &OwnerMark,
        staging: &NatStaging,
    ) -> Result<EnsureOutcome> {
        let table = Table::of(rule.kind);
        let rows = self.nat_rows(table)?;
        let same: Vec<&Value> = rows
            .iter()
            .filter(|r| str_field(r, table.description_field()) == rule.description)
            .collect();
        let labels = if same.is_empty() {
            Vec::new()
        } else {
            self.owner_categories()?
        };
        if let Some(foreign) = same
            .iter()
            .map(|r| owner_of_row(r, owner, &labels))
            .find(|o| *o != Owner::Ours)
        {
            return Err(Error::NotOwned(format!(
                "a nat rule described '{}' already exists on the appliance and is {} — refusing \
                 to adopt it by description; change the description in the manifest, or remove \
                 the rule on the appliance if it is really stale",
                rule.description,
                foreign.describe()
            )));
        }
        match same.as_slice() {
            [] => {}
            [row] => {
                let observed = NatObserved {
                    rules: rule_of_row(table, row).into_iter().collect(),
                    disabled: if table.enabled(row) {
                        Vec::new()
                    } else {
                        vec![rule.description.clone()]
                    },
                };
                let drift =
                    delonix_networking::nat::nat_drift(std::slice::from_ref(rule), &observed);
                if !drift.is_empty() {
                    return Err(Error::Drifted(format!(
                        "nat rule '{}' (this engine's) was changed on the appliance: {} — put \
                         it back, or replace the document so the engine recreates it",
                        rule.description,
                        drift.join("; ")
                    )));
                }
                return Ok(EnsureOutcome::AlreadyPresent);
            }
            many => {
                return Err(Error::Drifted(format!(
                    "{} nat rules described '{}' carry this engine's mark, and it created one — \
                     remove the copies on the appliance",
                    many.len(),
                    rule.description
                )));
            }
        }
        let category = self.ensure_owner_category(owner)?;
        let answer = self.request(
            reqwest::Method::POST,
            &format!("{}/add_rule", table.controller()),
            Some(&write_body(rule, &owner.label(), &category)),
        )?;
        let uuid = answer
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|u| is_uuid(u))
            .ok_or_else(|| {
                Error::Decode(format!(
                    "{}/add_rule saved the rule '{}' without answering its uuid: {}",
                    table.controller(),
                    rule.description,
                    truncate_chars(&answer.to_string(), 200)
                ))
            })?;
        staging.record(StagedNat {
            table,
            uuid: uuid.to_string(),
            description: rule.description.clone(),
            signature: Signature::of_rule(rule),
            op: StagedOp::Created,
        });
        Ok(EnsureOutcome::Created)
    }

    fn remove_nat(
        &self,
        description: &str,
        owner: &OwnerMark,
        staging: &NatStaging,
    ) -> Result<RemoveOutcome> {
        let mut removed = false;
        let mut left = None;
        let labels = self.owner_categories()?;
        for table in Table::BOTH {
            for row in self.nat_rows(table)? {
                if str_field(&row, table.description_field()) != description {
                    continue;
                }
                let found = owner_of_row(&row, owner, &labels);
                if found != Owner::Ours {
                    left = Some(found);
                    continue;
                }
                let uuid = row_uuid(&row, table.controller())?;
                self.request(
                    reqwest::Method::POST,
                    &format!("{}/del_rule/{uuid}", table.controller()),
                    None,
                )?;
                if let Some(signature) = Signature::of_row(table, &row) {
                    staging.record(StagedNat {
                        table,
                        uuid,
                        description: description.to_string(),
                        signature,
                        op: StagedOp::Deleted,
                    });
                }
                removed = true;
            }
        }
        Ok(match (removed, left) {
            (_, Some(found)) => RemoveOutcome::NotOwned(found),
            (true, None) => RemoveOutcome::Removed,
            (false, None) => RemoveOutcome::Absent,
        })
    }

    fn observe_nat(&self, owner: &OwnerMark) -> Result<NatObserved> {
        let labels = self.owner_categories()?;
        let mut out = NatObserved::default();
        for table in Table::BOTH {
            for row in self.nat_rows(table)? {
                if owner_of_row(&row, owner, &labels) != Owner::Ours {
                    continue;
                }
                let description = str_field(&row, table.description_field());
                if !table.enabled(&row) {
                    out.disabled.push(description.clone());
                }
                // A row of ours this client could not have written was
                // edited there: it is observed by description alone, so the
                // comparison names every field that no longer reads.
                out.rules.push(rule_of_row(table, &row).unwrap_or(NatRule {
                    description,
                    kind: match table {
                        Table::Source => NatKind::Source,
                        Table::Destination => NatKind::Destination,
                    },
                    interface: str_field(&row, "interface"),
                    source: String::new(),
                    protocol: None,
                    port: None,
                    target: String::new(),
                    target_port: None,
                }));
            }
        }
        Ok(out)
    }

    /// Applies what `staging` staged, when that is all that is staged, and
    /// proves it against the running packet filter.
    fn commit_nat(&self, staging: &NatStaging) -> Result<()> {
        let staged = staging.all();
        if let Err(refused) = self.refuse_nat_foreign(staging) {
            // What this value created is deleted again, so a later apply by
            // the operator does not push it. What it deleted cannot be put
            // back, and is named.
            let mut left = Vec::new();
            for c in &staged {
                match c.op {
                    StagedOp::Deleted => left.push(format!("nat rule '{}' deleted", c.description)),
                    StagedOp::Created => {
                        if let Err(e) = self.request(
                            reqwest::Method::POST,
                            &format!("{}/del_rule/{}", c.table.controller(), c.uuid),
                            None,
                        ) {
                            left.push(format!("nat rule '{}' created ({e})", c.description));
                        }
                    }
                }
            }
            staging.clear();
            if left.is_empty() {
                return Err(refused);
            }
            return Err(Error::ForeignPending(format!(
                "{refused} — and these changes of this engine are still staged on the \
                 appliance: {}",
                left.join("; ")
            )));
        }
        // One apply pushes the whole configuration (measured); each table's
        // own route is called for the tables this value wrote, so nothing
        // rests on that staying true.
        for table in Table::BOTH {
            if staged.iter().any(|c| c.table == table) {
                self.request(
                    reqwest::Method::POST,
                    &format!("{}/apply", table.controller()),
                    None,
                )?;
            }
        }
        let lines = self.nat_lines()?;
        // A removed rule's line may stay loaded for a rule still configured
        // with the same signature; only then is it not ours to see gone.
        let mut still_configured = Vec::new();
        for table in Table::BOTH {
            for row in self.nat_rows(table)? {
                if table.enabled(&row) {
                    still_configured.extend(Signature::of_row(table, &row));
                }
            }
        }
        let wrong: Vec<String> = staged
            .iter()
            .filter_map(|c| match (c.op, c.signature.loaded_in(&lines)) {
                (StagedOp::Created, false) => {
                    Some(format!("nat rule '{}' is not loaded", c.description))
                }
                (StagedOp::Deleted, true) if !still_configured.contains(&c.signature) => {
                    Some(format!("nat rule '{}' is still loaded", c.description))
                }
                _ => None,
            })
            .collect();
        if !wrong.is_empty() {
            return Err(Error::HttpStatus(format!(
                "the appliance answered the apply, but the running packet filter does not show \
                 it: {}",
                wrong.join("; ")
            )));
        }
        staging.clear();
        Ok(())
    }
}

/// The [`NatProvider`] of an OPNsense appliance. Shares the gateway
/// provider's client; what it staged is its own.
pub struct OpnsenseNatProvider {
    client: std::sync::Arc<Client>,
    staging: NatStaging,
}

impl OpnsenseNatProvider {
    pub fn connect(target: &crate::Target) -> Result<Self> {
        Ok(Self::sharing(std::sync::Arc::new(Client::connect(target)?)))
    }

    pub(crate) fn sharing(client: std::sync::Arc<Client>) -> Self {
        Self {
            client,
            staging: NatStaging::default(),
        }
    }
}

impl delonix_compute::vm_provider::Provider for OpnsenseNatProvider {
    fn id(&self) -> delonix_compute::vm_provider::ProviderId {
        delonix_compute::vm_provider::ProviderId(crate::ID)
    }

    fn capabilities(&self) -> delonix_compute::capability::ProviderReport {
        crate::capability_report(true)
    }
}

impl NatProvider for OpnsenseNatProvider {
    fn available(&self) -> bool {
        true
    }

    fn ensure_nat(
        &self,
        rule: &NatRule,
        owner: &OwnerMark,
    ) -> delonix_model::Result<EnsureOutcome> {
        rule.validate().map_err(delonix_model::Error::from)?;
        self.client
            .ensure_nat(rule, owner, &self.staging)
            .map_err(delonix_model::Error::from)
    }

    fn remove_nat(
        &self,
        description: &str,
        owner: &OwnerMark,
    ) -> delonix_model::Result<RemoveOutcome> {
        self.client
            .remove_nat(description, owner, &self.staging)
            .map_err(delonix_model::Error::from)
    }

    fn release_owner(&self, owner: &OwnerMark) -> delonix_model::Result<RemoveOutcome> {
        self.client
            .release_owner(owner)
            .map_err(delonix_model::Error::from)
    }

    fn observe(&self, owner: &OwnerMark) -> delonix_model::Result<NatObserved> {
        self.client
            .observe_nat(owner)
            .map_err(delonix_model::Error::from)
    }

    fn adopt_pending(&self, owner: &OwnerMark) -> delonix_model::Result<Vec<String>> {
        self.client
            .adopt_nat_pending(owner, &self.staging)
            .map_err(delonix_model::Error::from)
    }

    fn check_no_foreign_pending(&self) -> delonix_model::Result<()> {
        self.client
            .refuse_nat_rows_foreign(&self.staging)
            .map_err(delonix_model::Error::from)
    }

    fn commit(&self) -> delonix_model::Result<()> {
        self.client
            .commit_nat(&self.staging)
            .map_err(delonix_model::Error::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines() -> Vec<String> {
        let answer = serde_json::json!({ "rules": { "filter rules": {}, "nat rules": {
            "@0 no nat proto carp all": {},
            "@1 nat on vtnet0 inet from 10.77.0.0/24 to any -> (vtnet0:0) port 1024:65535": {},
            "@2 nat on vtnet0 inet from 10.78.0.9 to any -> 192.0.2.7": {},
            "@1 no rdr on vtnet0 proto tcp from any to (vtnet0:1) port = https": {},
            "@3 rdr on vtnet0 inet proto tcp from any to (vtnet0:1) port = 8443 -> 10.77.0.10 port 443": {},
        }}});
        running_nat_lines(&answer).unwrap()
    }

    #[test]
    fn a_rule_is_recognized_by_the_text_of_its_pf_line() {
        let l = lines();
        let s = |source: &str| Signature::Source {
            source: source.into(),
        };
        assert!(s("10.77.0.0/24").loaded_in(&l));
        assert!(s("10.78.0.9").loaded_in(&l), "a /32 prints as the address");
        assert!(
            !s("10.77.0.0/2").loaded_in(&l),
            "a prefix of a network is not it"
        );
        assert!(!s("10.79.0.0/24").loaded_in(&l));
        let d = |protocol: &str, target: &str, port: &str| Signature::Destination {
            protocol: protocol.into(),
            target: target.into(),
            port: port.into(),
        };
        assert!(d("tcp", "10.77.0.10", "443").loaded_in(&l));
        assert!(!d("udp", "10.77.0.10", "443").loaded_in(&l));
        assert!(
            !d("tcp", "10.77.0.10", "44").loaded_in(&l),
            "a prefix of a port is not it"
        );
        assert!(!d("tcp", "10.77.0.1", "443").loaded_in(&l));
    }

    #[test]
    fn an_answer_without_the_nat_section_is_refused_not_read_as_empty() {
        let e = running_nat_lines(&serde_json::json!({ "rules": { "filter rules": {} } }));
        assert!(matches!(e, Err(Error::Decode(_))), "{e:?}");
    }

    #[test]
    fn the_two_tables_take_the_mark_differently() {
        let snat = NatRule {
            description: "out".into(),
            kind: NatKind::Source,
            interface: "lan".into(),
            source: "10.77.0.0/24".into(),
            protocol: None,
            port: None,
            target: INTERFACE_ADDRESS.into(),
            target_port: None,
        };
        let body = write_body(&snat, "delonix-owner:x", "uuid-1");
        assert_eq!(body["rule"]["categories"], "uuid-1");
        assert_eq!(body["rule"]["target"], "lanip");
        let dnat = NatRule {
            description: "web".into(),
            kind: NatKind::Destination,
            source: "any".into(),
            protocol: Some("tcp".into()),
            port: Some(8443),
            target: "10.77.0.10".into(),
            target_port: Some(443),
            ..snat.clone()
        };
        let body = write_body(&dnat, "delonix-owner:x", "uuid-1");
        assert_eq!(body["rule"]["category"], "delonix-owner:x");
        assert_eq!(body["rule"]["destination"]["network"], "lanip");
        assert_eq!(body["rule"]["local-port"], "443");
    }

    /// The rows are the ones the appliance answered (26.1.2_5), trimmed.
    #[test]
    fn a_row_reads_back_as_the_rule_that_wrote_it() {
        let row = serde_json::json!({
            "uuid": "65fa1e5d-c952-4a55-afd9-7441e40c403b", "enabled": "1",
            "interface": "lan", "source_net": "10.77.0.0/24", "target": "lanip",
            "description": "out",
        });
        let r = rule_of_row(Table::Source, &row).unwrap();
        assert_eq!(
            (r.source.as_str(), r.target.as_str()),
            ("10.77.0.0/24", INTERFACE_ADDRESS)
        );
        assert!(Table::Source.enabled(&row));

        let row = serde_json::json!({
            "uuid": "574c3fed-8385-4f08-8c21-68c660ced18b", "disabled": "0",
            "interface": "lan", "protocol": "tcp", "source.network": "any",
            "destination.network": "lanip", "destination.port": "8443",
            "target": "10.77.0.10", "local-port": "443", "descr": "web",
        });
        let r = rule_of_row(Table::Destination, &row).unwrap();
        assert_eq!((r.port, r.target_port), (Some(8443), Some(443)));
        assert_eq!(r.protocol.as_deref(), Some("tcp"));
        assert_eq!(
            Signature::of_row(Table::Destination, &row),
            Some(Signature::Destination {
                protocol: "tcp".into(),
                target: "10.77.0.10".into(),
                port: "443".into()
            })
        );

        let elsewhere = serde_json::json!({
            "interface": "lan", "protocol": "tcp", "destination.network": "192.0.2.1",
            "destination.port": "80", "target": "10.77.0.10", "local-port": "80", "descr": "x",
        });
        assert!(rule_of_row(Table::Destination, &elsewhere).is_none());
    }
}
