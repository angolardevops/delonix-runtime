//! A [`GatewayProvider`] backed by an OPNsense appliance's own REST API
//! (ADR-0051) — the first real implementation of the node-egress /
//! perimeter gateway policy trait `delonix-sdn` defines.
//!
//! # What the live spike measured, and why the code looks like this
//! (ADR-0051, Phase 0, against a real OPNsense 26.1.2 appliance)
//!
//! **Only a generated key/secret pair authenticates.** A GUI account's
//! username/password, even the real `root` account, answers `401`. There
//! is no bootstrap-safe way to mint the FIRST key either — no CLI, no
//! console-menu option — only the web GUI or a root shell calling
//! `OPNsense\Auth\User::apikeys->add()` directly. This crate does not try
//! to automate that; a [`Target`]'s [`Auth`] is always a key/secret pair
//! someone already generated.
//!
//! **Two different answers for two different kinds of "not authenticated,"
//! and a naive redirect-following client would hide one of them.** Wrong
//! credentials answer `401`; NO credentials at all answer `302` — a
//! redirect toward the session-based GUI login this same route also
//! serves. [`Client::connect`] builds its `reqwest` client with
//! `redirect::Policy::none()` so a `302` shows up as a status to classify,
//! never silently followed into an HTML login page that would otherwise
//! turn into a confusing [`Error::Decode`].
//!
//! **A validation failure is HTTP 200, not 4xx.** The appliance answers
//! `{"result":"failed","validations":{"<field>":"<reason>", ...}}` at
//! `200` for a bad `add_rule`/`add_item` — a client that only branches on
//! HTTP status treats this as success. [`Client::request`] inspects every
//! 2xx body for this shape before returning it.
//!
//! **`add_item`/`add_rule` want a FLAT shape; `get`/`get_item` answer a
//! VERBOSE one** — Phalcon's own form-widget representation, where a
//! field like `type` is an object listing every option with a
//! `selected: 0|1` flag rather than a plain string. This crate only ever
//! WRITES the flat shape (`AliasSpec`/`RuleSpec`'s `Serialize` impls); it
//! never round-trips a `get` response back into a write.
//!
//! **`apply()` is synchronous.** `POST firewall/filter/apply` answered in
//! well under a second with `{"status":"OK\n\n"}` — the reload's own
//! captured stdout, never a task id to poll. There is no Proxmox-style
//! UPID/wait loop here; [`GatewayProvider::commit`] just makes the call.
//! `firewall/alias/reconfigure` is the alias table's own equivalent, and a
//! SEPARATE call: a rule that references an alias needs both, confirmed by
//! reading a freshly-applied rule back via `search_rule` before either was
//! reconfigured and finding the alias's content already denormalized into
//! `alias_meta_source_net`.
//!
//! # What this crate does NOT do
//!
//! It does not implement `set_item`/`set_rule` (update-in-place): that
//! request shape — the verbose form `get` returns, or the flat form
//! `add_item` accepts — was not part of the Phase 0 spike, and guessing it
//! risks the exact trap this module doc exists to avoid.
//! [`Client::ensure_alias`]/[`Client::ensure_rule`] never update an
//! existing alias/rule found under the same identity: one of this engine's
//! that still matches is [`delonix_networking::gateway::EnsureOutcome::AlreadyPresent`],
//! one that was edited on the appliance is [`Error::Drifted`].
//!
//! # Ownership (audit 62, §6 P1)
//!
//! The name of an alias and the description of a rule are how they are
//! FOUND, not who OWNS them. Every alias and rule this crate creates carries
//! the caller's [`OwnerMark`] as a firewall CATEGORY named
//! `delonix-owner:<token>` in its `categories` (ADR-0059 D1.5) — the
//! description stays exactly the declared text — and:
//!
//! * an `ensure_*` that finds the name/description WITHOUT the mark refuses
//!   ([`Error::NotOwned`]) instead of answering "already present" — the old
//!   answer is how an operator's hand-made rule was adopted and later
//!   deleted by a teardown;
//! * a `remove_*` deletes only what carries the mark, by uuid, and reports
//!   what it left ([`RemoveOutcome::NotOwned`]);
//! * the commit ([`Client::commit`]) applies only when everything staged on
//!   the appliance is what this caller staged ([`Staging`]), because
//!   `filter/apply` and `alias/reconfigure` push ALL of `config.xml`. The
//!   appliance has no "pending" flag to ask; [`Client::pending_changes`]
//!   compares the configured state with the running one, and its doc says
//!   what that comparison cannot see.

pub mod capabilities;
mod error;

pub use capabilities::capability_report;
pub use error::{Error, Result, MAX_RESPONSE_BYTES};

use delonix_networking::gateway::{EnsureOutcome, GatewayAlias, GatewayProvider, GatewayRule};
use delonix_networking::ownership::{Owner, OwnerMark, RemoveOutcome};
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

