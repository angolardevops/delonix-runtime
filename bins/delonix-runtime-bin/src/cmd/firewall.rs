//! `delonix net ingress` / `delonix net egress` — the single firewall surface.
//!
//! Both groups edit ONE source of truth: the per-container [`ContainerFw`]
//! (persisted on the `Container`, enforced as nft rules in the ingress netns).
//! `ingress` owns inbound (`dir=in`) rules + the DNAT publishes; `egress` owns
//! outbound (`dir=out`) rules + the per-network egress-to-Internet policy. A
//! container only has a firewall when it lives on a custom network (it has an
//! IP on the `delonix0` bridge) — `--net host` containers share the host stack
//! and are rejected honestly.

use super::kinds as k;
use clap::Subcommand;
use clap_complete::engine::ArgValueCandidates;
use delonix_compute::Container;
use delonix_model::ports::StateRepository;
use delonix_model::records::{fw_port_ok, fw_proto_ok, fw_src_ok, FwRule};
use delonix_model::{Error, Result};
use delonix_sdn::infra;
use delonix_state::Store;
use serde::{Deserialize, Serialize};

use super::manifest::{self, ManifestDoc};
use super::output;
use super::util::open_stores;

/// `allow` (accept) or `deny` (drop) — the action baked into a rule or policy.
#[derive(clap::ValueEnum, Clone, Copy, PartialEq)]
pub enum Action {
    Allow,
    Deny,
}
impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Allow => "allow",
            Action::Deny => "deny",
        }
    }
}

/// How a network's egress to the Internet is governed.
#[derive(clap::ValueEnum, Clone, Copy)]
pub enum EgressMode {
    /// Allow all egress (the default).
    Allow,
    /// Block all egress to the Internet.
    Deny,
    /// Deny all egress EXCEPT DNS and the CIDRs given in `--to` (allowlist).
    Allowlist,
}

