//! Honesty conditions — the piece that stops a resource from LYING by
//! omission. Several Kinds apply best-effort and, when a privilege/host
//! prerequisite is missing, the resource is created but does not do what it
//! appears to (an NFS `Storage` in rootless does not mount; a hard quota in
//! rootless is only monitored; a macvlan `Network` stays in the registry with
//! no physical plane; a `restartPolicy` on a Cloud Hypervisor VM is not
//! supervised). Instead of leaving this silent, each Kind can declare
//! `conditions` (kubectl-style: a boolean state + actionable `reason`) that
//! `stack describe` shows to the user.
//!
//! **No persisted state**: conditions are COMPUTED from the spec + an
//! environment probe, on the fly — the same "the stack has no registry of its
//! own" philosophy as `describe`. `conditions_for` is pure (it receives the
//! already-probed `Env`), so it is testable without depending on the machine's
//! real state.

use super::kinds as k;
use super::manifest::ManifestDoc;

pub use delonix_stack::condition::Condition;

/// Probed host environment (best-effort). Explicit fields = `conditions_for`
/// pure and testable without touching the real host.
#[derive(Debug, Clone)]
pub struct Env {
    /// No root privilege (network `mount -t` and the hard quota need
    /// CAP_SYS_ADMIN, which a rootless session does not have in the init namespace).
    pub rootless: bool,
    /// Helper `mount.nfs` present on the PATH.
    pub mount_nfs: bool,
    /// Helper `mount.cifs` present on the PATH.
    pub mount_cifs: bool,
    /// Helper `mount.davfs` present on the PATH.
    pub mount_davfs: bool,
    /// `cloud-hypervisor` binary available — decides the VM's AUTO backend
    /// (present → CH; absent → falls back to libvirt). Mirrors `select_backend`.
    pub cloud_hypervisor: bool,
    /// `wg` usable on the host — the prerequisite of an ENCRYPTED overlay
    /// (`wgIp`). Without it `realize_overlay` fails closed and the uplink never
    /// comes up.
    pub wg: bool,
    /// Networks that have a PHYSICAL plane — the ones `infra::network_list()`
    /// knows, i.e. those a workload can actually attach to.
    ///
    /// **The only field here that is about a RESOURCE rather than a
    /// capability**, and it earns the exception: without it the `Realized`
    /// condition was derived from the manifest and answered «is this driver
    /// implementable», never «was this network built». Probed once for the whole
    /// plan, like the rest — it is one `read_dir`.
    pub realized_networks: std::collections::BTreeSet<String>,
}

impl Env {
    /// Probes the host for real. Reuses `delonix_linux::is_rootless` (the
    /// canonical privilege helper, the same one the rest of the runtime uses).
    pub fn probe() -> Env {
        Env {
            rootless: delonix_linux::is_rootless(),
            mount_nfs: which("mount.nfs"),
            mount_cifs: which("mount.cifs"),
            mount_davfs: which("mount.davfs"),
            cloud_hypervisor: which("cloud-hypervisor"),
            // The SAME function `realize_overlay` gates on, not a `which("wg")`
            // lookalike: a condition that disagrees with the realizer is worse
            // than no condition at all.
            wg: delonix_sdn::wg::available(),
            realized_networks: delonix_sdn::infra::network_list()
                .into_iter()
                .map(|d| d.name)
                .collect(),
        }
    }
}

/// `is the binary on the PATH?` — scans `$PATH` PLUS the canonical sbin
/// directories. The mount helpers (`mount.nfs`/`mount.cifs`/`mount.davfs`) live
/// in `/sbin`/`/usr/sbin`, which often are NOT on a user session's `$PATH` —
/// without including them, the condition would report `MountHelperMissing`
/// when the helper exists (honesty turning into misinformation).
fn which(bin: &str) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let sbins = ["/sbin", "/usr/sbin", "/usr/local/sbin"].map(std::path::PathBuf::from);
    std::env::split_paths(&path)
        .chain(sbins)
        .any(|dir| dir.join(bin).is_file())
}

/// Reads a top-level string field from the raw `spec`, accepting any of `keys`
/// (to cover the canonical AND the legacy alias — e.g. `restartPolicy`/`restart_policy`).
fn spec_str<'a>(doc: &'a ManifestDoc, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| doc.spec.get(k).and_then(|v| v.as_str()))
}

