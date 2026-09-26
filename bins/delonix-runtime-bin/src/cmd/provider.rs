//! `delonix provider ls|describe|matrix` — what each provider says it can do,
//! against the versioned capability catalog (ADR-0050).
//!
//! `ls` and `describe` are MEASURED on this host: each provider probes what it
//! needs (a binary, `/dev/kvm`, `qemu:///system`, a delegated cgroup,
//! `bridge-nf-call-iptables`) and a declared "supported" that the host cannot
//! honour reads `unavailable-on-host`, with the missing piece named. A remote
//! provider (Proxmox) declares and does not connect: its health says
//! `NotProbed`, never a guess.
//!
//! `matrix` is the DECLARED view — every host assumed complete — and is what
//! `docs/providers/capability-matrix.md` is generated from; a test keeps the
//! file equal to the output, so the published matrix cannot drift from the
//! code that produces it (the same guard the JSON schema has).
//!
//! The JSON of `ls` mirrors `ProviderInfo` in `proto/delonix/node/v1/common.proto`
//! field by field (`id`, `kind`, `available`, `capabilities[].name/supported/
//! detail`, `health`): when `ListProviders` is served, the handler maps this
//! struct and adds nothing.

use clap::Subcommand;
use delonix_compute::capability::{
    Capability, CapabilityReport, CapabilityState, Domain, ProviderKind, ProviderReport,
    CATALOG_VERSION,
};
use delonix_model::{Error, Result};

#[derive(Subcommand)]
pub enum ProviderCmd {
    /// Every provider this build knows, probed on this host.
    ///
    /// One row per (provider, kind) with the count of capabilities in each
    /// state. A provider that is not configured (a remote one without a
    /// target) is listed as unavailable, not hidden.
    Ls {
        /// Only providers of this kind: compute, network, storage.
        #[arg(long, value_parser = parse_kind)]
        kind: Option<ProviderKind>,
        /// Machine-readable JSON (ADR-0005), one element per provider.
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: super::output::OutputFormat,
    },
    /// Every capability of ONE provider, with its state and the reason.
    Describe {
        /// Provider id: libvirt, cloud-hypervisor, proxmox, linux.
        id: String,
        /// Disambiguates `linux`, which is a compute, a network and a storage
        /// provider at once. Omitted: all of its kinds.
        #[arg(long, value_parser = parse_kind)]
        kind: Option<ProviderKind>,
        /// Contact the configured Proxmox target and measure its cluster (nodes,
        /// quorum, shared storage, HA, SDN zones) with read-only requests. Only
        /// for `proxmox`: the local providers are measured on every describe.
        #[arg(long)]
        probe: bool,
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: super::output::OutputFormat,
    },
    /// The declared matrix, host-independent, as Markdown.
    ///
    /// What `docs/providers/capability-matrix.md` is generated from. Nothing
    /// is probed: every host is assumed complete, so the states are the
    /// providers' own claims — and each `supported` names its evidence.
    Matrix,
    /// The node's providers file (ADR-0054): what it says, and whether it is valid.
    #[command(subcommand)]
    Config(ConfigCmd),
}

#[derive(Subcommand)]
pub enum ConfigCmd {
    /// The provider configuration this process reads, with where each value comes from.
    ///
    /// The file (and any file the precedence ignores), the default provider and
    /// its source, and every provider with its settings. A secret is never
    /// printed: only where it comes from — a file path, a `kind: Secret` name,
    /// or the environment.
    Show {
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: super::output::OutputFormat,
    },
    /// Checks a providers file without registering or contacting anything.
    ///
    /// The format, a default provider that has an entry, and what the file
    /// points at: the token file (readable only by its owner), the CA, the
    /// secret, the URL and node syntax. Exit 0 when valid, 1 when not.
    Validate {
        /// The file to check. Omitted: the file this process would read.
        #[arg(short = 'f', long = "file")]
        file: Option<std::path::PathBuf>,
    },
    /// Prints the JSON Schema of the providers file.
    ///
    /// The same schema published as `docs/schema/v1/providers.json`, generated
    /// from the types the engine reads — an editor that loads it underlines a
    /// misspelt key before `provider config validate` is ever run.
    Schema,
}

fn parse_kind(s: &str) -> std::result::Result<ProviderKind, String> {
    match s {
        "compute" => Ok(ProviderKind::Compute),
        "network" => Ok(ProviderKind::Network),
        "storage" => Ok(ProviderKind::Storage),
        "image" => Ok(ProviderKind::Image),
        other => Err(format!(
            "unknown provider kind '{other}' (compute, network, storage, image)"
        )),
    }
}