/// How this client authenticates — a generated key/secret pair, the only
/// form OPNsense's REST API accepts (ADR-0051 Phase 0, measured: a GUI
/// account's username/password answers `401` the same as a wrong key).
///
/// `Debug` is written by hand: the secret must never reach a log, a panic
/// message or an error.
#[derive(Clone)]
pub struct Auth {
    pub key: String,
    pub secret: String,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth")
            .field("key", &self.key)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// Where the appliance is, and how to get in.
#[derive(Debug, Clone)]
pub struct Target {
    /// `https://<host>` (no path). The scheme is part of it: `http://`
    /// sends the key/secret in the clear, and that is the caller's
    /// explicit choice to make, not a default this code picks.
    pub base_url: String,
    pub auth: Auth,
    /// Accept a certificate this host cannot verify. A stock OPNsense
    /// serves a self-signed one — measured against
    /// `opnsense-adr0051-spike` in this same session — so many real
    /// targets need this, but it removes the check that stops another
    /// machine answering in the appliance's name, taking the key/secret
    /// with it. Opt-in, never a fallback after a TLS error.
    pub insecure_tls: bool,
    /// A CA certificate (PEM) to trust for this appliance IN ADDITION to
    /// the system roots, instead of switching verification off entirely.
    pub ca_cert_pem: Option<Vec<u8>>,
}

/// An address alias to create — the flat write shape `firewall/alias/
/// add_item` accepts (ADR-0051 Phase 0, measured), never the verbose form
/// `get`/`get_item` answer.
#[derive(Debug, Serialize)]
struct AliasWrite {
    name: String,
    #[serde(rename = "type")]
    kind: &'static str,
    /// Multiple values are newline-joined — OPNsense's own convention for
    /// this field (confirmed by the module doc's own reading of
    /// `alias/get`'s `content` map, which lists one entry per line of the
    /// same field on read).
    content: String,
    description: String,
    /// The owner category's uuid (a `ModelRelationField`, comma-joined uuids
    /// on write and on read).
    #[serde(skip_serializing_if = "String::is_empty")]
    categories: String,
}

fn alias_write(alias: &GatewayAlias) -> AliasWrite {
    AliasWrite {
        name: alias.name.clone(),
        kind: match alias.kind {
            delonix_networking::gateway::AliasKind::Host => "host",
            delonix_networking::gateway::AliasKind::Network => "network",
        },
        content: alias.content.join("\n"),
        description: alias.description.clone(),
        categories: String::new(),
    }
}

/// A perimeter filter rule to create — the flat write shape `firewall/
/// filter/add_rule` accepts (ADR-0051 Phase 0, measured: the docs' own
/// worked example uses exactly these four fields).
#[derive(Debug, Serialize)]
struct RuleWrite {
    description: String,
    source_net: String,
    destination_net: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    categories: String,
}

fn rule_write(rule: &GatewayRule) -> RuleWrite {
    RuleWrite {
        description: rule.description.clone(),
        source_net: rule.source.clone(),
        destination_net: rule.destination.clone(),
        protocol: rule.protocol.clone(),
        categories: String::new(),
    }
}

/// A client for one OPNsense appliance's firewall API.
#[derive(Debug)]
pub struct Client {
    http: reqwest::blocking::Client,
    base: String,
    auth: Auth,
}

impl Client {
    /// Builds the client and proves the credential works — the same
    /// "authenticate at construction, not at first real use" contract
    /// `delonix-proxmox::Client::connect` already documents, and exactly
    /// what ADR-0051's Phase 0 spike did by hand (`GET core/firmware/
    /// status`) before anything else.
    pub fn connect(target: &Target) -> Result<Self> {
        let mut builder = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            // One appliance, a handful of sequential calls: a pool this
            // small is all it ever uses.
            .pool_max_idle_per_host(4)
            // The appliance's own answer to "you are not authenticated"
            // for a missing credential is a 302 toward its GUI login
            // (ADR-0051 Phase 0, measured) — following it would turn a
            // clear Unauthorized into a confusing Decode failure over the
            // login page's HTML.
            .redirect(reqwest::redirect::Policy::none())
            .danger_accept_invalid_certs(target.insecure_tls);
        if let Some(pem) = &target.ca_cert_pem {
            let cert = reqwest::Certificate::from_pem(pem).map_err(|e| {
                Error::ClientBuild(format!(
                    "the CA certificate given for {} is not a PEM certificate: {e}",
                    target.base_url
                ))
            })?;
            builder = builder.add_root_certificate(cert);
        }
        let http = builder
            .build()
            .map_err(|e| Error::ClientBuild(format!("could not build the HTTP client: {e}")))?;
        let me = Self {
            http,
            base: target.base_url.trim_end_matches('/').to_string(),
            auth: target.auth.clone(),
        };
        // Prove the credential before anything else — a wrong key fails
        // here, not halfway through provisioning a rule.
        me.request(reqwest::Method::GET, "core/firmware/status", None)?;
        Ok(me)
    }

    fn request(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<Value> {
        let url = format!("{}/api/{}", self.base, path.trim_start_matches('/'));
        let mut req = self
            .http
            .request(method.clone(), &url)
            .basic_auth(&self.auth.key, Some(&self.auth.secret));
        req = match body {
            Some(b) => req.json(b),
            // A bodyless POST still needs an explicit `Content-Length: 0` —
            // measured live (ADR-0051 Phase 2): the appliance's web server
            // answers `411 Length Required` to a POST with no body and no
            // length header at all, which is what `reqwest` sends by
            // default when nothing is attached. `curl -X POST` with no
            // `-d` does not hit this because curl adds that header itself;
            // `reqwest` does not, so it has to be explicit here.
            None if method == reqwest::Method::POST => req.body(Vec::<u8>::new()),
            None => req,
        };
        let resp = req
            .send()
            .map_err(|e| Error::Request(format!("{method} {path}: {e}")))?;
        let status = resp.status();
        if status.is_redirection() {
            return Err(Error::Unauthorized(format!(
                "{method} {path}: HTTP {status} — no credentials were sent (redirected toward \
                 the GUI login); check Target.auth is set"
            )));
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::Unauthorized(format!(
                "{method} {path}: HTTP 401 — authentication failed; check the API key/secret \
                 (a GUI account's username/password does not work here)"
            )));
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            return Err(Error::Forbidden(format!(
                "{method} {path}: HTTP 403 — the API key lacks the privilege this route needs"
            )));
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::RouteNotFound(format!(
                "{method} {path}: HTTP 404 — no such route on this appliance/version"
            )));
        }
        let text = self.redact(&read_bounded(resp, &format!("{method} {path}"))?);
        if !status.is_success() {
            return Err(Error::HttpStatus(format!(
                "{method} {path}: HTTP {status}: {}",
                truncate_chars(text.trim(), 400)
            )));
        }
        let json: Value = serde_json::from_str(&text).map_err(|e| {
            Error::Decode(format!(
                "{method} {path}: {e}: {}",
                truncate_chars(text.trim(), 400)
            ))
        })?;
        if let Some(e) = validation_failure(&json) {
            return Err(e);
        }
        Ok(json)
    }