#[derive(Subcommand)]
pub enum IngressCmd {
    /// Allow inbound traffic to a container: `[proto/]port` from an optional CIDR.
    Allow {
        /// Container the rule belongs to. Must be on the SDN (`--net <network>`) —
        /// a `--net host`/`none` container has no firewall to govern.
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        // ONE paragraph on purpose: clap turns a second one into `long_help`, and
        // the catalog looks the rendered string up VERBATIM — a multi-paragraph
        // help comes out untranslated under `--l18n=pt`. The fact is the whole
        // point of the line, so it goes in the sentence and not below it.
        /// `tcp/5432`, `udp/53`, `5432` (any proto), or `tcp/*` (all ports) — the CONTAINER's port and never the host's, because the DNAT already rewrote it in `prerouting`.
        port: String,
        /// Only from this source CIDR (default: anywhere).
        #[arg(long)]
        from: Option<String>,
        /// Free-form note kept with the rule.
        #[arg(long)]
        note: Option<String>,
    },
    /// Deny inbound traffic to a container (same shape as `allow`).
    Deny {
        /// Container the rule belongs to. Must be on the SDN (`--net <network>`) —
        /// a `--net host`/`none` container has no firewall to govern.
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        // ONE paragraph on purpose: clap turns a second one into `long_help`, and
        // the catalog looks the rendered string up VERBATIM — a multi-paragraph
        // help comes out untranslated under `--l18n=pt`. The fact is the whole
        // point of the line, so it goes in the sentence and not below it.
        /// `tcp/5432`, `udp/53`, `5432` (any proto), or `tcp/*` (all ports) — the CONTAINER's port and never the host's, because the DNAT already rewrote it in `prerouting`.
        port: String,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Set the default inbound policy when no rule matches.
    Policy {
        /// Container to govern. Must be on the SDN (`--net <network>`).
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        policy: Action,
    },
    /// Publish a host port to the container (DNAT through the ingress).
    Publish {
        /// Container to govern. Must be on the SDN (`--net <network>`).
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        /// `[hostIp:]hostPort:containerPort[/tcp|udp]` or just `port`.
        /// SAFE BY DEFAULT: without `hostIp` the port binds to `127.0.0.1` only —
        /// reachable from the host itself, NOT from another machine. Name the address to
        /// widen it: `0.0.0.0:5070:5070/udp` (every interface), `192.168.1.10:8080:80`
        /// (one), or the libvirt gateway to reach it from VMs (see `delonix vm reach`).
        /// The resolved address is RECORDED, so it survives a `container start`.
        spec: String,
    },
    /// Remove a published host port.
    Unpublish {
        /// Container to govern. Must be on the SDN (`--net <network>`).
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        /// The HOST port to stop publishing — this one IS the host's, because a
        /// publish is `hostPort:containerPort` and the host side is its identity.
        host_port: String,
    },
    /// Show the inbound firewall (policy + rules) and published ports.
    Ls {
        /// Container to inspect (omit to list every container's inbound state).
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: Option<String>,
        /// Output format: `table` (default) or `json` (ADR-0005). `json` carries
        /// `governed` as its own field — a container off the SDN cannot HAVE a
        /// firewall, and a script must tell that from «open» without parsing a
        /// human sentence.
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: output::OutputFormat,
    },
    /// Remove inbound rule(s) matching `[proto/]port` (all protos if none given).
    Rm {
        /// Container to govern. Must be on the SDN (`--net <network>`).
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        /// `tcp/5432`, `5432` (any proto), or `*` (all ports) — the CONTAINER's
        /// port, the same one the rule was written with.
        port: String,
        /// Only rules from this source CIDR (default: any recorded source).
        #[arg(long)]
        from: Option<String>,
    },
    /// Remove all inbound rules (keeps published ports).
    Clear {
        /// Container to govern. Must be on the SDN (`--net <network>`).
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
    },
}

#[derive(Subcommand)]
pub enum EgressCmd {
    /// Allow outbound traffic from a container: `[proto/]port` to an optional CIDR.
    Allow {
        /// Container to govern. Must be on the SDN (`--net <network>`).
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        /// `tcp/5432`, `udp/53`, `5432` (any proto), or `tcp/*` (all ports) — the port on the DESTINATION this container is reaching for.
        port: String,
        /// Only to this destination CIDR (default: anywhere).
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Deny outbound traffic from a container (same shape as `allow`).
    Deny {
        /// Container to govern. Must be on the SDN (`--net <network>`).
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        /// `tcp/5432`, `udp/53`, `5432` (any proto), or `tcp/*` (all ports) — the port on the DESTINATION this container is reaching for.
        port: String,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Set the default outbound policy when no rule matches.
    Policy {
        /// Container to govern. Must be on the SDN (`--net <network>`).
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        policy: Action,
    },
    /// Govern a whole network's egress to the Internet.
    Net {
        /// Network whose egress this governs — the policy applies to every workload attached to it.
        #[arg(add = ArgValueCandidates::new(super::complete::networks))]
        network: String,
        mode: EgressMode,
        /// CIDRs for `allowlist` mode (comma-separated), e.g. `10.0.0.0/8,1.1.1.1/32`.
        #[arg(long)]
        to: Option<String>,
    },
    /// Allow a network's egress to a HOSTNAME (and `*.hostname`). Repeatable.
    ///
    /// Learnt live from DNS answers — the FQDN allowlist nft/CIDR can't
    /// express.
    Host {
        /// Network whose egress this governs — the policy applies to every workload attached to it.
        #[arg(add = ArgValueCandidates::new(super::complete::networks))]
        network: String,
        /// e.g. `github.com` (matches `github.com` and `*.github.com`).
        hostname: String,
    },
    /// Show the outbound firewall (policy + rules).
    Ls {
        /// Container to inspect (omit to list every container's outbound state).
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: Option<String>,
        /// Output format: `table` (default) or `json` (ADR-0005). `json` carries
        /// `governed` as its own field — a container off the SDN cannot HAVE a
        /// firewall, and a script must tell that from «open» without parsing a
        /// human sentence.
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: output::OutputFormat,
    },
    /// Remove outbound rule(s) matching `[proto/]port` (all protos if none given).
    Rm {
        /// Container to govern. Must be on the SDN (`--net <network>`).
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
        /// `tcp/5432`, `5432` (any proto), or `*` (all ports) — the CONTAINER's
        /// port, the same one the rule was written with.
        port: String,
        /// Only rules to this destination CIDR (default: any recorded destination).
        #[arg(long)]
        to: Option<String>,
    },
    /// Show a NETWORK's egress policy.
    ///
    /// CIDR allowlist, FQDN hosts, and the IPs currently learnt from DNS for
    /// those hosts.
    Show {
        /// Network whose egress this governs — the policy applies to every workload attached to it.
        #[arg(add = ArgValueCandidates::new(super::complete::networks))]
        network: String,
    },
    /// Remove all outbound rules.
    Clear {
        /// Container to govern. Must be on the SDN (`--net <network>`).
        #[arg(add = ArgValueCandidates::new(super::complete::containers))]
        container: String,
    },
}

/// `delonix net l4guard` — the ingress-wide L4 DDoS guard (per-source
/// connection rate + concurrent-connection cap on `tap0`). Until now this was
/// reachable ONLY through a `kind: Egress`/`FirewallPolicy` manifest with
/// `scope: network` + `rateLimit` — an operator reacting to an ongoing flood
/// had to write and apply a manifest to turn it on. `set`/`clear` expose the
/// same `infra::set_l4_guard`/`clear_l4_guard` the manifest path already
/// calls (zero new dataplane); `status` is new — the guard had no query verb
/// at all before this, so "is it even on" could only be answered by `nft
/// list` inside the holder's netns, which an operator has no route to.
#[derive(Subcommand)]
pub enum L4guardCmd {
    /// Turn the guard on (or update it): new conns/s and concurrent conns, per source IP.
    Set {
        /// New connections per second allowed from ONE source address.
        conn_rate: u32,
        /// Concurrent connections allowed from ONE source address at a time.
        conn_max: u32,
    },
    /// Turn the guard off.
    Clear,
    /// Show whether the guard is active, with its drop counters.
    Status,
}

pub fn run_l4guard(cmd: L4guardCmd) -> Result<()> {
    match cmd {
        L4guardCmd::Set {
            conn_rate,
            conn_max,
        } => {
            infra::set_l4_guard(conn_rate, conn_max)?;
            println!(
                "{}",
                super::po::tf(
                    "l4guard: active — up to {rate} new connection(s)/s and {max} concurrent connection(s) per source IP",
                    &[
                        ("rate", &conn_rate.to_string()),
                        ("max", &conn_max.to_string()),
                    ],
                )
            );
            Ok(())
        }
        L4guardCmd::Clear => {
            infra::clear_l4_guard()?;
            println!("{}", super::po::t("l4guard: cleared"));
            Ok(())
        }
        L4guardCmd::Status => {
            let rules = infra::l4_guard_status()?;
            if rules.is_empty() {
                println!("{}", super::po::t("l4guard: not active"));
                return Ok(());
            }
            println!("{}", super::po::t("l4guard: active"));
            for (text, packets, bytes) in rules {
                println!("  {text}  ({packets} packets, {bytes} bytes dropped)");
            }
            Ok(())
        }
    }
}

pub fn run_ingress(cmd: IngressCmd) -> Result<()> {
    let (_images, store) = open_stores()?;
    match cmd {
        IngressCmd::Allow {
            container,
            port,
            from,
            note,
        } => add_rule(&store, &container, "in", Action::Allow, &port, from, note),
        IngressCmd::Deny {
            container,
            port,
            from,
            note,
        } => add_rule(&store, &container, "in", Action::Deny, &port, from, note),
        IngressCmd::Policy { container, policy } => set_policy(&store, &container, "in", policy),
        IngressCmd::Publish { container, spec } => {
            let mut c = store.load(&container)?;
            super::container::publish_live(&store, &mut c, &spec)
        }
        IngressCmd::Unpublish {
            container,
            host_port,
        } => {
            let mut c = store.load(&container)?;
            super::container::unpublish_live(&store, &mut c, &host_port)
        }
        IngressCmd::Ls { container, output } => match container {
            Some(c) => list_rules(&store, &c, "in"),
            None => list_all(&store, "in", output),
        },
        IngressCmd::Rm {
            container,
            port,
            from,
        } => remove_rule(&store, &container, "in", &port, from),
        IngressCmd::Clear { container } => clear_dir(&store, &container, "in"),
    }
}

pub fn run_egress(cmd: EgressCmd) -> Result<()> {
    let (_images, store) = open_stores()?;
    match cmd {
        EgressCmd::Allow {
            container,
            port,
            to,
            note,
        } => add_rule(&store, &container, "out", Action::Allow, &port, to, note),
        EgressCmd::Deny {
            container,
            port,
            to,
            note,
        } => add_rule(&store, &container, "out", Action::Deny, &port, to, note),
        EgressCmd::Policy { container, policy } => set_policy(&store, &container, "out", policy),
        EgressCmd::Net { network, mode, to } => egress_net(&network, mode, to),
        EgressCmd::Host { network, hostname } => egress_host(&network, &hostname),
        EgressCmd::Show { network } => egress_show(&network),
        EgressCmd::Ls { container, output } => match container {
            Some(c) => list_rules(&store, &c, "out"),
            None => list_all(&store, "out", output),
        },
        EgressCmd::Rm {
            container,
            port,
            to,
        } => remove_rule(&store, &container, "out", &port, to),
        EgressCmd::Clear { container } => clear_dir(&store, &container, "out"),
    }
}

/// Split `[proto/]port` into a validated `(proto, port)`. `proto` defaults to
/// `any`; `port` accepts a number, a `n-m` range, or `*`.
fn parse_port_spec(spec: &str) -> Result<(String, String)> {
    let (proto, port) = match spec.split_once('/') {
        Some((p, port)) => (p.to_string(), port.to_string()),
        None => ("any".to_string(), spec.to_string()),
    };
    if !fw_proto_ok(&proto) {
        return Err(Error::Invalid(format!(
            "invalid proto '{proto}' (tcp|udp|any)"
        )));
    }
    if !fw_port_ok(&port) {
        return Err(Error::Invalid(format!(
            "invalid port '{port}' (1-65535, a range n-m, or *)"
        )));
    }
    Ok((proto, port))
}

/// The container's SDN IP, or an error explaining why a firewall can't attach.
pub(crate) fn require_sdn_ip(c: &Container) -> Result<String> {
    c.ip.clone().filter(|s| !s.is_empty()).ok_or_else(|| {
        Error::Invalid(format!(
            "'{}' has no firewall: it is not on a custom network (attach it with `--net <network>`; `--net host` shares the host stack)",
            c.name
        ))
    })
}

/// `""`, `0.0.0.0/0` and `*` all mean "from/to anywhere" — the
/// dataplane treats them alike (see `fw_chain_body`); normalize to compare.
fn norm_any(s: &str) -> &str {
    if s == "0.0.0.0/0" || s == "*" {
        ""
    } else {
        s
    }
}

/// `true` if two values of a field overlap in the first-match sense:
/// equal, or one is one of the given wildcards. (Conservative approximation — does
/// not parse `n-m` ranges; serves the shadow WARNING, not exact replacement.)
fn field_overlaps(a: &str, b: &str, wilds: &[&str]) -> bool {
    a == b || wilds.contains(&a) || wilds.contains(&b)
}

/// A rule's `[proto/]port` spec, to reproduce in `ingress rm`.
fn rule_spec(r: &FwRule) -> String {
    if r.proto.is_empty() || r.proto == "any" {
        r.port.clone()
    } else {
        format!("{}/{}", r.proto, r.port)
    }
}

/// Like `Store::update`, but for a closure that can itself fail — every
/// firewall mutation needs to call `infra::apply_firewall` (a kernel
/// syscall) partway through, and `Store::update`'s own closure only returns
/// a commit/abort `bool`, with no room to propagate an error from inside it.
///
/// BUG FOUND (code review): every firewall mutation in this file used to do
/// a bare `store.load` -> mutate in memory -> `infra::apply_firewall` ->
/// `store.save`, with NO lock held between the read and the write.
/// `Store::update` exists *precisely* to sequence this kind of
/// read-modify-write across processes (`flock`, see its own doc comment) —
/// it just was never used here. Concrete race: two firewall commands against
/// the same container (or a firewall command racing a concurrent reconcile
/// save) both read the same starting state, both apply their own change to
/// the kernel successfully, but only the LAST `save` wins on disk — the
/// other's rule is live in `nft` right now but silently missing from the
/// persisted record, so it vanishes on the next `container start` (which
/// only re-applies what's persisted).
/// The container a policy names — or, when no container has that name, the POD of
/// that name seen as one target ([`super::pod::pod_view`]). Read-only.
pub(crate) fn load_governed(store: &Store, target: &str) -> Result<Container> {
    match store.load(target) {
        Ok(c) => Ok(c),
        Err(e) => {
            let e: Error = e.into();
            if e.is_not_found() {
                if let Some((_, view)) = super::pod::pod_view(store, target)? {
                    return Ok(view);
                }
            }
            Err(e)
        }
    }
}

/// The key to hand [`update_locked`] for a record found by scanning: the pod's
/// name for a pod member (its policy lives on the pod, not on the member), the
/// container's id otherwise.
/// The NAME a policy is declared under: the pod's for a pod member (its policy
/// lives on the pod, not on the member), the container's own otherwise.
///
/// The read side needs the name where the write side needs the key: an operator
/// writes `net ingress allow <pod>`, and every way of reading it has to answer
/// to that same word.
pub(crate) fn governed_name(c: &Container) -> String {
    c.pod
        .as_deref()
        .and_then(super::pod::pod_of_netns)
        .map(str::to_string)
        .unwrap_or_else(|| c.name.clone())
}

/// [`governed_name`]'s sibling for a WRITE: the key `update_locked` takes.
pub(crate) fn governed_key(c: &Container) -> String {
    c.pod
        .as_deref()
        .and_then(super::pod::pod_of_netns)
        .map(str::to_string)
        .unwrap_or_else(|| c.id.clone())
}

pub(crate) fn update_locked<F>(store: &Store, id_or_name: &str, f: F) -> Result<Container>
where
    F: FnOnce(&mut Container) -> Result<bool>,
{
    // A pod is governed as ONE target: the closure runs on its view (netns id, pod
    // address) and only the firewall and annotations are written back to the head
    // member, under the head's lock.
    if let Err(e) = store.load(id_or_name) {
        let e: Error = e.into();
        if e.is_not_found() {
            if let Some((head_id, view)) = super::pod::pod_view(store, id_or_name)? {
                return update_pod_locked(store, id_or_name, &head_id, view, f);
            }
        }
    }
    let mut err = None;
    let c = store.update(id_or_name, |c| match f(c) {
        Ok(commit) => commit,
        Err(e) => {
            err = Some(e);
            false
        }
    })?;
    match err {
        Some(e) => Err(e),
        None => Ok(c),
    }
}

fn update_pod_locked<F>(
    store: &Store,
    pod: &str,
    head_id: &str,
    view: Container,
    f: F,
) -> Result<Container>
where
    F: FnOnce(&mut Container) -> Result<bool>,
{
    let ip = view.ip.clone().unwrap_or_default();
    let mut err = None;
    let mut out = view;
    store.update(head_id, |head| {
        let mut v = super::pod::as_pod_view(head, pod, &ip);
        match f(&mut v) {
            Ok(commit) => {
                if commit {
                    head.firewall = v.firewall.clone();
                    head.annotations = v.annotations.clone();
                }
                out = v;
                commit
            }
            Err(e) => {
                err = Some(e);
                false
            }
        }
    })?;
    match err {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

/// One place to reject a bad CIDR, so every entry point says the same thing — and says
/// the useful thing for the mistake that actually happens: an IPv6 CIDR. It used to
/// pass validation and then surface as a raw nft parse error from deep inside the
/// dataplane, because the ruleset is a v4 `table ip` (reproduced live).
pub(crate) fn check_cidr(src: &str) -> Result<()> {
    if src.is_empty() || fw_src_ok(src) {
        return Ok(());
    }
    Err(Error::Invalid(if src.contains(':') {
        super::po::tf(
            "invalid CIDR '{src}' — IPv6 is not supported (the SDN and the firewall are IPv4 only)",
            &[("src", src)],
        )
    } else {
        super::po::tf(
            "invalid CIDR '{src}' (expected an IPv4 address or CIDR, e.g. 10.0.0.0/8)",
            &[("src", src)],
        )
    }))
}

/// The `origin` an imperative `net ingress`/`net egress allow`/`deny` writes
/// on its rule — deterministic from the exact match tuple a repeat
/// invocation is judged against (`dir`/`proto`/`port`/normalized `src`), so
/// this is a rename of the KEY `add_rule`'s replace logic already matched
/// on, not new behavior: the same `(dir, proto, port, src)` still replaces
/// itself one-for-one, byte-identical UX (ADR-0029, decision 1). What
/// changes is that the key now lives in `FwRule.origin` — the SAME field
/// `kind: NetworkAccessRule` uses — instead of a bespoke 4-field comparison,
/// so `add_rule` and `network_access_rule::set_rule_by_origin` are one
/// mechanism with two writers, and a CLI-added rule becomes addressable via
/// `get networkaccessrules`/`describe`/`delete networkaccessrules` for free.
/// The `cli:` prefix keeps this namespace apart from manifest document names
/// (a `NetworkAccessRule`'s `metadata.name` can never collide with it and
/// still express the same match — the two mechanisms stay independent, per
/// the ADR: a `NetworkAccessRule` document with the identical tuple gets its
/// OWN origin and neither one silently erases the other's rule).
fn cli_rule_origin(dir: &str, proto: &str, port: &str, src: &str) -> String {
    format!("cli:{dir}:{proto}:{port}:{}", norm_any(src))
}

/// A rule's `origin` names a `kind: NetworkAccessRule` MANIFEST document, as
/// opposed to [`cli_rule_origin`]'s synthetic key for an imperative `net
/// ingress`/`net egress allow`/`deny`. The distinction matters in exactly one
/// place — `apply_fw_doc`'s `FirewallPolicy` replace, below — because now
/// that both writers put something in `origin`, "has an origin" alone no
/// longer means "independently-lifecycled document ADR-0028 protects";
/// giving `FirewallPolicy` the OLD "wipe every rule of this direction"
/// behavior for CLI-added rules (and only the NEW protection for genuine
/// `NetworkAccessRule` documents) keeps `FirewallPolicy`'s own "declares the
/// WHOLE state of a direction" contract exactly as it was before this
/// refactor — a `net ingress allow` is not a promise that a document
/// replacing the whole direction won't override it.
fn is_manifest_origin(origin: &str) -> bool {
    !origin.starts_with("cli:")
}

fn add_rule(
    store: &Store,
    name: &str,
    dir: &str,
    action: Action,
    port_spec: &str,
    cidr: Option<String>,
    note: Option<String>,
) -> Result<()> {
    let (proto, port) = parse_port_spec(port_spec)?;
    let src = cidr.unwrap_or_default();
    check_cidr(&src)?;
    let origin = cli_rule_origin(dir, &proto, &port, &src);
    let mut replaced: Vec<String> = Vec::new();
    let mut shadow: Option<(String, String)> = None;
    let c = update_locked(store, name, |c| {
        // Guard only: rejects a container off the SDN. The addresses the firewall is
        // keyed on come from `container_ips` (primary + every additional network).
        require_sdn_ip(c)?;
        let mut fw = super::container::firewall_or_new(c);
        fw.enabled = true;
        // The LAST command wins (ufw semantics): a new rule for the SAME match
        // (dir/proto/port/source) REPLACES the existing one. Without this, `deny 8069`
        // followed by `allow 8069` left the service blocked forever — the rules
        // accumulated and the nft chain is first-match terminal: the old deny,
        // above, always won (real bug report). Matched by `origin` now, not by
        // re-deriving the 4-field tuple — see `cli_rule_origin`.
        let same_match = |r: &FwRule| r.origin.as_deref() == Some(origin.as_str());
        replaced = fw
            .rules
            .iter()
            .filter(|r| same_match(r))
            .map(|r| r.action.clone())
            .collect();
        fw.rules.retain(|r| !same_match(r));
        fw.rules.push(FwRule {
            dir: dir.to_string(),
            proto: proto.clone(),
            port: port.clone(),
            src: src.clone(),
            action: action.as_str().to_string(),
            note: note.clone().unwrap_or_default(),
            origin: Some(origin.clone()),
        });
        // Shadow: an EARLIER overlapping rule (e.g. `deny any/8069` vs
        // `allow tcp/8069`) with the opposite action still matches first — the new
        // rule never gets evaluated. Warning here avoids the "I applied the allow and
        // it stays blocked" without explanation.
        shadow = fw
            .rules
            .iter()
            .take(fw.rules.len() - 1)
            .find(|r| {
                r.dir == dir
                    && r.action != action.as_str()
                    && field_overlaps(&r.proto, &proto, &["any", ""])
                    && field_overlaps(&r.port, &port, &["*", ""])
                    && field_overlaps(norm_any(&r.src), norm_any(&src), &[""])
            })
            .map(|r| (r.action.clone(), rule_spec(r)));
        super::container::apply_firewall_everywhere(c, &fw)?;
        c.firewall = Some(fw);
        Ok(true)
    })?;
    let arrow = if dir == "in" { "inbound" } else { "outbound" };
    println!(
        "{}: {arrow} rule added ({})",
        c.name,
        output::bold(&format!("{} {port_spec}", action.as_str()))
    );
    if let Some(old) = replaced.iter().find(|a| *a != action.as_str()) {
        println!(
            "{}",
            super::po::tf(
                "  (replaces the previous {old} rule for this match — the last command wins)",
                &[("old", old)],
            )
        );
    }
    if let Some((sh_action, sh_spec)) = shadow {
        let group = if dir == "in" { "ingress" } else { "egress" };
        output::warn(&super::po::tf(
            "an earlier overlapping rule ({action} {spec}) still matches first and can override this one — remove it with `delonix {group} rm {name} {spec}`",
            &[
                ("action", &sh_action),
                ("spec", &sh_spec),
                ("group", group),
                ("name", &c.name),
            ],
        ));
    }
    Ok(())
}

/// Remove rule(s) matching `[proto/]port` (+ CIDR, if given). The SPEC's
/// wildcards work as a filter: `rm c 8069` (proto `any`) removes the tcp/udp/any
/// rules for that port; `rm c '*'` removes all; without `--from`, any source.
/// Complements `clear` (all-or-nothing) with surgical removal.
fn remove_rule(
    store: &Store,
    name: &str,
    dir: &str,
    port_spec: &str,
    cidr: Option<String>,
) -> Result<()> {
    let (proto, port) = parse_port_spec(port_spec)?;
    let src = cidr.unwrap_or_default();
    check_cidr(&src)?;
    let mut n = 0usize;
    let c = update_locked(store, name, |c| {
        let ip = require_sdn_ip(c)?;
        let mut fw = super::container::firewall_or_new(c);
        let rm_match = |r: &FwRule| {
            r.dir == dir
                && (proto == "any" || r.proto == proto)
                && (port == "*" || r.port == port)
                && (norm_any(&src).is_empty() || norm_any(&r.src) == norm_any(&src))
        };
        let before = fw.rules.len();
        fw.rules.retain(|r| !rm_match(r));
        n = before - fw.rules.len();
        if n == 0 {
            let arrow = if dir == "in" { "inbound" } else { "outbound" };
            return Err(Error::Invalid(format!(
                "'{}' has no {arrow} rule matching {port_spec}",
                c.name
            )));
        }
        // Same rule as `clear`: with no rules and no explicit policies, the firewall
        // disappears entirely (clean chain) instead of leaving an empty record — but
        // only where that leaves nothing to enforce (see `firewall_disposable`).
        let empty = firewall_disposable(c, &fw);
        if empty {
            infra::clear_firewall(&ip);
        } else {
            super::container::apply_firewall_everywhere(c, &fw)?;
        }
        c.firewall = if empty { None } else { Some(fw) };
        Ok(true)
    })?;
    let arrow = if dir == "in" { "inbound" } else { "outbound" };
    println!(
        "{}",
        super::po::tf(
            "{name}: {n} {arrow} rule(s) removed ({spec})",
            &[
                ("name", &c.name),
                ("n", &n.to_string()),
                ("arrow", arrow),
                ("spec", port_spec),
            ],
        )
    );
    Ok(())
}

/// May this container's firewall be torn down entirely — chain, `@fwmap` entries and
/// record — now that its rules are gone?
///
/// Only when NOTHING is left to enforce. No rules and no explicit policy is not
/// enough: outside `default` the chain also carries the namespace isolation (the
/// same-namespace accept and the cross-namespace drop), and it lives nowhere else.
/// `ingress rm`/`clear` of the last rule used to tear it down with the rules, so
/// removing one allow opened the container to every namespace (NaaS audit P0-4) —
/// and cleared the record too, so a restart did not bring the isolation back either.
fn firewall_disposable(c: &Container, fw: &delonix_model::records::ContainerFw) -> bool {
    fw.rules.is_empty()
        && fw.policy_in.is_empty()
        && fw.policy_out.is_empty()
        && c.namespace == "default"
}

fn set_policy(store: &Store, name: &str, dir: &str, policy: Action) -> Result<()> {
    let c = update_locked(store, name, |c| {
        // Guard only: rejects a container off the SDN. The addresses the firewall is
        // keyed on come from `container_ips` (primary + every additional network).
        require_sdn_ip(c)?;
        let mut fw = super::container::firewall_or_new(c);
        fw.enabled = true;
        if dir == "in" {
            fw.policy_in = policy.as_str().to_string();
        } else {
            fw.policy_out = policy.as_str().to_string();
        }
        super::container::apply_firewall_everywhere(c, &fw)?;
        c.firewall = Some(fw);
        Ok(true)
    })?;
    let arrow = if dir == "in" { "inbound" } else { "outbound" };
    println!("{}: default {arrow} policy = {}", c.name, policy.as_str());
    // A `deny` default silently kills every published port whose CONTAINER port no rule
    // covers: the DNAT stays installed, the packet dies in the per-container chain, and
    // nothing in the output said so. Name the ports and the exact command that reopens them.
    if dir == "in" && policy == Action::Deny {
        let fw = c.firewall.clone().unwrap_or_default();
        for p in &c.ports {
            let Ok((_, cont_port, proto)) = delonix_sdn::parse_publish(p) else {
                continue;
            };
            match published_reach(&fw, &cont_port, &proto) {
                PublishReach::Blocked => println!(
                    "{}",
                    super::po::tf(
                        "warning: published port '{spec}' is now BLOCKED — reopen it with \
                         `delonix net ingress allow {name} {cont_port}` (the CONTAINER port: \
                         DNAT runs before the firewall)",
                        &[("spec", p), ("name", &c.name), ("cont_port", &cont_port)],
                    )
                ),
                // Not a warning: this is the shape a default-deny is usually set up FOR.
                // Saying it out loud still helps, because the source that keeps working
                // is easy to lose track of once the policy flips.
                PublishReach::Sources(s) => println!(
                    "{}",
                    super::po::tf(
                        "published port '{spec}' now answers only {from} \
                         (a client on the host's own loopback arrives as the gateway and will not match)",
                        &[("spec", p), ("from", &s.join(","))],
                    )
                ),
                PublishReach::Open => {}
            }
        }
    }
    Ok(())
}

/// One row of `net ingress ls` / `net egress ls`.
///
/// `governed` is the field the TABLE folds into the POLICY cell as
/// `n/a (host net)`: a container off the SDN cannot HAVE a firewall
/// (`require_sdn_ip` rejects every mutation for it). A script must be able to
/// tell «open» from «not governed at all» without parsing a human sentence —
/// that is the whole reason ADR-0005 exists.
#[derive(serde::Serialize)]
struct FwLsRow {
    name: String,
    /// `null` when the container is not governed — never `"allow"`, which would
    /// read as a decision that nothing is making.
    #[serde(skip_serializing_if = "Option::is_none")]
    policy: Option<String>,
    governed: bool,
    rules: usize,
    /// Published ports (ingress) or the networks the policy targets (egress).
    targets: Vec<String>,
}

/// Overview of every container's firewall state in one table — `ls` without an
/// argument, like `docker ps`. Per-container detail stays in `ls <container>`.
fn list_all(store: &Store, dir: &str, format: output::OutputFormat) -> Result<()> {
    let format = super::config::resolve_output(&super::util::state_root(), format);
    if format == output::OutputFormat::Json {
        let rows: Vec<FwLsRow> = store
            .list()?
            .into_iter()
            .map(|c| {
                let fw = c.firewall.clone().unwrap_or_default();
                let policy = if dir == "in" {
                    &fw.policy_in
                } else {
                    &fw.policy_out
                };
                let governed = c.ip.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
                let mut targets: Vec<String> = if dir == "in" {
                    c.ports.clone()
                } else {
                    let mut nets: Vec<String> = c.network.clone().into_iter().collect();
                    nets.extend(c.extra_networks.iter().map(|e| e.network.clone()));
                    nets
                };
                targets.retain(|t| !t.is_empty());
                FwLsRow {
                    name: c.name.clone(),
                    policy: governed.then(|| {
                        if policy.is_empty() {
                            "allow".to_string()
                        } else {
                            policy.clone()
                        }
                    }),
                    governed,
                    rules: fw.rules.iter().filter(|r| r.dir == dir).count(),
                    targets,
                }
            })
            .collect();
        return output::print_json(&rows);
    }
    let mut t = output::Table::new(&[
        "NAME",
        "POLICY",
        "RULES",
        if dir == "in" { "PUBLISHED" } else { "NETWORKS" },
    ]);
    for c in store.list()? {
        let fw = c.firewall.clone().unwrap_or_default();
        let policy = if dir == "in" {
            &fw.policy_in
        } else {
            &fw.policy_out
        };
        // Honest POLICY column: a container off the SDN cannot HAVE a firewall
        // (`require_sdn_ip` rejects every mutation for it), so "allow (default)" read as
        // "governed and open" when the truth is "not governed at all".
        let governed = c.ip.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
        let policy = if !governed {
            "n/a (host net)".to_string()
        } else if policy.is_empty() {
            "allow (default)".to_string()
        } else {
            policy.clone()
        };
        let rules = fw.rules.iter().filter(|r| r.dir == dir).count();
        let last = if dir == "in" {
            c.ports.join(", ")
        } else {
            // Main network + extras (multi-homing) — the targets of the egress policy.
            let mut nets: Vec<String> = c.network.clone().into_iter().collect();
            nets.extend(c.extra_networks.iter().map(|e| e.network.clone()));
            nets.join(", ")
        };
        t.row(vec![c.name.clone(), policy, rules.to_string(), last]);
    }
    t.print();
    Ok(())
}

/// One row of `get networkpolicies` — a container's policy in ONE direction.
#[derive(serde::Serialize)]
struct PolicyLsRow {
    target: String,
    direction: String,
    policy: String,
    rules: usize,
}

/// `delonix get networkpolicies` — the listing `FirewallPolicy`/`NetworkPolicy`
/// never had: `net ingress ls`/`net egress ls` each answer ONE direction, and
/// neither folds the two into a single global table. A `FirewallPolicy`
/// document has no registry of its own (see `network_access_rule.rs`'s doc
/// comment for the same fact about `NetworkAccessRule`) — its identity here is
/// `<target>/<direction>`, the same pair `describe`/`delete networkpolicies`
/// parse back out (`split_policy_name`), not a document name nothing persists.
///
/// Only GOVERNED containers get a row (an `--net host`/`none` container has no
/// firewall to have a policy in — same rule `list_all`'s POLICY column already
/// applies). A container with neither direction governed prints nothing,
/// exactly like it does not appear in `net ingress ls`/`net egress ls` today.
pub(crate) fn list_all_policies(output: output::OutputFormat) -> Result<()> {
    let output = super::config::resolve_output(&super::util::state_root(), output);
    let (_images, store) = open_stores()?;
    // A POD is ONE target. Its members share a netns and one address, and the
    // policy lives on the head member — so listing the records one by one named
    // the MEMBER where the operator wrote the POD, and said «allow (default), 0
    // rules» on every other member. Measured 2026-10-06: a two-member pod with
    // one ingress rule printed four rows, and two of them reported no policy
    // over a policy that exists. A listing that denies a firewall is worse than
    // no listing.
    //
    // So the records collapse by [`governed_name`], and a target keeps the
    // record that HAS a firewall: `Some` wins over `None`, whatever order the
    // store lists them in.
    let mut by_target: std::collections::BTreeMap<
        String,
        Option<delonix_model::records::ContainerFw>,
    > = std::collections::BTreeMap::new();
    for c in store.list()? {
        let governed = c.ip.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
        if !governed {
            continue;
        }
        let slot = by_target.entry(governed_name(&c)).or_insert(None);
        if slot.is_none() {
            *slot = c.firewall.clone();
        }
    }
    let mut rows = Vec::new();
    for (target, fw) in by_target {
        let fw = fw.unwrap_or_default();
        for (dir, label, policy) in [
            ("in", "ingress", &fw.policy_in),
            ("out", "egress", &fw.policy_out),
        ] {
            rows.push(PolicyLsRow {
                target: target.clone(),
                direction: label.into(),
                policy: if policy.is_empty() {
                    "allow (default)".to_string()
                } else {
                    policy.clone()
                },
                rules: fw.rules.iter().filter(|r| r.dir == dir).count(),
            });
        }
    }
    if output == output::OutputFormat::Json {
        return output::print_json(&rows);
    }
    let mut t = output::Table::new(&["TARGET", "DIRECTION", "POLICY", "RULES"]);
    for r in &rows {
        t.row(vec![
            r.target.clone(),
            r.direction.clone(),
            r.policy.clone(),
            r.rules.to_string(),
        ]);
    }
    t.print();
    Ok(())
}

/// Splits `<target>/<direction>` — the identity `get networkpolicies` prints
/// and `describe`/`delete networkpolicies` parse back. Mirrors
/// `netroute::split_route_name`'s reasoning: a `FirewallPolicy` is not
/// addressable by a document name (nothing persists one), so the generic
/// verbs key by what the resource actually IS — here, the (target, direction)
/// pair `validate_graph` already treats as this Kind's true identity.
pub(crate) fn split_policy_name(name: &str) -> Result<(&str, &str)> {
    let (target, dir) = name.split_once('/').ok_or_else(|| {
        Error::Invalid(format!(
            "'{name}' is not a policy name (expected `<target>/ingress` or `<target>/egress`)"
        ))
    })?;
    match dir {
        "ingress" => Ok((target, "in")),
        "egress" => Ok((target, "out")),
        other => Err(Error::Invalid(format!(
            "'{other}' is not ingress|egress (in '{name}')"
        ))),
    }
}

/// `delonix describe networkpolicies <target>/<direction>` — reuses the exact
/// per-container detail `net ingress ls <target>`/`net egress ls <target>`
/// already print, rather than inventing a second rendering of the same rules.
pub(crate) fn cmd_describe_policy(names: &[String]) -> Result<()> {
    let (_images, store) = open_stores()?;
    for name in names {
        let (target, dir) = split_policy_name(name)?;
        list_rules(&store, target, dir)?;
    }
    Ok(())
}

/// `delonix delete networkpolicies <target>/<direction>` — the policy for that
/// ONE direction resets to allow-all and its rules go with it; the other
/// direction is untouched.
///
/// It is NOT `net ingress clear <target>`, and this doc-comment said it was.
/// That verb removes the RULES of a direction and leaves the default to the
/// `policy` verb, which is coherent and is what its message promises. Here the
/// object being deleted IS the policy, so the default has to go too — measured
/// 2026-10-06 before the fix: with a `deny` in force, this answered rc 0 and
/// «removed 0 inbound rule(s)», `get`/`describe networkpolicies` still read
/// `deny`, and a ping from another container was still blocked. A delete that
/// reports success and deletes nothing.
pub(crate) fn cmd_delete_policy(names: &[String]) -> Result<()> {
    let (_images, store) = open_stores()?;
    for name in names {
        let (target, dir) = split_policy_name(name)?;
        clear_dir_with(&store, target, dir, true)?;
    }
    Ok(())
}

pub(crate) fn list_rules(store: &Store, name: &str, dir: &str) -> Result<()> {
    // `load_governed` and not `store.load`: every MUTATION already resolves a pod
    // by its name (`net ingress allow <pod>` works), and only the reads did not —
    // measured 2026-10-06, `net ingress ls <pod>` and `describe
    // networkpolicies <pod>/<dir>` answered `no such container` for a policy the
    // engine had just accepted under that exact name. A rule you can write and
    // cannot see is worse than one you cannot write.
    let c = load_governed(store, name)?;
    let fw = c.firewall.clone().unwrap_or_default();
    let policy = if dir == "in" {
        &fw.policy_in
    } else {
        &fw.policy_out
    };
    let default = if policy.is_empty() {
        "allow (default)"
    } else {
        policy.as_str()
    };
    let arrow = if dir == "in" { "INBOUND" } else { "OUTBOUND" };
    println!(
        "{} firewall for {} — default policy: {}",
        arrow, c.name, default
    );
    // Live counters straight off the dataplane. A firewall that cannot say whether a
    // rule ever matched is half a tool — this is the column an operator reads to tell
    // a rule that is protecting something from one that is dead weight (or worse,
    // silently shadowed by an earlier rule). Empty when the holder is down, in which
    // case the rules still print, with `-` instead of a made-up zero.
    let counters =
        c.ip.as_deref()
            .filter(|s| !s.is_empty())
            .map(delonix_sdn::infra::fw_counters)
            .unwrap_or_default();
    let hits = |r: &FwRule| match delonix_sdn::infra::fw_rule_tail(r) {
        Some(tail) => match counters.get(&tail) {
            Some((packets, bytes)) => (packets.to_string(), output::fmt_size(*bytes)),
            None => ("-".into(), "-".into()),
        },
        None => ("-".into(), "-".into()),
    };
    let mut t = output::Table::new(&[
        "PROTO",
        "PORT",
        if dir == "in" { "FROM" } else { "TO" },
        "ACTION",
        "PACKETS",
        "BYTES",
        "NOTE",
    ]);
    for r in fw.rules.iter().filter(|r| r.dir == dir) {
        let (packets, bytes) = hits(r);
        t.row(vec![
            or_any(&r.proto),
            or_any(&r.port),
            or_any(&r.src),
            r.action.clone(),
            packets,
            bytes,
            r.note.clone(),
        ]);
    }
    let mut blocked: Vec<(String, String)> = Vec::new();
    if dir == "in" {
        // A `--net host`/`none` container has no per-container chain at all (see
        // `require_sdn_ip`): its publishes are pure slirp hostfwds, governed by nothing
        // here. Saying `allow` would claim a policy that does not exist.
        let governed = c.ip.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
        for p in &c.ports {
            let (cont_port, proto) = delonix_sdn::parse_publish(p)
                .map(|(_, cp, pr)| (cp, pr))
                .unwrap_or_else(|_| (String::new(), "tcp".into()));
            let reach = if governed {
                published_reach(&fw, &cont_port, &proto)
            } else {
                PublishReach::Open
            };
            if let PublishReach::Blocked = reach {
                blocked.push((p.clone(), cont_port.clone()));
            }
            let (from, action) = match &reach {
                PublishReach::Open => ("any".to_string(), "allow"),
                PublishReach::Sources(s) => (s.join(","), "allow"),
                PublishReach::Blocked => ("any".to_string(), "BLOCKED"),
            };
            let note = if !governed {
                "DNAT (host net — no firewall)".to_string()
            } else if matches!(reach, PublishReach::Sources(_)) {
                // Worth spelling out, because it is the one case where the FROM column
                // does not describe every client: a request from the host's own
                // loopback reaches the container as the slirp gateway, so it does not
                // match a source rule written for the real client address.
                "DNAT (loopback clients arrive as the gateway)".to_string()
            } else {
                "DNAT".to_string()
            };
            t.row(vec![
                "publish".into(),
                p.clone(),
                from,
                action.to_string(),
                "-".into(),
                "-".into(),
                note,
            ]);
        }
    }
    t.print();
    for (spec, cont_port) in &blocked {
        println!();
        println!(
            "{}",
            super::po::tf(
                "warning: '{spec}' is published (DNAT is in place) but the inbound firewall \
                 drops it — the port answers nothing.",
                &[("spec", spec)],
            )
        );
        println!(
            "{}",
            super::po::tf(
                "  DNAT runs before the firewall, so a rule must name the CONTAINER port: \
                 `delonix net ingress allow {name} {cont_port}`",
                &[("name", &c.name), ("cont_port", cont_port)],
            )
        );
    }
    Ok(())
}

/// Does an inbound rule's port field cover `port`? Mirrors `fw_chain_body`:
/// empty/`*` means every port; otherwise an exact match or a `n-m` range.
fn port_covers(rule_port: &str, port: &str) -> bool {
    if rule_port.is_empty() || rule_port == "*" {
        return true;
    }
    let Ok(p) = port.parse::<u32>() else {
        return rule_port == port;
    };
    match rule_port.split_once('-') {
        Some((a, b)) => match (a.parse::<u32>(), b.parse::<u32>()) {
            (Ok(a), Ok(b)) => (a..=b).contains(&p),
            _ => false,
        },
        None => rule_port.parse::<u32>().map(|r| r == p).unwrap_or(false),
    }
}

/// The EFFECTIVE inbound verdict for a published port, resolved the way the dataplane
/// resolves it — the reason this exists at all: `ingress ls` used to print every publish
/// as `allow / DNAT` unconditionally, so a container under `policy deny` showed its port
/// as open while `curl` got nothing. The table has to answer the question it is read for.
///
/// Two facts drive it: (1) DNAT happens at `prerouting`, so by the time the per-container
/// chain sees the packet the destination port is the CONTAINER port, never the host port
/// — a rule must name `cont_port` to govern a publish; (2) the chain is first-match
/// terminal, so the first covering rule wins over the default policy.
///
/// Source-restricted rules are NOT ignored — they are the third answer. A publish
/// governed by `policy deny` + `allow <port> --from <cidr>` is neither open nor
/// blocked, and calling it `BLOCKED` (which this used to do) is wrong in the most
/// useful configuration there is: expose a port to exactly one network. Source
/// filtering does work on published ports, because the client address survives the
/// hop for every non-loopback client — see [`delonix_sdn::SLIRP_GW`].
enum PublishReach {
    /// Reachable from anywhere the bind address allows.
    Open,
    /// Reachable only from these sources.
    Sources(Vec<String>),
    /// The firewall drops it — the port answers nothing.
    Blocked,
}

fn published_reach(
    fw: &delonix_model::records::ContainerFw,
    cont_port: &str,
    proto: &str,
) -> PublishReach {
    let mut sources: Vec<String> = Vec::new();
    for r in fw.rules.iter().filter(|r| r.dir == "in") {
        let proto_covers = r.proto.is_empty() || r.proto == "any" || r.proto == proto;
        if !proto_covers || !port_covers(&r.port, cont_port) {
            continue;
        }
        let src = norm_any(&r.src);
        if src.is_empty() {
            // A rule with no source is terminal for EVERY source: whatever came
            // before it still stands, nothing after it is ever reached.
            return match (r.action.as_str(), sources.is_empty()) {
                ("allow", _) => PublishReach::Open,
                (_, true) => PublishReach::Blocked,
                (_, false) => PublishReach::Sources(sources),
            };
        }
        if r.action == "allow" {
            sources.push(src.to_string());
        }
    }
    match (fw.policy_in == "deny", sources.is_empty()) {
        (false, _) => PublishReach::Open,
        (true, true) => PublishReach::Blocked,
        (true, false) => PublishReach::Sources(sources),
    }
}

fn or_any(s: &str) -> String {
    if s.is_empty() || s == "*" {
        "any".to_string()
    } else {
        s.to_string()
    }
}

/// The containers some policy document in this manifest governs: the `target` of
/// a `NetworkPolicy` (a `Dependency` has already been lowered to one) or a
/// `NetworkAccessRule`, in the default container scope. These are the workloads
/// that must not carry traffic before their policy is in place (ADR-0069).
pub(crate) fn policy_targets(docs: &[ManifestDoc]) -> std::collections::BTreeSet<&str> {
    let mut out = std::collections::BTreeSet::new();
    for doc in docs {
        match doc.kind.as_str() {
            k::FIREWALL_POLICY => {
                if let Ok(spec) = manifest::spec_of::<FwDocSpec>(doc) {
                    if spec.scope.as_deref().unwrap_or("container") == "container" {
                        if let Some(t) = doc.spec.get("target").and_then(|v| v.as_str()) {
                            out.insert(t);
                        }
                    }
                }
            }
            k::NETWORK_ACCESS_RULE => {
                if let Some(t) = doc.spec.get("target").and_then(|v| v.as_str()) {
                    out.insert(t);
                }
            }
            _ => {}
        }
    }
    out
}

/// The VMs (`scope: vm`) or system containers (`scope: systemcontainer`) some
/// `NetworkPolicy` of this manifest governs. Their firewall is the provider's,
/// so they are created closed there, not in the holder (ADR-0069 D6).
pub(crate) fn guest_policy_targets<'a>(
    docs: &'a [ManifestDoc],
    scope: &str,
) -> std::collections::BTreeSet<&'a str> {
    manifest::of_kind(docs, k::FIREWALL_POLICY)
        .into_iter()
        .filter(|d| d.spec.get("scope").and_then(|v| v.as_str()) == Some(scope))
        .filter_map(|d| d.spec.get("target").and_then(|v| v.as_str()))
        .collect()
}

/// The directions of `target` that a `scope` policy of this manifest declares.
fn guest_declared_directions(
    docs: &[ManifestDoc],
    target: &str,
    scope: &str,
) -> std::collections::BTreeSet<&'static str> {
    let mut out = std::collections::BTreeSet::new();
    for doc in manifest::of_kind(docs, k::FIREWALL_POLICY) {
        if doc.spec.get("target").and_then(|v| v.as_str()) != Some(target)
            || doc.spec.get("scope").and_then(|v| v.as_str()) != Some(scope)
        {
            continue;
        }
        match doc.spec.get("direction").and_then(|v| v.as_str()) {
            Some("ingress") => {
                out.insert("in");
            }
            Some("egress") => {
                out.insert("out");
            }
            _ => {}
        }
    }
    out
}

/// The directions (`in`/`out`) the manifest sets a default policy for on `target`.
fn declared_directions(
    docs: &[ManifestDoc],
    target: &str,
) -> std::collections::BTreeSet<&'static str> {
    let mut out = std::collections::BTreeSet::new();
    for doc in manifest::of_kind(docs, k::FIREWALL_POLICY) {
        if doc.spec.get("target").and_then(|v| v.as_str()) != Some(target) {
            continue;
        }
        if doc
            .spec
            .get("scope")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s != "container")
        {
            continue;
        }
        match doc.spec.get("direction").and_then(|v| v.as_str()) {
            Some("ingress") => {
                out.insert("in");
            }
            Some("egress") => {
                out.insert("out");
            }
            _ => {}
        }
    }
    out
}

