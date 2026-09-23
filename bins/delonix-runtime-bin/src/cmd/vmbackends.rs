//! Registers the VM backends this process knows how to configure.
//!
//! The engine seeds the two LOCAL backends itself (libvirt, Cloud Hypervisor):
//! they need no configuration, so nothing here touches them. A REMOTE backend
//! is different — it needs an endpoint, a node name and a credential, and none
//! of that belongs in an engine crate. This is where a process that has those
//! puts them in (ADR-0008, decision 2).
//!
//! **Costs nothing when unconfigured.** With no `DELONIX_PROXMOX_URL` this is a
//! failed `env::var` and a return, and even when it IS configured nothing
//! connects: the registry stores a factory, and the node is contacted the first
//! time somebody selects the backend.
//!
//! # Why environment variables and not a manifest field
//!
//! `create_with` resolves the backend on its own, from the record or from
//! `cfg.backend`; it never receives a target. So the target has to be in place
//! BEFORE the engine is called — which is what registration is for. An
//! environment variable is what a process-wide setting looks like here, the
//! same shape `DELONIX_VM_BACKEND` already has, and the secret reference goes
//! through the same `kind: Secret` store every other credential in this CLI
//! uses (`tunnel::resolve_token`, `provision::auth_of`, `storage`).

use super::po;
use super::util::state_root;
use delonix_model::{Error, Result};

/// Reads the process-wide backend configuration and registers what is there.
///
/// Called once at startup. A misconfigured target is **reported and skipped**,
/// not fatal: `DELONIX_PROXMOX_TOKEN` with a typo must not stop
/// `delonix container ls` from running. The name then stays unregistered, and
/// `--backend proxmox` says how to configure it — which is the state the
/// operator is actually in.
pub fn register_configured() {
    if let Err(e) = register_proxmox() {
        eprintln!(
            "{}",
            po::tf(
                "warning: the Proxmox backend was configured but could not be registered: {err}",
                &[("err", &e.to_string())]
            )
        );
    }
}

/// `Ok(())` with nothing done when no Proxmox target is configured.
fn register_proxmox() -> Result<()> {
    register_proxmox_with(&|key| nonempty(std::env::var(key).ok()))
}

/// [`register_proxmox`] with the configuration read through `lookup`, so a test can
/// hand it a map instead of writing the PROCESS environment — tests run on parallel
/// threads, and a `set_var` there races every other reader of the environment.
fn register_proxmox_with(lookup: &dyn Fn(&str) -> Option<String>) -> Result<()> {
    let Some(url) = lookup("DELONIX_PROXMOX_URL") else {
        return Ok(());
    };
    let node = lookup("DELONIX_PROXMOX_NODE").ok_or_else(|| {
        Error::Invalid(
            po::t(
                "DELONIX_PROXMOX_URL is set but DELONIX_PROXMOX_NODE is not — this backend \
                 addresses ONE node and never picks one for you (the name `GET /nodes` reports, \
                 e.g. `pve`)",
            )
            .into(),
        )
    })?;
    let auth = proxmox_auth(lookup)?;
    // Opt-in, never a fallback after a TLS error: a stock Proxmox serves a
    // self-signed certificate, but skipping the check also removes what stops
    // another machine answering in the node's name — with the credential.
    let insecure_tls = lookup("DELONIX_PROXMOX_INSECURE_TLS")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .unwrap_or(false);

    // How this node is cabled, not a property of any VM — which is why it is
    // configured with the target and not in a manifest. A per-VM `bridge:`
    // still wins over this default.
    let bridge = lookup("DELONIX_PROXMOX_BRIDGE");
    let vlan = lookup("DELONIX_PROXMOX_VLAN")
        .map(|v| parse_vlan(&v))
        .transpose()?;
    // A CA to verify the node with, instead of switching verification off:
    // the honest answer for a node whose certificate an internal CA signed.
    let ca_cert_pem = lookup("DELONIX_PROXMOX_CA_FILE")
        .map(|path| {
            std::fs::read(&path).map_err(|e| {
                delonix_model::Error::Invalid(format!(
                    "DELONIX_PROXMOX_CA_FILE: could not read '{path}': {e}"
                ))
            })
        })
        .transpose()?;

    // The route trace for the coverage matrix (ADR-0049): read here, once,
    // and handed to the client — the library never reads the environment.
    let opts = delonix_proxmox::ClientOptions {
        trace_routes: lookup(delonix_proxmox::TRACE_ROUTES_ENV)
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from),
        ..Default::default()
    };
    delonix_proxmox::register_with(
        delonix_proxmox::Target {
            base_url: url,
            node,
            auth,
            insecure_tls,
            bridge,
            vlan,
            ca_cert_pem,
        },
        opts,
    )
}

