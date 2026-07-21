//! `delonix storage` — NETWORK storage mountable as a volume, inspired by
//! Kubernetes PersistentVolumes. A shared folder (NFS, SMB/CIFS, WebDAV)
//! from a NAS (TrueNAS, Synology, Nextcloud, …) becomes available as a named
//! volume that any container mounts with `-v <name>:/path`.
//!
//! Under the hood it is a `delonix-volume` volume with a network driver — the
//! `Storage` is the FRIENDLY declaration (server/share/credentials) that
//! translates into the mount `device`/`options`; `volumes ls` shows it with its driver.

use std::path::PathBuf;

use clap::Subcommand;
use delonix_runtime_core::{Error, Result};
use delonix_volume::VolumeStore;
use serde::Deserialize;

use super::manifest::{self, ManifestDoc};
use super::output;
use super::util::state_root;

#[derive(Subcommand)]
pub enum StorageCmd {
    /// Storage/volumes dashboard (KPIs + table) — TUI, or `--once` snapshot.
    Dash {
        #[arg(long)]
        once: bool,
    },
    /// Create (and mount) a network storage.
    Create {
        name: String,
        /// Type: `nfs` | `cifs`/`smb` (Samba/Windows) | `webdav` (Nextcloud/ownCloud).
        #[arg(long, value_parser = ["nfs", "cifs", "smb", "webdav"])]
        r#type: String,
        /// Server (host/IP), or the base URL in the `webdav` case.
        #[arg(long)]
        server: String,
        /// Export/share: NFS path (`/mnt/pool/media`), CIFS share name
        /// (`media`), or the path in the WebDAV URL (`/remote.php/dav/...`).
        #[arg(long)]
        share: String,
        /// User (cifs/webdav).
        #[arg(long)]
        username: Option<String>,
        /// Password (cifs/webdav) — prefer `--password-secret` to avoid exposing it.
        #[arg(long)]
        password: Option<String>,
        /// Vault secret with the `password` key (cifs/webdav) — does not leak in shell history.
        #[arg(long = "password-secret")]
        password_secret: Option<String>,
        /// Mount read-only.
        #[arg(long = "read-only")]
        read_only: bool,
        /// Extra mount options (`vers=4.1,soft`), appended to the derived ones.
        #[arg(long)]
        options: Option<String>,
    },
    /// List the network storages (volumes with a network driver).
    Ls,
    /// Details of a storage.
    Inspect {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::volumes))]
        name: String,
    },
    /// Remove (and unmount) a storage. The DATA stays on the NAS — only the
    /// local mount is torn down, like docker.
    Rm {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::volumes))]
        name: String,
    },
    /// Apply the `kind: Storage` documents from a manifest.
    Apply {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
}

/// `spec` of `kind: Storage`.
#[derive(Debug, Deserialize)]
struct StorageSpec {
    /// `nfs` | `cifs`/`smb` | `webdav`.
    r#type: String,
    server: String,
    share: String,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    /// Vault secret (`password` key).
    #[serde(default, rename = "passwordSecret")]
    password_secret: Option<String>,
    #[serde(default, rename = "readOnly")]
    read_only: bool,
    #[serde(default, rename = "mountOptions")]
    mount_options: Option<String>,
}

/// Names accepted in the `kind: Storage` `spec`, for the unknown-field warning.
pub(crate) const STORAGE_SPEC_FIELDS: &[&str] = &[
    "type",
    "server",
    "share",
    "username",
    "password",
    "passwordSecret",
    "readOnly",
    "mountOptions",
];

/// The mount parameters derived from a storage declaration.
struct MountSpec {
    driver: String,
    device: String,
    options: Option<String>,
}

/// Builds `(driver, device, options)` from the friendly declaration.
/// **Pure function** (secret resolution is done beforehand, by the caller) so the
/// type→device/options mapping is testable without touching the vault or mounting.
fn build_mount(
    r#type: &str,
    server: &str,
    share: &str,
    username: Option<&str>,
    password: Option<&str>,
    read_only: bool,
    extra: Option<&str>,
) -> Result<MountSpec> {
    let (driver, device) = match r#type {
        "nfs" => ("nfs", format!("{server}:{share}")),
        "cifs" | "smb" => (
            "cifs",
            format!("//{server}/{}", share.trim_start_matches('/')),
        ),
        "webdav" => {
            // server may already come with a scheme; otherwise assume https.
            let base = if server.contains("://") {
                server.to_string()
            } else {
                format!("https://{server}")
            };
            (
                "davfs",
                format!(
                    "{}/{}",
                    base.trim_end_matches('/'),
                    share.trim_start_matches('/')
                ),
            )
        }
        other => {
            return Err(Error::Invalid(format!(
                "tipo de storage desconhecido: '{other}' (nfs|cifs|smb|webdav)"
            )))
        }
    };
    // Options: credentials (cifs), ro, and the user's extras — in this order.
    let mut opts: Vec<String> = Vec::new();
    if driver == "cifs" {
        if let Some(u) = username {
            opts.push(format!("username={u}"));
        }
        if let Some(p) = password {
            opts.push(format!("password={p}"));
        }
    }
    if read_only {
        opts.push("ro".to_string());
    }
    if let Some(e) = extra {
        if !e.is_empty() {
            opts.push(e.to_string());
        }
    }
    let options = if opts.is_empty() {
        None
    } else {
        Some(opts.join(","))
    };
    Ok(MountSpec {
        driver: driver.to_string(),
        device,
        options,
    })
}