/// The conditions of a document. Empty = nothing to flag (the common case).
pub fn conditions_for(doc: &ManifestDoc, env: &Env) -> Vec<Condition> {
    match doc.kind.as_str() {
        k::VOLUME => {
            let mut c = volume(doc, env);
            c.extend(network_share(doc, env));
            c
        }
        k::NETWORK => network(doc, env),
        k::VM => {
            let mut c = vm(doc, env);
            c.extend(vm_volumes(doc));
            c
        }
        _ => Vec::new(),
    }
}

/// `Volume.Mounted` — mounting NFS/CIFS/WebDAV needs CAP_SYS_ADMIN and the right
/// mount helper on the host; without either, the volume is created but the mount
/// fails silently (best-effort). See `delonix-volume::ensure_mounted`.
///
/// **This went SILENT for a while and the silence is the lesson.** It used to be
/// dispatched by `kind: Storage`, and when that Kind folded into `kind: Volume`
/// (a network-share block) no document carried the old Kind any more — so the
/// one condition whose entire job is to say «created, but it does not actually
/// mount» stopped saying it. The honesty mechanism failed honestly-quietly,
/// which is the worst way for it to fail. It now reads the BLOCK, and the block
/// is where the type lives.
fn network_share(doc: &ManifestDoc, env: &Env) -> Vec<Condition> {
    let Some(ty) = ["nfs", "cifs", "webdav"]
        .into_iter()
        .find(|b| doc.spec.get(b).is_some_and(|v| !v.is_null()))
    else {
        return Vec::new();
    };
    if env.rootless {
        return vec![Condition::bad(
            "Mounted",
            "RequiresCapSysAdmin",
            super::po::tf(
                "mounting '{ty}' requires CAP_SYS_ADMIN — run as root or in a privileged session; in rootless the mount is best-effort and fails",
                &[("ty", ty)],
            ),
        )];
    }
    let (helper, present) = match ty {
        "cifs" | "smb" => ("mount.cifs", env.mount_cifs),
        "webdav" => ("mount.davfs", env.mount_davfs),
        _ => ("mount.nfs", env.mount_nfs),
    };
    if !present {
        return vec![Condition::bad(
            "Mounted",
            "MountHelperMissing",
            super::po::tf(
                "helper '{helper}' not in PATH — install it on the host to mount '{ty}'",
                &[("helper", helper), ("ty", ty)],
            ),
        )];
    }
    vec![Condition::ok("Mounted")]
}

/// `Volume.QuotaEnforced` — the hard quota uses an ext4 loopback (`losetup`),
/// which requires root; in rootless there is only a monitored alert, no real
/// cap. With no quota declared, there is nothing to flag.
fn volume(doc: &ManifestDoc, env: &Env) -> Vec<Condition> {
    let has_quota = doc.spec.get("quota").is_some_and(|v| !v.is_null());
    if !has_quota {
        return Vec::new();
    }
    if env.rootless {
        vec![Condition::bad(
            "QuotaEnforced",
            "RequiresRoot",
            super::po::t(
                "the hard quota requires root (losetup/CAP_SYS_ADMIN) — in rootless it is only MONITORED, no real cap",
            ),
        )]
    } else {
        vec![Condition::ok("QuotaEnforced")]
    }
}

/// `Network.Realized` — `macvlan`/`ipvlan` stay in the `NetworkStore` with
/// nothing a container can attach to: their physical plane needs `CAP_NET_ADMIN`
/// in the host's init-netns, which the rootless model does not have.
///
/// **`overlay` is NOT one of them, and used to be listed here as if it were.**
/// `network create --driver overlay` calls `realize_overlay`, which brings up
/// the bridge, the VXLAN uplink (`dlxvx<vni>` mastering it) and WireGuard —
/// entirely inside the holder netns, so entirely without host privilege. The
/// record it writes allocates a `base` octet exactly like a bridge network's,
/// and nothing on the attach path gates on the driver, so containers attach to
/// it like any other. Saying otherwise reported this engine's most advanced
/// networking as unimplemented, in the plan, to the person who had just asked
/// for it.
///
/// What IS a real prerequisite is the ENCRYPTED overlay: with `wgIp` set,
/// `realize_overlay` refuses before touching the VXLAN when `wg` is missing
/// (otherwise the FDB would point at peer addresses reachable only through a
/// tunnel that never comes up — a silently blackholed uplink). That one is
/// worth declaring, and it is the reason this function takes an `Env`.
fn network(doc: &ManifestDoc, env: &Env) -> Vec<Condition> {
    let driver = spec_str(doc, &["driver"]).unwrap_or("bridge");
    match driver {
        "macvlan" | "ipvlan" => vec![Condition::bad(
            "Realized",
            "DriverNotImplemented",
            super::po::tf(
                "driver '{driver}' has no physical plane yet — it stays in the registry but containers only attach to `bridge`",
                &[("driver", driver)],
            ),
        )],
        "overlay" if spec_str(doc, &["wgIp", "wg_ip"]).is_some() && !env.wg => {
            vec![Condition::bad(
                "Realized",
                "WireguardMissing",
                super::po::t(
                    "encrypted overlay (wgIp) but 'wg' is unavailable on the host — install wireguard-tools + the kernel module, or drop wgIp for plain (unencrypted) VXLAN transport",
                ),
            )]
        }
        _ => vec![Condition::ok("Realized")],
    }
}