/// What a held container's policies become when the held state ends: a
/// direction the manifest declared keeps what its policy document set; one it
/// did not declare returns to the unset default (open), which is exactly what the
/// container would have had without the hold.
fn released_policies(
    fw: &delonix_model::records::ContainerFw,
    declared: &std::collections::BTreeSet<&'static str>,
) -> delonix_model::records::ContainerFw {
    let mut out = fw.clone();
    if !declared.contains("in") {
        out.policy_in.clear();
    }
    if !declared.contains("out") {
        out.policy_out.clear();
    }
    out
}

/// Names of the governed containers that are still closed. `None` when the
/// store cannot be read (the caller then says nothing rather than guessing).
pub(crate) fn held_targets(docs: &[ManifestDoc]) -> Option<Vec<String>> {
    let (_, store) = super::util::open_stores().ok()?;
    let mut out: Vec<String> = policy_targets(docs)
        .into_iter()
        .filter(|t| {
            load_governed(&store, t).is_ok_and(|c| {
                c.annotations
                    .contains_key(super::container::POLICY_HOLD_ANNOTATION)
            })
        })
        .map(str::to_string)
        .collect();
    if let Ok(vms) =
        delonix_state::JsonStore::<delonix_compute::Vm>::open(super::util::state_root().join("vms"))
    {
        out.extend(
            guest_policy_targets(docs, "vm")
                .into_iter()
                .filter(|t| {
                    vms.get(t).is_ok_and(|v| {
                        v.annotations
                            .contains_key(delonix_compute::vm::POLICY_HOLD_ANNOTATION)
                    })
                })
                .map(str::to_string),
        );
    }
    out.extend(
        guest_policy_targets(docs, "systemcontainer")
            .into_iter()
            .filter(|t| super::system_container::is_held(t))
            .map(str::to_string),
    );
    Some(out)
}

