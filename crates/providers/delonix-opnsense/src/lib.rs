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
//! existing alias/rule found under the same identity; they report it
//! [`delonix_sdn::gateway::EnsureOutcome::AlreadyPresent`] and leave it
//! untouched.

mod error;

pub use error::{Error, Result, MAX_RESPONSE_BYTES};

use delonix_sdn::gateway::{
    EnsureOutcome, GatewayAlias, GatewayProvider, GatewayRule,
};
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
}

fn alias_write(alias: &GatewayAlias) -> AliasWrite {
    AliasWrite {
        name: alias.name.clone(),
        kind: match alias.kind {
            delonix_sdn::gateway::AliasKind::Host => "host",
            delonix_sdn::gateway::AliasKind::Network => "network",
        },
        content: alias.content.join("\n"),
        description: alias.description.clone(),
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
}

fn rule_write(rule: &GatewayRule) -> RuleWrite {
    RuleWrite {
        description: rule.description.clone(),
        source_net: rule.source.clone(),
        destination_net: rule.destination.clone(),
        protocol: rule.protocol.clone(),
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

    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
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
        let text = read_bounded(resp, &format!("{method} {path}"))?;
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

    fn find_uuid_by(&self, search_path: &str, rows_key_match: impl Fn(&Value) -> bool) -> Result<Option<String>> {
        let body = self.request(
            reqwest::Method::POST,
            search_path,
            Some(&serde_json::json!({})),
        )?;
        let rows = body
            .get("rows")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(rows
            .iter()
            .find(|row| rows_key_match(row))
            .and_then(|row| row.get("uuid"))
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    /// Ensures an address alias exists, by name. Never updates one already
    /// there (see the module doc).
    pub fn ensure_alias(&self, alias: &GatewayAlias) -> Result<EnsureOutcome> {
        let name = alias.name.clone();
        if self
            .find_uuid_by("firewall/alias/search_item", move |row| {
                row.get("name").and_then(Value::as_str) == Some(name.as_str())
            })?
            .is_some()
        {
            return Ok(EnsureOutcome::AlreadyPresent);
        }
        let write = alias_write(alias);
        self.request(
            reqwest::Method::POST,
            "firewall/alias/add_item",
            Some(&serde_json::json!({ "alias": write })),
        )?;
        Ok(EnsureOutcome::Created)
    }

    /// Removes an alias by name; a no-op if none exists.
    pub fn remove_alias(&self, name: &str) -> Result<()> {
        let want = name.to_string();
        let uuid = self.find_uuid_by("firewall/alias/search_item", move |row| {
            row.get("name").and_then(Value::as_str) == Some(want.as_str())
        })?;
        if let Some(uuid) = uuid {
            self.request(
                reqwest::Method::POST,
                &format!("firewall/alias/del_item/{uuid}"),
                None,
            )?;
        }
        Ok(())
    }

    /// Ensures a perimeter filter rule exists, by description. Never
    /// updates one already there (see the module doc).
    pub fn ensure_rule(&self, rule: &GatewayRule) -> Result<EnsureOutcome> {
        let description = rule.description.clone();
        if self
            .find_uuid_by("firewall/filter/search_rule", move |row| {
                row.get("description").and_then(Value::as_str) == Some(description.as_str())
            })?
            .is_some()
        {
            return Ok(EnsureOutcome::AlreadyPresent);
        }
        let write = rule_write(rule);
        self.request(
            reqwest::Method::POST,
            "firewall/filter/add_rule",
            Some(&serde_json::json!({ "rule": write })),
        )?;
        Ok(EnsureOutcome::Created)
    }

    /// Removes a rule by description; a no-op if none exists.
    pub fn remove_rule(&self, description: &str) -> Result<()> {
        let want = description.to_string();
        let uuid = self.find_uuid_by("firewall/filter/search_rule", move |row| {
            row.get("description").and_then(Value::as_str) == Some(want.as_str())
        })?;
        if let Some(uuid) = uuid {
            self.request(
                reqwest::Method::POST,
                &format!("firewall/filter/del_rule/{uuid}"),
                None,
            )?;
        }
        Ok(())
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
                .map(|(field, reason)| {
                    format!("{field}: {}", reason.as_str().unwrap_or_default())
                })
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
}

impl OpnsenseGatewayProvider {
    pub fn connect(target: &Target) -> Result<Self> {
        Ok(Self::sharing(std::sync::Arc::new(Client::connect(target)?)))
    }

    /// Wraps an already-connected, possibly shared client — what
    /// [`register_with`]'s cached factory hands back on every selection
    /// after the first, instead of reconnecting.
    fn sharing(client: std::sync::Arc<Client>) -> Self {
        Self { client }
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
    delonix_sdn::gateway::register_gateway_provider(delonix_sdn::gateway::GatewayProviderRegistration {
        id: ID,
        aliases: &[],
        new: Box::new(move || {
            let mut slot = shared.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = slot.as_ref() {
                return Ok(Box::new(OpnsenseGatewayProvider::sharing(c.clone()))
                    as Box<dyn GatewayProvider>);
            }
            let c = std::sync::Arc::new(
                Client::connect(&target).map_err(|e| delonix_sdn::Error::from(e.into_root()))?,
            );
            *slot = Some(c.clone());
            Ok(Box::new(OpnsenseGatewayProvider::sharing(c)) as Box<dyn GatewayProvider>)
        }),
    })?;
    Ok(())
}

impl GatewayProvider for OpnsenseGatewayProvider {
    fn id(&self) -> &'static str {
        ID
    }

    fn available(&self) -> bool {
        // Registered only once `Client::connect` has already proven the
        // credential (ADR-0008's own reasoning for a remote VmBackend):
        // by the time this value exists, it IS available.
        true
    }

    fn ensure_alias(&self, alias: &GatewayAlias) -> delonix_model::Result<EnsureOutcome> {
        self.client.ensure_alias(alias).map_err(Error::into_root)
    }

    fn remove_alias(&self, name: &str) -> delonix_model::Result<()> {
        self.client.remove_alias(name).map_err(Error::into_root)
    }

    fn ensure_rule(&self, rule: &GatewayRule) -> delonix_model::Result<EnsureOutcome> {
        self.client.ensure_rule(rule).map_err(Error::into_root)
    }

    fn remove_rule(&self, description: &str) -> delonix_model::Result<()> {
        self.client
            .remove_rule(description)
            .map_err(Error::into_root)
    }

    fn commit(&self) -> delonix_model::Result<()> {
        self.client.reconfigure_aliases().map_err(Error::into_root)?;
        self.client.apply_filter().map_err(Error::into_root)
    }
}