/// Resolves the password: inline `--password`, or the `password` key of a secret.
fn resolve_password(password: Option<String>, secret: Option<String>) -> Result<Option<String>> {
    if let Some(p) = password {
        return Ok(Some(p));
    }
    let Some(name) = secret else { return Ok(None) };
    let store = delonix_runtime_core::SecretStore::open(state_root())?;
    let s = store.load(&name)?;
    s.data.get("password").cloned().map(Some).ok_or_else(|| {
        Error::Invalid(super::po::tf(
            "secret '{name}' has no 'password' key",
            &[("name", &name)],
        ))
    })
}

pub fn run(action: StorageCmd) -> Result<()> {
    let store = VolumeStore::open(state_root())?;
    match action {
        StorageCmd::Dash { once } => {
            return super::dash::run(super::dash::DashScope::Storage, once)
        }
        StorageCmd::Create {
            name,
            r#type,
            server,
            share,
            username,
            password,
            password_secret,
            read_only,
            options,
        } => {
            let pw = resolve_password(password, password_secret)?;
            let m = build_mount(
                &r#type,
                &server,
                &share,
                username.as_deref(),
                pw.as_deref(),
                read_only,
                options.as_deref(),
            )?;
            let v = store.create_with(&name, &m.driver, Some(m.device.clone()), m.options)?;
            println!(
                "storage '{}' criado e montado ({} · {})",
                v.name, m.driver, m.device
            );
        }
        StorageCmd::Ls => {
            let mut t = output::Table::new(&["NAME", "TYPE", "DEVICE", "MOUNTPOINT"]);
            for v in store.list()? {
                if delonix_volume::is_network_driver(&v.driver) {
                    t.row(vec![
                        v.name,
                        v.driver,
                        v.device.unwrap_or_default(),
                        v.mountpoint,
                    ]);
                }
            }
            t.print();
        }
        StorageCmd::Inspect { name } => {
            let v = store.inspect(&name)?;
            let mut d = output::Describe::new();
            d.field("Name", &v.name);
            d.field("Type", &v.driver);
            d.field_opt("Device", v.device.as_deref());
            d.field("Mountpoint", &v.mountpoint);
            d.field_opt("Options", v.options.as_deref());
            d.field("Created", output::fmt_local(v.created_unix));
            d.print();
        }
        StorageCmd::Rm { name } => {
            store.remove(&name)?;
            println!("storage '{name}' removido (desmontado; os dados ficam no NAS)");
        }
        StorageCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)?;
        }
    }
    Ok(())
}

/// Applies the `kind: Storage` from a manifest (idempotent by name — the
/// store's `create_with` does not recreate one that already exists).
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let store = VolumeStore::open(state_root())?;
    for doc in manifest::of_kind(docs, "Storage") {
        let name = &doc.metadata.name;
        manifest::warn_unknown_fields(doc, STORAGE_SPEC_FIELDS);
        let spec: StorageSpec = manifest::spec_of(doc)?;
        let pw = resolve_password(spec.password, spec.password_secret)?;
        let m = build_mount(
            &spec.r#type,
            &spec.server,
            &spec.share,
            spec.username.as_deref(),
            pw.as_deref(),
            spec.read_only,
            spec.mount_options.as_deref(),
        )?;
        store.create_with(name, &m.driver, Some(m.device), m.options)?;
        println!("storage/{name}: garantido ({})", m.driver);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::build_mount;

    #[test]
    fn nfs_forma_servidor_export() {
        let m = build_mount(
            "nfs",
            "10.0.0.5",
            "/mnt/pool/media",
            None,
            None,
            false,
            None,
        )
        .unwrap();
        assert_eq!(m.driver, "nfs");
        assert_eq!(m.device, "10.0.0.5:/mnt/pool/media");
        assert!(m.options.is_none());
    }

    #[test]
    fn cifs_forma_unc_com_credenciais_e_ro() {
        let m = build_mount(
            "smb",
            "nas.local",
            "media",
            Some("alice"),
            Some("s3cr3t"),
            true,
            Some("vers=3.0"),
        )
        .unwrap();
        assert_eq!(m.driver, "cifs"); // smb is an alias of cifs
        assert_eq!(m.device, "//nas.local/media");
        let o = m.options.unwrap();
        assert!(o.contains("username=alice"));
        assert!(o.contains("password=s3cr3t"));
        assert!(o.contains("ro"));
        assert!(o.contains("vers=3.0"));
    }

    #[test]
    fn webdav_monta_url_https_por_omissao() {
        let m = build_mount(
            "webdav",
            "cloud.example.com",
            "/remote.php/dav/files/alice",
            Some("alice"),
            None,
            false,
            None,
        )
        .unwrap();
        assert_eq!(m.driver, "davfs");
        assert_eq!(
            m.device,
            "https://cloud.example.com/remote.php/dav/files/alice"
        );
    }

    #[test]
    fn webdav_respeita_esquema_explicito() {
        let m = build_mount(
            "webdav",
            "http://192.168.1.10:8080",
            "dav",
            None,
            None,
            false,
            None,
        )
        .unwrap();
        assert_eq!(m.device, "http://192.168.1.10:8080/dav");
    }

    #[test]
    fn tipo_invalido_e_erro() {
        assert!(build_mount("s3", "x", "y", None, None, false, None).is_err());
    }
}