    /// The firewall categories that are owner labels (`delonix-owner:<token>`)
    /// — `(uuid, name)`. An operator's own categories are left out: they say
    /// nothing about who owns an object.
    fn owner_categories(&self) -> Result<Vec<(String, String)>> {
        Ok(self
            .search_rows("firewall/category/search_item")?
            .iter()
            .filter_map(|r| {
                let name = str_field(r, "name");
                delonix_networking::ownership::token_of_label(&name)?;
                Some((r.get("uuid")?.as_str()?.to_string(), name))
            })
            .collect())
    }

    /// The uuid of `owner`'s category, when it exists.
    fn owner_category(&self, owner: &OwnerMark) -> Result<Option<String>> {
        let label = owner.label();
        Ok(self
            .owner_categories()?
            .into_iter()
            .find(|(_, name)| *name == label)
            .map(|(uuid, _)| uuid))
    }

    /// The uuid of `owner`'s category, created the first time this record
    /// writes anything. A category is configuration only (nothing in pf), so
    /// creating it stages nothing an apply would push.
    fn ensure_owner_category(&self, owner: &OwnerMark) -> Result<String> {
        if let Some(uuid) = self.owner_category(owner)? {
            return Ok(uuid);
        }
        let answer = self.request(
            reqwest::Method::POST,
            "firewall/category/add_item",
            Some(&serde_json::json!({ "category": { "name": owner.label(), "auto": "0" } })),
        )?;
        answer
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|u| is_uuid(u))
            .map(str::to_string)
            .ok_or_else(|| {
                Error::Decode(format!(
                    "firewall/category/add_item saved the owner category '{}' without answering \
                     its uuid: {}",
                    owner.label(),
                    truncate_chars(&answer.to_string(), 200)
                ))
            })
    }

    /// Deletes `owner`'s category once nothing carries it — the last step of
    /// a teardown. The appliance itself refuses while an alias or rule still
    /// uses it ("Category in use"), which is surfaced, never forced.
    pub fn release_owner(&self, owner: &OwnerMark) -> Result<RemoveOutcome> {
        let Some(uuid) = self.owner_category(owner)? else {
            return Ok(RemoveOutcome::Absent);
        };
        self.request(
            reqwest::Method::POST,
            &format!("firewall/category/del_item/{uuid}"),
            None,
        )?;
        Ok(RemoveOutcome::Removed)
    }

    /// Every row of a `search_*` route — `rowCount: -1` is the grid's own
    /// "all rows" (`UIModelGrid::fetchBindRequest`, OPNsense 26.1.2), so an
    /// appliance with more aliases or rules than one page never hides the
    /// one this engine is looking for.
    fn search_rows(&self, search_path: &str) -> Result<Vec<Value>> {
        let body = self.request(
            reqwest::Method::POST,
            search_path,
            Some(&serde_json::json!({ "current": 1, "rowCount": -1 })),
        )?;
        Ok(body
            .get("rows")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Ensures an address alias exists, by name, owned by `owner` (see the
    /// module doc, «Ownership»). Created: the description carries the mark,
    /// and `staging` records the name so the commit knows it is ours.
    /// Found: ours and matching is [`EnsureOutcome::AlreadyPresent`]; ours
    /// and different is [`Error::Drifted`]; not ours is [`Error::NotOwned`].
    pub fn ensure_alias(
        &self,
        alias: &GatewayAlias,
        owner: &OwnerMark,
        staging: &Staging,
    ) -> Result<EnsureOutcome> {
        let rows = self.search_rows("firewall/alias/search_item")?;
        if let Some(row) = rows.iter().find(|r| str_field(r, "name") == alias.name) {
            let found = owner_of_row(row, owner, &self.owner_categories()?);
            if found != Owner::Ours {
                return Err(Error::NotOwned(format!(
                    "alias '{}' already exists on the appliance and is {} — refusing to adopt \
                     it by name; rename the alias in the manifest, or remove the one on the \
                     appliance if it is really stale",
                    alias.name,
                    found.describe()
                )));
            }
            let drift = alias_drift(row, alias);
            if !drift.is_empty() {
                return Err(Error::Drifted(format!(
                    "alias '{}' (this engine's) was changed on the appliance: {} — put it back, \
                     or replace the document so the engine recreates it",
                    alias.name,
                    drift.join("; ")
                )));
            }
            return Ok(EnsureOutcome::AlreadyPresent);
        }
        let mut write = alias_write(alias);
        write.categories = self.ensure_owner_category(owner)?;
        self.request(
            reqwest::Method::POST,
            "firewall/alias/add_item",
            Some(&serde_json::json!({ "alias": write })),
        )?;
        staging.record(StagedChange::alias(&alias.name, StagedOp::Created));
        Ok(EnsureOutcome::Created)
    }

    /// Removes an alias by name — only when `owner` owns it. Anything else
    /// under that name is left untouched and reported.
    pub fn remove_alias(
        &self,
        name: &str,
        owner: &OwnerMark,
        staging: &Staging,
    ) -> Result<RemoveOutcome> {
        let rows = self.search_rows("firewall/alias/search_item")?;
        let Some(row) = rows.iter().find(|r| str_field(r, "name") == name) else {
            return Ok(RemoveOutcome::Absent);
        };
        let found = owner_of_row(row, owner, &self.owner_categories()?);
        if found != Owner::Ours {
            return Ok(RemoveOutcome::NotOwned(found));
        }
        let uuid = row_uuid(row, "firewall/alias/search_item")?;
        self.request(
            reqwest::Method::POST,
            &format!("firewall/alias/del_item/{uuid}"),
            None,
        )?;
        staging.record(StagedChange::alias(name, StagedOp::Deleted));
        Ok(RemoveOutcome::Removed)
    }

    /// Ensures a perimeter filter rule exists, by description, owned by
    /// `owner`. The rules looked at are those whose description WITHOUT an
    /// owner mark equals the declared one: any of them not ours is a
    /// refusal (the operator's rule of the same name would be the one
    /// adopted, then deleted, under the old identity); two of ours is a
    /// drift (a duplicate nobody declared).
    pub fn ensure_rule(
        &self,
        rule: &GatewayRule,
        owner: &OwnerMark,
        staging: &Staging,
    ) -> Result<EnsureOutcome> {
        let rows = self.search_rows("firewall/filter/search_rule")?;
        let same: Vec<&Value> = rows
            .iter()
            .filter(|r| str_field(r, "description") == rule.description)
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
                "a rule described '{}' already exists on the appliance and is {} — refusing \
                 to adopt it by description; change the description in the manifest, or \
                 remove the rule on the appliance if it is really stale",
                rule.description,
                foreign.describe()
            )));
        }
        match same.as_slice() {
            [] => {}
            [row] => {
                let drift = rule_drift(row, rule);
                if !drift.is_empty() {
                    return Err(Error::Drifted(format!(
                        "rule '{}' (this engine's) was changed on the appliance: {} — put it \
                         back, or replace the document so the engine recreates it",
                        rule.description,
                        drift.join("; ")
                    )));
                }
                return Ok(EnsureOutcome::AlreadyPresent);
            }
            many => {
                return Err(Error::Drifted(format!(
                    "{} rules described '{}' carry this engine's mark, and it created one — \
                     remove the copies on the appliance",
                    many.len(),
                    rule.description
                )));
            }
        }
        let mut write = rule_write(rule);
        write.categories = self.ensure_owner_category(owner)?;
        let answer = self.request(
            reqwest::Method::POST,
            "firewall/filter/add_rule",
            Some(&serde_json::json!({ "rule": write })),
        )?;
        // The uuid is the rule's pf label once applied (`FilterRuleField::
        // serialize`), which is how the commit tells this rule from someone
        // else's staged one. Without it the commit would refuse our own rule.
        let uuid = answer
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|u| is_uuid(u))
            .ok_or_else(|| {
                Error::Decode(format!(
                    "firewall/filter/add_rule saved the rule '{}' without answering its uuid: {}",
                    rule.description,
                    truncate_chars(&answer.to_string(), 200)
                ))
            })?;
        staging.record(StagedChange::rule(
            uuid,
            &rule.description,
            StagedOp::Created,
        ));
        Ok(EnsureOutcome::Created)
    }

    /// Removes the rules with this description that `owner` owns. A rule
    /// that matches without the mark is left alone and reported.
    pub fn remove_rule(
        &self,
        description: &str,
        owner: &OwnerMark,
        staging: &Staging,
    ) -> Result<RemoveOutcome> {
        let rows = self.search_rows("firewall/filter/search_rule")?;
        let same: Vec<&Value> = rows
            .iter()
            .filter(|r| str_field(r, "description") == description)
            .collect();
        if same.is_empty() {
            return Ok(RemoveOutcome::Absent);
        }
        let labels = self.owner_categories()?;
        let mut removed = false;
        let mut left = None;
        for row in same {
            let found = owner_of_row(row, owner, &labels);
            if found != Owner::Ours {
                left = Some(found);
                continue;
            }
            let uuid = row_uuid(row, "firewall/filter/search_rule")?;
            self.request(
                reqwest::Method::POST,
                &format!("firewall/filter/del_rule/{uuid}"),
                None,
            )?;
            staging.record(StagedChange::rule(&uuid, description, StagedOp::Deleted));
            removed = true;
        }
        Ok(match (removed, left) {
            (_, Some(found)) => RemoveOutcome::NotOwned(found),
            (true, None) => RemoveOutcome::Removed,
            (false, None) => RemoveOutcome::Absent,
        })
    }

    /// Every change the appliance has staged and not applied, as far as its
    /// API lets it be SEEN — there is no "pending" or "dirty" flag in the
    /// firewall API (OPNsense 26.1.2: neither `FilterBaseController` nor
    /// `AliasController` has one; `apply`/`reconfigure` reload the whole
    /// `config.xml`). So the configured state is compared with the RUNNING
    /// one:
    ///
    /// * **rules** — the configured filter rules (`search_rule`) against the
    ///   labels loaded in pf (`diagnostics/firewall/pf_statistics/rules`,
    ///   `pfctl -vvsr` run on each call). An MVC rule's pf label IS its uuid
    ///   (`FilterRuleField::serialize`). An enabled rule not loaded, a
    ///   disabled one still loaded, and a loaded uuid no longer configured
    ///   are each pending.
    ///
    ///   Not `diagnostics/firewall/list_rule_ids`: measured on 26.1.2_5, its
    ///   `fetch_rule_labels` caches labels by pf line number and never drops
    ///   the lines past the end of a ruleset that got shorter, so a rule
    ///   deleted and applied stays listed for good. The engine's own deletion
    ///   then failed its commit and every later commit was refused.
    /// * **aliases** — the configured `host`/`network` aliases against pf's
    ///   tables (`alias_util/aliases`, `pfctl -sT`) and, for an alias whose
    ///   entries are all literal addresses, the table's content
    ///   (`alias_util/list/<name>`).
    ///
    /// A pf table left for an alias no longer configured is **not** read as
    /// pending. Measured on OPNsense 26.1.2_5: after an alias is deleted and
    /// both `alias/reconfigure` and `filter/apply` answer, its table stays
    /// loaded (with its old content) for as long as it was watched, because
    /// the appliance's `update_tables.py` only drops an orphan table on a full
    /// refresh that finds its file in `/var/db/aliastables`. Reading it as
    /// pending made the engine's own deletion fail its commit and then refused
    /// every later commit as a foreign change. What such a table could
    /// hide is harmless: the appliance refuses to delete an alias a rule still
    /// uses, and a deleted rule still loaded is caught by the rule check.
    ///
    /// What this does NOT see: an edit to a rule's match fields (source,
    /// destination, protocol…) that kept its uuid and its enabled state, and
    /// an alias whose entries are host names (resolved by the appliance, not
    /// comparable). Said here rather than implied by a green commit.
    pub fn pending_changes(&self) -> Result<Vec<PendingChange>> {
        let mut out = Vec::new();

        let rules = self.search_rows("firewall/filter/search_rule")?;
        let running = self.request(
            reqwest::Method::GET,
            "diagnostics/firewall/pf_statistics/rules",
            None,
        )?;
        let running = running_rule_labels(&running)?;
        let mut configured = std::collections::BTreeSet::new();
        for row in &rules {
            let Some(uuid) = row.get("uuid").and_then(Value::as_str) else {
                continue;
            };
            configured.insert(uuid.to_string());
            let enabled = str_field(row, "enabled") != "0";
            let loaded = running.contains(uuid);
            let what = match (enabled, loaded) {
                (true, false) => "created or enabled, not applied",
                (false, true) => "disabled, not applied",
                _ => continue,
            };
            out.push(PendingChange {
                kind: "rule",
                id: uuid.to_string(),
                label: str_field(row, "description"),
                what,
            });
        }
        for uuid in running.difference(&configured) {
            out.push(PendingChange {
                kind: "rule",
                id: uuid.clone(),
                label: String::new(),
                what: "deleted, not applied",
            });
        }

        let aliases = self.search_rows("firewall/alias/search_item")?;
        let tables: std::collections::BTreeSet<String> = self
            .request(reqwest::Method::GET, "firewall/alias_util/aliases", None)?
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        for row in &aliases {
            let name = str_field(row, "name");
            let kind = str_field(row, "type");
            if kind != "host" && kind != "network" || str_field(row, "enabled") == "0" {
                continue;
            }
            if !tables.contains(&name) {
                out.push(PendingChange {
                    kind: "alias",
                    id: name,
                    label: str_field(row, "description"),
                    what: "created, not applied",
                });
                continue;
            }
            let Some(want) = literal_entries(&str_field(row, "content")) else {
                continue;
            };
            let listed = self.search_rows_get(&format!("firewall/alias_util/list/{name}"))?;
            let have: std::collections::BTreeSet<String> = listed
                .iter()
                .filter_map(|r| r.get("ip").and_then(Value::as_str))
                .map(canonical_entry)
                .collect();
            if have != want {
                out.push(PendingChange {
                    kind: "alias",
                    id: name,
                    label: str_field(row, "description"),
                    what: "content changed, not applied",
                });
            }
        }
        Ok(out)
    }

    /// `GET` of a route answering `{"rows": [...]}`.
    fn search_rows_get(&self, path: &str) -> Result<Vec<Value>> {
        let body = self.request(reqwest::Method::GET, path, None)?;
        Ok(body
            .get("rows")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Refuses ([`Error::ForeignPending`]) when anything staged on the
    /// appliance is not in `staging`.
    pub fn check_no_foreign_pending(&self, staging: &Staging) -> Result<()> {
        let foreign: Vec<PendingChange> = self
            .pending_changes()?
            .into_iter()
            .filter(|p| !staging.covers(p))
            .collect();
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

    /// Applies what `staging` staged — and only when that is ALL that is
    /// staged. Checks again (someone may have staged since the pre-check),
    /// then `alias/reconfigure` and `filter/apply`, then proves the result:
    /// nothing of ours may still be pending afterwards.
    ///
    /// When the second check refuses, what this engine CREATED is deleted
    /// again (rules before aliases), so a later apply by the operator does
    /// not push it; what it DELETED cannot be restored and is named in the
    /// error.
    pub fn commit(&self, staging: &Staging) -> Result<()> {
        if let Err(refused) = self.check_no_foreign_pending(staging) {
            return Err(self.discard_staged(staging, refused));
        }
        self.reconfigure_aliases()?;
        self.apply_filter()?;
        let ours: Vec<String> = self
            .pending_changes()?
            .into_iter()
            .filter(|p| staging.covers(p))
            .map(|p| p.to_string())
            .collect();
        if !ours.is_empty() {
            return Err(Error::HttpStatus(format!(
                "the appliance answered the apply, but these changes of this engine are still \
                 not running: {}",
                ours.join("; ")
            )));
        }
        staging.clear();
        Ok(())
    }

    /// Deletes again what `staging` created (rules first: an alias a rule
    /// still references is refused by the appliance), and returns `refused`
    /// with what could not be undone appended.
    fn discard_staged(&self, staging: &Staging, refused: Error) -> Error {
        let changes = staging.take();
        let mut left = Vec::new();
        let mut order: Vec<&StagedChange> = changes.iter().filter(|c| c.kind == "rule").collect();
        order.extend(changes.iter().filter(|c| c.kind == "alias"));
        for c in order {
            if c.op == StagedOp::Deleted {
                left.push(format!("{} '{}' deleted", c.kind, c.label));
                continue;
            }
            let undo = if c.kind == "rule" {
                self.request(
                    reqwest::Method::POST,
                    &format!("firewall/filter/del_rule/{}", c.id),
                    None,
                )
                .map(drop)
            } else {
                self.search_rows("firewall/alias/search_item")
                    .and_then(
                        |rows| match rows.iter().find(|r| str_field(r, "name") == c.id) {
                            Some(row) => row_uuid(row, "firewall/alias/search_item"),
                            None => Err(Error::Decode(format!("alias '{}' not found", c.id))),
                        },
                    )
                    .and_then(|uuid| {
                        self.request(
                            reqwest::Method::POST,
                            &format!("firewall/alias/del_item/{uuid}"),
                            None,
                        )
                        .map(drop)
                    })
            };
            if let Err(e) = undo {
                left.push(format!("{} '{}' created ({e})", c.kind, c.label));
            }
        }
        if left.is_empty() {
            return refused;
        }
        let text = format!(
            "{refused} — and these changes of this engine are still staged on the appliance: {}",
            left.join("; ")
        );
        match refused {
            Error::ForeignPending(_) => Error::ForeignPending(text),
            _ => Error::HttpStatus(text),
        }
    }

    /// Activates staged alias changes. A SEPARATE call from
    /// [`Self::apply_filter`] — measured live (ADR-0051 Phase 0): a rule
    /// referencing a freshly-created alias resolved the alias's content
    /// correctly on read before either was reconfigured, but the two
    /// calls are independent and both are needed for the dataplane to
    /// pick the change up.
    pub fn reconfigure_aliases(&self) -> Result<()> {
        self.request(reqwest::Method::POST, "firewall/alias/reconfigure", None)?;
        Ok(())
    }

    /// Activates staged filter rule changes. Synchronous — measured live
    /// (ADR-0051 Phase 0): returns in well under a second with the
    /// reload's own stdout, never a task id.
    pub fn apply_filter(&self) -> Result<()> {
        self.request(reqwest::Method::POST, "firewall/filter/apply", None)?;
        Ok(())
    }
}

/// Whether a change staged on the appliance was made or undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StagedOp {
    Created,
    Deleted,
}

/// One write this engine staged and has not applied yet: a rule by uuid
/// (its pf label once applied), an alias by name (its pf table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedChange {
    pub kind: &'static str,
    pub id: String,
    /// The rule's description or the alias's name, for a message.
    pub label: String,
    pub op: StagedOp,
}