/// Every provider report, MEASURED on this host. Order: the VM backends in
/// registry order (the auto-detection preference), then the Linux provider's
/// three kinds with Proxmox's network report after the Linux one. A Proxmox target that is not registered in this process still
/// appears, declared and unavailable, so the list is the same set of names on
/// every host — a reader compares hosts by state, not by which rows exist.
pub fn measured_reports() -> Vec<ProviderReport> {
    let mut out = delonix_vm::provider_reports();
    let proxmox_configured = out.iter().any(|r| r.id == "proxmox");
    if !proxmox_configured {
        out.push(delonix_proxmox::capability_report(false));
    }
    out.push(delonix_linux::provider_report::report(
        &delonix_linux::provider_report::LinuxHost::probe(),
    ));
    out.push(delonix_sdn::provider_report::report(
        &delonix_sdn::provider_report::SdnHost::probe(),
    ));
    // The node's own per-VM firewall (ADR-0052): the second network
    // provider, next to the Linux one. Same configured flag as the VM backend
    // — it is the same target, answering a different port.
    out.push(delonix_proxmox::network_capability_report(
        proxmox_configured,
    ));
    out.push(delonix_volume::provider_report::report(
        &delonix_volume::provider_report::StorageHost::probe(),
    ));
    out
}

/// The same set, DECLARED: every host assumed complete, Proxmox assumed
/// configured. Pure — what the published matrix and its test read.
pub fn declared_reports() -> Vec<ProviderReport> {
    use delonix_vm::capabilities as vmc;
    vec![
        vmc::cloud_hypervisor_report(&vmc::CloudHypervisorHost::ASSUMED),
        vmc::libvirt_report(&vmc::LibvirtHost::ASSUMED),
        delonix_proxmox::capability_report(true),
        delonix_linux::provider_report::report(&delonix_linux::provider_report::LinuxHost::ASSUMED),
        delonix_sdn::provider_report::report(&delonix_sdn::provider_report::SdnHost::ASSUMED),
        delonix_proxmox::network_capability_report(true),
        delonix_volume::provider_report::report(
            &delonix_volume::provider_report::StorageHost::ASSUMED,
        ),
    ]
}

pub fn run(cmd: ProviderCmd) -> Result<()> {
    match cmd {
        ProviderCmd::Config(ConfigCmd::Show { output }) => config_show(output),
        ProviderCmd::Config(ConfigCmd::Validate { file }) => config_validate(file),
        ProviderCmd::Config(ConfigCmd::Schema) => {
            let text = serde_json::to_string_pretty(&super::providers_config::schema())
                .map_err(|e| Error::Invalid(format!("json output: {e}")))?;
            println!("{text}");
            Ok(())
        }
        ProviderCmd::Ls { kind, output } => {
            let reports: Vec<ProviderReport> = measured_reports()
                .into_iter()
                .filter(|r| kind.is_none_or(|k| r.kind == k))
                .collect();
            match output {
                super::output::OutputFormat::Json => {
                    let items: Vec<serde_json::Value> =
                        reports.iter().map(provider_info_json).collect();
                    super::output::print_json(&items)
                }
                super::output::OutputFormat::Table => {
                    print_ls(&reports);
                    // D2: a file the precedence skips is said, on one line —
                    // editing it would change nothing, and nothing else says so.
                    if let Ok(Some((chosen, _))) = super::providers_config::loaded() {
                        for p in super::providers_config::ignored(chosen) {
                            eprintln!(
                                "{}",
                                super::po::tf(
                                    "note: {path} exists but {chosen} is the providers file read here",
                                    &[
                                        ("path", &p.display().to_string()),
                                        ("chosen", &chosen.display().to_string()),
                                    ]
                                )
                            );
                        }
                    }
                    Ok(())
                }
            }
        }
        ProviderCmd::Describe {
            id,
            kind,
            probe,
            output,
        } => {
            let reports: Vec<ProviderReport> = measured_reports()
                .into_iter()
                .filter(|r| r.id == id && kind.is_none_or(|k| r.kind == k))
                .collect();
            if reports.is_empty() {
                let known: Vec<String> = {
                    let mut v: Vec<String> = measured_reports().into_iter().map(|r| r.id).collect();
                    v.dedup();
                    v
                };
                return Err(Error::NotFound(super::po::tf(
                    "provider '{id}' (known: {known})",
                    &[("id", &id), ("known", &known.join(", "))],
                )));
            }
            let cluster = if probe {
                Some(probe_cluster(&id)?)
            } else {
                None
            };
            match output {
                super::output::OutputFormat::Json => {
                    let items: Vec<serde_json::Value> = reports
                        .iter()
                        .map(|r| {
                            let mut v = provider_info_json(r);
                            if let (Some(c), Some(o)) = (&cluster, v.as_object_mut()) {
                                o.insert("cluster".into(), cluster_json(c));
                            }
                            v
                        })
                        .collect();
                    super::output::print_json(&items)
                }
                super::output::OutputFormat::Table => {
                    for r in &reports {
                        print_describe(r);
                    }
                    if let Some(c) = &cluster {
                        print_cluster(c);
                    }
                    Ok(())
                }
            }
        }
        ProviderCmd::Matrix => {
            print!("{}", matrix_markdown(&declared_reports()));
            Ok(())
        }
    }
}