/// A network that is in the declarative registry but has NO physical plane.
///
/// **«Realizable» is not «realized», and the `Realized` condition answered the
/// second question with the first**: it was derived entirely from the YAML —
/// `False` because the document said `macvlan`, never because anything was
/// looked at. So a network whose `NetDef` was missing reported `Realized=True`
/// while `container run --net <name>` failed with «does not exist», and
/// `stack plan` said «no changes» about it.
///
/// Reads the `NetDef` (through `Env`), the record of the PHYSICAL plane, and
/// deliberately NOT the bridge. That distinction is what makes it safe: the
/// bridge lives in the holder's EPHEMERAL netns and is gone on an idle node, so
/// probing it would report every healthy network as broken — the trap that made
/// `NetworkRoute` plan against its record instead. The `NetDef` is a file: it
/// outlives the holder, and its absence means the network was never built, or
/// was half-removed.
///
/// Only for a network the manifest expects to ALREADY be there — the caller
/// skips it on a `Create`, the same shape `Vm`'s unconverged-fields condition
/// uses.
pub(crate) fn network_not_realized(doc: &ManifestDoc, env: &Env) -> Option<Condition> {
    if doc.kind != k::NETWORK {
        return None;
    }
    // macvlan/ipvlan have their own, more specific reason above; overlay and
    // bridge are the ones that DO get a physical plane.
    if matches!(
        spec_str(doc, &["driver"]).unwrap_or("bridge"),
        "macvlan" | "ipvlan"
    ) {
        return None;
    }
    let name = doc.metadata.name.as_str();
    if env.realized_networks.contains(name) {
        return None;
    }
    Some(Condition::bad(
        "Realized",
        "NotRealized",
        super::po::tf(
            "network '{name}' is in the declarative registry but has no physical plane — nothing \
             can attach to it (`--net {name}` fails with «does not exist»). Re-create it with \
             `delonix network create {name}`, or remove it",
            &[("name", name)],
        ),
    ))
}

/// `Vm.RestartSupervised` — only the libvirt backend materializes the restart
/// policy (via `<on_crash>` in the XML); Cloud Hypervisor (the auto default)
/// does not supervise it. With no `restartPolicy` (or `no`), there is nothing
/// to flag.
fn vm(doc: &ManifestDoc, env: &Env) -> Vec<Condition> {
    let policy = spec_str(doc, &["restartPolicy", "restart_policy"]).unwrap_or("no");
    if policy.is_empty() || policy == "no" {
        return Vec::new();
    }
    // Which backend actually BOOTS — mirrors `select_backend`: explicit wins;
    // in auto (backend absent) Cloud Hypervisor is preferred IF the binary
    // exists, otherwise it falls back to libvirt. Which one supervises the restart
    // is the backend's own `vm.restart-policy.native` declaration (ADR-0050).
    let backend = match spec_str(doc, &["backend"]) {
        Some(b) => b.to_string(),
        None if env.cloud_hypervisor => "cloud-hypervisor".to_string(),
        None => "libvirt".to_string(),
    };
    // The predicate lives in `delonix-vm`, next to the backends, and is the SAME
    // one the apply path uses (`restart_policy_unsupervised`). It used to be
    // re-implemented here as `backend == "libvirt"`, which agreed by accident
    // and disagreed in what it SAID: any backend that was not libvirt got the
    // reason `BackendCloudHypervisor` and a sentence naming Cloud Hypervisor.
    // `proxmox` is a registered backend on a configured host
    // (`vmbackends::register_configured`), so a manifest asking for it was told
    // about a backend it had not chosen.
    if !delonix_vm::restart_policy_unsupervised(&backend, Some(policy)) {
        vec![Condition::ok("RestartSupervised")]
    } else {
        vec![Condition::bad(
            "RestartSupervised",
            "BackendDoesNotSupervise",
            super::po::tf(
                "restartPolicy '{policy}' is NOT supervised by the '{backend}' backend — use `backend: libvirt` to materialize it",
                &[("policy", policy), ("backend", &backend)],
            ),
        )]
    }
}

