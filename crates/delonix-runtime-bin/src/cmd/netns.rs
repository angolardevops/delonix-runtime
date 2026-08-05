//! `delonix netns` — low-level management of the rootless ingress infra (the
//! holder netns + `delonix0` bridge + single slirp). This is the plumbing that
//! `container run --net <network>` uses under the hood; exposing it helps debug
//! the network path directly (attach a netns, publish a port, inspect state).
//!
//! The hidden `netns holder` / `netns run <spec>` re-execs are intercepted in
//! `main` BEFORE clap (they're internal, not user-facing), so they don't appear
//! here — only the operational subcommands do.

use clap::Subcommand;
use delonix_net::infra;
use delonix_runtime_core::{ContainerFw, Error, Result};

#[derive(Subcommand)]
pub enum NetnsCmd {
    /// Bring the ingress infra up (idempotent): holder netns + delonix0 + single slirp.
    Up,
    /// Show the ingress infra status (holder/slirp pids, bridge, refcount).
    Status {
        /// Emit JSON instead of a human summary.
        #[arg(long)]
        json: bool,
    },
    /// Force tear-down of the ingress infra (kills slirp + holder, frees the netns).
    Down,
    /// Attach a netns to delonix0 via veth (the holder is the netns/veth factory).
    Attach {
        /// Netns name (typically a container id/short-id).
        name: String,
        /// IP in the infra subnet. Defaults to a deterministic one derived from `name`.
        #[arg(long)]
        ip: Option<String>,
    },
    /// Detach (and destroy) a previously attached netns.
    Detach { name: String },
    /// Run a command inside an attached netns (exercises the runtime join path).
    Exec {
        name: String,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Publish a port through the ingress (add_hostfwd + DNAT) to a container.
    Publish {
        /// Netns/container name (its IP is derived unless `--ip` is given).
        name: String,
        /// Port mapping `hostPort:containerPort[/tcp|udp]`.
        spec: String,
        /// Override the container IP (defaults to the deterministic one from `name`).
        #[arg(long)]
        ip: Option<String>,
    },
    /// Unpublish a host port from the ingress.
    Unpublish { host_port: String },
    /// Apply (or clear) a container's parameterizable firewall AT THE INGRESS.
    Firewall {
        /// Netns/container name.
        name: String,
        /// ContainerFw as JSON, e.g. `{"enabled":true,"policyIn":"deny","rules":[...]}`.
        #[arg(long, conflicts_with = "clear")]
        spec: Option<String>,
        /// Remove the container's firewall from the ingress.
        #[arg(long)]
        clear: bool,
    },
}

/// Decides, for one container record, whether it is a candidate for
/// re-attachment. Pure so the policy is testable without a store or a holder.
///
/// A candidate is a container that is RUNNING, still has a live pid, and holds a
/// wire inside the holder — which means EITHER a custom network of its own, OR
/// membership of a pod (whose shared netns lives in the holder just the same).
/// `--net host/none` containers carry their own slirp and a holder respawn does
/// not touch them.
///
/// Pods were missing here, and the omission was measured, not theorised: after a
/// holder respawn the reconciliation reported `recovered 1 container(s)` while a
/// pod sat next to it `Up 32 seconds` with `Network unreachable` — permanently
/// stranded, its isolation chain gone, and not a word about it. Worse than the
/// container case it was modelled on, because at least that one got reported.
pub(crate) fn is_reattach_candidate(
    status: &delonix_runtime_core::Status,
    network: Option<&str>,
    pid: Option<i32>,
    pod: Option<&str>,
) -> bool {
    let wired = network.map(|n| !n.is_empty()).unwrap_or(false)
        || pod.map(|p| !p.is_empty()).unwrap_or(false);
    matches!(status, delonix_runtime_core::Status::Running)
        && wired
        && pid.map(|p| p > 1).unwrap_or(false)
}

/// Finds containers stranded by a PREVIOUS holder and — unless told otherwise —
/// restarts them so they get their network back.
///
/// **The measured failure, and why the obvious fix is impossible.** When the
/// holder dies, containers keep L3 (the netns survives because `slirp4netns`
/// still references it) and lose only DNS and the control plane. When the holder
/// **comes back**, it builds a brand new netns, the old one is destroyed with
/// every veth inside it, and every previously-running container is
/// `Network unreachable` **permanently**. An in-place upgrade — the ordinary way
/// this engine is updated — hits exactly that. The damage was never the holder
/// dying; it was the holder returning.
///
/// The obvious repair is to adopt the live netns into the new holder
/// (`ip netns attach <name> <pid>`). **That cannot work here, and it is a kernel
/// rule rather than a missing feature** — both halves were tested live:
///
/// * adopting fails with `Bind /proc/<pid>/ns/net -> /run/netns/<n>: Permission
///   denied`. Bind-mounting a namespace file needs CAP_SYS_ADMIN over the
///   userns that OWNS it, and the container's netns belongs to the *dead*
///   holder's userns, which the new holder has no privilege in;
/// * pinning the namespaces up front so they outlive the holder fails earlier
///   still — the bind must land on a host-visible path, and that needs privilege
///   in the host's mount namespace, which is precisely what rootless does not
///   have.
///
/// So in the rootless model a holder's namespaces cannot outlive its process,
/// and a new holder cannot inherit the old one's containers. What IS available
/// is to notice exactly which containers were stranded and rebuild them the only
/// way that works: a restart, which recreates the netns properly. Disruptive,
/// but bounded and automatic — against the previous behaviour of leaving them
/// silently networkless forever.
///
/// Returns `(recovered, failed)`. `DELONIX_NO_AUTO_RECOVER=1` reports the
/// stranded containers and the exact command, without touching them — for
/// anyone who would rather choose the moment a database restarts.
fn reconcile_after_respawn() -> Result<(usize, usize)> {
    let store = delonix_runtime_core::Store::open(delonix_runtime_core::Store::default_root())?;
    let manual = std::env::var_os("DELONIX_NO_AUTO_RECOVER").is_some();
    let (mut ok, mut failed) = (0usize, 0usize);

    // Idempotence guard: a workload the CURRENT holder already serves is healthy,
    // and restarting it would be a self-inflicted outage. The netns to ask about
    // is the POD's for a member and the container's own otherwise.
    //
    // The answer is SNAPSHOTTED before the loop, and that is the whole point.
    // Asking live inside the loop was measured to break multi-container pods: the
    // members share ONE netns, so the first one recovered makes the holder serve
    // it, and every remaining member is then skipped as "healthy" while still
    // sitting in the old, dead netns. Live, a two-container pod came back with
    // `recovered 2 container(s)` and `pa-c0` on `Network unreachable` — the
    // reconciliation reporting success over a container it had just abandoned.
    //
    // Taken up front, the question is the right one: at the start of the pass the
    // holder either served the pod's netns (nothing died — skip every member) or
    // it did not (they are all stranded — restart every member). For a plain
    // container, whose netns is its own, snapshot and live are equivalent.
    let mut candidates = Vec::new();
    for mut c in store.list()? {
        delonix_runtime::reconcile_status(&mut c);
        if is_reattach_candidate(&c.status, c.network.as_deref(), c.pid, c.pod.as_deref()) {
            candidates.push(c);
        }
    }
    let mut served = std::collections::BTreeMap::new();
    for c in &candidates {
        let key = c.pod.clone().unwrap_or_else(|| c.id.clone());
        served
            .entry(key)
            .or_insert_with_key(|k: &String| infra::holder_serves_netns(k));
    }

    for c in candidates {
        if served
            .get(c.pod.as_deref().unwrap_or(&c.id))
            .copied()
            .unwrap_or(false)
        {
            continue;
        }
        if manual {
            eprintln!(
                "{}",
                super::po::tf(
                    "container '{name}' lost its network to a holder restart — recover with: \
                     delonix container restart {name}",
                    &[("name", &c.name)],
                )
            );
            failed += 1;
            continue;
        }
        let images = match delonix_image::ImageStore::open(delonix_image::ImageStore::default_root())
        {
            Ok(i) => i,
            Err(e) => {
                eprintln!(
                    "delonix: could not open the image store to recover '{}': {e}",
                    c.name
                );
                failed += 1;
                continue;
            }
        };
        match super::container::cmd_restart(&images, &store, &c.id, 10) {
            Ok(()) => ok += 1,
            Err(e) => {
                eprintln!("delonix: could not recover '{}': {e}", c.name);
                failed += 1;
            }
        }
    }
    Ok((ok, failed))
}

pub fn run(action: NetnsCmd) -> Result<()> {
    match action {
        NetnsCmd::Up => {
            if !delonix_runtime::is_rootless() {
                println!("ingress: in root mode the single ingress already exists (nft DNAT on the host); the infra netns is rootless-only.");
                return Ok(());
            }
            infra::ensure_up()?;
            // Recover containers stranded by a previous holder — see
            // `reconcile_after_respawn`. Runs BEFORE the IPv6 sweep below so the
            // sweep sees the wires it is meant to harden.
            match reconcile_after_respawn() {
                Ok((0, 0)) => {}
                Ok((ok, failed)) => {
                    println!(
                        "{}",
                        super::po::tf(
                            "recovered {ok} container(s) stranded by the previous holder (restarted)",
                            &[("ok", &ok.to_string())],
                        )
                    );
                    if failed > 0 {
                        eprintln!(
                            "{}",
                            super::po::tf(
                                "warning: {failed} container(s) stranded without network — see above",
                                &[("failed", &failed.to_string())],
                            )
                        );
                    }
                }
                Err(e) => eprintln!(
                    "{}",
                    super::po::tf(
                        "warning: could not reconcile the running containers: {e}",
                        &[("e", &e.to_string())],
                    )
                ),
            }
            // `up` asserts the DESIRED state of the infra, and "no container holds an
            // IPv6 address" is now part of that state. It matters here and not only at
            // attach time because of the in-place upgrade: the new binary lands while
            // the old holder and every container it serves keep running, and those
            // containers keep the unfiltered v6 addresses they were given. This engine
            // reconfigures live containers without restarting them — a firewall fix is
            // no reason to make an exception. Idempotent, so re-running costs nothing.
            match infra::disable_ipv6_live() {
                Ok(0) => {}
                Ok(n) => println!(
                    "{}",
                    super::po::tf(
                        "IPv6 refused on {n} running container netns (no restart needed)",
                        &[("n", &n.to_string())],
                    )
                ),
                Err(e) => eprintln!(
                    "{}",
                    super::po::tf(
                        "warning: could not refuse IPv6 on the running containers: {e}",
                        &[("e", &e.to_string())],
                    )
                ),
            }
            let st = infra::status();
            println!(
                "ingress UP — pin pid {} · control pid {} · slirp pid {} · bridge {} ({})",
                fmt_pid(st.holder_pid),
                fmt_pid(st.control_pid),
                fmt_pid(st.slirp_pid),
                st.bridge,
                st.gateway,
            );
            Ok(())
        }
        NetnsCmd::Status { json } => {
            let st = infra::status();
            if json {
                println!("{}", serde_json::to_string_pretty(&st).unwrap_or_default());
            } else {
                println!(
                    "ingress {} — pin {} · control {} · slirp {} · bridge {} ({}) · refcount {}",
                    if st.up { "UP" } else { "DOWN" },
                    fmt_pid(st.holder_pid),
                    fmt_pid(st.control_pid),
                    fmt_pid(st.slirp_pid),
                    st.bridge,
                    st.gateway,
                    st.refcount,
                );
            }
            Ok(())
        }
        NetnsCmd::Down => {
            infra::teardown();
            println!("ingress DOWN — infra netns torn down.");
            Ok(())
        }
        NetnsCmd::Attach { name, ip } => {
            let (netns, assigned) = infra::attach_container(&name, "ingress", "default")?;
            println!(
                "attached '{netns}' → {} on {} (refcount {})",
                ip.unwrap_or(assigned),
                infra::INFRA_BRIDGE,
                infra::status().refcount
            );
            Ok(())
        }
        NetnsCmd::Detach { name } => {
            infra::detach_container(&name, &infra::container_ip(&name));
            println!("detached '{name}' (refcount {})", infra::status().refcount);
            Ok(())
        }
        NetnsCmd::Exec { name, command } => {
            let argv = infra::join_argv(&name).ok_or_else(|| Error::Runtime {
                context: "ingress",
                message: "infra is not up".into(),
            })?;
            let status = std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .args(&command)
                .status()
                .map_err(|e| Error::Runtime {
                    context: "netns exec",
                    message: e.to_string(),
                })?;
            std::process::exit(status.code().unwrap_or(1));
        }
        NetnsCmd::Publish { name, spec, ip } => {
            let cip = ip.unwrap_or_else(|| infra::container_ip(&name));
            infra::publish_port(&cip, &spec)?;
            println!("published {spec} → {cip} through the ingress");
            Ok(())
        }
        NetnsCmd::Unpublish { host_port } => {
            infra::unpublish_port(&host_port);
            println!("unpublished host port {host_port}");
            Ok(())
        }
        NetnsCmd::Firewall { name, spec, clear } => {
            let ip = infra::container_ip(&name);
            if clear {
                infra::clear_firewall(&ip);
                println!("ingress firewall removed for '{name}'");
                return Ok(());
            }
            let json =
                spec.ok_or_else(|| Error::Invalid("missing --spec <json> or --clear".into()))?;
            let fw: ContainerFw = serde_json::from_str(&json)
                .map_err(|e| Error::Invalid(format!("firewall JSON: {e}")))?;
            infra::apply_firewall(&name, &ip, &fw)?;
            println!(
                "ingress firewall applied for '{name}' ({} rule(s))",
                fw.rules.len()
            );
            Ok(())
        }
    }
}

fn fmt_pid(p: Option<i32>) -> String {
    p.map(|p| p.to_string()).unwrap_or_else(|| "—".into())
}

#[cfg(test)]
mod tests {
    use super::is_reattach_candidate;
    use delonix_runtime_core::Status;