/// What `--probe` measured: the target it asked and the cluster around it.
pub struct ProbedCluster {
    url: String,
    node: String,
    facts: delonix_proxmox::cluster::ClusterFacts,
    verdicts: Vec<delonix_proxmox::cluster::ClusterVerdict>,
}

/// `provider describe proxmox --probe`: connects to the configured target —
/// the same one a VM operation would use — and reads its cluster with GETs
/// only (ADR-0049 slice 3). Refused for any other provider: the local ones
/// are measured on every describe, and a flag that did nothing there would
/// read as if it had.
fn probe_cluster(id: &str) -> Result<ProbedCluster> {
    if id != "proxmox" {
        return Err(Error::Invalid(super::po::tf(
            "--probe applies to a remote provider (proxmox); '{id}' is measured on this host by every describe",
            &[("id", id)],
        )));
    }
    let (target, opts) = super::vmbackends::proxmox_target()?.ok_or_else(|| {
        Error::Unavailable(super::po::t(
            "no Proxmox target is configured: set DELONIX_PROXMOX_URL, DELONIX_PROXMOX_NODE and a credential (DELONIX_PROXMOX_SECRET, or DELONIX_PROXMOX_TOKEN_ID with DELONIX_PROXMOX_TOKEN_FILE)",
        ).to_string())
    })?;
    let url = target.base_url.clone();
    let node = target.node.clone();
    let client = delonix_proxmox::Client::connect_with(&target, opts)?;
    let facts = client.cluster_facts()?;
    let verdicts = delonix_proxmox::cluster::cluster_verdicts(&facts);
    Ok(ProbedCluster {
        url,
        node,
        facts,
        verdicts,
    })
}

fn cluster_json(c: &ProbedCluster) -> serde_json::Value {
    serde_json::json!({
        "url": c.url,
        "node": c.node,
        "facts": c.facts,
        "verdicts": c.verdicts.iter().map(|v| serde_json::json!({
            "name": v.capability.name(),
            "offered": v.offered,
            "reason": v.reason,
        })).collect::<Vec<_>>(),
    })
}

fn print_cluster(c: &ProbedCluster) {
    let f = &c.facts;
    println!();
    println!(
        "{}",
        super::po::tf(
            "Cluster of the target, measured now ({url}, node {node}), read-only:",
            &[("url", &c.url), ("node", &c.node)],
        )
    );
    let yes_no = |b: bool| if b { "yes" } else { "no" };
    println!(
        "  {} {}",
        super::po::t("cluster:"),
        match (&f.cluster, f.quorate) {
            (Some(n), Some(q)) => format!("{n} (quorate: {})", yes_no(q)),
            _ => super::po::t("none — the node is not in a cluster").to_string(),
        }
    );
    let mut t = super::output::Table::new(&["NODE", "ONLINE", "TARGET"]);
    for n in &f.nodes {
        t.row(vec![
            n.name.clone(),
            yes_no(n.online).into(),
            if n.target { "yes" } else { "-" }.into(),
        ]);
    }
    t.print();
    let mut t = super::output::Table::new(&["STORAGE", "TYPE", "SHARED", "VM DISKS"]);
    for s in &f.storages {
        t.row(vec![
            s.id.clone(),
            s.kind.clone(),
            yes_no(s.shared).into(),
            yes_no(s.images).into(),
        ]);
    }
    t.print();
    println!(
        "  HA: {} · {} {} · SDN zones: {}",
        f.ha.master.as_deref().unwrap_or("no CRM master"),
        f.ha.resources,
        super::po::t("resource(s)"),
        f.sdn_zones
    );
    let mut t = super::output::Table::new(&["CAPABILITY", "THIS CLUSTER", "WHY"]);
    for v in &c.verdicts {
        t.row(vec![
            v.capability.name().to_string(),
            if v.offered { "offers it" } else { "does not" }.into(),
            v.reason.clone(),
        ]);
    }
    t.print();
    println!(
        "{}",
        super::po::t(
            "What the cluster offers is a fact about the cluster, not engine support: the backend addresses one node and never picks another (ADR-0049 slice 3)."
        )
    );
}