/// The credential, preferring an API token.
///
/// A token is revocable on the node without touching an account, and it does
/// not expire the way a password ticket does. The password form is accepted
/// because a freshly installed node has an account before it has any token.
fn proxmox_auth(lookup: &dyn Fn(&str) -> Option<String>) -> Result<delonix_proxmox::Auth> {
    // A `kind: Secret` first: a token on the command line lands in the shell
    // history and in `ps`.
    if let Some(name) = lookup("DELONIX_PROXMOX_SECRET") {
        let s = delonix_state::SecretStore::open(state_root())?.load(&name)?;
        let get = |k: &str| s.data.get(k).cloned();
        if let (Some(id), Some(secret)) = (
            get("tokenId").or_else(|| get("token_id")),
            get("tokenSecret").or_else(|| get("token_secret")),
        ) {
            return Ok(delonix_proxmox::Auth::ApiToken { id, secret });
        }
        if let (Some(username), Some(password)) = (get("username"), get("password")) {
            return Ok(delonix_proxmox::Auth::Password { username, password });
        }
        return Err(Error::Invalid(po::tf(
            "secret '{name}' has neither `tokenId`+`tokenSecret` nor `username`+`password`",
            &[("name", &name)],
        )));
    }
    if let (Some(id), Some(secret)) = (
        lookup("DELONIX_PROXMOX_TOKEN_ID"),
        credential_value(lookup, "DELONIX_PROXMOX_TOKEN")?,
    ) {
        return Ok(delonix_proxmox::Auth::ApiToken { id, secret });
    }
    if let (Some(username), Some(password)) = (
        lookup("DELONIX_PROXMOX_USER"),
        credential_value(lookup, "DELONIX_PROXMOX_PASSWORD")?,
    ) {
        return Ok(delonix_proxmox::Auth::Password { username, password });
    }
    Err(Error::Invalid(
        po::t(
            "DELONIX_PROXMOX_URL is set but no credential is: use DELONIX_PROXMOX_SECRET (a \
             `kind: Secret` with `tokenId`+`tokenSecret`, preferred), or DELONIX_PROXMOX_TOKEN_ID \
             with DELONIX_PROXMOX_TOKEN, or DELONIX_PROXMOX_USER with DELONIX_PROXMOX_PASSWORD",
        )
        .into(),
    ))
}

/// A credential from `<key>_FILE` (preferred) or, failing that, `<key>` itself.
///
/// A secret in an environment variable is inherited by every child this engine
/// spawns and readable in `/proc/<pid>/environ` for the life of the process, so
/// the plain form still works (it is how a CI job hands one over) but says so.
/// The file form is refused unless only its owner can read it — the same rule
/// `ssh` applies to a private key.
fn credential_value(lookup: &dyn Fn(&str) -> Option<String>, key: &str) -> Result<Option<String>> {
    if let Some(path) = lookup(&format!("{key}_FILE")) {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&path)?;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(Error::Invalid(po::tf(
                "{path} is readable by other users — chmod 600 it before using it as {key}_FILE",
                &[("path", &path), ("key", key)],
            )));
        }
        return Ok(nonempty(Some(std::fs::read_to_string(&path)?)));
    }
    let v = lookup(key);
    if v.is_some() {
        eprintln!(
            "{} {}",
            po::t("warning:"),
            po::tf(
                "{key} is set in the environment, where every child process inherits it — prefer {key}_FILE or DELONIX_PROXMOX_SECRET",
                &[("key", key)],
            )
        );
    }
    Ok(v)
}