/// `Vm.VolumesRequireLibvirt` — `spec.volumes` is only materializable by the
/// libvirt backend (virtio-9p; Cloud Hypervisor does not support it). The apply
/// auto-selects libvirt when there is no explicit backend; this flags the case
/// where the user FORCES `backend: cloud-hypervisor` with volumes (the boot
/// would refuse).
fn vm_volumes(doc: &ManifestDoc) -> Vec<Condition> {
    let has_volumes = doc
        .spec
        .get("volumes")
        .and_then(|v| v.as_sequence())
        .is_some_and(|s| !s.is_empty());
    if has_volumes && spec_str(doc, &["backend"]) == Some("cloud-hypervisor") {
        vec![Condition::bad(
            "VolumesRequireLibvirt",
            "BackendCloudHypervisor",
            super::po::t(
                "spec.volumes uses virtio-9p, which only the libvirt backend materializes — remove `backend: cloud-hypervisor` (apply picks libvirt on its own when there are volumes)",
            ),
        )]
    } else {
        Vec::new()
    }
}

/// The annotation that records, when a resource is CREATED by a stack apply,
/// the declared values of the spec fields the reconciler does not compare.
///
/// The plan cannot compare those fields against the machine (the record holds
/// `env` merged with the image's, `user` as a resolved uid, …), so it used to
/// name every one of them on every existing resource as «declared but NOT
/// applied» — including right after a recreate that had just applied them all,
/// and including fields that had not changed at all. Creation applies the
/// whole spec, so what was declared AT CREATION is what the resource has; the
/// manifest compared against that says exactly which of them changed since.
///
/// Written only on a create (`+` or `-/+`), never on a converge: an apply that
/// leaves the resource alone must not move this, or a changed `env` would be
/// recorded as applied while the old one keeps running.
pub(crate) const CREATED_SPEC: &str = "delonix.io/created-spec";

/// The uncompared fields of one manifest document, in the two forms the
/// comparison needs.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Declared {
    /// Every uncompared field with its value AFTER the Kind's defaults are
    /// filled in — what the resource is created with. Comparing these is what
    /// makes `env: []` and an absent `env`, or a rendered manifest replayed by
    /// `stack rollback` and the one the user wrote, the same declaration.
    pub values: std::collections::BTreeMap<String, String>,
    /// The uncompared fields the manifest actually WRITES — what gets named
    /// when there is nothing to compare against. Naming every defaulted field
    /// would turn the warning into a list of the whole struct.
    pub written: std::collections::BTreeSet<String>,
}

impl Declared {
    /// `raw` is the spec as written, `filled` the same spec with defaults
    /// (`spec_with_defaults`); a Kind that cannot fill passes `raw` twice.
    pub(crate) fn of(
        raw: &serde_yaml::Value,
        filled: &serde_yaml::Value,
        compared: &[&str],
        aliases: &[(&str, &str)],
        ignored: &[&str],
    ) -> Self {
        Declared {
            values: uncompared_values(filled, compared, aliases, ignored),
            written: uncompared_values(raw, compared, aliases, ignored)
                .into_keys()
                .collect(),
        }
    }
}

/// The values of the spec fields outside `compared`, keyed by the field's
/// canonical name (an alias of a compared field IS that field — a manifest
/// saying `restart:` was told `restartPolicy` was not compared).
pub(crate) fn uncompared_values(
    spec: &serde_yaml::Value,
    compared: &[&str],
    aliases: &[(&str, &str)],
    ignored: &[&str],
) -> std::collections::BTreeMap<String, String> {
    let Some(m) = spec.as_mapping() else {
        return Default::default();
    };
    m.iter()
        .filter_map(|(key, v)| {
            let key = key.as_str()?;
            let key = aliases
                .iter()
                .find(|(a, _)| *a == key)
                .map(|(_, c)| *c)
                .unwrap_or(key);
            if compared.contains(&key) || ignored.contains(&key) {
                return None;
            }
            Some((key.to_string(), canonical_json(v).to_string()))
        })
        .collect()
}