impl StagedChange {
    fn alias(name: &str, op: StagedOp) -> Self {
        Self {
            kind: "alias",
            id: name.to_string(),
            label: name.to_string(),
            op,
        }
    }

    fn rule(uuid: &str, description: &str, op: StagedOp) -> Self {
        Self {
            kind: "rule",
            id: uuid.to_string(),
            label: description.to_string(),
            op,
        }
    }
}

/// What ONE caller staged — one `NetworkGateway` apply or teardown, through
/// one [`OpnsenseGatewayProvider`] — so its commit can tell its own staged
/// changes from everybody else's. Separate from [`Client`] on purpose: the
/// client is shared by every provider value the registry builds.
#[derive(Debug, Default)]
pub struct Staging(std::sync::Mutex<Vec<StagedChange>>);

impl Staging {
    fn record(&self, change: StagedChange) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(change);
    }

    fn covers(&self, pending: &PendingChange) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|c| c.kind == pending.kind && c.id == pending.id)
    }

    fn take(&self) -> Vec<StagedChange> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn clear(&self) {
        drop(self.take());
    }

    /// What is staged now, for a test or a message.
    pub fn changes(&self) -> Vec<StagedChange> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// One change the appliance has configured and not applied, as
/// [`Client::pending_changes`] sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingChange {
    /// `rule` or `alias`.
    pub kind: &'static str,
    /// A rule's uuid, an alias's name.
    pub id: String,
    /// A rule's description, an alias's description — empty when the object
    /// is only in the running state (deleted, not applied).
    pub label: String,
    pub what: &'static str,
}