/// Ends the hold on every container this manifest governs, after the policy
/// layers ran. Idempotent: a container without the hold annotation is skipped,
/// so a second apply is a no-op, and a first apply that failed before reaching
/// this leaves the containers closed for the next one to release.
pub(crate) fn release_policy_holds(docs: &[ManifestDoc]) -> Result<usize> {
    let (_, store) = super::util::open_stores()?;
    let mut released = 0;
    for target in policy_targets(docs) {
        let Ok(c) = load_governed(&store, target) else {
            continue;
        };
        if !c
            .annotations
            .contains_key(super::container::POLICY_HOLD_ANNOTATION)
        {
            continue;
        }
        let declared = declared_directions(docs, target);
        update_locked(&store, target, |c| {
            // The annotation goes first: while it is there the dataplane is held
            // closed whatever is applied.
            c.annotations
                .remove(super::container::POLICY_HOLD_ANNOTATION);
            let fw = released_policies(&super::container::firewall_or_new(c), &declared);
            let empty = firewall_disposable(c, &fw);
            if let Some(ip) = c.ip.clone().filter(|s| !s.is_empty()) {
                if empty {
                    infra::clear_firewall(&ip);
                } else {
                    super::container::apply_firewall_everywhere(c, &fw)?;
                }
            }
            c.firewall = if empty { None } else { Some(fw) };
            Ok(true)
        })?;
        released += 1;
    }
    Ok(released)
}

/// Ends the hold on every VM this manifest governs with a `scope: vm` policy,
/// after the policy layers ran. A direction a document declared keeps what that
/// document wrote on the node's firewall; a direction none declared goes back to
/// open (the VM's own default, as without the hold). The annotation goes LAST: if
/// opening a direction fails, the VM stays marked closed and the next apply tries
/// again — never marked open while the node still drops.
pub(crate) fn release_vm_holds(docs: &[ManifestDoc]) -> Result<usize> {
    use delonix_vm::firewall::{Direction, Policy};
    let root = super::util::state_root();
    let store: delonix_state::JsonStore<delonix_compute::Vm> =
        delonix_state::JsonStore::open(root.join("vms"))?;
    let mut released = 0;
    for target in guest_policy_targets(docs, "vm") {
        let Ok(vm) = store.get(target) else { continue };
        if !vm
            .annotations
            .contains_key(delonix_compute::vm::POLICY_HOLD_ANNOTATION)
        {
            continue;
        }
        let declared = guest_declared_directions(docs, target, "vm");
        for (word, direction) in [("in", Direction::In), ("out", Direction::Out)] {
            if declared.contains(word) {
                continue;
            }
            delonix_vm::apply_firewall(
                &root,
                target,
                &Policy {
                    direction,
                    default_allow: true,
                    rules: Vec::new(),
                },
            )?;
        }
        store.update(target, |v| {
            v.annotations
                .remove(delonix_compute::vm::POLICY_HOLD_ANNOTATION);
            true
        })?;
        released += 1;
    }
    Ok(released)
}

/// [`release_vm_holds`] for `scope: systemcontainer` policies.
pub(crate) fn release_system_container_holds(docs: &[ManifestDoc]) -> Result<usize> {
    let mut released = 0;
    for target in guest_policy_targets(docs, "systemcontainer") {
        let declared = guest_declared_directions(docs, target, "systemcontainer");
        if super::system_container::release_hold(target, &declared)? {
            released += 1;
        }
    }
    Ok(released)
}

pub(crate) fn clear_dir(store: &Store, name: &str, dir: &str) -> Result<()> {
    clear_dir_with(store, name, dir, false)
}

/// [`clear_dir`], and `reset_policy` also puts that direction's default back to
/// allow-all.
///
/// The two verbs promise different things, and each now does what it says:
///
/// * `net ingress|egress clear <target>` removes the RULES of one direction and
///   says «removed N rule(s)». The default is the `policy` verb's to set, and
///   that split is coherent.
/// * `delete networkpolicies <target>/<direction>` is the Kind-generic DELETE,
///   and its own doc already said the direction «resets to allow-all». It did
///   not. Measured 2026-10-06 against the shipped engine: after
///   `delete networkpolicies <c>/ingress` with a `deny` in force, the command
///   answered rc 0 and «removed 0 inbound rule(s)», `get networkpolicies` still
///   listed the policy, `describe` still answered it, and a ping from another
///   container was STILL blocked. A delete that reports success and deletes
///   nothing leaves the operator with a workload closed by a policy they just
///   removed, and every read agreeing with the policy instead of with them.
pub(crate) fn clear_dir_with(
    store: &Store,
    name: &str,
    dir: &str,
    reset_policy: bool,
) -> Result<()> {
    let mut removed = 0usize;
    let mut nothing_to_clear = false;
    let mut policy_was = String::new();
    let c = update_locked(store, name, |c| {
        let mut fw = match c.firewall.clone() {
            Some(f) => f,
            None => {
                nothing_to_clear = true;
                return Ok(false);
            }
        };
        let before = fw.rules.len();
        fw.rules.retain(|r| r.dir != dir);
        removed = before - fw.rules.len();
        if reset_policy {
            let slot = if dir == "in" {
                &mut fw.policy_in
            } else {
                &mut fw.policy_out
            };
            policy_was = std::mem::take(slot);
        }
        // If nothing is left (no rules, both policies default, the open `default`
        // namespace), drop the firewall entirely and detach it from the ingress;
        // otherwise re-apply what remains — the namespace isolation included.
        let empty = firewall_disposable(c, &fw);
        if let Some(ip) = c.ip.clone().filter(|s| !s.is_empty()) {
            if empty {
                infra::clear_firewall(&ip);
            } else {
                super::container::apply_firewall_everywhere(c, &fw)?;
            }
        }
        c.firewall = if empty { None } else { Some(fw) };
        Ok(true)
    })?;
    if nothing_to_clear {
        println!("{}: no firewall to clear", c.name);
        return Ok(());
    }
    let arrow = if dir == "in" { "inbound" } else { "outbound" };
    if reset_policy && !policy_was.is_empty() {
        println!(
            "{}: removed {removed} {arrow} rule(s), and the {arrow} default is back to allow-all \
             (was {policy_was})",
            c.name
        );
    } else {
        println!("{}: removed {removed} {arrow} rule(s)", c.name);
    }
    Ok(())
}

fn egress_net(network: &str, mode: EgressMode, to: Option<String>) -> Result<()> {
    // The REAL bridge lives in the infra registry (NetDef, `dlxn{:08x}`), NOT in
    // the NetworkStore (`dlxn{:02x}{:04x}`) — using the wrong one makes the nft
    // rules never match traffic. resolve_net returns the bridge the holder created.
    let bridge = infra::resolve_net(network)?.bridge;
    match mode {
        EgressMode::Allow => {
            infra::set_egress_policy_net(&bridge, false)?;
            println!("network {network}: egress to the Internet ALLOWED");
        }
        EgressMode::Deny => {
            infra::set_egress_policy_net(&bridge, true)?;
            println!("network {network}: egress to the Internet DENIED");
        }
        EgressMode::Allowlist => {
            let raw =
                to.ok_or_else(|| Error::Invalid("allowlist mode needs `--to <cidr,...>`".into()))?;
            let cidrs: Vec<&str> = raw
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            for c in &cidrs {
                check_cidr(c)?;
            }
            infra::set_egress_policy_net_allowlist(&bridge, &cidrs)?;
            println!(
                "network {network}: egress DENIED except DNS + {}",
                cidrs.join(", ")
            );
        }
    }
    Ok(())
}