/// A VLAN tag, or an error naming the range.
///
/// **Out of range is an error, not a `None`.** Dropping it would put the VM on
/// the untagged network while the operator believes it is isolated on a VLAN —
/// a silent downgrade of a boundary, which is worse than refusing to start.
/// 0 and 4095 are reserved by 802.1Q.
fn parse_vlan(v: &str) -> Result<u16> {
    v.parse::<u16>()
        .ok()
        .filter(|t| (1..=4094).contains(t))
        .ok_or_else(|| {
            Error::Invalid(po::tf(
                "DELONIX_PROXMOX_VLAN: '{v}' is not a VLAN tag (1-4094)",
                &[("v", v)],
            ))
        })
}

/// An environment variable that is set AND not blank.
///
/// `Some("")` is what an exported-but-empty variable gives, and treating that
/// as configured turns a shell typo into "url has no scheme" at the wrong
/// moment.
///
/// Takes the raw value and not the key, so the rule is testable without writing the
/// process environment.
fn nonempty(raw: Option<String>) -> Option<String> {
    raw.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the early return: a host with no Proxmox must not pay
    /// for this, and must not see a warning about something it never asked for.
    #[test]
    fn a_credential_file_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "dlx-low-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("tok");
        std::fs::write(&f, "s3cret\n").unwrap();
        let path = f.display().to_string();
        let lookup = |k: &str| (k == "X_FILE").then(|| path.clone());
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(credential_value(&lookup, "X").is_err(), "world-readable");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            credential_value(&lookup, "X").unwrap().as_deref(),
            Some("s3cret")
        );
    }

    #[test]
    fn sem_configuracao_nao_regista_nada_e_nao_se_queixa() {
        // No env var of ours is set in a plain `cargo test` run.
        if nonempty(std::env::var("DELONIX_PROXMOX_URL").ok()).is_some() {
            eprintln!("SKIP: DELONIX_PROXMOX_URL esta definido neste ambiente");
            return;
        }
        assert!(register_proxmox().is_ok());
        // And the name stays unregistered, so `--backend proxmox` still says
        // how to configure it rather than resolving to a target nobody set.
        assert!(delonix_vm::select_backend(Some("proxmox")).is_err());
    }

    /// Half a configuration is the case worth refusing loudly: a URL with no
    /// node would otherwise have to guess which node to place VMs on, and
    /// guessing that is exactly what guardrail #2 keeps out of this repo.
    #[test]
    fn um_alvo_incompleto_diz_o_que_falta() {
        if nonempty(std::env::var("DELONIX_PROXMOX_URL").ok()).is_some() {
            eprintln!("SKIP: DELONIX_PROXMOX_URL esta definido neste ambiente");
            return;
        }
        let only_url = |key: &str| {
            (key == "DELONIX_PROXMOX_URL").then(|| "https://pve.local:8006".to_string())
        };
        let e = register_proxmox_with(&only_url).unwrap_err().to_string();
        assert!(e.contains("DELONIX_PROXMOX_NODE"), "{e}");
    }

    #[test]
    fn uma_variavel_exportada_mas_vazia_nao_conta_como_configurada() {
        assert!(nonempty(Some("   ".to_string())).is_none());
        assert!(nonempty(None).is_none());
        assert_eq!(nonempty(Some(" pve ".to_string())).as_deref(), Some("pve"));
    }
}