impl std::fmt::Display for PendingChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.label.is_empty() {
            write!(f, "{} {} ({})", self.kind, self.id, self.what)
        } else {
            write!(
                f,
                "{} {} '{}' ({})",
                self.kind, self.id, self.label, self.what
            )
        }
    }
}

/// Whose a row is: its `categories` (comma-joined category uuids) against
/// the owner labels. Our label wins over another record's on the same
/// object; no owner label at all is [`Owner::Unmarked`].
fn owner_of_row(row: &Value, owner: &OwnerMark, labels: &[(String, String)]) -> Owner {
    let mut other = None;
    for id in str_field(row, "categories")
        .split(',')
        .map(str::trim)
        .filter(|i| !i.is_empty())
    {
        let Some((_, name)) = labels.iter().find(|(uuid, _)| uuid == id) else {
            continue;
        };
        match owner.owner_of_label(name) {
            Some(Owner::Ours) => return Owner::Ours,
            Some(o) => other = Some(o),
            None => {}
        }
    }
    other.unwrap_or(Owner::Unmarked)
}

/// A string field of a search row — the grid answers the RAW value under the
/// field's name (and a display form under `%<field>`, never read here).
fn str_field(row: &Value, field: &str) -> String {
    match row.get(field) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn row_uuid(row: &Value, route: &str) -> Result<String> {
    row.get("uuid")
        .and_then(Value::as_str)
        .filter(|u| is_uuid(u))
        .map(str::to_string)
        .ok_or_else(|| Error::Decode(format!("{route}: a row without a uuid: {row}")))
}

/// `8-4-4-4-12` hexadecimal — an MVC object's uuid, and the only shape a
/// uuid read from the appliance may have before it goes into a URL path.
fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The entries of an alias's `content` as a set — the grid joins them with a
/// newline or a comma depending on the version, so both separate.
fn entries(content: &str) -> std::collections::BTreeSet<String> {
    content
        .split(['\n', ','])
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_string)
        .collect()
}