/// A YAML value as JSON with its mapping keys sorted, so the same declaration
/// written in a different key order is the same string on both sides.
fn canonical_json(v: &serde_yaml::Value) -> serde_json::Value {
    match v {
        serde_yaml::Value::Mapping(m) => {
            let mut pairs: Vec<(String, serde_json::Value)> = m
                .iter()
                .map(|(k, v)| {
                    let k = k.as_str().map(str::to_string).unwrap_or_else(|| {
                        serde_yaml::to_string(k)
                            .unwrap_or_default()
                            .trim()
                            .to_string()
                    });
                    (k, canonical_json(v))
                })
                .collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            serde_json::Value::Object(pairs.into_iter().collect())
        }
        serde_yaml::Value::Sequence(s) => {
            serde_json::Value::Array(s.iter().map(canonical_json).collect())
        }
        serde_yaml::Value::Tagged(t) => canonical_json(&t.value),
        other => serde_json::to_value(other).unwrap_or(serde_json::Value::Null),
    }
}

/// What can be said about the uncompared fields of an EXISTING resource.
#[derive(Debug, PartialEq)]
pub(crate) enum Uncompared {
    /// Nothing declared outside the compared set, or all of it exactly as it
    /// was when the resource was created — there is nothing to say.
    Unchanged,
    /// These changed since the resource was created, and an apply does not
    /// carry them to it.
    Changed(Vec<String>),
    /// The resource has no [`CREATED_SPEC`] (created by hand, or by an older
    /// version): whether these match is unknown.
    Unverifiable(Vec<String>),
}

/// Compares the manifest's uncompared fields against the creation record.
///
/// Both sides carry defaults, so a field removed from the manifest compares
/// its default against what the resource was created with — a change, because
/// nothing removes it short of a recreate.
///
/// A field only ONE side knows about and the manifest does not write is a
/// field the engine added or dropped between the version that created the
/// resource and this one. It is not something the user changed, and an
/// upgrade must not make every existing container warn.
pub(crate) fn uncompared_state(
    declared: &Declared,
    created: Option<&std::collections::BTreeMap<String, String>>,
) -> Uncompared {
    match created {
        None if declared.written.is_empty() => Uncompared::Unchanged,
        None => Uncompared::Unverifiable(declared.written.iter().cloned().collect()),
        Some(created) => {
            let changed: Vec<String> = declared
                .values
                .keys()
                .filter(|f| created.contains_key(*f))
                .chain(declared.written.iter())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .filter(|f| declared.values.get(*f) != created.get(*f))
                .cloned()
                .collect();
            if changed.is_empty() {
                Uncompared::Unchanged
            } else {
                Uncompared::Changed(changed)
            }
        }
    }
}

#[cfg(test)]
mod tests {

    const COMPARED: &[&str] = &["image", "restartPolicy"];
    const ALIASES: &[(&str, &str)] = &[("restart", "restartPolicy")];

    /// A stand-in for `spec_with_defaults`: `env` and `user` always present.
    fn decl(yaml: &str) -> Declared {
        let raw: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let mut filled = raw.clone();
        let m = filled.as_mapping_mut().unwrap();
        for (k, default) in [("env", "[]"), ("user", "''")] {
            m.entry(k.into())
                .or_insert_with(|| serde_yaml::from_str(default).unwrap());
        }
        Declared::of(&raw, &filled, COMPARED, ALIASES, &["detach"])
    }

    /// `restart:` is the legacy spelling of `restartPolicy`, which IS
    /// compared: the warning used to list it as not compared.
    #[test]
    fn an_alias_of_a_compared_field_is_that_field() {
        let d = decl("image: a\nrestart: always\ndetach: true\nenv: [A=1]\n");
        assert_eq!(d.written.iter().collect::<Vec<_>>(), ["env"]);
    }

    /// The same declaration in another key order is the same value — or every
    /// reordered manifest would read as a change.
    #[test]
    fn key_order_inside_a_value_does_not_count() {
        assert_eq!(decl("tmpfs: {a: 1, b: 2}\n"), decl("tmpfs: {b: 2, a: 1}\n"));
    }

    /// Measured 2026-10-02: right after `--replace` recreated the container
    /// with the new env, the plan still said «declared but NOT applied: env».
    #[test]
    fn unchanged_since_creation_says_nothing() {
        let d = decl("image: a\nenv: [A=new]\ncommand: [sleep, '1']\n");
        assert_eq!(uncompared_state(&d, Some(&d.values)), Uncompared::Unchanged);
    }

    /// Measured 2026-10-02: `stack rollback` replays the RENDERED manifest
    /// (every default written out), and against a record of the raw one the
    /// warning named 31 fields nobody had touched. Defaults on both sides.
    #[test]
    fn a_rendered_manifest_is_the_same_declaration_as_the_written_one() {
        let written = decl("env: [A=1]\n");
        let rendered = decl("env: [A=1]\nuser: ''\n");
        assert_eq!(
            uncompared_state(&rendered, Some(&written.values)),
            Uncompared::Unchanged
        );
    }