// ---- declarative: `kind: Ingress` / `kind: Egress` ---------------------------

/// A `kind: Ingress`/`Egress` document. Each doc is the DESIRED STATE of one
/// direction (inbound for `Ingress`, outbound for `Egress`) for its `target`
/// container — applying it REPLACES that direction's rules and policy, leaving
/// the other direction untouched, so an `Ingress` and an `Egress` doc compose
/// on the same container. Allowlist by default (`defaultPolicy: deny`), like a
/// k8s NetworkPolicy.
#[derive(Deserialize, Serialize, schemars::JsonSchema)]
pub(crate) struct FwDocSpec {
    /// `ingress`|`egress` — only for `kind: NetworkPolicy` (the direction comes
    /// from the Kind for the legacy `Egress`). Captured so the dry-run round-trip
    /// preserves it; `apply` reads it directly from `doc.spec`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    direction: Option<String>,
    /// `container` (default), `network`, `vm` or `systemcontainer`. In `network`
    /// (only `Egress`), the `target` is a NETWORK NAME and the per-network egress
    /// policy + CIDR/FQDN allowlist + L4 rate-limit apply — not per-container L4
    /// rules. In `vm`, the `target` is a VM NAME and the rules land on the
    /// firewall of the node the VM runs on — today a Proxmox node; any other
    /// backend refuses (ADR-0052). In `systemcontainer`, the `target` is a
    /// `SystemContainer` and the rules land on its own firewall on the node, the
    /// same way (ADR-0058).
    #[serde(default)]
    scope: Option<String>,
    /// `container` (default): container name. `network`: network name. `vm`: VM name.
    target: String,
    /// `allow` or `deny` when no rule matches. Default `deny` (allowlist).
    #[serde(default, rename = "defaultPolicy")]
    default_policy: Option<String>,
    #[serde(default)]
    rules: Vec<FwDocRule>,
    // ---- only `scope: network` (per-network Egress) ---------------------------
    /// CIDRs allowed when `defaultPolicy: deny` (egress allowlist, besides
    /// DNS). Translates to `set_egress_policy_net_allowlist`.
    #[serde(default, rename = "allowCidrs")]
    allow_cidrs: Vec<String>,
    /// FQDNs allowed (and `*.fqdn`), learnt LIVE from DNS (DNS-snooping).
    /// Translates to `set_egress_host` per host.
    #[serde(default, rename = "fqdnAllowlist")]
    fqdn_allowlist: Vec<String>,
    /// L4 protection (conn-rate/conn-max) — **GLOBAL** to the rootless ingress, not
    /// per-network (the engine API `set_l4_guard` is global). Translates to `set_l4_guard`.
    #[serde(default, rename = "rateLimit")]
    rate_limit: Option<RateLimitSpec>,
}

/// `spec.rateLimit` — the ingress L4 DDoS protection (global). `{connRate: 0,
/// connMax: 0}` explicitly TURNS OFF the guard (clear_l4_guard).
#[derive(Deserialize, Serialize, schemars::JsonSchema)]
struct RateLimitSpec {
    /// New connections per second allowed.
    #[serde(default, rename = "connRate")]
    conn_rate: u32,
    /// Maximum concurrent connections.
    #[serde(default, rename = "connMax")]
    conn_max: u32,
}

/// Names accepted in the `spec` of `kind: Ingress`/`Egress`, for the unknown-field
/// warning (the `rules[]` is validated by `FwDocRule`'s deserialization).
pub(crate) const FW_SPEC_FIELDS: &[&str] = &[
    "direction",
    "scope",
    "target",
    "defaultPolicy",
    "rules",
    "allowCidrs",
    "fqdnAllowlist",
    "rateLimit",
];

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
struct FwDocRule {
    /// `tcp`/`udp`/`any` (default `any`).
    #[serde(default)]
    proto: Option<String>,
    /// Port, range `n-m`, or `*`.
    port: String,
    /// Source CIDR (ingress) — the other end of inbound traffic.
    #[serde(default)]
    from: Option<String>,
    /// Destination CIDR (egress) — the other end of outbound traffic.
    #[serde(default)]
    to: Option<String>,
    /// Source **by workload name** (ingress), resolved to that container's SDN
    /// address at apply time.
    ///
    /// Exists because **a container's IP is not stable**: it comes from the SDN
    /// and changes on a restart. A policy written as a CIDR is therefore a policy
    /// that silently stops matching the workload it was written for — the same
    /// lesson `vm bridge` already paid for, where the follow-up recorded is
    /// exactly "discovery by NAME, so as not to depend on dynamic IPs".
    ///
    /// A NAME and not a permissive `from` that also accepts names: a value that
    /// fails to parse as a CIDR falling back to "treat it as a name" would be a
    /// silent reinterpretation on a security field, which is the one place this
    /// engine refuses to guess.
    #[serde(default, rename = "fromWorkload")]
    from_workload: Option<String>,
    /// Destination by workload name (egress). Same reasoning as
    /// [`FwDocRule::from_workload`].
    #[serde(default, rename = "toWorkload")]
    to_workload: Option<String>,
    /// `allow` (default) or `deny`.
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

/// Applies every `Ingress` and `Egress` document in the manifest. Called last in
/// `stack apply` (the target containers must already exist).
/// Dry-run: the firewall spec (`Egress`/`FirewallPolicy`) with defaults materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: FwDocSpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let (_images, store) = open_stores()?;
    // `kind: NetworkPolicy` is now the ONLY firewall Kind. `kind: Ingress` stopped
    // being firewall a while back (it is the k8s-shaped L7 Ingress, see
    // `cmd::httproute`), and `kind: Egress` is rewritten into a FirewallPolicy with
    // `direction: egress` at load time (`manifest::lower_egress`) — so by the time
    // anything reaches here there is one Kind, one struct and one direction field.
    for doc in manifest::of_kind(docs, k::FIREWALL_POLICY) {
        let dir = match doc.spec.get("direction").and_then(|v| v.as_str()) {
            Some("ingress") => "in",
            Some("egress") => "out",
            other => {
                return Err(Error::Invalid(super::po::tf(
                    "FirewallPolicy/{name}: direction is required and ∈ {{ingress, egress}} (got {other})",
                    &[
                        ("name", &doc.metadata.name),
                        ("other", &format!("{other:?}")),
                    ],
                )));
            }
        };
        apply_fw_doc(&store, doc, dir)?;
    }
    Ok(())
}

/// Fields the reconciler compares for a `kind: NetworkPolicy`.
///
/// `defaultPolicy` and `rules` converge HOT — `apply_fw_doc` already replaces
/// the whole direction, in place, with no container restart. `target` and
/// `direction` do not: they IDENTIFY which direction of which container this
/// policy governs, so changing one leaves the old target's rules exactly where
/// they were. That is a `Replace`, and the recreation clears the direction on
/// the old target — which is the only way the change means what it reads like.
pub(crate) const RECONCILED_FW_FIELDS: &[&str] =
    &["target", "direction", "defaultPolicy", "rules", "scope"];

/// One rule, rendered as one comparable string.
///
/// Sorted by the caller, never here: the ORDER of allow rules in a policy does
/// not change what it permits (the chain is built from the whole set), so a
/// reordered manifest must not read as a change.
fn rule_key(r: &FwDocRule) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        r.action.as_deref().unwrap_or("allow"),
        r.proto.as_deref().unwrap_or("any"),
        r.port,
        r.from.as_deref().or(r.to.as_deref()).unwrap_or(""),
        r.from_workload
            .as_deref()
            .or(r.to_workload.as_deref())
            .unwrap_or(""),
    )
}

/// The same rendering, from a persisted [`FwRule`].
///
/// A persisted rule holds the RESOLVED address, never the workload name it may
/// have come from — so a policy written with `fromWorkload` compares its
/// resolved `/32` against the record's. That is why the desired side resolves
/// too (below): comparing a name against an address would report drift on every
/// plan for every rule that names a peer.
fn stored_rule_key(r: &delonix_model::records::FwRule) -> String {
    format!("{}|{}|{}|{}|", r.action, r.proto, r.port, r.src)
}

/// What the manifest declares, for the reconciler.
pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec: FwDocSpec = manifest::spec_of(doc)?;
    let (_images, store) = open_stores()?;
    let mut f = std::collections::BTreeMap::new();
    f.insert("target".into(), spec.target.clone());
    f.insert(
        "direction".into(),
        spec.direction.clone().unwrap_or_default(),
    );
    f.insert(
        "scope".into(),
        spec.scope.clone().unwrap_or_else(|| "container".into()),
    );
    f.insert(
        "defaultPolicy".into(),
        spec.default_policy.clone().unwrap_or_else(|| "deny".into()),
    );
    // scope: vm / systemcontainer — the node keeps the rules IN ORDER and
    // the first match wins, so order IS meaning here (unlike the container
    // chain, built from the set): the keys are not sorted.
    if is_guest_scope(spec.scope.as_deref()) {
        let dir = if spec.direction.as_deref() == Some("egress") {
            "out"
        } else {
            "in"
        };
        let policy = vm_policy(&doc.kind, &doc.metadata.name, &spec, dir)?;
        f.insert(
            "rules".into(),
            policy
                .rules
                .iter()
                .map(|r| r.key())
                .collect::<Vec<_>>()
                .join(","),
        );
        return Ok(super::reconcile::Desired {
            kind: k::FIREWALL_POLICY.into(),
            name: doc.metadata.name.clone(),
            fields: f,
            converges: true,
            ownable: false,
        });
    }
    let mut keys: Vec<String> = Vec::new();
    for r in &spec.rules {
        // Resolve a workload name to its address, exactly as the apply will —
        // the record stores addresses. A workload that does not exist yet
        // resolves to nothing and the rule compares by name; the policy cannot
        // have been applied yet either, so there is nothing to be wrong about.
        let by_name = r.from_workload.clone().or_else(|| r.to_workload.clone());
        let resolved = by_name
            .as_deref()
            .and_then(|w| workload_cidr(&store, w).ok());
        match resolved {
            Some(cidr) => keys.push(format!(
                "{}|{}|{}|{}|",
                r.action.as_deref().unwrap_or("allow"),
                r.proto.as_deref().unwrap_or("any"),
                r.port,
                cidr
            )),
            None => keys.push(rule_key(r)),
        }
    }
    keys.sort();
    f.insert("rules".into(), keys.join(","));
    Ok(super::reconcile::Desired {
        kind: k::FIREWALL_POLICY.into(),
        name: doc.metadata.name.clone(),
        fields: f,
        converges: true,
        // NOT prunable, and for a different reason than an `Image`: a policy has
        // no record of its own — it lives on the target container's
        // `ContainerFw`. Once it leaves the manifest, nothing on disk says which
        // target and direction it governed, so there is nothing a prune could
        // safely clear. This matches what the firewall docs already promise
        // («removing the Dependency does NOT unprotect the `to`»); what changes
        // is that the plan no longer implies otherwise.
        ownable: false,
    })
}

/// What is on the machine, for the reconciler.
///
/// A firewall policy has no record of its own — it lives on the TARGET
/// container's `ContainerFw`. So the actual side is keyed by the document name
/// (what the plan matches on) and read from the target named by that document.
pub(crate) fn actual(docs: &[ManifestDoc]) -> Result<Vec<super::reconcile::Actual>> {
    let (_images, store) = open_stores()?;
    let mut out = Vec::new();
    for doc in manifest::of_kind(docs, k::FIREWALL_POLICY) {
        let Ok(spec) = manifest::spec_of::<FwDocSpec>(doc) else {
            continue;
        };
        if is_guest_scope(spec.scope.as_deref()) {
            if let Some(a) = actual_vm(doc, &spec)? {
                out.push(a);
            }
            continue;
        }
        let Ok(c) = load_governed(&store, &spec.target) else {
            continue; // target not created yet — the plan will say Create
        };
        let Some(fw) = &c.firewall else { continue };
        let dir = match spec.direction.as_deref() {
            Some("ingress") => "in",
            Some("egress") => "out",
            _ => continue,
        };
        let mut f = std::collections::BTreeMap::new();
        f.insert("target".into(), spec.target.clone());
        f.insert(
            "direction".into(),
            spec.direction.clone().unwrap_or_default(),
        );
        f.insert(
            "scope".into(),
            spec.scope.clone().unwrap_or_else(|| "container".into()),
        );
        let policy = if dir == "in" {
            &fw.policy_in
        } else {
            &fw.policy_out
        };
        f.insert(
            "defaultPolicy".into(),
            if policy.is_empty() {
                "deny".into()
            } else {
                policy.clone()
            },
        );
        let mut keys: Vec<String> = fw
            .rules
            .iter()
            .filter(|r| r.dir == dir)
            .map(stored_rule_key)
            .collect();
        keys.sort();
        f.insert("rules".into(), keys.join(","));
        out.push(super::reconcile::Actual {
            kind: k::FIREWALL_POLICY.into(),
            name: doc.metadata.name.clone(),
            fields: f,
            owner: c.labels.get(super::reconcile::STACK_LABEL).cloned(),
            last_applied: c
                .annotations
                .get(&format!("{}/{}", super::reconcile::LAST_APPLIED, dir))
                .and_then(|raw| super::reconcile::decode_last_applied(raw)),
        });
    }
    Ok(out)
}

/// The `scope: vm` side of [`actual`]: read from the VM's node, through the
/// backend. A VM not created yet is `None` (the plan says Create); any other
/// failure — the node unreachable, a backend with no VM firewall — is an
/// error, never an empty policy that would read as "nothing applied yet".
/// `vm` and `systemcontainer` put the policy on the provider's own firewall
/// for that guest, with the same rules and the same ordered comparison.
fn is_guest_scope(scope: Option<&str>) -> bool {
    matches!(scope, Some("vm") | Some("systemcontainer"))
}

fn actual_vm(doc: &ManifestDoc, spec: &FwDocSpec) -> Result<Option<super::reconcile::Actual>> {
    use delonix_vm::firewall::Direction;
    let direction = match spec.direction.as_deref() {
        Some("ingress") => Direction::In,
        Some("egress") => Direction::Out,
        _ => return Ok(None),
    };
    let scope = spec.scope.clone().unwrap_or_default();
    let policy = if scope == "systemcontainer" {
        match super::system_container::read_firewall(&spec.target, direction)? {
            Some(p) => p,
            None => return Ok(None),
        }
    } else {
        let root = super::util::state_root();
        if delonix_vm::list(&root)?
            .iter()
            .all(|v| v.name != spec.target)
        {
            return Ok(None);
        }
        delonix_vm::read_firewall(&root, &spec.target, direction)?
    };
    let mut f = std::collections::BTreeMap::new();
    f.insert("target".into(), spec.target.clone());
    f.insert(
        "direction".into(),
        spec.direction.clone().unwrap_or_default(),
    );
    f.insert("scope".into(), scope);
    f.insert(
        "defaultPolicy".into(),
        if policy.default_allow {
            "allow"
        } else {
            "deny"
        }
        .into(),
    );
    f.insert(
        "rules".into(),
        policy
            .rules
            .iter()
            .map(|r| r.key())
            .collect::<Vec<_>>()
            .join(","),
    );
    Ok(Some(super::reconcile::Actual {
        kind: k::FIREWALL_POLICY.into(),
        name: doc.metadata.name.clone(),
        fields: f,
        owner: None,
        last_applied: None,
    }))
}

