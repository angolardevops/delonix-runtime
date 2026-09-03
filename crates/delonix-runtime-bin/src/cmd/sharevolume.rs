//! The write path for a SHARE — a `kind: Volume` with a `share:` block (what
//! `kind: ShareVolume` used to be a Kind of its own for, and what `delonix
//! sharevolume` used to be a whole command group for — B5 CLI collapse).
//!
//! **Mechanism (deliberately no new mount machinery)**: `spec.share.from`
//! names an already-mounted volume (typically a network one — see
//! `cmd::storage`). A share is a REAL subdirectory of that volume's
//! mountpoint (`<parent-mountpoint>/shares/<namespace>/<name>`), registered
//! as its OWN named `delonix-volume` volume via
//! `VolumeStore::register_external` — a volume whose `mountpoint` points
//! OUTSIDE the store's usual `_data` convention. Two consequences fall out
//! for free, with zero new code:
//! - **Isolation** is plain path confinement: a container that bind-mounts
//!   `-v <share>:/data` only ever sees ITS subdirectory — it cannot reach a
//!   sibling's without traversing `..`, which no mount here allows.
//! - **Consumption needs nothing new**: `container run -v <name>:/target`
//!   already resolves a named volume purely by reading its
//!   `Volume.mountpoint` — a share-registered volume is indistinguishable to
//!   that code from any other named volume.
//!
//! **Reading a share back is `volume ls`/`describe`'s job now, not this
//! module's**: a share IS a `Volume` record (`.parent` set), so listing,
//! describing and removing one go through the exact same code every other
//! volume does (`cmd::volume::cmd_ls`/`describe_one`/`cmd_rm_with`), with
//! `--purge-data` covering what `sharevolume rm --purge-data` used to.
//!
//! **Quota is SOFT only** (measured usage + alert threshold) — the HARD
//! quota path (`delonix-volume`'s ext4-loopback-image) needs local block
//! storage and doesn't compose with a subdirectory of a network mount; this
//! is stated up front rather than silently downgraded.

use delonix_runtime_core::{Error, Result};
use delonix_volume::VolumeStore;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(crate) struct ShareVolumeSpec {
    /// Name of an existing `kind: Storage` (a network-backed `delonix-volume`).
    #[serde(rename = "storageRef")]
    storage_ref: String,
    /// Human size (`5G`, `500M`, ...). Omit = unlimited (still measured/shown).
    #[serde(default)]
    quota: Option<String>,
    /// Usage percentage above which `ls`/`describe` flag a WARN (default 90).
    #[serde(default, rename = "alertPct")]
    alert_pct: Option<u8>,
}

pub const SHAREVOLUME_SPEC_FIELDS: &[&str] = &["storageRef", "quota", "alertPct"];

/// Applies one `kind: Volume` that carries a `share:` block.
///
/// The single entry point from the declarative side (`cmd::volume::apply`)
/// AND from the imperative one (`volume create --parent`) — one function,
/// so the two paths cannot disagree about what "ensure this share exists"
/// means.
pub(crate) fn apply_share(
    name: &str,
    from: &str,
    quota: Option<&str>,
    alert_pct: Option<u8>,
    namespace: &str,
) -> Result<()> {
    let spec = ShareVolumeSpec {
        storage_ref: from.to_string(),
        quota: quota.map(str::to_string),
        alert_pct,
    };
    apply_one(&super::util::state_root(), name, &spec, namespace)
}

/// A namespace safe as ONE path component. Rejects the traversal and separator cases
/// instead of sanitizing them into something else: a namespace that silently becomes a
/// different namespace is how a tenant reads another tenant's shares.
fn safe_ns(namespace: &str) -> String {
    if namespace.is_empty()
        || namespace == "."
        || namespace == ".."
        || namespace.contains('/')
        || namespace.starts_with('.')
    {
        return "default".to_string();
    }
    namespace.to_string()
}

