//! **Capability ceiling** — a node-level upper bound on the Linux capabilities
//! any container created through the CRI may end up with, regardless of what the
//! kubelet asks for.
//!
//! # Why this exists on the runtime side
//!
//! Everything in a pod's `securityContext` reaches this runtime already
//! authorized: `create_container` translates `privileged: true` into
//! `--cap-add ALL` and `capabilities.add` into one `--cap-add` per name, with no
//! opinion of its own. That is the correct default (the runtime is not the
//! admission controller), but it means the ONLY thing standing between a
//! `privileged: true` PodSpec and every capability the kernel has is the API
//! server's admission chain — a different process, on a different machine,
//! whose configuration this node cannot see or verify.
//!
//! The ceiling is the node's own answer: a bound the operator sets locally, that
//! holds even if Pod Security admission is misconfigured, bypassed by a direct
//! `crictl` call against this socket, or served by an API server that a static
//! pod manifest never went through. Defense in depth, not a replacement for
//! admission.
//!
//! # What it does and does NOT cover
//!
//! **Capabilities only.** A `privileged: true` container still gets
//! `seccomp=unconfined`, a writable `/sys`, and its own cgroup namespace — those
//! are separate axes of `--privileged` and clamping capabilities does not make a
//! privileged pod safe. The bound is deliberately narrow and says so, rather than
//! implying a hardening it does not deliver.
//!
//! # Configuration
//!
//! `DELONIX_CRI_CAP_CEILING` (or `delonix serve cri --cap-ceiling`):
//!
//! | Value | Meaning |
//! |---|---|
//! | unset / empty / `all` | **no ceiling** — byte-for-byte the previous behavior |
//! | `none` | no capabilities at all, for anyone |
//! | `default` | exactly the engine's default kept set ([`delonix_runtime::capabilities::KEPT_CAPS`]) |
//! | `default,NET_ADMIN,…` | the default set plus the named ones |
//! | `CHOWN,NET_BIND_SERVICE,…` | exactly the named ones (`CAP_` prefix optional, case-insensitive) |
//!
//! `DELONIX_CRI_CAP_CEILING_MODE` (or `--cap-ceiling-mode`): `reject` (default)
//! or `clamp`.
//!
//! - **`reject`** — a container whose `securityContext` explicitly asks for a
//!   capability above the ceiling FAILS at `CreateContainer`, with the offending
//!   names in the error the kubelet surfaces on the pod. Fail-closed: an operator
//!   who set a ceiling wants to know that a workload wanted past it, not to
//!   discover later that it ran with less than it asked for and misbehaved in
//!   some unrelated way.
//! - **`clamp`** — the same request is silently reduced to the ceiling and logged
//!   at `warn`. For hardening a node that already runs workloads whose PodSpecs
//!   cannot be changed today. It trades honesty toward the workload for
//!   availability, which is why it is not the default.
//!
//! In BOTH modes the *implicit* baseline — the engine's default kept set, which a
//! container gets without asking for anything — is reduced to the ceiling without
//! any error. Lowering a default the workload never requested is what "ceiling"
//! means; refusing every pod on the node because the runtime's own default is
//! wider than the bound would make the feature unusable.
//!
//! A malformed value makes the server REFUSE TO START. A typo that quietly
//! resolved to "no ceiling" is the exact failure this module exists to prevent.

use delonix_runtime::capabilities::{
    all_caps_mask, cap_num, default_kept_mask, names_from_mask, resolve_cap_keep,
};

/// Environment variable that carries the ceiling itself.
pub const CEILING_ENV: &str = "DELONIX_CRI_CAP_CEILING";
/// Environment variable that carries the enforcement mode.
pub const MODE_ENV: &str = "DELONIX_CRI_CAP_CEILING_MODE";

/// What to do with a request that exceeds the ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CeilingMode {
    /// Fail `CreateContainer` (default).
    Reject,
    /// Reduce to the ceiling and log a warning.
    Clamp,
}

