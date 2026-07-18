//! Conditions de honestidade — a peça que impede um recurso de MENTIR por
//! omissão. Vários Kinds aplicam best-effort e, quando um pré-requisito de
//! privilégio/host falta, o recurso é criado mas não faz o que aparenta (um
//! `Storage` NFS em rootless não monta; uma quota dura em rootless só é
//! monitorizada; um `Network` macvlan fica em registo sem plano físico; um
//! `restartPolicy` numa VM Cloud Hypervisor não é supervisionado). Em vez de
//! deixar isto em silêncio, cada Kind pode declarar `conditions` (estilo
//! kubectl: um estado booleano + `reason` accionável) que o `stack describe`
//! mostra ao utilizador.
//!
//! **Sem estado persistido**: as conditions são CALCULADAS a partir do spec + um
//! probe do ambiente, na hora — a mesma filosofia "o stack não tem registo
//! próprio" do `describe`. `conditions_for` é puro (recebe o `Env` já probado),
//! para ser testável sem depender do estado real da máquina.

use super::manifest::ManifestDoc;

/// Uma condition de um recurso — `ok=false` é o que interessa (o pré-requisito
/// em falta). `reason` é um código curto estável; `message` é accionável.
#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    pub kind: &'static str,
    pub ok: bool,
    pub reason: &'static str,
    pub message: String,
}

impl Condition {
    fn ok(kind: &'static str) -> Self {
        Condition { kind, ok: true, reason: "", message: String::new() }
    }
    fn bad(kind: &'static str, reason: &'static str, message: impl Into<String>) -> Self {
        Condition { kind, ok: false, reason, message: message.into() }
    }
}

/// Ambiente probado do host (best-effort). Campos explícitos = `conditions_for`
/// puro e testável sem tocar no host real.
#[derive(Debug, Clone)]
pub struct Env {
    /// Sem privilégio de root (o `mount -t` de rede e a quota dura precisam de
    /// CAP_SYS_ADMIN, que uma sessão rootless não tem no namespace de init).
    pub rootless: bool,
    /// Helper `mount.nfs` presente no PATH.
    pub mount_nfs: bool,
    /// Helper `mount.cifs` presente no PATH.
    pub mount_cifs: bool,
    /// Helper `mount.davfs` presente no PATH.
    pub mount_davfs: bool,
    /// Binário `cloud-hypervisor` disponível — decide o backend AUTO da VM
    /// (presente → CH; ausente → cai para libvirt). Espelha `select_backend`.
    pub cloud_hypervisor: bool,
}

impl Env {
    /// Proba o host de verdade. Reutiliza `delonix_runtime::is_rootless` (o
    /// helper canónico de privilégio, o mesmo que o resto do runtime usa).
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

/// `binário está no PATH?` — varre `$PATH` MAIS os directórios sbin canónicos.
/// Os helpers de mount (`mount.nfs`/`mount.cifs`/`mount.davfs`) vivem em
/// `/sbin`/`/usr/sbin`, que muitas vezes NÃO estão no `$PATH` de uma sessão de
/// utilizador — sem os incluir, a condition reportaria `MountHelperMissing`
/// quando o helper existe (honestidade a virar desinformação).
fn which(bin: &str) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let sbins = ["/sbin", "/usr/sbin", "/usr/local/sbin"].map(std::path::PathBuf::from);
    std::env::split_paths(&path).chain(sbins).any(|dir| dir.join(bin).is_file())
}

/// Lê um campo string de topo do `spec` cru, aceitando qualquer um de `keys`
/// (para cobrir o canónico E o alias legado — ex.: `restartPolicy`/`restart_policy`).
fn spec_str<'a>(doc: &'a ManifestDoc, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| doc.spec.get(k).and_then(|v| v.as_str()))
}

/// As conditions de um documento. Vazio = nada a assinalar (o caso comum).
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

/// `Storage.Mounted` — montar NFS/CIFS/WebDAV precisa de CAP_SYS_ADMIN e do
/// helper de mount certo no host; sem qualquer um, o volume é criado mas o mount
/// falha em silêncio (best-effort). Ver `delonix-volume::ensure_mounted`.
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

/// `Volume.QuotaEnforced` — a quota dura usa um loopback ext4 (`losetup`), que
/// exige root; em rootless só há alerta monitorizado, sem cap real. Sem quota
/// declarada, não há nada a assinalar.
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

/// `Network.Realized` — só o driver `bridge` tem plano físico (bridge do holder
/// rootless); `macvlan`/`ipvlan`/`overlay` ficam no `NetworkStore` sem nada que
/// um container consiga atachar. Ver a nota em `cmd::network`.
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