/// An address or CIDR as pf lists it: a single-host CIDR (`/32`, `/128`) is
/// shown as the bare address.
fn canonical_entry(e: &str) -> String {
    let e = e.trim();
    match e.split_once('/') {
        Some((ip, "32")) if ip.parse::<std::net::Ipv4Addr>().is_ok() => ip.to_string(),
        Some((ip, "128")) if ip.parse::<std::net::Ipv6Addr>().is_ok() => ip.to_string(),
        _ => e.to_string(),
    }
}

/// The alias's entries in pf's form when EVERY one is a literal address or
/// CIDR; `None` when one is a host name, which the appliance resolves and
/// this engine cannot compare.
fn literal_entries(content: &str) -> Option<std::collections::BTreeSet<String>> {
    let all = entries(content);
    all.iter()
        .all(|e| {
            let ip = e.split_once('/').map_or(e.as_str(), |(ip, _)| ip);
            ip.parse::<std::net::IpAddr>().is_ok()
        })
        .then(|| all.iter().map(|e| canonical_entry(e)).collect())
}

/// How an owned alias differs from the declaration, field by field.
fn alias_drift(row: &Value, want: &GatewayAlias) -> Vec<String> {
    let mut out = Vec::new();
    let kind = alias_write(want).kind;
    let have_kind = str_field(row, "type");
    if have_kind != kind {
        out.push(format!("type is '{have_kind}', declared '{kind}'"));
    }
    let have = entries(&str_field(row, "content"));
    let declared: std::collections::BTreeSet<String> =
        want.content.iter().map(|e| e.trim().to_string()).collect();
    if have != declared {
        out.push(format!(
            "content is [{}], declared [{}]",
            have.into_iter().collect::<Vec<_>>().join(", "),
            declared.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    let description = str_field(row, "description");
    if description != want.description.trim() {
        out.push(format!(
            "description is '{description}', declared '{}'",
            want.description.trim()
        ));
    }
    if str_field(row, "enabled") == "0" {
        out.push("it is disabled".to_string());
    }
    out
}

/// The uuid labels of the filter rules pf has loaded, from
/// `pf_statistics/rules`: one key per pf rule, its text as `pfctl -vvsr`
/// prints it, ending in `label "<uuid>"`. A `TCP/UDP` rule is two pf rules
/// with one label. An answer without the `filter rules` section is refused
/// rather than read as "nothing loaded", which would call every configured
/// rule unapplied.
fn running_rule_labels(answer: &Value) -> Result<std::collections::BTreeSet<String>> {
    let rules = answer
        .get("rules")
        .and_then(|r| r.get("filter rules"))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            Error::Decode(format!(
                "diagnostics/firewall/pf_statistics/rules answered without its filter rules: {}",
                truncate_chars(&answer.to_string(), 200)
            ))
        })?;
    Ok(rules
        .keys()
        .filter_map(|line| {
            let label = line.split(" label \"").nth(1)?;
            let uuid = label.split('"').next()?;
            is_uuid(uuid).then(|| uuid.to_string())
        })
        .collect())
}

/// How an owned rule differs from the declaration, field by field. An
/// absent protocol is the appliance's `any`.
fn rule_drift(row: &Value, want: &GatewayRule) -> Vec<String> {
    let mut out = Vec::new();
    for (field, declared) in [
        ("source_net", want.source.as_str()),
        ("destination_net", want.destination.as_str()),
    ] {
        let have = str_field(row, field);
        if have != declared {
            out.push(format!("{field} is '{have}', declared '{declared}'"));
        }
    }
    let have = str_field(row, "protocol");
    let have = if have.is_empty() {
        "any".to_string()
    } else {
        have
    };
    let declared = want.protocol.as_deref().unwrap_or("any");
    if !have.eq_ignore_ascii_case(declared) {
        out.push(format!("protocol is '{have}', declared '{declared}'"));
    }
    if str_field(row, "enabled") == "0" {
        out.push("it is disabled".to_string());
    }
    out
}

impl Client {
    /// `text` without this client's credential: the secret, and the Basic
    /// header value it is sent in, which a proxy or an error page can echo.
    /// Every answer passes here before it can reach an error message
    /// (ADR-0059 D5; the grep-for-the-secret rule of ADR-0049).
    fn redact(&self, text: &str) -> String {
        let basic = base64_std(format!("{}:{}", self.auth.key, self.auth.secret).as_bytes());
        delonix_model::redact_known(text, &[&self.auth.secret, &basic])
    }
}

/// Standard base64 with padding (RFC 4648 §4), the encoding of an HTTP Basic
/// credential. Only used to recognise one in an answer, so it lives here
/// instead of adding a dependency.
fn base64_std(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> shift) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Reads at most [`MAX_RESPONSE_BYTES`]; one byte more is a refusal, never
/// a silently cut body handed to a parser.
fn read_bounded(resp: reqwest::blocking::Response, path: &str) -> Result<String> {
    use std::io::Read;
    let mut buf = Vec::new();
    resp.take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| Error::Request(format!("reading the answer from {path} failed: {e}")))?;
    if buf.len() > MAX_RESPONSE_BYTES {
        return Err(Error::ResponseTooLarge(format!(
            "the answer from {path} exceeded {} MiB — refusing to read it",
            MAX_RESPONSE_BYTES / (1024 * 1024)
        )));
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// `s`, cut to at most `max` BYTES, backing off to the nearest character
/// boundary — never a byte-index slice that can land inside a multi-byte
/// UTF-8 sequence and panic (the same fix `delonix-proxmox` needed after
/// its own security review).
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

/// `{"result":"failed", ...}` at HTTP 200 (ADR-0051 Phase 0, measured) is
/// this API's error channel, not its status line. With a `validations`
/// object, every `<record>.<field>: <reason>` pair is named; without one
/// (a non-POST request to a POST-only action, also measured), the message
/// says so instead of inventing a field that was not there.
fn validation_failure(body: &Value) -> Option<Error> {
    if body.get("result").and_then(Value::as_str) != Some("failed") {
        return None;
    }
    match body.get("validations").and_then(Value::as_object) {
        Some(validations) if !validations.is_empty() => {
            let mut msgs: Vec<String> = validations
                .iter()
                .map(|(field, reason)| format!("{field}: {}", reason.as_str().unwrap_or_default()))
                .collect();
            msgs.sort();
            Some(Error::Validation(msgs.join("; ")))
        }
        _ => Some(Error::HttpStatus(
            "the appliance refused this call (\"result\":\"failed\", no per-field validations \
             — check the HTTP verb and the route)"
                .to_string(),
        )),
    }
}

/// The [`GatewayProvider`] this crate exists to provide: OPNsense's
/// firewall API, wrapped.
pub struct OpnsenseGatewayProvider {
    client: std::sync::Arc<Client>,
    /// What THIS value staged — one per apply, while the client is shared.
    staging: Staging,
}

impl OpnsenseGatewayProvider {
    pub fn connect(target: &Target) -> Result<Self> {
        Ok(Self::sharing(std::sync::Arc::new(Client::connect(target)?)))
    }

    /// Wraps an already-connected, possibly shared client — what
    /// [`register_with`]'s cached factory hands back on every selection
    /// after the first, instead of reconnecting.
    fn sharing(client: std::sync::Arc<Client>) -> Self {
        Self {
            client,
            staging: Staging::default(),
        }
    }
}

/// The canonical id this provider registers under.
pub const ID: &str = "opnsense";

/// Registers this provider with `delonix-sdn`'s gateway registry, under
/// [`ID`] (mirrors `delonix_proxmox::register_with`).
///
/// **Nothing here does I/O until the registered factory is actually
/// selected** — the same contract `register_gateway_provider`'s own doc
/// documents. The connection is cached once made and NOT cached on
/// failure: an appliance that was down when first selected must not stay
/// "down" for the rest of the process.
pub fn register_with(target: Target) -> delonix_model::Result<()> {
    let shared: std::sync::Mutex<Option<std::sync::Arc<Client>>> = std::sync::Mutex::new(None);
    delonix_networking::gateway::register_gateway_provider(
        delonix_networking::gateway::GatewayProviderRegistration {
            id: ID,
            aliases: &[],
            new: Box::new(move || {
                let mut slot = shared.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(c) = slot.as_ref() {
                    return Ok(Box::new(OpnsenseGatewayProvider::sharing(c.clone()))
                        as Box<dyn GatewayProvider>);
                }
                let c = std::sync::Arc::new(
                    Client::connect(&target)
                        .map_err(|e| delonix_networking::Error::from(e.into_root()))?,
                );
                *slot = Some(c.clone());
                Ok(Box::new(OpnsenseGatewayProvider::sharing(c)) as Box<dyn GatewayProvider>)
            }),
        },
    )?;
    Ok(())
}

// Every failure crosses the trait with its dictionary number
// (`delonix_model::Error::from`), never `into_root`, which strips the carrier:
// measured live on the zone provider, a DX-5340 refusal arrived as a bare 5000.
/// The skeleton every role port extends (ADR-0059 D1 rule 4). The report is
/// the declared one — a remote appliance is never contacted to answer it —
/// and this value exists only once its registration was configured.
impl delonix_compute::vm_provider::Provider for OpnsenseGatewayProvider {
    fn id(&self) -> delonix_compute::vm_provider::ProviderId {
        delonix_compute::vm_provider::ProviderId(ID)
    }

    fn capabilities(&self) -> delonix_compute::capability::ProviderReport {
        capability_report(true)
    }
}

impl GatewayProvider for OpnsenseGatewayProvider {
    fn available(&self) -> bool {
        // Registered only once `Client::connect` has already proven the
        // credential (ADR-0008's own reasoning for a remote VmBackend):
        // by the time this value exists, it IS available.
        true
    }

    fn ensure_alias(
        &self,
        alias: &GatewayAlias,
        owner: &OwnerMark,
    ) -> delonix_model::Result<EnsureOutcome> {
        self.client
            .ensure_alias(alias, owner, &self.staging)
            .map_err(delonix_model::Error::from)
    }

    fn remove_alias(&self, name: &str, owner: &OwnerMark) -> delonix_model::Result<RemoveOutcome> {
        self.client
            .remove_alias(name, owner, &self.staging)
            .map_err(delonix_model::Error::from)
    }

    fn ensure_rule(
        &self,
        rule: &GatewayRule,
        owner: &OwnerMark,
    ) -> delonix_model::Result<EnsureOutcome> {
        self.client
            .ensure_rule(rule, owner, &self.staging)
            .map_err(delonix_model::Error::from)
    }

    fn remove_rule(
        &self,
        description: &str,
        owner: &OwnerMark,
    ) -> delonix_model::Result<RemoveOutcome> {
        self.client
            .remove_rule(description, owner, &self.staging)
            .map_err(delonix_model::Error::from)
    }

    fn release_owner(&self, owner: &OwnerMark) -> delonix_model::Result<RemoveOutcome> {
        self.client
            .release_owner(owner)
            .map_err(delonix_model::Error::from)
    }

    fn check_no_foreign_pending(&self) -> delonix_model::Result<()> {
        self.client
            .check_no_foreign_pending(&self.staging)
            .map_err(delonix_model::Error::from)
    }

    fn commit(&self) -> delonix_model::Result<()> {
        self.client
            .commit(&self.staging)
            .map_err(delonix_model::Error::from)
    }
}

#[cfg(test)]
mod base64_tests {
    /// The RFC 4648 §10 test vectors.
    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(super::base64_std(i.as_bytes()), o, "{i}");
        }
    }
}