    /// A field the engine gained after the resource was created is not a
    /// change the user made: an upgrade must not make every container warn.
    /// Unless the manifest writes it — then it is new, and not applied.
    #[test]
    fn a_field_the_engine_added_since_creation_is_not_a_change() {
        let mut created = decl("env: [A=1]\n").values;
        created.remove("user");
        assert_eq!(
            uncompared_state(&decl("env: [A=1]\n"), Some(&created)),
            Uncompared::Unchanged
        );
        assert_eq!(
            uncompared_state(&decl("env: [A=1]\nuser: app\n"), Some(&created)),
            Uncompared::Changed(vec!["user".into()])
        );
    }

    /// Only what changed is named — `command` was listed beside `env` although
    /// only `env` had been edited.
    #[test]
    fn only_the_fields_that_changed_are_named() {
        let created = decl("env: [A=old]\ncommand: [sleep, '1']\nuser: app\n").values;
        let now = decl("env: [A=new]\ncommand: [sleep, '1']\n");
        assert_eq!(
            uncompared_state(&now, Some(&created)),
            Uncompared::Changed(vec!["env".into(), "user".into()]),
            "a field dropped from the manifest is still on the resource"
        );
    }

    #[test]
    fn without_a_creation_record_it_is_unverifiable_not_unapplied() {
        let d = decl("env: [A=1]\n");
        assert_eq!(
            uncompared_state(&d, None),
            Uncompared::Unverifiable(vec!["env".into()])
        );
        // Defaults alone are nothing to name: only what the manifest writes.
        assert_eq!(
            uncompared_state(&decl("image: a\n"), None),
            Uncompared::Unchanged
        );
    }

    use super::*;
    use crate::cmd::manifest::{ManifestDoc, Metadata};

    fn doc(kind: &str, spec_yaml: &str) -> ManifestDoc {
        ManifestDoc {
            api_version: "delonix.io/v1".into(),
            kind: kind.into(),
            metadata: Metadata {
                name: "t".into(),
                namespace: None,
                labels: Default::default(),
                annotations: Default::default(),
            },
            spec: serde_yaml::from_str(spec_yaml).unwrap(),
        }
    }

    fn env(rootless: bool, nfs: bool, cifs: bool, davfs: bool) -> Env {
        // cloud_hypervisor: true by default in tests that do not exercise it.
        Env {
            rootless,
            mount_nfs: nfs,
            mount_cifs: cifs,
            mount_davfs: davfs,
            cloud_hypervisor: true,
            // wg present by default: the tests that exercise it say so.
            wg: true,
            // No physical plane by default — the tests that care name theirs.
            realized_networks: Default::default(),
        }
    }

    /// `Env` with the given networks actually realized on the host.
    fn env_with_networks(names: &[&str]) -> Env {
        Env {
            realized_networks: names.iter().map(|s| s.to_string()).collect(),
            ..env(false, true, true, true)
        }
    }

    /// A montagem de rede é declarada por um BLOCO num `kind: Volume` desde que
    /// o `kind: Storage` se fundiu nele — e é do bloco que o tipo vem.
    #[test]
    fn share_de_rede_em_rootless_exige_cap_sys_admin() {
        let c = conditions_for(
            &doc("Volume", "nfs: { server: nas, share: /x }"),
            &env(true, true, true, true),
        );
        assert_eq!(c.len(), 1);
        assert!(!c[0].ok);
        assert_eq!(c[0].reason, "RequiresCapSysAdmin");
    }

    #[test]
    fn share_de_rede_sem_helper_assinala_helper_em_falta() {
        // cifs needs mount.cifs; absent → MountHelperMissing.
        let c = conditions_for(
            &doc("Volume", "cifs: { server: nas, share: media }"),
            &env(false, true, false, true),
        );
        assert_eq!(c[0].reason, "MountHelperMissing");
        // with the helper present → OK.
        let c = conditions_for(
            &doc("Volume", "cifs: { server: nas, share: media }"),
            &env(false, true, true, true),
        );
        assert!(c[0].ok);
    }

