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
use delonix_runtime_core::{Error, Result};

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
    let Some(url) = env_nonempty("DELONIX_PROXMOX_URL") else {
        return Ok(());
    };
    let node = env_nonempty("DELONIX_PROXMOX_NODE").ok_or_else(|| {
        Error::Invalid(
            po::t(
                "DELONIX_PROXMOX_URL is set but DELONIX_PROXMOX_NODE is not — this backend \
                 addresses ONE node and never picks one for you (the name `GET /nodes` reports, \
                 e.g. `pve`)",
            )
            .into(),
        )
    })?;
    let auth = proxmox_auth()?;
    // Opt-in, never a fallback after a TLS error: a stock Proxmox serves a
    // self-signed certificate, but skipping the check also removes what stops
    // another machine answering in the node's name — with the credential.
    let insecure_tls = env_nonempty("DELONIX_PROXMOX_INSECURE_TLS")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .unwrap_or(false);

    // How this node is cabled, not a property of any VM — which is why it is
    // configured with the target and not in a manifest. A per-VM `bridge:`
    // still wins over this default.
    let bridge = env_nonempty("DELONIX_PROXMOX_BRIDGE");
    let vlan = env_nonempty("DELONIX_PROXMOX_VLAN")
        .map(|v| parse_vlan(&v))
        .transpose()?;

    delonix_proxmox::register(delonix_proxmox::Target {
        base_url: url,
        node,
        auth,
        insecure_tls,
        bridge,
        vlan,
    })
}

/// The credential, preferring an API token.
///
/// A token is revocable on the node without touching an account, and it does
/// not expire the way a password ticket does. The password form is accepted
/// because a freshly installed node has an account before it has any token.
fn proxmox_auth() -> Result<delonix_proxmox::Auth> {
    // A `kind: Secret` first: a token on the command line lands in the shell
    // history and in `ps`.
    if let Some(name) = env_nonempty("DELONIX_PROXMOX_SECRET") {
        let s = delonix_runtime_core::SecretStore::open(state_root())?.load(&name)?;
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
        env_nonempty("DELONIX_PROXMOX_TOKEN_ID"),
        env_nonempty("DELONIX_PROXMOX_TOKEN"),
    ) {
        return Ok(delonix_proxmox::Auth::ApiToken { id, secret });
    }
    if let (Some(username), Some(password)) = (
        env_nonempty("DELONIX_PROXMOX_USER"),
        env_nonempty("DELONIX_PROXMOX_PASSWORD"),
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
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the early return: a host with no Proxmox must not pay
    /// for this, and must not see a warning about something it never asked for.
    #[test]
    fn sem_configuracao_nao_regista_nada_e_nao_se_queixa() {
        // No env var of ours is set in a plain `cargo test` run.
        if env_nonempty("DELONIX_PROXMOX_URL").is_some() {
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
        if env_nonempty("DELONIX_PROXMOX_URL").is_some() {
            eprintln!("SKIP: DELONIX_PROXMOX_URL esta definido neste ambiente");
            return;
        }
        // SAFETY: single-threaded assertion on process env; the test restores it.
        std::env::set_var("DELONIX_PROXMOX_URL", "https://pve.local:8006");
        let e = register_proxmox().unwrap_err().to_string();
        std::env::remove_var("DELONIX_PROXMOX_URL");
        assert!(e.contains("DELONIX_PROXMOX_NODE"), "{e}");
    }

    #[test]
    fn uma_variavel_exportada_mas_vazia_nao_conta_como_configurada() {
        std::env::set_var("DELONIX_TESTE_VAZIA", "   ");
        assert!(env_nonempty("DELONIX_TESTE_VAZIA").is_none());
        std::env::set_var("DELONIX_TESTE_VAZIA", " pve ");
        assert_eq!(env_nonempty("DELONIX_TESTE_VAZIA").as_deref(), Some("pve"));
        std::env::remove_var("DELONIX_TESTE_VAZIA");
    }
}