/// The contract's `ProviderInfo`, as JSON, from a report. `supported` is the
/// contract's one bit; `state` and `detail` carry the five-way answer next to it.
pub fn provider_info_json(r: &ProviderReport) -> serde_json::Value {
    serde_json::json!({
        "id": r.id,
        "kind": r.kind.as_str(),
        "available": r.available,
        "health": {
            "status": match r.health.status {
                delonix_compute::capability::HealthStatus::Healthy => "healthy",
                delonix_compute::capability::HealthStatus::Unavailable => "unavailable",
                delonix_compute::capability::HealthStatus::Unknown => "unknown",
            },
            "reason": r.health.reason,
            "message": r.health.message,
        },
        "catalog_version": CATALOG_VERSION,
        "capabilities": r.capabilities.iter().map(|c| serde_json::json!({
            "name": c.capability.name(),
            "domain": c.domain.as_str(),
            "supported": c.state.is_usable(),
            "state": c.state.label(),
            "detail": c.state.detail(),
        })).collect::<Vec<_>>(),
    })
}

const STATES: [&str; 6] = [
    "supported",
    "partial",
    "unsupported-by-provider",
    "requires-external-component",
    "not-implemented",
    "unavailable-on-host",
];

fn print_ls(reports: &[ProviderReport]) {
    println!(
        "{}",
        super::po::tf(
            "Providers of delonix {version}, catalog {catalog}, measured on this host ({total} capabilities in the catalog).",
            &[
                ("version", env!("CARGO_PKG_VERSION")),
                ("catalog", CATALOG_VERSION),
                ("total", &Capability::ALL.len().to_string()),
            ],
        )
    );
    println!();
    let mut t = super::output::Table::new(&[
        "ID",
        "KIND",
        "AVAILABLE",
        "HEALTH",
        "SUPPORTED",
        "PARTIAL",
        "UNSUPPORTED",
        "EXTERNAL",
        "NOT-IMPL",
        "UNAVAILABLE",
    ]);
    for r in reports {
        t.row(vec![
            r.id.clone(),
            r.kind.as_str().to_string(),
            if r.available { "yes" } else { "no" }.to_string(),
            r.health.reason.to_string(),
            r.count("supported").to_string(),
            r.count("partial").to_string(),
            r.count("unsupported-by-provider").to_string(),
            r.count("requires-external-component").to_string(),
            r.count("not-implemented").to_string(),
            r.count("unavailable-on-host").to_string(),
        ]);
    }
    t.print();
    let notes: Vec<&ProviderReport> = reports
        .iter()
        .filter(|r| !r.health.message.is_empty())
        .collect();
    if !notes.is_empty() {
        println!();
        for r in notes {
            println!("{}/{}: {}", r.id, r.kind.as_str(), r.health.message);
        }
    }
    println!(
        "\n{}",
        super::po::t(
            "`supported` names its evidence (a battery check, a scenario or a test); `partial` has a written limit; `unavailable-on-host` is a declared yes this host cannot honour. `delonix provider describe <id>` lists the reasons."
        )
    );
}