    /// **A regressão que isto fixa.** A condição era despachada por
    /// `kind: Storage`; quando esse Kind se fundiu no `Volume`, deixou de haver
    /// documentos com aquele nome e a condição cujo trabalho inteiro é dizer
    /// «criado, mas não monta» ficou MUDA. Um volume local não a tem; um com
    /// bloco de rede tem sempre.
    #[test]
    fn um_volume_local_nao_tem_condicao_de_montagem_e_um_de_rede_tem() {
        let local = conditions_for(
            &doc("Volume", "driver: local"),
            &env(true, true, true, true),
        );
        assert!(
            local.iter().all(|c| c.kind != "Mounted"),
            "um volume local não monta nada: {local:?}"
        );
        let rede = conditions_for(
            &doc("Volume", "nfs: { server: nas, share: /x }"),
            &env(true, true, true, true),
        );
        assert!(
            rede.iter().any(|c| c.kind == "Mounted" && !c.ok),
            "a condição de montagem voltou a ficar muda: {rede:?}"
        );
    }

    #[test]
    fn volume_quota_rootless_e_so_monitorizada() {
        let c = conditions_for(&doc("Volume", "quota: 2g"), &env(true, true, true, true));
        assert_eq!(c[0].reason, "RequiresRoot");
        // root with quota → OK.
        let c = conditions_for(&doc("Volume", "quota: 2g"), &env(false, true, true, true));
        assert!(c[0].ok);
        // no quota → no condition.
        assert!(conditions_for(
            &doc("Volume", "driver: local"),
            &env(true, true, true, true)
        )
        .is_empty());
    }

    /// **«Realizável» não é «realizada», e a condição respondia à segunda
    /// pergunta com a primeira.**
    ///
    /// O `Realized` saía inteiramente do YAML: dizia `False` porque o documento
    /// dizia `macvlan`, nunca porque alguém tivesse olhado para a máquina. Uma
    /// rede cujo plano físico desapareceu — `NetDef` apagado, `rm` a meio —
    /// reportava `Realized=True`, o `stack plan` dizia «sem alterações», e o
    /// `container run --net <nome>` falhava com «does not exist».
    ///
    /// Lê o registo do plano FÍSICO e não a bridge: aquele é um ficheiro que
    /// sobrevive ao holder, esta vive na netns efémera e num nó ocioso não
    /// existe — sondá-la daria por partida toda a rede saudável, que é
    /// exactamente o erro que o `NetworkRoute` evitou ao planear contra o
    /// registo.
    #[test]
    fn uma_rede_sem_plano_fisico_e_assinalada() {
        let d = doc("Network", "driver: bridge");
        // Realizada de facto → nada a dizer.
        assert!(network_not_realized(&d, &env_with_networks(&["t"])).is_none());
        // No registo declarativo mas sem plano físico → é isto que era invisível.
        let c = network_not_realized(&d, &env_with_networks(&["outra"])).expect("condição");
        assert_eq!(c.reason, "NotRealized");
        assert!(!c.ok);
        // Um driver que NUNCA tem plano físico já tem razão própria, mais
        // específica — não se lhe sobrepõe uma genérica.
        for drv in ["macvlan", "ipvlan"] {
            assert!(
                network_not_realized(
                    &doc("Network", &format!("driver: {drv}")),
                    &env_with_networks(&[])
                )
                .is_none(),
                "{drv}"
            );
        }
        // E não se aplica a outro Kind nenhum.
        assert!(
            network_not_realized(&doc("Volume", "driver: local"), &env_with_networks(&[]))
                .is_none()
        );
    }

    #[test]
    fn network_driver_nao_implementado_e_assinalado() {
        // These two genuinely have no physical plane without CAP_NET_ADMIN in
        // the host's init-netns. `overlay` is NOT one of them — the previous
        // version of this test looped over all three and so FIXED THE BUG in
        // place: the one networking fundamental this engine implements was
        // reported to the user as unimplemented, and the test agreed.
        for d in ["macvlan", "ipvlan"] {
            let c = conditions_for(
                &doc("Network", &format!("driver: {d}")),
                &env(false, true, true, true),
            );
            assert_eq!(c[0].reason, "DriverNotImplemented", "driver {d}");
        }
        let c = conditions_for(
            &doc("Network", "driver: bridge"),
            &env(false, true, true, true),
        );
        assert!(c[0].ok);
    }

    /// A plain overlay is realized in the holder (bridge + VXLAN uplink), with
    /// no host privilege. Reverting the fix makes this fail.
    #[test]
    fn overlay_simples_e_realizado() {
        let c = conditions_for(
            &doc("Network", "driver: overlay\nvni: 42"),
            &env(true, true, true, true),
        );
        assert!(c[0].ok, "overlay reported as not realized: {:?}", c[0]);
    }