impl CeilingMode {
    /// Parses the mode. Fail-closed: an unknown word is an error, never a
    /// fallback to the permissive side.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "reject" | "enforce" => Ok(Self::Reject),
            "clamp" | "trim" => Ok(Self::Clamp),
            other => Err(format!(
                "invalid capability ceiling mode {other:?}: expected `reject` or `clamp`"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Reject => "reject",
            Self::Clamp => "clamp",
        }
    }
}

/// The node's capability ceiling. `None` mask = unlimited (no ceiling).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapCeiling {
    mask: Option<u64>,
    mode: CeilingMode,
}

impl Default for CapCeiling {
    fn default() -> Self {
        Self::unlimited()
    }
}

impl CapCeiling {
    /// No ceiling — every CRI request is translated exactly as before.
    pub fn unlimited() -> Self {
        Self {
            mask: None,
            mode: CeilingMode::Reject,
        }
    }

    /// Parses a ceiling spec and a mode. See the module docs for the grammar.
    pub fn parse(spec: &str, mode: &str) -> Result<Self, String> {
        let mode = CeilingMode::parse(mode)?;
        let spec = spec.trim();
        if spec.is_empty() {
            return Ok(Self { mask: None, mode });
        }
        let mut mask = 0u64;
        let mut unlimited = false;
        let mut saw_token = false;
        for tok in spec
            .split([',', ' ', '\t', ';'])
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            saw_token = true;
            match tok.to_ascii_lowercase().as_str() {
                "all" => unlimited = true,
                // `none` contributes nothing; it exists so that "no capabilities"
                // is spellable at all — an empty value means "unset", which is
                // the opposite.
                "none" => {}
                "default" => mask |= default_kept_mask(),
                _ => match cap_num(tok) {
                    Some(n) => mask |= 1u64 << n,
                    None => {
                        return Err(format!(
                            "unknown capability {tok:?} in {CEILING_ENV}: use names like \
                             NET_ADMIN or CAP_NET_ADMIN, or the keywords all/none/default"
                        ))
                    }
                },
            }
        }
        if !saw_token {
            // Only separators (e.g. `,,`) — too close to a truncated value to
            // guess at; refuse instead of silently meaning "unset".
            return Err(format!("empty capability ceiling in {CEILING_ENV}"));
        }
        // `all` anywhere in the list wins: it can only widen, and a spec that
        // mixes it with names is a contradiction resolved the permissive way it
        // is written, not silently narrowed.
        Ok(Self {
            mask: if unlimited { None } else { Some(mask) },
            mode,
        })
    }

    /// Reads the ceiling from the environment (`DELONIX_CRI_CAP_CEILING` /
    /// `DELONIX_CRI_CAP_CEILING_MODE`).
    pub fn from_env() -> Result<Self, String> {
        Self::parse(
            &std::env::var(CEILING_ENV).unwrap_or_default(),
            &std::env::var(MODE_ENV).unwrap_or_default(),
        )
    }

    /// Whether no ceiling is in force.
    pub fn is_unlimited(&self) -> bool {
        self.mask.is_none()
    }

    /// One-line description for the startup banner and the `status` verbose info.
    pub fn describe(&self) -> String {
        match self.mask {
            None => "none (unlimited)".to_string(),
            Some(0) => format!("no capabilities (mode={})", self.mode.as_str()),
            Some(m) => format!(
                "{} (mode={})",
                names_from_mask(m).join(","),
                self.mode.as_str()
            ),
        }
    }

    /// The capabilities an EXPLICIT request asks for that the ceiling forbids, as
    /// names the operator can read. Empty when there is no ceiling, when the mode
    /// is `clamp`, or when the request fits.
    ///
    /// `privileged` is treated as a request for everything the kernel has — which
    /// is what the CRI translation makes of it — and is reported by the names it
    /// would gain beyond the ceiling, so the error says what was actually denied
    /// instead of just repeating the word "privileged".
    pub fn rejected(&self, cap_add: &[String], privileged: bool) -> Vec<&'static str> {
        let Some(mask) = self.mask else {
            return Vec::new();
        };
        if self.mode != CeilingMode::Reject {
            return Vec::new();
        }
        let wants_all = privileged || cap_add.iter().any(|c| c.eq_ignore_ascii_case("all"));
        let requested = if wants_all {
            all_caps_mask()
        } else {
            cap_add
                .iter()
                .filter_map(|c| cap_num(c))
                .fold(0u64, |m, n| m | (1u64 << n))
        };
        names_from_mask(requested & !mask)
    }

    /// The `--cap-*` arguments `start_container` should emit, or `None` to keep
    /// emitting exactly what it emitted before this module existed (no ceiling).
    ///
    /// With a ceiling in force the form is always `--cap-drop ALL` followed by one
    /// `--cap-add` per capability of the final set. That final set is computed
    /// with the ENGINE's own [`resolve_cap_keep`], not with a second
    /// implementation of the same rules: whatever the engine would have granted,
    /// intersected with the ceiling. `--cap-drop ALL` + explicit adds resolves
    /// back to precisely that mask (there is a round-trip test for it in
    /// `delonix_runtime::capabilities`), so the clamp cannot drift from the
    /// engine's semantics as `--cap-add`/`--cap-drop` handling evolves.
    pub fn cap_args(
        &self,
        cap_add: &[String],
        cap_drop: &[String],
        privileged: bool,
    ) -> Option<Vec<String>> {
        let mask = self.mask?;
        let granted = if privileged {
            // Mirrors the engine: `privileged` means every capability, and any
            // `cap_drop` the pod also set still applies on top.
            all_caps_mask() & resolve_cap_keep(cap_drop, &["ALL".to_string()])
        } else {
            resolve_cap_keep(cap_drop, cap_add)
        };
        let mut args = vec!["--cap-drop".to_string(), "ALL".to_string()];
        for name in names_from_mask(granted & mask) {
            args.push("--cap-add".to_string());
            args.push(name.to_string());
        }
        Some(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn ausente_ou_all_significa_sem_tecto() {
        for spec in ["", "   ", "all", "ALL", "default,all"] {
            let c = CapCeiling::parse(spec, "").unwrap();
            assert!(c.is_unlimited(), "spec {spec:?} devia ficar sem tecto");
            assert_eq!(c.cap_args(&s(&["NET_ADMIN"]), &[], true), None);
            assert!(c.rejected(&s(&["SYS_ADMIN"]), true).is_empty());
        }
    }

    /// Fail-closed: a malformed ceiling must be an error, never a silent
    /// downgrade to "unlimited" — that is the failure mode the whole module
    /// exists to prevent.
    #[test]
    fn valor_invalido_e_erro_nunca_tecto_vazio() {
        for bad in ["NET_ADMINN", "sys admin", "CAP_", ",,", "cap_net_adminx"] {
            assert!(
                CapCeiling::parse(bad, "").is_err(),
                "spec {bad:?} devia ser recusada"
            );
        }
        assert!(CapCeiling::parse("CHOWN", "wide-open").is_err());
        // ...and a valid one is accepted with either separator or casing.
        let c = CapCeiling::parse("cap_chown net_admin,SETUID", "clamp").unwrap();
        assert_eq!(c.describe(), "CHOWN,SETUID,NET_ADMIN (mode=clamp)");
    }

    #[test]
    fn none_e_um_tecto_vazio_nao_a_ausencia_de_tecto() {
        let c = CapCeiling::parse("none", "").unwrap();
        assert!(!c.is_unlimited());
        assert_eq!(
            c.cap_args(&[], &[], false),
            Some(s(&["--cap-drop", "ALL"])),
            "com tecto vazio o container não fica com capability nenhuma"
        );
    }

    #[test]
    fn tecto_reduz_o_baseline_sem_erro_nenhum() {
        // A pod that asks for NOTHING still gets the engine's default set; the
        // ceiling lowers it, and that is never an error.
        let c = CapCeiling::parse("CHOWN,NET_BIND_SERVICE,SYS_ADMIN", "").unwrap();
        assert!(c.rejected(&[], false).is_empty());
        let args = c.cap_args(&[], &[], false).unwrap();
        assert_eq!(
            args,
            s(&[
                "--cap-drop",
                "ALL",
                "--cap-add",
                "CHOWN",
                "--cap-add",
                "NET_BIND_SERVICE"
            ]),
            "SYS_ADMIN está no tecto mas não no baseline — o tecto não CONCEDE nada"
        );
    }

    #[test]
    fn modo_reject_nomeia_as_capabilities_negadas() {
        let c = CapCeiling::parse("default,NET_ADMIN", "reject").unwrap();
        assert!(
            c.rejected(&s(&["NET_ADMIN"]), false).is_empty(),
            "NET_ADMIN está no tecto"
        );
        assert_eq!(
            c.rejected(&s(&["NET_ADMIN", "CAP_SYS_ADMIN", "sys_module"]), false),
            vec!["SYS_MODULE", "SYS_ADMIN"]
        );
        // `privileged` is reported by what it would actually gain.
        let denied = c.rejected(&[], true);
        assert!(denied.contains(&"SYS_ADMIN") && denied.contains(&"MKNOD"));
        assert!(
            !denied.contains(&"CHOWN") && !denied.contains(&"NET_ADMIN"),
            "o que o tecto permite não aparece como negado"
        );
    }

    #[test]
    fn modo_clamp_nao_recusa_e_corta_o_privileged() {
        let c = CapCeiling::parse("default,NET_ADMIN", "clamp").unwrap();
        assert!(c.rejected(&[], true).is_empty(), "clamp nunca recusa");
        let args = c.cap_args(&[], &[], true).unwrap();
        assert_eq!(args[0], "--cap-drop");
        assert_eq!(args[1], "ALL");
        let added: Vec<&str> = args[2..]
            .chunks(2)
            .map(|p| p[1].as_str())
            .collect::<Vec<_>>();
        assert!(added.contains(&"NET_ADMIN"));
        assert!(
            !added.contains(&"SYS_ADMIN") && !added.contains(&"SYS_MODULE"),
            "um privileged cortado não pode manter SYS_ADMIN/SYS_MODULE: {added:?}"
        );
    }

    /// `cap_drop` from the pod still applies THROUGH the ceiling: the workload
    /// asking for less than the bound must get less, not the bound.
    #[test]
    fn cap_drop_do_pod_continua_a_valer_por_baixo_do_tecto() {
        let c = CapCeiling::parse("default", "clamp").unwrap();
        let args = c.cap_args(&[], &s(&["CHOWN"]), false).unwrap();
        let added: Vec<&str> = args[2..].chunks(2).map(|p| p[1].as_str()).collect();
        assert!(!added.contains(&"CHOWN"), "o pod pediu para largar CHOWN");
        assert!(added.contains(&"SETUID"));

        // ...and also on top of `privileged`, mirroring the engine.
        let args = c.cap_args(&[], &s(&["ALL"]), true).unwrap();
        assert_eq!(args, s(&["--cap-drop", "ALL"]));
    }

    /// The clamp must be expressible as engine flags: what we emit has to resolve
    /// back to exactly the mask we intended, or the ceiling would be advisory.
    #[test]
    fn os_flags_emitidos_resolvem_para_o_conjunto_pretendido() {
        let c = CapCeiling::parse("default,NET_ADMIN", "clamp").unwrap();
        let args = c
            .cap_args(&s(&["SYS_ADMIN", "NET_ADMIN"]), &[], false)
            .unwrap();
        // Replay the argv the way `cmd_run` would.
        let mut add = Vec::new();
        let mut drop = Vec::new();
        let mut it = args.iter();
        while let Some(flag) = it.next() {
            let val = it.next().unwrap().clone();
            match flag.as_str() {
                "--cap-add" => add.push(val),
                "--cap-drop" => drop.push(val),
                other => panic!("flag inesperada {other}"),
            }
        }
        let got = resolve_cap_keep(&drop, &add);
        let want = default_kept_mask() | (1u64 << 12); // + NET_ADMIN, sem SYS_ADMIN
        assert_eq!(
            got,
            want,
            "got {:?} want {:?}",
            names_from_mask(got),
            names_from_mask(want)
        );
    }
}