/// Converges a policy: re-apply the document. `apply_fw_doc` already replaces
/// the whole direction, so «converging» is exactly «applying» — there is no
/// partial path to write, and writing one would be a second way to build the
/// same chain.
pub(crate) fn converge_doc(doc: &ManifestDoc) -> Result<()> {
    let (_images, store) = open_stores()?;
    let dir = match doc.spec.get("direction").and_then(|v| v.as_str()) {
        Some("ingress") => "in",
        Some("egress") => "out",
        _ => return Ok(()),
    };
    apply_fw_doc(&store, doc, dir)
}

/// Resolves a workload name to the `/32` of its SDN address.
///
/// Fails LOUDLY when the container has no address, and says why: a workload on
/// `--net host`/`none` has no address on the SDN for a rule to name, and
/// producing an empty source instead would silently widen the rule to
/// `0.0.0.0/0` — turning "allow this one peer" into "allow everyone", which is
/// the worst possible way for a firewall to fail.
///
/// Resolves through `Store::load`, and that is the whole point of this line.
/// The scan it replaced was `list().find(|c| c.name == name)` — first match
/// wins — while names stopped being globally unique in ADR-0011 §3, precisely
/// so two tenants can both own a `db`. `Store::load`'s own doc-comment already
/// names this failure ("an `apply` touching `db` would silently pick a
/// tenant"); the target of a policy was moved over to it and the OTHER END of
/// a rule was left behind. Measured on `origin/main` before the fix: a
/// `FirewallPolicy` in namespace `teamA` with `fromWorkload: db` resolved to
/// **teamB's** address and wrote an `allow` for it. That is a firewall failing
/// OPEN, across a tenant boundary, without a word — the qualified
/// `<namespace>/<name>` form is the way to say which one you meant.
fn workload_cidr(store: &Store, name: &str) -> Result<String> {
    let c = load_governed(store, name).map_err(|e| match e.into_root() {
        // The store says "no such container"; here the useful sentence names
        // the ROLE the missing thing was playing, so keep it.
        Error::NotFound(_) => Error::Invalid(super::po::tf(
            "workload '{name}' does not exist (a rule names it as the other end)",
            &[("name", name)],
        )),
        // The ambiguity refusal is passed through verbatim: it already lists
        // the candidates, and rewording it here would give the same condition
        // two voices depending on which path reached it.
        other => other,
    })?;
    let ip = c.ip.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| {
        Error::Invalid(super::po::tf(
            "workload '{name}' has no address on the SDN (is it on a custom network?) — \
             a rule cannot name it",
            &[("name", name)],
        ))
    })?;
    Ok(format!("{ip}/32"))
}

/// Applies ONE firewall document (Ingress/Egress/FirewallPolicy) in the `dir`
/// direction ("in"/"out"). The label in messages uses the document's real Kind.
fn apply_fw_doc(store: &Store, doc: &ManifestDoc, dir: &str) -> Result<()> {
    let kind = doc.kind.as_str();
    let spec: FwDocSpec = manifest::spec_of(doc)?;

    // Validate the scope explicitly — a typo (`netowrk`) must not fall silently
    // into the container path and fail later with 'container does not exist'.
    let scope = spec.scope.as_deref().unwrap_or("container");
    if !matches!(scope, "container" | "network" | "vm" | "systemcontainer") {
        return Err(Error::Invalid(super::po::tf(
            "{kind}/{name}: invalid scope '{scope}' (use container|network|vm|systemcontainer)",
            &[
                ("kind", kind),
                ("name", &doc.metadata.name),
                ("scope", scope),
            ],
        )));
    }

    // scope: network — PER-NETWORK egress policy (Egress only). The `target`
    // is a network name; wires up the engine APIs that only had a CLI.
    if scope == "network" {
        if dir != "out" {
            return Err(Error::Invalid(super::po::tf(
                "{kind}/{name}: scope: network is only supported in Egress (there is no per-network INGRESS policy)",
                &[("kind", kind), ("name", &doc.metadata.name)],
            )));
        }
        return apply_network_egress(kind, &doc.metadata.name, &spec);
    }

    // scope: vm — the node's OWN firewall for that VM (ADR-0052).
    if scope == "vm" {
        let policy = vm_policy(kind, &doc.metadata.name, &spec, dir)?;
        delonix_vm::apply_firewall(&super::util::state_root(), &spec.target, &policy)?;
        println!(
            "{kind}/{}: applied to VM {} on its node's firewall ({} rule(s), default {})",
            doc.metadata.name,
            spec.target,
            policy.rules.len(),
            if policy.default_allow {
                "allow"
            } else {
                "deny"
            }
        );
        return Ok(());
    }

    // scope: systemcontainer — the provider's OWN firewall for that system
    // container (ADR-0058, plan 63 slice 5): the same policy, the same node
    // switches, under the container's firewall routes.
    if scope == "systemcontainer" {
        let policy = vm_policy(kind, &doc.metadata.name, &spec, dir)?;
        super::system_container::apply_firewall(&spec.target, &policy)?;
        println!(
            "{kind}/{}: applied to system container {} on its node's firewall ({} rule(s), default {})",
            doc.metadata.name,
            spec.target,
            policy.rules.len(),
            if policy.default_allow {
                "allow"
            } else {
                "deny"
            }
        );
        return Ok(());
    }

    // Pure spec validation first (no container/lock involved) — fail fast on
    // a bad manifest before ever touching the store.
    let policy = spec.default_policy.as_deref().unwrap_or("deny");
    if !matches!(policy, "allow" | "deny") {
        return Err(Error::Invalid(format!(
            "{kind}/{}: defaultPolicy must be allow|deny",
            doc.metadata.name
        )));
    }
    let mut new_rules = Vec::new();
    for r in &spec.rules {
        let proto = r.proto.clone().unwrap_or_else(|| "any".into());
        if !fw_proto_ok(&proto) {
            return Err(Error::Invalid(format!(
                "{kind}/{}: invalid proto '{proto}'",
                doc.metadata.name
            )));
        }
        if !fw_port_ok(&r.port) {
            return Err(Error::Invalid(format!(
                "{kind}/{}: invalid port '{}'",
                doc.metadata.name, r.port
            )));
        }
        // A rule names the other end EITHER by address or by workload, never
        // both — two answers to "who is the other end" is a contradiction, and
        // on a firewall a contradiction is not something to resolve by
        // precedence.
        let by_name = r.from_workload.clone().or_else(|| r.to_workload.clone());
        if by_name.is_some() && (r.from.is_some() || r.to.is_some()) {
            return Err(Error::Invalid(super::po::tf(
                "{kind}/{name}: a rule uses both an address (from/to) and a workload \
                 (fromWorkload/toWorkload) — pick one",
                &[("kind", kind), ("name", &doc.metadata.name)],
            )));
        }
        let src = match &by_name {
            Some(w) => workload_cidr(store, w)
                .map_err(|e| Error::Invalid(format!("{kind}/{}: {e}", doc.metadata.name)))?,
            None => {
                let s = r.from.clone().or_else(|| r.to.clone()).unwrap_or_default();
                if let Err(e) = check_cidr(&s) {
                    return Err(Error::Invalid(format!("{kind}/{}: {e}", doc.metadata.name)));
                }
                s
            }
        };
        let action = r.action.clone().unwrap_or_else(|| "allow".into());
        if !matches!(action.as_str(), "allow" | "deny") {
            return Err(Error::Invalid(format!(
                "{kind}/{}: action must be allow|deny",
                doc.metadata.name
            )));
        }
        new_rules.push(FwRule {
            dir: dir.to_string(),
            proto,
            port: r.port.clone(),
            src,
            action,
            note: r.note.clone().unwrap_or_default(),
            // `kind: FirewallPolicy` — never owned by a `NetworkAccessRule`
            // document (see `apply_fw_doc`'s origin-aware `retain`, above).
            origin: None,
        });
    }
    let mut n = 0usize;
    update_locked(store, &spec.target, |c| {
        // Guard only: rejects a container off the SDN. The addresses the firewall is
        // keyed on come from `container_ips` (primary + every additional network).
        require_sdn_ip(c)?;
        let mut fw = super::container::firewall_or_new(c);
        fw.enabled = true;
        // Declarative: this direction is fully replaced by the document —
        // but only the UNOWNED rules of it, and rules an imperative `net
        // ingress`/`net egress allow`/`deny` added are NOT exempt (see
        // `is_manifest_origin`: they carry an `origin` now too, since
        // ADR-0029, but not one naming an independently-lifecycled document).
        // A rule whose origin DOES name a `kind: NetworkAccessRule` document
        // survives — wiping it here would be the exact "two policies, one
        // silently erases the other while both report success" bug this
        // module already refuses at the manifest level for two FirewallPolicy
        // documents (see `stack.rs`'s duplicate (target,direction) check) —
        // except this time between two DIFFERENT Kinds, which that check
        // cannot see.
        fw.rules
            .retain(|r| r.dir != dir || r.origin.as_deref().is_some_and(is_manifest_origin));
        if dir == "in" {
            fw.policy_in = policy.to_string();
        } else {
            fw.policy_out = policy.to_string();
        }
        fw.rules.extend(new_rules.iter().cloned());
        super::container::apply_firewall_everywhere(c, &fw)?;
        n = fw.rules.iter().filter(|r| r.dir == dir).count();
        c.firewall = Some(fw);
        Ok(true)
    })?;
    println!(
        "{kind}/{}: applied to {} ({n} rule(s), default {policy})",
        doc.metadata.name, spec.target
    );
    Ok(())
}

/// A `scope: vm` document as the engine's VM firewall policy (ADR-0052), built
/// as policy IR and lowered by `Policy::from_ir` (ADR-0059 F3c). Pure:
/// validates everything before anything is sent.
///
/// `fromWorkload`/`toWorkload` are REFUSED here, not resolved: they resolve to
/// an address on this engine's SDN, and a VM filtered by its node's firewall
/// does not sit on that SDN — the name would become an address the VM never
/// sees traffic from, a rule that matches nothing while reading as a peer.
/// The `scope: network` fields are refused for the same reason they are for a
/// container: they mean something else.
fn vm_policy(
    kind: &str,
    name: &str,
    spec: &FwDocSpec,
    dir: &str,
) -> Result<delonix_vm::firewall::Policy> {
    use delonix_net_rules::policy as ir;
    use delonix_vm::firewall::{Policy, Proto};
    if !spec.allow_cidrs.is_empty() || !spec.fqdn_allowlist.is_empty() || spec.rate_limit.is_some()
    {
        return Err(Error::Invalid(super::po::tf(
            "{kind}/{name}: allowCidrs/fqdnAllowlist/rateLimit are only for scope: network",
            &[("kind", kind), ("name", name)],
        )));
    }
    let default = spec.default_policy.as_deref().unwrap_or("deny");
    if !matches!(default, "allow" | "deny") {
        return Err(Error::Invalid(format!(
            "{kind}/{name}: defaultPolicy must be allow|deny"
        )));
    }
    let mut rules = Vec::new();
    for r in &spec.rules {
        if r.from_workload.is_some() || r.to_workload.is_some() {
            return Err(Error::Invalid(super::po::tf(
                "{kind}/{name}: fromWorkload/toWorkload name an address on this engine's SDN, \
                 which a VM filtered by its node's firewall is not on — use from/to with a CIDR",
                &[("kind", kind), ("name", name)],
            )));
        }
        let proto_s = r.proto.as_deref().unwrap_or("any");
        let proto = Proto::parse(proto_s)
            .ok_or_else(|| Error::Invalid(format!("{kind}/{name}: invalid proto '{proto_s}'")))?;
        if !fw_port_ok(&r.port) {
            return Err(Error::Invalid(format!(
                "{kind}/{name}: invalid port '{}'",
                r.port
            )));
        }
        let peer = r.from.clone().or_else(|| r.to.clone()).unwrap_or_default();
        if let Err(e) = check_cidr(&peer) {
            return Err(Error::Invalid(format!("{kind}/{name}: {e}")));
        }
        let action = r.action.as_deref().unwrap_or("allow");
        if !matches!(action, "allow" | "deny") {
            return Err(Error::Invalid(format!(
                "{kind}/{name}: action must be allow|deny"
            )));
        }
        // Through the policy IR's own parse (ADR-0059 F3c): a port, a protocol
        // and a peer mean here exactly what they mean for a container's record.
        let stored = delonix_model::records::FwRule {
            dir: dir.to_string(),
            proto: proto.as_str().to_string(),
            port: r.port.clone(),
            src: peer,
            action: action.to_string(),
            ..Default::default()
        };
        let rule = delonix_networking::policy::rule_of(&stored)
            .map_err(|why| Error::Invalid(format!("{kind}/{name}: {why}")))?;
        rules.push(rule);
    }
    let policy = ir::Policy {
        direction: if dir == "in" {
            ir::Direction::Ingress
        } else {
            ir::Direction::Egress
        },
        default: if default == "allow" {
            ir::Action::Allow
        } else {
            ir::Action::Deny
        },
        rules,
    };
    Policy::from_ir(&policy).map_err(|why| Error::Invalid(format!("{kind}/{name}: {why}")))
}

