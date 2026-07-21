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
#[derive(Debug, Clone, PartialEq)]
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
        "Storage" => storage(doc, env),
        "Volume" => volume(doc, env),
        "Network" => network(doc),
        "Vm" => {
            let mut c = vm(doc, env);
            c.extend(vm_volumes(doc));
            c
        }
        _ => Vec::new(),
    }
}

/// `Storage.Mounted` — mounting NFS/CIFS/WebDAV needs CAP_SYS_ADMIN and the
/// right mount helper on the host; without either, the volume is created but the
/// mount fails silently (best-effort). See `delonix-volume::ensure_mounted`.
fn storage(doc: &ManifestDoc, env: &Env) -> Vec<Condition> {
    let ty = spec_str(doc, &["type"]).unwrap_or("nfs");
    if env.rootless {
        return vec![Condition::bad(
            "Mounted",
            "RequiresCapSysAdmin",
            format!("montar '{ty}' precisa de CAP_SYS_ADMIN — corre como root ou numa sessão privilegiada; em rootless o mount é best-effort e falha"),
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
            format!("o helper '{helper}' não está no PATH — instala-o no host para montar '{ty}'"),
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
            "a quota dura precisa de root (losetup/CAP_SYS_ADMIN) — em rootless é só MONITORIZADA, sem cap real",
        )]
    } else {
        vec![Condition::ok("QuotaEnforced")]
    }
}

/// `Network.Realized` — only the `bridge` driver has a physical plane (the
/// rootless holder's bridge); `macvlan`/`ipvlan`/`overlay` stay in the
/// `NetworkStore` with nothing a container can attach to. See the note in
/// `cmd::network`.
fn network(doc: &ManifestDoc) -> Vec<Condition> {
    let driver = spec_str(doc, &["driver"]).unwrap_or("bridge");
    match driver {
        "macvlan" | "ipvlan" | "overlay" => vec![Condition::bad(
            "Realized",
            "DriverNotImplemented",
            format!("o driver '{driver}' ainda não tem plano físico — fica no registo mas os containers só atacham `bridge`"),
        )],
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
            format!("restartPolicy '{policy}' NÃO é supervisionado no Cloud Hypervisor — usa `backend: libvirt` para o materializar"),
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
            "spec.volumes usa virtio-9p, que só o backend libvirt materializa — remove `backend: cloud-hypervisor` (o apply escolhe libvirt sozinho quando há volumes)".to_string(),
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
        }
    }

    #[test]
    fn storage_rootless_exige_cap_sys_admin() {
        let c = conditions_for(&doc("Storage", "type: nfs"), &env(true, true, true, true));
        assert_eq!(c.len(), 1);
        assert!(!c[0].ok);
        assert_eq!(c[0].reason, "RequiresCapSysAdmin");
    }

    #[test]
    fn storage_root_sem_helper_assinala_helper_em_falta() {
        // cifs needs mount.cifs; absent → MountHelperMissing.
        let c = conditions_for(
            &doc("Storage", "type: cifs"),
            &env(false, true, false, true),
        );
        assert_eq!(c[0].reason, "MountHelperMissing");
        // with the helper present → OK.
        let c = conditions_for(&doc("Storage", "type: cifs"), &env(false, true, true, true));
        assert!(c[0].ok);
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
        for d in ["macvlan", "ipvlan", "overlay"] {
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
            rootless: false,
            mount_nfs: true,
            mount_cifs: true,
            mount_davfs: true,
            cloud_hypervisor: false,
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
