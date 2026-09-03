//! Network-share mounting primitives (NFS/SMB-CIFS/WebDAV) — the friendly
//! declaration (server/share/credentials) that a `kind: Volume`'s
//! `nfs:`/`cifs:`/`webdav:` block, and `volume create --type <t>`, both
//! translate into a mount `device`/`options`.
//!
//! **B5 CLI collapse**: `delonix storage` used to be a whole command group
//! of its own (`create`/`ls`/`dash`/`rm`/`inspect`). `dash`/`rm`/`inspect`
//! were the first to fold into `volume dash`/`volume rm`/`delete volume`/
//! `volume inspect`; `create` and `ls` — the two pieces that had no
//! imperative equivalent on `volume create`/`volume ls` — are now covered by
//! `volume create --type <nfs|cifs|smb|webdav> --server … --share …` and
//! `volume ls`'s `PARENT`/`QUOTA` columns. `delonix storage` no longer
//! exists as a group; this module is now purely the shared mount-building
//! logic both `volume create` and the `kind: Volume` apply path call into.

use std::path::{Path, PathBuf};

use delonix_runtime_core::{Error, Result};
use serde::{Deserialize, Serialize};

use super::util::state_root;

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

/// A network share, as declared inside a `kind: Volume` (`spec.nfs`,
/// `spec.cifs`, `spec.webdav`).
///
/// This is `StorageSpec` minus the `type` field — the type is now the BLOCK's
/// name, the same shape `kind: Workload` uses (`spec.container`/`spec.vm`), and
/// for the same reason: a type that names its own block cannot contradict it.
///
/// `kind: Volume` and `kind: Storage` used to describe the SAME mount two ways
/// and land in the SAME store, with nothing to say which to use — `volumes ls`
/// showed both (one store) while `storage ls` showed only some, so the same
/// question got different answers depending on the command.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub(crate) struct NetShareSpec {
    pub(crate) server: String,
    pub(crate) share: String,
    #[serde(default)]
    pub(crate) username: Option<String>,
    #[serde(default)]
    pub(crate) password: Option<String>,
    /// Vault secret (`password` key) — preferred over an inline `password`.
    #[serde(default, rename = "passwordSecret")]
    pub(crate) password_secret: Option<String>,
    #[serde(default, rename = "readOnly")]
    pub(crate) read_only: bool,
    #[serde(default, rename = "mountOptions")]
    pub(crate) mount_options: Option<String>,
}

/// Fields accepted inside a network-share block (drift-guard).
pub(crate) const NET_SHARE_FIELDS: &[&str] = &[
    "server",
    "share",
    "username",
    "password",
    "passwordSecret",
    "readOnly",
    "mountOptions",
];

/// Where a share's credentials file lives. Derived, never guessed, from the ONE
/// place that names it ([`credentials_name`]) — the writer, the remover and this
/// must not be able to disagree about which file belongs to which name.
fn credentials_path(name: &str) -> PathBuf {
    state_root().join("storage").join(credentials_name(name))
}

/// Whether this share needs a credentials file at all.
fn needs_credentials(kind: &str, b: &NetShareSpec) -> bool {
    matches!(kind, "cifs" | "smb")
        && (b.username.is_some() || b.password.is_some() || b.password_secret.is_some())
}

/// The mount a share block describes — **pure**: computes the driver, device and
/// options without touching disk and without opening the secret vault.
///
/// That purity is what lets `stack plan`/`--dry-run` describe a network volume
/// at all: writing a credentials file to compute a plan would make planning a
/// side effect, and computing a plan must never create anything.
pub(crate) fn share_mount(name: &str, kind: &str, b: &NetShareSpec) -> Result<MountSpec> {
    let creds = needs_credentials(kind, b).then(|| credentials_path(name));
    build_mount(
        kind,
        &b.server,
        &b.share,
        creds.as_deref(),
        b.read_only,
        b.mount_options.as_deref(),
    )
}

/// Writes the share's credentials file, if it needs one. The side-effecting half
/// of [`share_mount`], called only from an apply.
pub(crate) fn ensure_share_credentials(name: &str, kind: &str, b: &NetShareSpec) -> Result<()> {
    if !needs_credentials(kind, b) {
        return Ok(());
    }
    let pw = resolve_password(b.password.clone(), b.password_secret.clone())?;
    write_cifs_credentials(name, b.username.as_deref(), pw.as_deref())?;
    Ok(())
}