fn print_describe(r: &ProviderReport) {
    let mut d = super::output::Describe::new();
    d.field("Provider", &r.id)
        .field("Kind", r.kind.as_str())
        .field("Available", if r.available { "yes" } else { "no" })
        .field(
            "Health",
            format!(
                "{} ({}){}",
                match r.health.status {
                    delonix_compute::capability::HealthStatus::Healthy => "healthy",
                    delonix_compute::capability::HealthStatus::Unavailable => "unavailable",
                    delonix_compute::capability::HealthStatus::Unknown => "unknown",
                },
                r.health.reason,
                if r.health.message.is_empty() {
                    String::new()
                } else {
                    format!(": {}", r.health.message)
                }
            ),
        )
        .field("Catalog", CATALOG_VERSION);
    d.print();
    let mut t = super::output::Table::new(&["DOMAIN", "CAPABILITY", "STATE", "DETAIL"]);
    for c in &r.capabilities {
        t.row(vec![
            c.domain.as_str().to_string(),
            c.capability.name().to_string(),
            c.state.label().to_string(),
            c.state.detail(),
        ]);
    }
    t.print();
    println!();
}

fn cell(s: &CapabilityState) -> String {
    let d = s.detail();
    if d.is_empty() {
        s.label().to_string()
    } else {
        format!("{} — {}", s.label(), d.replace('|', "\\|"))
    }
}