/// Applies a `scope: network` `Egress` — per-network egress policy + CIDR/
/// FQDN allowlist + L4 rate-limit. Mirrors exactly the CLI's `egress net`/`egress
/// host`/`l4guard`, but declaratively. **Desired state**: each field is applied
/// exactly as it stands in the document.
fn apply_network_egress(kind: &str, name: &str, spec: &FwDocSpec) -> Result<()> {
    if !spec.rules.is_empty() {
        return Err(Error::Invalid(super::po::tf(
            "{kind}/{name}: `rules` is only for scope: container — in scope: network use allowCidrs/fqdnAllowlist",
            &[("kind", kind), ("name", name)],
        )));
    }
    let policy = spec.default_policy.as_deref().unwrap_or("allow");
    if !matches!(policy, "allow" | "deny") {
        return Err(Error::Invalid(format!(
            "{kind}/{name}: defaultPolicy must be allow|deny"
        )));
    }
    // The allowlist (CIDR/FQDN) ONLY takes effect with `deny` — with `allow` egress
    // stays open and the list would be silently discarded (the user would think
    // they closed the network). Clear error instead of a false show of restriction.
    if policy == "allow" && (!spec.allow_cidrs.is_empty() || !spec.fqdn_allowlist.is_empty()) {
        return Err(Error::Invalid(super::po::tf(
            "{kind}/{name}: allowCidrs/fqdnAllowlist only make sense with defaultPolicy: deny (with allow, egress stays open)",
            &[("kind", kind), ("name", name)],
        )));
    }
    // VALIDATE EVERYTHING before applying ANYTHING (fail-before-touching): an
    // invalid CIDR or FQDN midway must not leave egress in a partial state.
    for c in &spec.allow_cidrs {
        if let Err(e) = check_cidr(c) {
            return Err(Error::Invalid(format!("{kind}/{name}: {e}")));
        }
    }
    for host in &spec.fqdn_allowlist {
        if !fw_host_ok(host) {
            return Err(Error::Invalid(super::po::tf(
                "{kind}/{name}: invalid hostname '{host}'",
                &[("kind", kind), ("name", name), ("host", host)],
            )));
        }
    }

    // The REAL bridge lives in the infra registry (not the NetworkStore) — see egress_net.
    let bridge = infra::resolve_net(&spec.target)?.bridge;

    if policy == "deny" && !spec.allow_cidrs.is_empty() {
        // deny + allowCidrs → allowlist (denies everything except DNS + these CIDRs).
        let cidrs: Vec<&str> = spec.allow_cidrs.iter().map(String::as_str).collect();
        infra::set_egress_policy_net_allowlist(&bridge, &cidrs)?;
    } else {
        // allow → no restriction; deny (no CIDRs) → deny everything (only DNS passes).
        infra::set_egress_policy_net(&bridge, policy == "deny")?;
    }

    // FQDN allowlist — learnt live from DNS (DNS-snooping), adds `*.host`.
    for host in &spec.fqdn_allowlist {
        infra::set_egress_host(&bridge, host)?;
    }

    // L4 rate-limit (GLOBAL — not per-network). `{0,0}` = EXPLICITLY turn off the
    // guard (clear_l4_guard), not "l4guard 0 0" (whose zero semantics is ambiguous).
    if let Some(rl) = &spec.rate_limit {
        if rl.conn_rate == 0 && rl.conn_max == 0 {
            infra::clear_l4_guard()?;
        } else {
            infra::set_l4_guard(rl.conn_rate, rl.conn_max)?;
        }
    }

    let extras = format!(
        "{} CIDR + {} FQDN{}",
        spec.allow_cidrs.len(),
        spec.fqdn_allowlist.len(),
        if spec.rate_limit.is_some() {
            " + rateLimit"
        } else {
            ""
        }
    );
    println!(
        "{}",
        super::po::tf(
            "{kind}/{name}: per-network egress applied to '{target}' (default {policy}, {extras})",
            &[
                ("kind", kind),
                ("name", name),
                ("target", &spec.target),
                ("policy", policy),
                ("extras", &extras),
            ],
        )
    );
    Ok(())
}

/// A valid hostname/FQDN for the egress allowlist (alphanumeric labels +
/// hyphen, separated by `.`, ≤253). Rejects anything that could inject into an nft set.
fn fw_host_ok(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && !l.starts_with('-')
                && !l.ends_with('-')
        })
}

/// `egress show <net>` — the network's egress policy (CIDR allowlist + FQDN hosts
/// + the IPs currently learnt from DNS for those hosts).
fn egress_show(network: &str) -> Result<()> {
    let def = infra::network_get(network)
        .ok_or_else(|| Error::NotFound(format!("network '{network}'")))?;
    let policy = def
        .egress
        .policy
        .as_deref()
        .unwrap_or("allow (default — no egress restriction)");
    println!(
        "egress for network {} (bridge {}):",
        output::bold(network),
        def.bridge
    );
    println!("  policy: {policy}");
    if def.egress.hosts.is_empty() {
        println!("  FQDN allowlist: (none)");
    } else {
        println!("  FQDN allowlist ({} host(s)):", def.egress.hosts.len());
        for h in &def.egress.hosts {
            println!("    {h}  (and *.{h})");
        }
        let learnt = infra::egress_members(&def.bridge);
        if learnt.is_empty() {
            println!("  learnt IPs (live): (none yet — resolve a host from a container)");
        } else {
            println!("  learnt IPs (live): {}", learnt.join(", "));
        }
    }
    Ok(())
}