/// `Vm.RestartSupervised` — só o backend libvirt materializa a política de
/// restart (via `<on_crash>` no XML); o Cloud Hypervisor (default do auto) não a
/// supervisiona. Sem `restartPolicy` (ou `no`), não há nada a assinalar.
fn vm(doc: &ManifestDoc, env: &Env) -> Vec<Condition> {
    let policy = spec_str(doc, &["restartPolicy", "restart_policy"]).unwrap_or("no");
    if policy.is_empty() || policy == "no" {
        return Vec::new();
    }
    // Que backend ARRANCA de facto — espelha `select_backend`: explícito manda;
    // no auto (backend ausente) prefere-se Cloud Hypervisor SE o binário existir,
    // senão cai para libvirt. Só o libvirt supervisiona o restart.
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

/// `Vm.VolumesRequireLibvirt` — `spec.volumes` só é materializável pelo backend
/// libvirt (virtio-9p; o Cloud Hypervisor não o suporta). O apply auto-selecciona
/// libvirt quando não há backend explícito; isto assinala o caso em que o
/// utilizador FORÇA `backend: cloud-hypervisor` com volumes (o boot recusaria).
fn vm_volumes(doc: &ManifestDoc) -> Vec<Condition> {
    let has_volumes = doc.spec.get("volumes").and_then(|v| v.as_sequence()).is_some_and(|s| !s.is_empty());
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
            metadata: Metadata { name: "t".into(), labels: Default::default(), annotations: Default::default() },
            spec: serde_yaml::from_str(spec_yaml).unwrap(),
        }
    }

    fn env(rootless: bool, nfs: bool, cifs: bool, davfs: bool) -> Env {
        // cloud_hypervisor: true por defeito nos testes que não o exercitam.
        Env { rootless, mount_nfs: nfs, mount_cifs: cifs, mount_davfs: davfs, cloud_hypervisor: true }
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
        // cifs precisa de mount.cifs; ausente → MountHelperMissing.
        let c = conditions_for(&doc("Storage", "type: cifs"), &env(false, true, false, true));
        assert_eq!(c[0].reason, "MountHelperMissing");
        // com o helper presente → OK.
        let c = conditions_for(&doc("Storage", "type: cifs"), &env(false, true, true, true));
        assert!(c[0].ok);
    }

    #[test]
    fn volume_quota_rootless_e_so_monitorizada() {
        let c = conditions_for(&doc("Volume", "quota: 2g"), &env(true, true, true, true));
        assert_eq!(c[0].reason, "RequiresRoot");
        // root com quota → OK.
        let c = conditions_for(&doc("Volume", "quota: 2g"), &env(false, true, true, true));
        assert!(c[0].ok);
        // sem quota → nenhuma condition.
        assert!(conditions_for(&doc("Volume", "driver: local"), &env(true, true, true, true)).is_empty());
    }

    #[test]
    fn network_driver_nao_implementado_e_assinalado() {
        for d in ["macvlan", "ipvlan", "overlay"] {
            let c = conditions_for(&doc("Network", &format!("driver: {d}")), &env(false, true, true, true));
            assert_eq!(c[0].reason, "DriverNotImplemented", "driver {d}");
        }
        let c = conditions_for(&doc("Network", "driver: bridge"), &env(false, true, true, true));
        assert!(c[0].ok);
    }

    #[test]
    fn vm_volumes_com_ch_explicito_exige_libvirt() {
        // volumes + backend cloud-hypervisor explícito → condition.
        let c = conditions_for(&doc("Vm", "disk: d\nbackend: cloud-hypervisor\nvolumes: [ { name: x, mountPath: /x } ]"), &env(false, true, true, true));
        assert!(c.iter().any(|x| x.reason == "BackendCloudHypervisor" && x.kind == "VolumesRequireLibvirt"), "{c:?}");
        // volumes sem backend explícito (auto → libvirt) → sem esta condition.
        let c = conditions_for(&doc("Vm", "disk: d\nvolumes: [ { name: x, mountPath: /x } ]"), &env(false, true, true, true));
        assert!(!c.iter().any(|x| x.kind == "VolumesRequireLibvirt"), "{c:?}");
    }

    #[test]
    fn vm_restart_no_cloud_hypervisor_nao_e_supervisionado() {
        // backend ausente (auto → CH) + restartPolicy canónico → não supervisionado.
        let c = conditions_for(&doc("Vm", "disk: d\nrestartPolicy: always"), &env(false, true, true, true));
        assert_eq!(c[0].reason, "BackendCloudHypervisor");
        // alias legado restart_policy + backend libvirt → supervisionado.
        let c = conditions_for(&doc("Vm", "disk: d\nrestart_policy: always\nbackend: libvirt"), &env(false, true, true, true));
        assert!(c[0].ok);
        // Fix #3: backend AUSENTE (auto) num host SEM cloud-hypervisor → cai para
        // libvirt → supervisionado (não avisa BackendCloudHypervisor à toa).
        let sem_ch = Env { rootless: false, mount_nfs: true, mount_cifs: true, mount_davfs: true, cloud_hypervisor: false };
        let c = conditions_for(&doc("Vm", "disk: d\nrestartPolicy: always"), &sem_ch);
        assert!(c[0].ok, "sem cloud-hypervisor o auto cai para libvirt, que supervisiona");
        // sem restartPolicy (ou `no`) → nenhuma condition.
        assert!(conditions_for(&doc("Vm", "disk: d"), &env(false, true, true, true)).is_empty());
        assert!(conditions_for(&doc("Vm", "disk: d\nrestartPolicy: no"), &env(false, true, true, true)).is_empty());
    }
}