/// The published matrix: one table per kind, one column per provider of that
/// kind, one row per capability grouped by domain. Deterministic — the test
/// compares it byte for byte with the checked-in file.
pub fn matrix_markdown(reports: &[ProviderReport]) -> String {
    let mut out = String::new();
    out.push_str("# Provider capability matrix\n\n");
    out.push_str(&format!(
        "Generated by `delonix provider matrix` (engine {}, catalog {}). **Do not edit**: a test in \
         `bins/delonix-runtime-bin` fails when this file differs from the output.\n\n",
        env!("CARGO_PKG_VERSION"),
        CATALOG_VERSION
    ));
    out.push_str(
        "This is the DECLARED view — every host assumed complete. `delonix provider ls` measures the \
         same declarations on a real host and turns a declared yes the host cannot honour into \
         `unavailable-on-host`. States: `supported` (implemented and exercised — the evidence names a \
         battery check `check:`, a section `e2e:`, a chaos scenario `chaos:`, a unit test `test:` or a \
         live test `live:`), `partial` (implemented with a written limit, or without live proof), \
         `unsupported-by-provider`, `requires-external-component`, `not-implemented`. See ADR-0050.\n\n",
    );
    for kind in [
        ProviderKind::Compute,
        ProviderKind::Network,
        ProviderKind::Storage,
    ] {
        let cols: Vec<&ProviderReport> = reports.iter().filter(|r| r.kind == kind).collect();
        if cols.is_empty() {
            continue;
        }
        out.push_str(&format!("## {}\n\n", kind.as_str()));
        // Totals per provider, so a reader gets the number before the table.
        for r in &cols {
            let counts: Vec<String> = STATES
                .iter()
                .map(|s| format!("{} {}", r.count(s), s))
                .filter(|s| !s.starts_with("0 "))
                .collect();
            out.push_str(&format!(
                "- **{}**: {} of {} — {}\n",
                r.id,
                r.count("supported"),
                r.capabilities.len(),
                counts.join(", ")
            ));
        }
        out.push('\n');
        out.push_str("| domain | capability |");
        for r in &cols {
            out.push_str(&format!(" {} |", r.id));
        }
        out.push('\n');
        out.push_str("|---|---|");
        for _ in &cols {
            out.push_str("---|");
        }
        out.push('\n');
        let rows: Vec<Capability> = Capability::ALL
            .iter()
            .copied()
            .filter(|c| c.kind() == kind)
            .collect();
        let mut last: Option<Domain> = None;
        for c in rows {
            let dom = if last == Some(c.domain()) {
                ""
            } else {
                c.domain().as_str()
            };
            last = Some(c.domain());
            out.push_str(&format!("| {} | `{}` |", dom, c.name()));
            for r in &cols {
                let st = r
                    .capabilities
                    .iter()
                    .find(|x| x.capability == c)
                    .map(|x: &CapabilityReport| cell(&x.state))
                    .unwrap_or_else(|| "?".to_string());
                out.push_str(&format!(" {st} |"));
            }
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// Where the credential of a Proxmox target comes from, never its value.
fn auth_source(lookup: &dyn Fn(&str) -> Option<String>) -> (String, String) {
    if let Some(name) = lookup("DELONIX_PROXMOX_SECRET") {
        return ("secret".into(), format!("kind: Secret '{name}'"));
    }
    if let Some(id) = lookup("DELONIX_PROXMOX_TOKEN_ID") {
        let from = match lookup("DELONIX_PROXMOX_TOKEN_FILE") {
            Some(f) => format!("file {f}"),
            None if lookup("DELONIX_PROXMOX_TOKEN").is_some() => {
                super::po::t("the environment (redacted)").to_string()
            }
            None => super::po::t("nowhere — the token is missing").to_string(),
        };
        return ("api-token".into(), format!("{id}, secret from {from}"));
    }
    if let Some(user) = lookup("DELONIX_PROXMOX_USER") {
        return (
            "password".into(),
            super::po::tf(
                "{user} — a password, weaker than an API token",
                &[("user", &user)],
            ),
        );
    }
    ("none".into(), super::po::t("no credential").to_string())
}

fn config_show(output: super::output::OutputFormat) -> Result<()> {
    use super::providers_config as pc;
    let loaded = pc::loaded();
    let (file, cfg) = match loaded {
        Ok(Some((p, c))) => (Some(p.clone()), Some(c)),
        Ok(None) => (None, None),
        Err(e) => return Err(Error::Invalid(e.to_string())),
    };
    let ignored: Vec<String> = file
        .as_ref()
        .map(|f| {
            pc::ignored(f)
                .iter()
                .map(|p| p.display().to_string())
                .collect()
        })
        .unwrap_or_default();

    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let (default, default_source) = if let Some(v) = env("DELONIX_VM_BACKEND") {
        (Some(v), "environment (DELONIX_VM_BACKEND)".to_string())
    } else if let Some(v) = cfg.and_then(|c| c.default_provider.clone()) {
        (Some(v), "file".to_string())
    } else if let Some(v) = delonix_vm::get_default_backend(&super::util::state_root()) {
        (Some(v), "legacy vm default-backend".to_string())
    } else {
        (None, "auto-detection".to_string())
    };

    // One row per provider: the local ones the file lists, and the Proxmox
    // target from wherever this process takes it (D4).
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for p in cfg.map(|c| c.providers.as_slice()).unwrap_or(&[]) {
        match p {
            pc::ProviderEntry::Libvirt(_) => {
                rows.push(serde_json::json!({"type": "libvirt", "source": "file"}))
            }
            pc::ProviderEntry::CloudHypervisor(_) => {
                rows.push(serde_json::json!({"type": "cloud-hypervisor", "source": "file"}))
            }
            pc::ProviderEntry::Proxmox(_) => {}
        }
    }
    let px_lookup = super::vmbackends::configured_lookup()?;
    if px_lookup("DELONIX_PROXMOX_URL").is_some() {
        let source = if env("DELONIX_PROXMOX_URL").is_some() {
            "environment"
        } else {
            "file"
        };
        let (auth_kind, auth) = auth_source(&*px_lookup);
        rows.push(serde_json::json!({
            "type": "proxmox",
            "source": source,
            "url": px_lookup("DELONIX_PROXMOX_URL"),
            "node": px_lookup("DELONIX_PROXMOX_NODE"),
            "auth": auth_kind,
            "credential": auth,
            "caFile": px_lookup("DELONIX_PROXMOX_CA_FILE"),
            "insecureSkipVerify": px_lookup("DELONIX_PROXMOX_INSECURE_TLS").is_some(),
            "bridge": px_lookup("DELONIX_PROXMOX_BRIDGE"),
            "vlan": px_lookup("DELONIX_PROXMOX_VLAN"),
        }));
    }

    match output {
        super::output::OutputFormat::Json => {
            let doc = serde_json::json!({
                "file": file.as_ref().map(|f| f.display().to_string()),
                "ignored": ignored,
                "defaultProvider": default,
                "defaultSource": default_source,
                "providers": rows,
            });
            let text = serde_json::to_string_pretty(&doc)
                .map_err(|e| Error::Invalid(format!("json output: {e}")))?;
            println!("{text}");
            Ok(())
        }
        super::output::OutputFormat::Table => {
            let none = super::po::t("none — the providers file does not exist").to_string();
            println!(
                "{:<18}{}",
                super::po::t("File:"),
                file.as_ref()
                    .map(|f| f.display().to_string())
                    .unwrap_or(none)
            );
            for i in &ignored {
                println!(
                    "{:<18}{}",
                    super::po::t("Ignored:"),
                    super::po::tf("{path} (another file wins)", &[("path", i)])
                );
            }
            println!(
                "{:<18}{} ({})",
                super::po::t("Default provider:"),
                default.as_deref().unwrap_or("-"),
                default_source
            );
            println!("{}", super::po::t("Providers:"));
            if rows.is_empty() {
                println!("  -");
            }
            for r in &rows {
                let t = r["type"].as_str().unwrap_or("?");
                let src = r["source"].as_str().unwrap_or("?");
                println!(
                    "  {t:<18}{}",
                    super::po::tf("from {source}", &[("source", src)])
                );
                for key in ["url", "node", "credential", "caFile", "bridge", "vlan"] {
                    if let Some(v) = r.get(key).and_then(|v| v.as_str()) {
                        println!("    {key:<16}{v}");
                    }
                }
                if r["insecureSkipVerify"].as_bool() == Some(true) {
                    println!(
                        "    {:<16}{}",
                        "tls",
                        super::po::t("certificate NOT verified (insecureSkipVerify)")
                    );
                }
            }
            Ok(())
        }
    }
}

fn config_validate(file: Option<std::path::PathBuf>) -> Result<()> {
    use super::providers_config as pc;
    let path = match file {
        Some(f) => f,
        None => match pc::loaded() {
            Ok(Some((p, _))) => p.clone(),
            Ok(None) => return Err(Error::Invalid(
                super::po::t(
                    "there is no providers file to validate (none of the three locations exists)",
                )
                .into(),
            )),
            Err(e) => return Err(Error::Invalid(e.to_string())),
        },
    };
    let content = std::fs::read_to_string(&path)
        .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
    let cfg = pc::parse(&content, &path)?;
    pc::validate(&cfg, &path)?;
    println!(
        "{}",
        super::po::tf("{path}: valid", &[("path", &path.display().to_string())])
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn repo() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    /// Every `supported` names evidence that EXISTS. This is what makes the
    /// word mean something: a declaration that cites a check nobody wrote is a
    /// red test here, not a published claim.
    #[test]
    fn every_supported_capability_cites_evidence_that_exists() {
        let root = repo();
        let e2e = std::fs::read_to_string(root.join("scripts/e2e.sh")).unwrap();
        let chaos = std::fs::read_to_string(root.join("scripts/chaos.sh")).unwrap();
        let mut bad = Vec::new();
        for r in declared_reports() {
            for c in &r.capabilities {
                let CapabilityState::Supported { evidence } = &c.state else {
                    continue;
                };
                let ok = match evidence.split_once(':') {
                    Some(("e2e", title)) => e2e.contains(&format!("section \"{title}\"")),
                    Some(("check", title)) => e2e.contains(&format!("check \"{title}\"")),
                    Some(("chaos", name)) => chaos.contains(&format!("{name}()")),
                    Some(("test" | "live", rest)) => match rest.split_once("::") {
                        Some((path, func)) => std::fs::read_to_string(root.join(path))
                            .map(|s| s.contains(&format!("fn {func}(")))
                            .unwrap_or(false),
                        None => root.join(rest).is_file(),
                    },
                    _ => false,
                };
                if !ok {
                    bad.push(format!(
                        "{}/{} {} -> {evidence}",
                        r.id,
                        r.kind.as_str(),
                        c.capability.name()
                    ));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "evidence that names nothing in the repository:\n{}",
            bad.join("\n")
        );
    }

    /// The checked-in matrix IS the output. Regenerate with
    /// `delonix provider matrix > docs/providers/capability-matrix.md`.
    #[test]
    fn the_published_matrix_is_the_generated_one() {
        let path = repo().join("docs/providers/capability-matrix.md");
        let want = matrix_markdown(&declared_reports());
        let have = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            have == want,
            "docs/providers/capability-matrix.md is stale — run `delonix provider matrix > docs/providers/capability-matrix.md`"
        );
    }

    #[test]
    fn every_provider_answers_every_row_of_its_kind() {
        for r in declared_reports() {
            let n = Capability::ALL
                .iter()
                .filter(|c| c.kind() == r.kind)
                .count();
            assert_eq!(r.capabilities.len(), n, "{}/{}", r.id, r.kind.as_str());
        }
    }

    #[test]
    fn the_json_carries_the_contracts_provider_info_fields() {
        let v = provider_info_json(&declared_reports()[1]);
        assert_eq!(v["id"], "libvirt");
        assert_eq!(v["kind"], "compute");
        assert!(v["available"].is_boolean());
        assert!(v["health"]["reason"].is_string());
        let caps = v["capabilities"].as_array().unwrap();
        assert!(caps.iter().all(|c| c["name"].is_string()
            && c["supported"].is_boolean()
            && c["detail"].is_string()));
    }
}
