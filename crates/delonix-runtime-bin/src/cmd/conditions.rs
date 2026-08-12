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

use super::manifest::ManifestDoc;

/// A condition of a resource — `ok=false` is what matters (the missing
/// prerequisite). `reason` is a short stable code; `message` is actionable.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Condition {
    pub kind: &'static str,
    pub ok: bool,
    pub reason: &'static str,
    pub message: String,
}

impl Condition {
    fn ok(kind: &'static str) -> Self {
        Condition {
            kind,
            ok: true,
            reason: "",
            message: String::new(),
        }
    }
    fn bad(kind: &'static str, reason: &'static str, message: impl Into<String>) -> Self {
        Condition {
            kind,
            ok: false,
            reason,
            message: message.into(),
        }
    }
}

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
}

impl Env {
    /// Probes the host for real. Reuses `delonix_runtime::is_rootless` (the
    /// canonical privilege helper, the same one the rest of the runtime uses).
    pub fn probe() -> Env {
        Env {
            rootless: delonix_runtime::is_rootless(),
            mount_nfs: which("mount.nfs"),
            mount_cifs: which("mount.cifs"),
            mount_davfs: which("mount.davfs"),
            cloud_hypervisor: which("cloud-hypervisor"),
            // The SAME function `realize_overlay` gates on, not a `which("wg")`
            // lookalike: a condition that disagrees with the realizer is worse
            // than no condition at all.
            wg: delonix_net::wg::available(),
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
        "Volume" => {
            let mut c = volume(doc, env);
            c.extend(network_share(doc, env));
            c
        }
        "Network" => network(doc, env),
        "Vm" => {
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
    // exists, otherwise it falls back to libvirt. Only libvirt supervises the restart.
    let backend = match spec_str(doc, &["backend"]) {
        Some(b) => b.to_string(),
        None if env.cloud_hypervisor => "cloud-hypervisor".to_string(),
        None => "libvirt".to_string(),
    };
    if backend == "libvirt" {
        vec![Condition::ok("RestartSupervised")]
    } else {
        vec![Condition::bad(
            "RestartSupervised",
            "BackendCloudHypervisor",
            super::po::tf(
                "restartPolicy '{policy}' is NOT supervised on Cloud Hypervisor — use `backend: libvirt` to materialize it",
                &[("policy", policy)],
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

#[cfg(test)]
mod tests {
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
                "Vm",
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
            &doc("Vm", "disk: d\nvolumes: [ { name: x, mountPath: /x } ]"),
            &env(false, true, true, true),
        );
        assert!(
            !c.iter().any(|x| x.kind == "VolumesRequireLibvirt"),
            "{c:?}"
        );
    }

    #[test]
    fn vm_restart_no_cloud_hypervisor_nao_e_supervisionado() {
        // backend absent (auto → CH) + canonical restartPolicy → not supervised.
        let c = conditions_for(
            &doc("Vm", "disk: d\nrestartPolicy: always"),
            &env(false, true, true, true),
        );
        assert_eq!(c[0].reason, "BackendCloudHypervisor");
        // legacy alias restart_policy + libvirt backend → supervised.
        let c = conditions_for(
            &doc("Vm", "disk: d\nrestart_policy: always\nbackend: libvirt"),
            &env(false, true, true, true),
        );
        assert!(c[0].ok);
        // Fix #3: backend ABSENT (auto) on a host WITHOUT cloud-hypervisor → falls
        // back to libvirt → supervised (does not warn BackendCloudHypervisor needlessly).
        let sem_ch = Env {
            cloud_hypervisor: false,
            ..env(false, true, true, true)
        };
        let c = conditions_for(&doc("Vm", "disk: d\nrestartPolicy: always"), &sem_ch);
        assert!(
            c[0].ok,
            "sem cloud-hypervisor o auto cai para libvirt, que supervisiona"
        );
        // no restartPolicy (or `no`) → no condition.
        assert!(conditions_for(&doc("Vm", "disk: d"), &env(false, true, true, true)).is_empty());
        assert!(conditions_for(
            &doc("Vm", "disk: d\nrestartPolicy: no"),
            &env(false, true, true, true)
        )
        .is_empty());
    }
}