    /// Um MEMBRO DE POD também fica sem rede num respawn — a netns partilhada
    /// morre com o holder antigo tal como qualquer veth. Sem isto, a
    /// reconciliação anunciava «recovered 1 container(s)» com um pod ao lado
    /// `Up 32 seconds` e `Network unreachable`, para sempre e em silêncio.
    #[test]
    fn membro_de_pod_tambem_e_candidato_a_recuperacao() {
        // Um membro de pod NÃO tem rede custom no registo (é `--net host` para si
        // próprio) — é o campo `pod` que prova que tem um fio dentro do holder.
        assert!(is_reattach_candidate(
            &Status::Running,
            None,
            Some(42),
            Some("pod-pa")
        ));
        // …e as outras condições continuam a valer para ele.
        assert!(!is_reattach_candidate(
            &Status::Stopped,
            None,
            Some(42),
            Some("pod-pa")
        ));
        assert!(!is_reattach_candidate(
            &Status::Running,
            None,
            None,
            Some("pod-pa")
        ));
        // Um `pod` vazio não é membro de pod nenhum.
        assert!(!is_reattach_candidate(
            &Status::Running,
            None,
            Some(42),
            Some("")
        ));
    }

    /// Só é candidato a recuperação quem PODE ter ficado sem rede num respawn do
    /// holder: a correr, numa rede custom (só essas têm veth no holder — o
    /// `--net host/none` traz slirp próprio e é indiferente ao respawn), e com
    /// um pid vivo. Alargar isto reiniciaria containers saudáveis.
    #[test]
    fn so_recupera_containers_a_correr_em_rede_custom() {
        assert!(is_reattach_candidate(
            &Status::Running,
            Some("dev"),
            Some(42),
            None
        ));

        // parado / criado / morto → não se toca
        for st in [Status::Created, Status::Stopped, Status::Paused] {
            assert!(!is_reattach_candidate(&st, Some("dev"), Some(42), None));
        }
        // sem rede custom (host/none) → o respawn não lhe mexeu
        assert!(!is_reattach_candidate(
            &Status::Running,
            None,
            Some(42),
            None
        ));
        assert!(!is_reattach_candidate(
            &Status::Running,
            Some(""),
            Some(42),
            None
        ));
        // sem pid utilizável → não há nada para recuperar
        assert!(!is_reattach_candidate(
            &Status::Running,
            Some("dev"),
            None,
            None
        ));
        assert!(!is_reattach_candidate(
            &Status::Running,
            Some("dev"),
            Some(0),
            None
        ));
        assert!(!is_reattach_candidate(
            &Status::Running,
            Some("dev"),
            Some(1),
            None
        ));
    }
}