/// `egress host <net> <hostname>` — FQDN allowlist for a network's egress.
fn egress_host(network: &str, hostname: &str) -> Result<()> {
    let bridge = infra::resolve_net(network)?.bridge;
    infra::set_egress_host(&bridge, hostname)?;
    println!(
        "network {network}: egress now allows {} (and *.{}) — learnt live from DNS",
        hostname, hostname
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    fn docs_of(yaml: &str) -> Vec<ManifestDoc> {
        manifest::load_str(yaml, "t").unwrap()
    }

    const HOLD_MANIFEST: &str = "\
apiVersion: delonix.io/v1
kind: Container
metadata: { name: web }
spec: { image: alpine }
---
apiVersion: delonix.io/v1
kind: Container
metadata: { name: free }
spec: { image: alpine }
---
apiVersion: delonix.io/v1
kind: NetworkPolicy
metadata: { name: p }
spec: { target: web, direction: ingress, defaultPolicy: deny, rules: [] }
---
apiVersion: delonix.io/v1
kind: NetworkAccessRule
metadata: { name: r }
spec: { target: db, direction: egress, port: '53' }
";

    /// A guest manifest: a VM and a system container governed by `scope`
    /// policies, plus a container-scoped policy that must NOT make its target a
    /// guest target, and a `scope: vm` policy on a name that is also a
    /// container's.
    const GUEST_MANIFEST: &str = "\
apiVersion: delonix.io/v1
kind: NetworkPolicy
metadata: { name: a }
spec: { target: vm1, scope: vm, direction: ingress, defaultPolicy: deny, rules: [] }
---
apiVersion: delonix.io/v1
kind: NetworkPolicy
metadata: { name: b }
spec: { target: vm1, scope: vm, direction: egress, defaultPolicy: allow, rules: [] }
---
apiVersion: delonix.io/v1
kind: NetworkPolicy
metadata: { name: c }
spec: { target: sc1, scope: systemcontainer, direction: ingress, defaultPolicy: deny, rules: [] }
---
apiVersion: delonix.io/v1
kind: NetworkPolicy
metadata: { name: d }
spec: { target: web, direction: ingress, defaultPolicy: deny, rules: [] }
";

    /// The scope is what decides, and a policy of another scope never makes a
    /// guest a target: a VM born closed because of a container policy would be
    /// closed with nothing to open it.
    #[test]
    fn a_guest_is_a_target_only_through_a_policy_of_its_own_scope() {
        let d = docs_of(GUEST_MANIFEST);
        assert_eq!(
            guest_policy_targets(&d, "vm")
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["vm1"]
        );
        assert_eq!(
            guest_policy_targets(&d, "systemcontainer")
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["sc1"]
        );
        // The container-scoped policy's target is not a guest target in either scope.
        for scope in ["vm", "systemcontainer"] {
            assert!(
                !guest_policy_targets(&d, scope).contains("web"),
                "a container policy leaked into scope {scope}"
            );
        }
    }

    /// The directions a guest's policies declare are the ones that keep what was
    /// written; an undeclared one goes back to the guest's own default. Reading
    /// the wrong scope here would open a direction a policy had just closed.
    #[test]
    fn a_guests_declared_directions_come_from_its_own_scope_only() {
        let d = docs_of(GUEST_MANIFEST);
        assert_eq!(
            guest_declared_directions(&d, "vm1", "vm")
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["in", "out"],
            "both directions are declared for vm1"
        );
        assert_eq!(
            guest_declared_directions(&d, "sc1", "systemcontainer")
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["in"],
            "only ingress is declared for sc1, so egress returns to open"
        );
        assert!(
            guest_declared_directions(&d, "vm1", "systemcontainer").is_empty(),
            "the same name in another scope declares nothing"
        );
        assert!(
            guest_declared_directions(&d, "web", "vm").is_empty(),
            "a container-scoped policy declares nothing for a VM"
        );
    }

    #[test]
    fn only_the_containers_a_policy_names_are_governed() {
        let d = docs_of(HOLD_MANIFEST);
        let t: Vec<_> = policy_targets(&d).into_iter().collect();
        assert_eq!(
            t,
            vec!["db", "web"],
            "a NetworkAccessRule target counts; an unnamed container does not"
        );
    }

    #[test]
    fn a_direction_the_manifest_never_declared_returns_to_open_and_a_declared_one_keeps_its_policy()
    {
        let d = docs_of(HOLD_MANIFEST);
        let declared = declared_directions(&d, "web");
        assert_eq!(declared.iter().copied().collect::<Vec<_>>(), vec!["in"]);
        let held = super::super::container::policy_hold_firewall("default");
        // The policy document already set `in`; `out` is still the hold's deny.
        let after = released_policies(&held, &declared);
        assert_eq!(after.policy_in, "deny", "declared: untouched");
        assert_eq!(after.policy_out, "", "undeclared: back to the open default");
        // A rule-only target (a NetworkAccessRule): nothing declared, both open.
        let after = released_policies(&held, &declared_directions(&d, "db"));
        assert_eq!(
            (after.policy_in.as_str(), after.policy_out.as_str()),
            ("", "")
        );
    }

    #[test]
    fn the_hold_is_written_with_policies_an_older_holder_enforces() {
        let h = super::super::container::policy_hold_firewall("teamA");
        assert!(h.enabled && h.rules.is_empty());
        assert_eq!(
            (h.policy_in.as_str(), h.policy_out.as_str()),
            ("deny", "deny")
        );
        assert_eq!(h.namespace, "teamA");
    }

    use super::*;

    fn vm_spec(yaml: &str) -> FwDocSpec {
        serde_yaml::from_str(yaml).expect("a FwDocSpec")
    }

    #[test]
    fn a_scope_vm_policy_normalises_every_port_and_every_address() {
        let spec = vm_spec(
            "scope: vm\ntarget: web\ndirection: ingress\nrules:\n\
             - {port: '*', from: 0.0.0.0/0}\n\
             - {proto: tcp, port: '8000-8080', from: 10.0.0.0/8, action: deny}\n",
        );
        let p = vm_policy("NetworkPolicy", "p", &spec, "in").unwrap();
        assert!(!p.default_allow, "a policy with no default denies");
        assert_eq!(p.rules[0].port, None);
        assert_eq!(p.rules[0].peer, None);
        assert_eq!(p.rules[1].key(), "deny|tcp|8000-8080|10.0.0.0/8|");
    }

    #[test]
    fn a_scope_vm_policy_refuses_a_workload_name_and_the_network_fields() {
        let named = vm_spec("scope: vm\ntarget: web\nrules:\n- {port: '22', fromWorkload: db}\n");
        let e = vm_policy("NetworkPolicy", "p", &named, "in").unwrap_err();
        assert!(e.to_string().contains("fromWorkload"), "{e}");
        let net = vm_spec("scope: vm\ntarget: web\nallowCidrs: [10.0.0.0/8]\n");
        assert!(vm_policy("NetworkPolicy", "p", &net, "out").is_err());
        let proto = vm_spec("scope: vm\ntarget: web\nrules:\n- {proto: icmp, port: '*'}\n");
        assert!(vm_policy("NetworkPolicy", "p", &proto, "in").is_err());
    }

    /// A `scope: vm` rule goes through the policy IR's parse (ADR-0059 F3c):
    /// a reversed range is refused as it is for a container, and a prefix with
    /// host bits is sent as the network it names.
    #[test]
    fn a_scope_vm_policy_is_parsed_by_the_policy_ir() {
        let reversed = vm_spec("scope: vm\ntarget: web\nrules:\n- {proto: tcp, port: '90-80'}\n");
        let e = vm_policy("NetworkPolicy", "p", &reversed, "in").unwrap_err();
        assert!(e.to_string().contains("90-80"), "{e}");
        let host_bits =
            vm_spec("scope: vm\ntarget: web\nrules:\n- {port: '22', from: 10.0.0.5/24}\n");
        let p = vm_policy("NetworkPolicy", "p", &host_bits, "in").unwrap();
        assert_eq!(p.rules[0].key(), "allow|any|22|10.0.0.0/24|");
        let host = vm_spec("scope: vm\ntarget: web\nrules:\n- {port: '22', from: 10.0.0.5/32}\n");
        let p = vm_policy("NetworkPolicy", "p", &host, "in").unwrap();
        assert_eq!(p.rules[0].key(), "allow|any|22|10.0.0.5|");
    }

    /// The identity `get networkpolicies` prints and `describe`/`delete
    /// networkpolicies` have to parse back — the round-trip `list_all_policies`
    /// and its two consumers depend on.
    #[test]
    fn split_policy_name_reads_target_and_direction_back() {
        assert_eq!(split_policy_name("web/ingress").unwrap(), ("web", "in"));
        assert_eq!(split_policy_name("web/egress").unwrap(), ("web", "out"));
        assert!(split_policy_name("web").is_err());
        assert!(split_policy_name("web/sideways").is_err());
    }

    /// The two verbs that clear a direction promise different things, and each
    /// has to do what it says.
    ///
    /// Measured 2026-10-06 against the shipped engine: `delete networkpolicies
    /// <c>/ingress` on a container with a `deny` in force answered rc 0 and
    /// «removed 0 inbound rule(s)», `get networkpolicies` still listed the
    /// policy, and the deny was still enforced on the wire. A DELETE that
    /// reports success and deletes nothing.
    ///
    /// Verified to fail with the fix reverted: `clear_dir_with(.., true)` then
    /// leaves `policy_in == "deny"` and the record keeps its firewall.
    #[test]
    fn the_generic_delete_resets_the_direction_and_clear_keeps_the_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // No IP: this test is about the RECORD, which is what every read shows.
        seed(root, "cccc000000000003", "web", "default", "");
        let store = Store::open(root).unwrap();
        let fw = delonix_model::records::ContainerFw {
            policy_in: "deny".into(),
            rules: vec![rule("in", "tcp", "80", "", "allow")],
            ..Default::default()
        };
        store
            .update("cccc000000000003", |c| {
                c.firewall = Some(fw.clone());
                true
            })
            .unwrap();

        // `net ingress clear` removes the RULES and says so; the default is the
        // `policy` verb's to set, and that split is what its message promises.
        clear_dir(&store, "web", "in").unwrap();
        let c = store.load("cccc000000000003").unwrap();
        let kept = c
            .firewall
            .as_ref()
            .expect("a non-default policy is not disposable");
        assert!(
            kept.rules.is_empty(),
            "the rules of that direction are gone"
        );
        assert_eq!(kept.policy_in, "deny", "`clear` does not touch the default");

        // The Kind-generic DELETE resets the direction, and with nothing left
        // the firewall goes with it — so every read agrees with the operator.
        clear_dir_with(&store, "web", "in", true).unwrap();
        let c = store.load("cccc000000000003").unwrap();
        assert!(
            c.firewall.is_none(),
            "nothing was left to govern, so the record keeps no firewall: {:?}",
            c.firewall
        );
    }

    /// Writes a container record straight into a temp store. Only the fields
    /// the resolver reads are set; everything else comes from `Default`, which
    /// is what keeps this test about name resolution and nothing else.
    fn seed(root: &std::path::Path, id: &str, name: &str, namespace: &str, ip: &str) {
        let store = Store::open(root.to_path_buf()).unwrap();
        let mut c = delonix_compute::Container::new(
            id.into(),
            name.into(),
            "img".into(),
            vec!["x".into()],
            "max".into(),
        );
        c.namespace = namespace.into();
        c.ip = Some(ip.into());
        store.save(&c).unwrap();
    }

    // Cross-tenant regression, measured against `origin/main` before the fix:
    // two containers named `db` in different namespaces, and the rule endpoint
    // resolved to ONE of them silently — a `FirewallPolicy` for teamA writing
    // an `allow` for teamB's address.
    //
    // Verified to fail with the fix reverted: the old
    // `list().find(|c| c.name == name)` returns Ok("10.0.0.22/32") here.
    #[test]
    fn a_rule_refuses_a_workload_name_that_two_namespaces_share() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        seed(root, "aaaa000000000001", "db", "teamA", "10.0.0.11");
        seed(root, "bbbb000000000002", "db", "teamB", "10.0.0.22");
        let store = Store::open(root).unwrap();

        let err = workload_cidr(&store, "db").unwrap_err().to_string();
        assert!(
            err.contains("several namespaces")
                && err.contains("teamA/db")
                && err.contains("teamB/db"),
            "an ambiguous name had to be refused naming both candidates, got: {err}"
        );

        // And the qualified form still resolves — a refusal with no way to say
        // what you meant would just be the capability removed.
        assert_eq!(
            workload_cidr(&store, "teamB/db").unwrap(),
            "10.0.0.22/32",
            "the qualified form has to keep working"
        );
    }

    fn rule(dir: &str, proto: &str, port: &str, src: &str, action: &str) -> FwRule {
        FwRule {
            dir: dir.into(),
            proto: proto.into(),
            port: port.into(),
            src: src.into(),
            action: action.into(),
            note: String::new(),
            origin: None,
        }
    }

    // Bug-report regression: `deny 8069` followed by `allow 8069` accumulated and
    // the deny (above, first-match) won forever. The replacement compares with
    // norm_any: source ""/"0.0.0.0/0"/"*" are the same match.
    #[test]
    fn norm_any_iguala_as_tres_formas_de_qualquer_origem() {
        assert_eq!(norm_any(""), norm_any("0.0.0.0/0"));
        assert_eq!(norm_any(""), norm_any("*"));
        assert_eq!(norm_any("10.0.0.0/8"), "10.0.0.0/8");
    }

    /// The origin key `add_rule` writes must be deterministic (the whole
    /// point — a repeat `net ingress allow` for the same match has to find
    /// and replace its own earlier rule, byte-identical UX to the pre-origin
    /// 4-field comparison) and must normalize `src` the same way `norm_any`
    /// already does for the match itself (ADR-0029, decision 1).
    #[test]
    fn cli_rule_origin_is_deterministic_and_normalizes_the_src() {
        assert_eq!(
            cli_rule_origin("in", "tcp", "8069", ""),
            cli_rule_origin("in", "tcp", "8069", "")
        );
        assert_eq!(
            cli_rule_origin("in", "tcp", "8069", ""),
            cli_rule_origin("in", "tcp", "8069", "0.0.0.0/0")
        );
        assert_ne!(
            cli_rule_origin("in", "tcp", "8069", ""),
            cli_rule_origin("out", "tcp", "8069", "")
        );
        assert_ne!(
            cli_rule_origin("in", "tcp", "8069", ""),
            cli_rule_origin("in", "tcp", "5432", "")
        );
    }

    /// The distinction `apply_fw_doc`'s `FirewallPolicy` replace relies on:
    /// a CLI-synthesized origin must NOT read as a manifest document's name,
    /// or a `FirewallPolicy` apply would stop wiping imperative rules it is
    /// supposed to fully replace (see `is_manifest_origin`'s doc comment).
    #[test]
    fn is_manifest_origin_tells_a_document_from_a_cli_origin() {
        assert!(!is_manifest_origin(&cli_rule_origin(
            "in", "tcp", "8069", ""
        )));
        assert!(is_manifest_origin("allow-web-from-lan"));
    }

    // A full `apply_fw_doc` integration test would need a live ingress
    // holder (`apply_firewall_everywhere` talks to the real control socket
    // regardless of `c.ip`, confirmed by running one against this store —
    // it fails with "ingress holder is down") — not available in a plain
    // `cargo test`, the same limitation this codebase already accepts for
    // other holder-touching code paths, proven by unit test and code review
    // rather than a live run; `apply_fw_doc` itself has no existing test
    // either, for the same reason. The fix — `is_manifest_origin` — is
    // exercised directly above; the one-line wiring at the retain call site
    // (`fw.rules.retain(|r| r.dir != dir ||
    // r.origin.as_deref().is_some_and(is_manifest_origin))`) was verified
    // by hand instead.

    #[test]
    fn field_overlaps_apanha_coringas_e_iguais() {
        // `deny any/8069` shadows `allow tcp/8069` — the warning must fire.
        assert!(field_overlaps("any", "tcp", &["any", ""]));
        assert!(field_overlaps("8069", "8069", &["*", ""]));
        assert!(field_overlaps("*", "8069", &["*", ""]));
        assert!(!field_overlaps("tcp", "udp", &["any", ""]));
        assert!(!field_overlaps("8069", "5432", &["*", ""]));
    }

    #[test]
    fn rule_spec_reproduz_o_formato_do_cli() {
        assert_eq!(rule_spec(&rule("in", "any", "8069", "", "deny")), "8069");
        assert_eq!(
            rule_spec(&rule("in", "tcp", "5432", "", "allow")),
            "tcp/5432"
        );
    }

    fn net_spec(policy: &str, cidrs: &[&str], fqdns: &[&str], rules: Vec<FwDocRule>) -> FwDocSpec {
        FwDocSpec {
            direction: None,
            scope: Some("network".into()),
            target: "n".into(),
            default_policy: Some(policy.into()),
            rules,
            allow_cidrs: cidrs.iter().map(|s| s.to_string()).collect(),
            fqdn_allowlist: fqdns.iter().map(|s| s.to_string()).collect(),
            rate_limit: None,
        }
    }

    #[test]
    fn network_egress_recusa_allowlist_com_policy_allow() {
        // #1: allow + allowlist = restriction only in appearance → clear error.
        let e = apply_network_egress(
            "Egress",
            "e",
            &net_spec("allow", &["10.0.0.0/8"], &[], vec![]),
        )
        .unwrap_err();
        assert!(
            e.to_string()
                .contains("only make sense with defaultPolicy: deny"),
            "{e}"
        );
        let e = apply_network_egress(
            "Egress",
            "e",
            &net_spec("allow", &[], &["github.com"], vec![]),
        )
        .unwrap_err();
        assert!(
            e.to_string()
                .contains("only make sense with defaultPolicy: deny"),
            "{e}"
        );
    }

    /// A publish under `policy deny` + `allow <port> --from <cidr>` is the single most
    /// useful shape there is (expose a port to exactly one network) and it used to be
    /// reported as `BLOCKED`, with a warning claiming "the port answers nothing" —
    /// while it answered that source perfectly well. Validated live before and after:
    /// allowed source 200, other source nothing.
    #[test]
    fn publish_restrito_a_uma_origem_nao_e_bloqueado() {
        let rule = |port: &str, src: &str, action: &str| FwRule {
            dir: "in".into(),
            proto: "any".into(),
            port: port.into(),
            src: src.into(),
            action: action.into(),
            note: String::new(),
            origin: None,
        };
        let fw = |policy: &str, rules: Vec<FwRule>| delonix_model::records::ContainerFw {
            enabled: true,
            policy_in: policy.into(),
            policy_out: String::new(),
            rules,
            namespace: "default".into(),
        };
        // deny + a source-restricted allow → reachable, from that source.
        match published_reach(
            &fw("deny", vec![rule("80", "10.0.0.0/8", "allow")]),
            "80",
            "tcp",
        ) {
            PublishReach::Sources(s) => assert_eq!(s, vec!["10.0.0.0/8"]),
            _ => panic!("a source-restricted publish is neither open nor blocked"),
        }
        // deny with nothing covering the port → genuinely blocked.
        assert!(matches!(
            published_reach(&fw("deny", vec![]), "80", "tcp"),
            PublishReach::Blocked
        ));
        // A general allow covering the port opens it to everyone.
        assert!(matches!(
            published_reach(&fw("deny", vec![rule("80", "", "allow")]), "80", "tcp"),
            PublishReach::Open
        ));
        // First-match terminal: a general DENY placed BEFORE the source rule wins for
        // every source, so nothing gets through.
        assert!(matches!(
            published_reach(
                &fw(
                    "allow",
                    vec![rule("80", "", "deny"), rule("80", "10.0.0.0/8", "allow")]
                ),
                "80",
                "tcp"
            ),
            PublishReach::Blocked
        ));
        // ...and placed AFTER it, the source that was already allowed keeps working.
        match published_reach(
            &fw(
                "allow",
                vec![rule("80", "10.0.0.0/8", "allow"), rule("80", "", "deny")],
            ),
            "80",
            "tcp",
        ) {
            PublishReach::Sources(s) => assert_eq!(s, vec!["10.0.0.0/8"]),
            _ => panic!("the earlier source rule still matches first"),
        }
    }

    #[test]
    fn network_egress_valida_tudo_antes_de_tocar_no_motor() {
        // These errors fire BEFORE resolve_net (which would need the ingress
        // running) — pure validation, testable without infra.
        // #3: invalid CIDR.
        assert!(
            apply_network_egress("Egress", "e", &net_spec("deny", &["nope"], &[], vec![]))
                .unwrap_err()
                .to_string()
                .contains("invalid CIDR")
        );
        // #3: invalid FQDN (injection).
        assert!(
            apply_network_egress("Egress", "e", &net_spec("deny", &[], &["x;rm -rf"], vec![]))
                .unwrap_err()
                .to_string()
                .contains("invalid hostname")
        );
        // `rules` in scope network.
        let rules = vec![FwDocRule {
            from_workload: None,
            to_workload: None,
            proto: None,
            port: "80".into(),
            from: None,
            to: None,
            action: None,
            note: None,
        }];
        assert!(
            apply_network_egress("Egress", "e", &net_spec("deny", &[], &[], rules))
                .unwrap_err()
                .to_string()
                .contains("`rules` is only for scope: container")
        );
    }

    #[test]
    fn fw_host_ok_aceita_fqdn_valido_recusa_lixo() {
        assert!(fw_host_ok("github.com"));
        assert!(fw_host_ok("sub.dominio-x.example.co"));
        assert!(!fw_host_ok("")); // empty
        assert!(!fw_host_ok("a b.com")); // space
        assert!(!fw_host_ok("x;rm -rf.com")); // injection
        assert!(!fw_host_ok("-lead.com")); // label starts with a hyphen
        assert!(!fw_host_ok("trail-.com")); // label ends with a hyphen
        assert!(!fw_host_ok("a..b")); // empty label
    }

    #[test]
    fn parse_port_spec_defaults_proto_to_any() {
        assert_eq!(
            parse_port_spec("5432").unwrap(),
            ("any".into(), "5432".into())
        );
        assert_eq!(
            parse_port_spec("tcp/5432").unwrap(),
            ("tcp".into(), "5432".into())
        );
        assert_eq!(
            parse_port_spec("udp/*").unwrap(),
            ("udp".into(), "*".into())
        );
    }

    #[test]
    fn parse_port_spec_rejects_bad_proto_and_port() {
        assert!(parse_port_spec("sctp/80").is_err());
        assert!(parse_port_spec("tcp/99999").is_err());
    }

    /// The read side answers to the word the write side takes. A pod member's
    /// policy is declared under the POD's name, so that is the name a listing
    /// shows; a plain container answers to its own.
    #[test]
    fn a_pod_member_is_named_after_its_pod_and_a_container_after_itself() {
        let mut c = sdn_container("default");
        assert_eq!(governed_name(&c), "web", "a plain container: its own name");
        c.pod = Some("pod-web".into());
        assert_eq!(
            governed_name(&c),
            "web",
            "a member of pod `web`: the pod's name"
        );
        c.name = "web-sidecar".into();
        c.pod = Some("pod-team".into());
        assert_eq!(
            governed_name(&c),
            "team",
            "the POD's name, never the member's"
        );
        // A `pod` that is not a pod netns is not a pod: the member's own name stands.
        c.pod = Some("something-else".into());
        assert_eq!(governed_name(&c), "web-sidecar");
    }

    fn sdn_container(ns: &str) -> Container {
        let mut c = Container::new(
            "id".into(),
            "web".into(),
            "img".into(),
            vec!["sh".to_string()],
            "max".into(),
        );
        c.namespace = ns.into();
        c.ip = Some("10.209.0.5".into());
        c
    }

    /// NaaS audit P0-4. `ingress rm`/`clear` of the last rule tore the chain down —
    /// and outside `default` the chain is ALSO the namespace isolation. Measured with the
    /// previous binary: after `allow` + `rm` on a `team` container, the chain was gone,
    /// the record was `None`, and a container of another namespace reached it 2/2.
    #[test]
    fn removing_the_last_rule_drops_the_firewall_only_in_the_default_namespace() {
        let empty = delonix_model::records::ContainerFw {
            enabled: true,
            namespace: "teamA".into(),
            ..Default::default()
        };
        assert!(
            !firewall_disposable(&sdn_container("teamA"), &empty),
            "the isolation of teamA lives in this chain and nowhere else"
        );
        let open = delonix_model::records::ContainerFw::default();
        assert!(firewall_disposable(&sdn_container("default"), &open));
        let mut with_policy = open.clone();
        with_policy.policy_out = "deny".into();
        assert!(!firewall_disposable(
            &sdn_container("default"),
            &with_policy
        ));
    }

    /// The other half of P0-4: a container outside `default` that is started with NO
    /// firewall record (the state `rm`/`clear` used to leave behind, or any record that
    /// lost it) still gets its namespace enforced; and a record that has to be CREATED
    /// for it inherits its namespace instead of `default`, which inverted the isolation.
    #[test]
    fn the_container_namespace_is_a_firewall_to_enforce_even_without_a_record() {
        use super::super::container::{firewall_or_new, firewall_to_enforce};
        let c = sdn_container("teamA");
        let fw = firewall_to_enforce(&c).expect("teamA has isolation to enforce");
        assert!(fw.enabled && fw.namespace == "teamA" && fw.rules.is_empty());
        assert!(firewall_to_enforce(&sdn_container("default")).is_none());
        assert_eq!(firewall_or_new(&c).namespace, "teamA");

        let mut kept = sdn_container("teamA");
        let mut rec = firewall_or_new(&kept);
        rec.enabled = true;
        rec.policy_in = "deny".into();
        kept.firewall = Some(rec.clone());
        assert_eq!(firewall_to_enforce(&kept).unwrap().policy_in, "deny");
    }
}