/// The mount parameters derived from a storage declaration.
pub(crate) struct MountSpec {
    pub(crate) driver: String,
    pub(crate) device: String,
    pub(crate) options: Option<String>,
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
///
/// **What happens when the NAS goes away mid-write, and why it is left that way.**
/// The only options emitted here are `credentials=` (cifs), `ro`, and the caller's extras
/// — no `soft`, no `timeo`, no `retrans`. So NFS keeps its own default, `hard`: an
/// in-flight write does not fail, it **blocks indefinitely** in uninterruptible sleep, and
/// the process cannot be killed until the server answers. That is the correct default and
/// is deliberately not changed here — `soft` turns the same outage into an `EIO` in the
/// middle of a write, which for a database is silent corruption instead of a stall, and
/// this whole area exists to not lose data.
///
/// It is documented because the operator-visible symptom ("the container is wedged and
/// won't die") points nowhere near the NAS on its own. The escape hatch already exists and
/// needs no new flag: pass `soft,timeo=50,retrans=2` (or `intr` on old kernels) through the
/// extra mount options for a workload that would rather see an error than wait.
///
/// NOT measured: this host cannot mount NFS/CIFS at all (`mount -t` needs `CAP_SYS_ADMIN`,
/// unavailable rootless), so the behaviour above is read off the emitted options plus
/// `nfs(5)`, not observed. See `docs/discovery/46_GAPS_ENCONTRADOS.md` §1, line 12.
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
            return Err(Error::Invalid(super::po::tf(
                "unknown storage type: '{other}' (nfs|cifs|smb|webdav)",
                &[("other", other)],
            )))
        }
    };
    // Options: credentials (cifs), ro, and the user's extras — in this order.
    let mut opts: Vec<String> = Vec::new();
    if driver == "cifs" {
        if let Some(path) = credentials_file {
            opts.push(format!("credentials={}", path.display()));
        }
    } else if credentials_file.is_some() {
        // FAIL CLOSED instead of ignoring them. `--username`/`--password` are
        // documented as "(cifs/webdav)" and the credentials file was written for
        // both, but only the `cifs` branch ever referenced it — so for `webdav`
        // (davfs) and `nfs` the credentials were accepted and then silently
        // dropped, leaving the mount to fail with an opaque auth error or hang
        // waiting for davfs to prompt. davfs2 does not take a credentials path as
        // a mount option at all: it reads `/etc/davfs2/secrets` (root-owned,
        // host-level configuration this engine has no business writing), which is
        // why this is a clear refusal and not a quiet best-effort.
        return Err(Error::Invalid(super::po::tf(
            "storage type '{type}' does not support --username/--password here: only `cifs`/`smb` \
             take a credentials file. For `webdav`, put the credentials in \
             /etc/davfs2/secrets on the host; for `nfs`, authentication is by \
             export/host, not by user.",
            &[("type", r#type)],
        )));
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

/// Sanitized filename of a storage's credentials file — the ONE place that
/// derives it, so the writer and the remover can never disagree about which file
/// belongs to a given storage name.
fn credentials_name(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{safe}.cifs-credentials")
}

/// Deletes a storage's credentials file, if this name ever had one.
///
/// **`storage rm` used to be the only caller.** `store.remove(name)` only
/// touches `<root>/volumes/<name>/`, while the NAS username+password live in
/// `<root>/storage/<name>.cifs-credentials` — so the credential outlived the
/// storage it belonged to, indefinitely, and was only ever overwritten if someone
/// happened to re-create a storage with the exact same name. Removing a storage
/// has to take its secret with it. Since the B5 CLI collapse (`storage rm` cut in
/// favour of the generic `volume rm`/`delete volume`), `volume::cmd_rm_with` calls
/// this UNCONDITIONALLY for every volume removed — a plain local volume never had
/// a file at this path, so the call is a harmless no-op for it, and a
/// network-share volume removed through the generic verb no longer leaks its
/// credentials the way it silently did before this call moved here.
///
/// Best-effort by design: failing to unlink a credentials file must not block the
/// removal of the volume itself (which is what the operator asked for), but it is
/// reported so it never disappears in silence.
pub(crate) fn remove_credentials(name: &str) {
    let path = state_root().join("storage").join(credentials_name(name));
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => super::output::warn(&super::po::tf(
            "could not remove the credentials file {path}: {err}",
            &[
                ("path", &path.display().to_string()),
                ("err", &e.to_string()),
            ],
        )),
    }
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
            super::po::t("storage username/password cannot contain a line break").into(),
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
    let _ = &safe_name; // kept for the comment above; the shared helper derives it
    let path = dir.join(credentials_name(name));
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
