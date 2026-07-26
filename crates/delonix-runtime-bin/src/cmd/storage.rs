//! `delonix storage` — NETWORK storage mountable as a volume, inspired by
//! Kubernetes PersistentVolumes. A shared folder (NFS, SMB/CIFS, WebDAV)
//! from a NAS (TrueNAS, Synology, Nextcloud, …) becomes available as a named
//! volume that any container mounts with `-v <name>:/path`.
//!
//! Under the hood it is a `delonix-volume` volume with a network driver — the
//! `Storage` is the FRIENDLY declaration (server/share/credentials) that
//! translates into the mount `device`/`options`; `volumes ls` shows it with its driver.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use delonix_runtime_core::{Error, Result};
use delonix_volume::VolumeStore;
use serde::{Deserialize, Serialize};

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
#[derive(Debug, Deserialize, Serialize)]
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
/// **Pure function** (secret resolution AND the credentials file, if any,
/// are done beforehand, by the caller — see [`write_cifs_credentials`]) so
/// the type→device/options mapping is testable without touching the vault
/// or mounting.
///
/// BUG FOUND, fixed here: this used to take `username`/`password` directly
/// and inline them as `username=...,password=...` into the comma-joined
/// `-o` mount options string. Two real problems: (1) `mount.cifs` runs as
/// root (CAP_SYS_ADMIN required) with the password as a literal process
/// ARGUMENT — `/proc/<pid>/cmdline` is world-readable, so any local user
/// could read the NAS credential straight out of it while the mount ran,
/// defeating the entire point of `--password-secret`/`kind: Secret`; (2)
/// CIFS options are comma-delimited with NO escaping — a password
/// containing a comma silently truncated the credential (confusing auth
/// failures) or, from an untrusted manifest Secret, let the tail be
/// interpreted as INJECTED mount options (e.g. `file_mode=0777`).
/// `mount.cifs(8)` documents `credentials=<file>` for exactly this reason.
/// Now takes an already-written credentials file path instead.
fn build_mount(
    r#type: &str,
    server: &str,
    share: &str,
    credentials_file: Option<&Path>,
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
        if let Some(path) = credentials_file {
            opts.push(format!("credentials={}", path.display()));
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

/// Writes a `mount.cifs` credentials file (mode 0600) for `username`/
/// `password`, replacing the old inline `-o username=...,password=...`.
/// `None` if neither is set (nothing to write, `build_mount` gets no
/// `credentials_file`). Rejects a newline in either value — the
/// credentials file format is one `key=value` per line, and a password
/// containing `\ndomain=x` would inject an extra directive the same way a
/// comma used to in the old inline form.
fn write_cifs_credentials(
    name: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<Option<PathBuf>> {
    write_cifs_credentials_in(&state_root().join("storage"), name, username, password)
}

/// Testable core of [`write_cifs_credentials`] — takes the directory
/// explicitly instead of going through `state_root()` (which reads the
/// process-wide `DELONIX_ROOT` env var, unsafe to mutate from parallel
/// `cargo test` threads).
fn write_cifs_credentials_in(
    dir: &Path,
    name: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<Option<PathBuf>> {
    if username.is_none() && password.is_none() {
        return Ok(None);
    }
    if [username, password]
        .into_iter()
        .flatten()
        .any(|v| v.contains('\n'))
    {
        return Err(Error::Invalid(
            "username/password do storage não podem conter uma quebra de linha".into(),
        ));
    }
    let mut content = String::new();
    if let Some(u) = username {
        content.push_str(&format!("username={u}\n"));
    }
    if let Some(p) = password {
        content.push_str(&format!("password={p}\n"));
    }
    std::fs::create_dir_all(dir)?;
    // Local sanitize (not a validator elsewhere in scope at this point —
    // this runs BEFORE `store.create_with`'s own name check): strips
    // anything that isn't alnum/`-`/`_`/`.`, closing path traversal via the
    // storage name regardless of what validates it downstream.
    let safe_name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = dir.join(format!("{safe_name}.cifs-credentials"));
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let _ = std::fs::remove_file(&path); // leftover OF OURS from a previous create/apply of the same name
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(content.as_bytes())?;
    Ok(Some(path))
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
            let creds = write_cifs_credentials(&name, username.as_deref(), pw.as_deref())?;
            let m = build_mount(
                &r#type,
                &server,
                &share,
                creds.as_deref(),
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
/// Dry-run: the spec with every `#[serde(default)]` materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: StorageSpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let store = VolumeStore::open(state_root())?;
    for doc in manifest::of_kind(docs, "Storage") {
        let name = &doc.metadata.name;
        manifest::warn_unknown_fields(doc, STORAGE_SPEC_FIELDS);
        let spec: StorageSpec = manifest::spec_of(doc)?;
        let pw = resolve_password(spec.password, spec.password_secret)?;
        let creds = write_cifs_credentials(name, spec.username.as_deref(), pw.as_deref())?;
        let m = build_mount(
            &spec.r#type,
            &spec.server,
            &spec.share,
            creds.as_deref(),
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
    use std::path::PathBuf;

    #[test]
    fn nfs_forma_servidor_export() {
        let m = build_mount("nfs", "10.0.0.5", "/mnt/pool/media", None, false, None).unwrap();
        assert_eq!(m.driver, "nfs");
        assert_eq!(m.device, "10.0.0.5:/mnt/pool/media");
        assert!(m.options.is_none());
    }

    #[test]
    fn cifs_forma_unc_com_ficheiro_de_credenciais_e_ro() {
        // BUG regression guard: the password used to be inlined as
        // `password=...` right in the mount options (visible in
        // `/proc/<pid>/cmdline`, and comma-delimited with no escaping).
        // Now `build_mount` only ever sees an opaque credentials file path.
        let creds = PathBuf::from("/tmp/delonix-storage-test.cifs-credentials");
        let m = build_mount(
            "smb",
            "nas.local",
            "media",
            Some(&creds),
            true,
            Some("vers=3.0"),
        )
        .unwrap();
        assert_eq!(m.driver, "cifs"); // smb is an alias of cifs
        assert_eq!(m.device, "//nas.local/media");
        let o = m.options.unwrap();
        assert!(o.contains("credentials=/tmp/delonix-storage-test.cifs-credentials"));
        assert!(
            !o.contains("password="),
            "a password nunca deve ir para os options"
        );
        assert!(o.contains("ro"));
        assert!(o.contains("vers=3.0"));
    }

    #[test]
    fn webdav_monta_url_https_por_omissao() {
        let m = build_mount(
            "webdav",
            "cloud.example.com",
            "/remote.php/dav/files/alice",
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
            false,
            None,
        )
        .unwrap();
        assert_eq!(m.device, "http://192.168.1.10:8080/dav");
    }

    #[test]
    fn tipo_invalido_e_erro() {
        assert!(build_mount("s3", "x", "y", None, false, None).is_err());
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "delonix-storage-test-{tag}-{}-{}",
            std::process::id(),
            line!()
        ))
    }

    #[test]
    fn write_cifs_credentials_sem_username_nem_password_devolve_none() {
        let dir = tmp_dir("none");
        assert_eq!(
            super::write_cifs_credentials_in(&dir, "x", None, None).unwrap(),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_cifs_credentials_escreve_0600_e_recusa_quebra_de_linha() {
        let dir = tmp_dir("write");
        // BUG regression guard: a password containing a newline could inject
        // an extra `key=value` line into the credentials file itself (the
        // same class of injection the comma used to allow inline).
        assert!(super::write_cifs_credentials_in(
            &dir,
            "nas",
            Some("alice"),
            Some("s3\ndomain=EVIL")
        )
        .is_err());

        let path = super::write_cifs_credentials_in(&dir, "nas", Some("alice"), Some("s3cr3t"))
            .unwrap()
            .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("username=alice"));
        assert!(content.contains("password=s3cr3t"));
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "ficheiro de credenciais tem de ser 0600");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