fn apply_one(root: &Path, name: &str, spec: &ShareVolumeSpec, namespace: &str) -> Result<()> {
    let namespace = safe_ns(namespace);
    // The parent Storage is NOT namespaced: it is the NAS mount itself, node
    // infrastructure, and scoping it would mean one mount per namespace of the same
    // export. What gets scoped is the SHARE carved out of it.
    let vstore = VolumeStore::open(root)?;
    let scoped = VolumeStore::open_scoped(root, &namespace)?;
    // An already-existing share keeps the path it was created with. `apply` is
    // "ensure present", so recomputing the path on a re-apply would move the
    // share's data out from under it and orphan every byte already written there.
    let existing_mountpoint = scoped.inspect(name).ok().map(|v| v.mountpoint);
    let parent = vstore.inspect(&spec.storage_ref).map_err(|_| {
        Error::Invalid(super::po::tf(
            "ShareVolume '{name}': storageRef '{storage_ref}' does not exist — create it first \
             (`delonix volume create --type <nfs|cifs|smb|webdav> ...` / `kind: Volume`)",
            &[("name", name), ("storage_ref", &spec.storage_ref)],
        ))
    })?;
    let quota_bytes = spec
        .quota
        .as_deref()
        .map(|q| {
            delonix_volume::parse_size_bytes(q).ok_or_else(|| {
                Error::Invalid(super::po::tf(
                    "invalid quota: {q}",
                    &[("q", &format!("{q:?}"))],
                ))
            })
        })
        .transpose()?;

    // `register_external`'s own name-charset validation runs BEFORE it
    // touches disk — this join can't escape `<parent>/shares/` with a name
    // that will end up being rejected anyway.
    // Shares live under their namespace: `<storage>/shares/<ns>/<name>`. Without the
    // namespace component two tenants that both call their share `db` get the SAME
    // directory — and `--purge-data` on one deletes the other's data.
    let subdir = match &existing_mountpoint {
        Some(mp) => std::path::PathBuf::from(mp),
        None => Path::new(&parent.mountpoint)
            .join("shares")
            .join(&namespace)
            .join(name),
    };
    let vol = scoped.register_external(
        name,
        &subdir,
        quota_bytes,
        spec.alert_pct,
        Some(&spec.storage_ref),
    )?;
    println!(
        "volume/{name}: {} ({} -> {})",
        super::po::t("ready"),
        spec.storage_ref,
        vol.mountpoint
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "delonix-sharevolume-test-{tag}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            seq
        ))
    }

    #[test]
    fn um_share_e_possuivel_por_uma_stack() {
        let tmp = tmp_root("owned");
        let vstore = VolumeStore::open(&tmp).unwrap();
        vstore.create("nas-shared").unwrap();
        apply_one(
            &tmp,
            "db",
            &ShareVolumeSpec {
                storage_ref: "nas-shared".into(),
                quota: None,
                alert_pct: None,
            },
            "teamA",
        )
        .unwrap();
        let scoped = VolumeStore::open_scoped(&tmp, "teamA").unwrap();
        scoped
            .set_metadata(
                "db",
                &[("delonix.io/stack".to_string(), Some("loja".to_string()))],
                &[],
            )
            .unwrap();
        let v = scoped.inspect("db").unwrap();
        assert_eq!(
            v.labels.get("delonix.io/stack").map(String::as_str),
            Some("loja")
        );
        assert_eq!(v.parent.as_deref(), Some("nas-shared"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_recusa_storage_ref_inexistente() {
        let tmp = tmp_root("no-parent");
        let spec = ShareVolumeSpec {
            storage_ref: "nao-existe".to_string(),
            quota: None,
            alert_pct: None,
        };
        let err = apply_one(&tmp, "sv1", &spec, "default").unwrap_err();
        assert!(format!("{err}").contains("storageRef"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_e_idempotente_e_isola_por_subdirectorio() {
        let tmp = tmp_root("idempotent");
        let vstore = VolumeStore::open(&tmp).unwrap();
        // The parent "Storage" — a plain local volume stands in for a
        // network one here (register_external doesn't care which).
        vstore.create("nas-shared").unwrap();
        let spec = ShareVolumeSpec {
            storage_ref: "nas-shared".to_string(),
            quota: Some("1M".to_string()),
            alert_pct: Some(80),
        };
        apply_one(&tmp, "tenant-a", &spec, "default").unwrap();
        apply_one(&tmp, "tenant-b", &spec, "default").unwrap();

        let scoped = VolumeStore::open_scoped(&tmp, "default").unwrap();
        let a = scoped.inspect("tenant-a").unwrap();
        let b = scoped.inspect("tenant-b").unwrap();
        assert_ne!(
            a.mountpoint, b.mountpoint,
            "cada tenant tem o SEU subdirectório"
        );
        assert!(a.mountpoint.contains("nas-shared"));
        assert!(a.mountpoint.ends_with("tenant-a"));
        assert_eq!(a.quota_bytes, Some(1024 * 1024));

        // Idempotent re-apply: same name, `created_unix` preserved.
        std::thread::sleep(std::time::Duration::from_millis(5));
        apply_one(&tmp, "tenant-a", &spec, "default").unwrap();
        let a2 = scoped.inspect("tenant-a").unwrap();
        assert_eq!(a.created_unix, a2.created_unix);
        assert_eq!(
            a.mountpoint, a2.mountpoint,
            "um re-apply não pode mudar o directório debaixo dos dados"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// O invariante do B2: dois inquilinos, o MESMO nome, e caminhos distintos.
    ///
    /// Antes do escopo, o caminho dos dados era `<storage>/shares/<nome>` — sem namespace —
    /// por isso dois `db` partilhavam o directório. O teste afirma as duas coisas que têm de
    /// valer ao mesmo tempo: caminhos distintos, e `-v db` a resolver para o `db` da SUA
    /// namespace.
    #[test]
    fn dois_namespaces_com_o_mesmo_share_nao_se_tocam() {
        let tmp = tmp_root("two-ns");
        let vstore = VolumeStore::open(&tmp).unwrap();
        vstore.create("nas-shared").unwrap();
        let spec = ShareVolumeSpec {
            storage_ref: "nas-shared".to_string(),
            quota: None,
            alert_pct: None,
        };
        apply_one(&tmp, "db", &spec, "teamA").unwrap();
        apply_one(&tmp, "db", &spec, "teamB").unwrap();

        let a = VolumeStore::open_scoped(&tmp, "teamA")
            .unwrap()
            .inspect("db")
            .unwrap();
        let b = VolumeStore::open_scoped(&tmp, "teamB")
            .unwrap()
            .inspect("db")
            .unwrap();
        assert_ne!(
            a.mountpoint, b.mountpoint,
            "cada namespace tem o SEU caminho"
        );
        assert!(a.mountpoint.contains("teamA"), "{}", a.mountpoint);
        assert!(b.mountpoint.contains("teamB"), "{}", b.mountpoint);

        // `-v db:/data` resolve para o db da namespace de quem monta, nao para o outro.
        let m = vstore.resolve_spec_in("db:/data", "teamB").unwrap();
        assert_eq!(m.source, b.mountpoint, "teamB tem de receber o SEU db");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