    /// The prerequisite that IS real: an encrypted overlay needs `wg`, and
    /// `realize_overlay` refuses without it rather than blackholing the uplink.
    #[test]
    fn overlay_cifrado_sem_wg_e_assinalado() {
        let no_wg = Env {
            wg: false,
            ..env(true, true, true, true)
        };
        let c = conditions_for(
            &doc("Network", "driver: overlay\nvni: 42\nwgIp: 10.9.0.1"),
            &no_wg,
        );
        assert_eq!(c[0].reason, "WireguardMissing");
        // …and with `wg` on the host it is realized like any other overlay.
        let c = conditions_for(
            &doc("Network", "driver: overlay\nvni: 42\nwgIp: 10.9.0.1"),
            &env(true, true, true, true),
        );
        assert!(c[0].ok);
    }

    #[test]
    fn vm_volumes_com_ch_explicito_exige_libvirt() {
        // volumes + explicit cloud-hypervisor backend → condition.
        let c = conditions_for(
            &doc(
                "VirtualMachine",
                "disk: d\nbackend: cloud-hypervisor\nvolumes: [ { name: x, mountPath: /x } ]",
            ),
            &env(false, true, true, true),
        );
        assert!(
            c.iter()
                .any(|x| x.reason == "BackendCloudHypervisor" && x.kind == "VolumesRequireLibvirt"),
            "{c:?}"
        );
        // volumes with no explicit backend (auto → libvirt) → without this condition.
        let c = conditions_for(
            &doc(
                "VirtualMachine",
                "disk: d\nvolumes: [ { name: x, mountPath: /x } ]",
            ),
            &env(false, true, true, true),
        );
        assert!(
            !c.iter().any(|x| x.kind == "VolumesRequireLibvirt"),
            "{c:?}"
        );
    }

    /// A third backend existed and the condition spoke as if there were two.
    ///
    /// `proxmox` is registered on a host that configures it
    /// (`vmbackends::register_configured`), so `backend: proxmox` is a manifest
    /// anyone can write. It used to come back with the reason
    /// `BackendCloudHypervisor` and a sentence about Cloud Hypervisor — naming a
    /// backend the author had not chosen, which sends them to read the wrong
    /// documentation.
    #[test]
    fn an_unsupervised_backend_is_named_by_its_own_name() {
        let c = conditions_for(
            &doc(
                "VirtualMachine",
                "disk: d\nrestartPolicy: always\nbackend: proxmox",
            ),
            &env(false, true, true, true),
        );
        assert_eq!(c[0].reason, "BackendDoesNotSupervise");
        assert!(
            c[0].message.contains("proxmox"),
            "the message must name proxmox: {}",
            c[0].message
        );
        assert!(
            !c[0].message.contains("Cloud Hypervisor"),
            "and must NOT name a backend the author did not choose: {}",
            c[0].message
        );
    }

    #[test]
    fn vm_restart_no_cloud_hypervisor_nao_e_supervisionado() {
        // backend absent (auto → CH) + canonical restartPolicy → not supervised.
        let c = conditions_for(
            &doc("VirtualMachine", "disk: d\nrestartPolicy: always"),
            &env(false, true, true, true),
        );
        assert_eq!(c[0].reason, "BackendDoesNotSupervise");
        assert!(
            c[0].message.contains("cloud-hypervisor"),
            "the message must name the backend that will actually boot: {}",
            c[0].message
        );
        // legacy alias restart_policy + libvirt backend → supervised.
        let c = conditions_for(
            &doc(
                "VirtualMachine",
                "disk: d\nrestart_policy: always\nbackend: libvirt",
            ),
            &env(false, true, true, true),
        );
        assert!(c[0].ok);
        // Fix #3: backend ABSENT (auto) on a host WITHOUT cloud-hypervisor → falls
        // back to libvirt → supervised (does not warn needlessly).
        let sem_ch = Env {
            cloud_hypervisor: false,
            ..env(false, true, true, true)
        };
        let c = conditions_for(
            &doc("VirtualMachine", "disk: d\nrestartPolicy: always"),
            &sem_ch,
        );
        assert!(
            c[0].ok,
            "sem cloud-hypervisor o auto cai para libvirt, que supervisiona"
        );
        // no restartPolicy (or `no`) → no condition.
        assert!(conditions_for(
            &doc("VirtualMachine", "disk: d"),
            &env(false, true, true, true)
        )
        .is_empty());
        assert!(conditions_for(
            &doc("VirtualMachine", "disk: d\nrestartPolicy: no"),
            &env(false, true, true, true)
        )
        .is_empty());
    }
}
