//! Shared catalogue of the "recipes" for preparing a host for
//! `kubeadm`/`kubelet`/`kubectl` — the technically sensitive part (signed
//! apt repository, the right packages, swap, kernel modules, sysctls) that MUST
//! stay identical between `cmd::vmimage::build` (applies them offline, once,
//! via `virt-customize`, in the golden image) and `cmd::cluster::apply` (applies them
//! live, over SSH, on arbitrary hosts) — never diverges between the two
//! pipelines. The account (`delonix` user/root password) and installation of the
//! `delonix-cri` binary stay OUT of here, deliberately: they use different native
//! mechanisms per transport (virt-customize's `--root-password` flags
//! vs `chpasswd` over SSH; `--copy-in` vs `scp`) — forcing a common
//! shell-command there would bring no real benefit, only risk.

/// An idempotent recipe: `check` (shell command; exit code 0 = already
/// satisfied) + `apply` (shell command that satisfies it). `cmd::cluster::apply`
/// uses `check`+`apply` over SSH on LIVE hosts; `cmd::vmimage::build` uses
/// `apply_offline()` — the variant with no live effects (`modprobe`/`sysctl
/// --system`/`swapoff` make no sense in the virt-customize chroot, whose
/// kernel is the build HOST's: only persistence in /etc matters, the
/// effects happen on the VM's first boot).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HostRecipe {
    pub name: &'static str,
    pub check: String,
    pub apply: String,
    /// Variant for offline image customization (None = same as `apply`).
    pub offline: Option<String>,
}

impl HostRecipe {
    /// The command to use in an image build (offline): persistence only.
    pub fn apply_offline(&self) -> &str {
        self.offline.as_deref().unwrap_or(&self.apply)
    }
}

/// Resolves "what version did the caller ask for" into a CONCRETE number —
/// the ONE place where "the caller did not choose" becomes
/// [`vmimage::DEFAULT_K8S_VERSION`].
///
/// BUG FOUND (2026-10-15, by reading the assembled `virt-customize` command
/// line of a live `delonix vm image build -t k8s-golden`): this fallback used
/// to be an INDEPENDENT hardcoded `stable:/v1.31` inside `k8s_repo_version`,
/// while `DEFAULT_K8S_VERSION` had already moved to 1.36. A build with no
/// `--k8s-version` therefore ran
/// `curl -fsSL https://pkgs.k8s.io/core:/stable:/v1.31/deb/Release.key` —
/// four minor versions older than the documented default, and
/// `DEFAULT_K8S_VERSION` was dead code on that path. Two independent defaults
/// for one number always drift, so there is now exactly one: this function.
/// [`k8s_repo_version`] below CANNOT default anymore (it takes a concrete
/// `&str`), which is what stops the drift from coming back.
///
/// `"stable"` is the historical "newest in the channel" sentinel. No entry
/// point can reach it today — `cluster::valid_version` allows only digits and
/// dots, so it rejects the word on both the `--k8s-version` and the
/// `spec.k8sVersion` paths — so it resolves to the default as well, rather
/// than leaving the version unpinned.
pub(crate) fn resolve_k8s_version(k8s_version: Option<&str>) -> String {
    match k8s_version {
        Some(v) if v != "stable" => v.to_string(),
        _ => super::vmimage::DEFAULT_K8S_VERSION.to_string(),
    }
}

/// The `pkgs.k8s.io` repository path for a CONCRETE version: `"1.36"` →
/// `"stable:/v1.36"`.
///
/// Takes `&str` and not `Option<&str>` ON PURPOSE: the caller resolves with
/// [`resolve_k8s_version`] first, so this function has no default of its own
/// to drift away from `DEFAULT_K8S_VERSION` — see that function for the build
/// this exact divergence shipped.
///
/// BUG FOUND: `cluster.rs::valid_version` explicitly accepts (and its own
/// doc-comment/tests assert) a PATCH version like `1.31.2` — but
/// `pkgs.k8s.io` only ever publishes MINOR-level repo directories
/// (`stable:/v1.31/deb/`, never `.../v1.31.2/deb/`). A `spec.k8sVersion:
/// "1.31.2"` passed `valid_version` and produced a 404 repo URL, failing
/// `apt-get update` on every host of the cluster during `prepare_host` —
/// well after the manifest was accepted as valid. Truncate to major.minor
/// for the REPO path only; `--kubernetes-version` (kubeadm's own flag,
/// built elsewhere) still gets the full version, which is correct there.
pub(crate) fn k8s_repo_version(k8s_version: &str) -> String {
    format!("stable:/v{}", major_minor(k8s_version))
}

/// `"1.31.2"` → `"1.31"`; `"1.31"` → `"1.31"` unchanged (no third
/// component to drop). Pure, tested against the exact repo-404 scenario.
fn major_minor(v: &str) -> &str {
    match v.match_indices('.').nth(1) {
        Some((idx, _)) => &v[..idx],
        None => v,
    }
}

/// Builds the catalogue — `extra_packages` extends the list of packages
/// installed in the `kubeadm/kubelet/kubectl` recipe without touching this function
/// (it is the "100% parameterized" part of the pipeline).
/// `k8s_version` is CONCRETE (already through [`resolve_k8s_version`]) — the
/// catalogue never decides the version by omission.
pub(crate) fn k8s_host_recipes(k8s_version: &str, extra_packages: &[String]) -> Vec<HostRecipe> {
    let repo = k8s_repo_version(k8s_version);
    let mut packages = vec![
        "kubeadm".to_string(),
        "kubelet".to_string(),
        "kubectl".to_string(),
    ];
    packages.extend(extra_packages.iter().cloned());
    let packages_str = packages.join(" ");

    let mut all = vec![
        HostRecipe {
            name: "repositório pkgs.k8s.io configurado",
            check: "test -f /etc/apt/sources.list.d/kubernetes.list".to_string(),
            apply: format!(
                "curl -fsSL https://pkgs.k8s.io/core:/{repo}/deb/Release.key | \
                 gpg --dearmor -o /etc/apt/keyrings/kubernetes-apt-keyring.gpg && \
                 echo 'deb [signed-by=/etc/apt/keyrings/kubernetes-apt-keyring.gpg] \
                 https://pkgs.k8s.io/core:/{repo}/deb/ /' > /etc/apt/sources.list.d/kubernetes.list"
            ),
            offline: None,
        },
        HostRecipe {
            name: "kubeadm/kubelet/kubectl instalados e retidos",
            check: "command -v kubeadm >/dev/null 2>&1".to_string(),
            apply: format!(
                "apt-get update && apt-get install -y {packages_str} && apt-mark hold kubeadm kubelet kubectl"
            ),
            offline: None,
        },
    ];
    all.extend(k8s_config_recipes());
    all
}

/// The recipes that do NOT need network — only persistence in `/etc` (swap,
/// kernel modules, sysctls). They are the subset that the golden image's **offline**
/// build reuses as-is (`cmd::vmimage`, `--offline`): there, the apt
/// repository and the `apt-get install` are replaced by `.deb`s already downloaded and
/// VERIFIED on the host + `dpkg -i`, so the `virt-customize` appliance can
/// run with `--no-network`. `k8s_host_recipes` = the 2 network ones + these, so
/// `cluster apply` (live hosts) keeps seeing the full catalogue.
pub(crate) fn k8s_config_recipes() -> Vec<HostRecipe> {
    vec![
        HostRecipe {
            name: "swap desligado",
            check: "! swapon --show | grep -q .".to_string(),
            apply: "swapoff -a && sed -i '/[[:space:]]swap[[:space:]]/d' /etc/fstab".to_string(),
            offline: Some("sed -i '/[[:space:]]swap[[:space:]]/d' /etc/fstab".to_string()),
        },
        HostRecipe {
            name: "módulos kernel overlay/br_netfilter carregados",
            check: "lsmod | grep -q br_netfilter && lsmod | grep -q overlay".to_string(),
            apply: "printf 'overlay\\nbr_netfilter\\n' > /etc/modules-load.d/k8s.conf && \
                    modprobe overlay && modprobe br_netfilter"
                .to_string(),
            offline: Some(
                "printf 'overlay\\nbr_netfilter\\n' > /etc/modules-load.d/k8s.conf".to_string(),
            ),
        },
        HostRecipe {
            name: "sysctls de rede do kubelet/CNI aplicados",
            check: "[ \"$(sysctl -n net.ipv4.ip_forward)\" = \"1\" ]".to_string(),
            apply: "printf 'net.bridge.bridge-nf-call-iptables=1\\nnet.ipv4.ip_forward=1\\n\
                    net.bridge.bridge-nf-call-ip6tables=1\\n' > /etc/sysctl.d/k8s.conf && \
                    sysctl --system"
                .to_string(),
            offline: Some(
                "printf 'net.bridge.bridge-nf-call-iptables=1\\nnet.ipv4.ip_forward=1\\n\
                 net.bridge.bridge-nf-call-ip6tables=1\\n' > /etc/sysctl.d/k8s.conf"
                    .to_string(),
            ),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::vmimage::DEFAULT_K8S_VERSION;

    /// A version for the cases that are not ABOUT the version — concrete,
    /// because the catalogue no longer accepts "unspecified", and deliberately
    /// NOT `DEFAULT_K8S_VERSION`: these tests must not start passing or
    /// failing because the default moved.
    const ANY: &str = "1.33";

    #[test]
    fn extra_packages_entram_na_receita_de_instalacao() {
        let recipes = k8s_host_recipes(ANY, &["ipvsadm".to_string()]);
        let install = recipes
            .iter()
            .find(|r| r.name.contains("instalados"))
            .unwrap();
        assert!(install.apply.contains("ipvsadm"));
        assert!(install.apply.contains("kubeadm"));
    }

    #[test]
    fn k8s_version_explicita_entra_no_repo() {
        let recipes = k8s_host_recipes("1.30", &[]);
        let repo_step = recipes
            .iter()
            .find(|r| r.name.contains("repositório"))
            .unwrap();
        assert!(repo_step.apply.contains("stable:/v1.30"));
    }

    #[test]
    fn k8s_version_patch_e_truncada_para_major_minor_no_repo() {
        // BUG regression guard: `valid_version` (cluster.rs) explicitly
        // accepts a patch version like "1.31.2" (its own doc-comment says
        // so) but pkgs.k8s.io only ever publishes MINOR-level repo
        // directories — the untruncated version produced a 404 repo URL,
        // failing `apt-get update` on every host, well past manifest
        // validation.
        let recipes = k8s_host_recipes("1.31.2", &[]);
        let repo_step = recipes
            .iter()
            .find(|r| r.name.contains("repositório"))
            .unwrap();
        assert!(
            repo_step.apply.contains("stable:/v1.31/"),
            "{}",
            repo_step.apply
        );
        assert!(!repo_step.apply.contains("v1.31.2"));
    }

    #[test]
    fn major_minor_trunca_so_quando_ha_terceira_componente() {
        assert_eq!(super::major_minor("1.31.2"), "1.31");
        assert_eq!(super::major_minor("1.31"), "1.31");
        assert_eq!(super::major_minor("1"), "1");
    }

    /// REGRESSION (2026-10-15): this test was previously named after the
    /// `stable:/v1.31` repo it expected, and asserted the BUG — that an
    /// omitted version produced that repo — while `DEFAULT_K8S_VERSION` said
    /// 1.36. It passed happily, because it only ever checked this default in
    /// isolation from the one it was supposed to agree with.
    #[test]
    fn no_version_uses_the_default_version() {
        let repo = k8s_repo_version(&resolve_k8s_version(None));
        assert_eq!(repo, format!("stable:/v{DEFAULT_K8S_VERSION}"));
        assert!(
            !repo.contains("v1.31"),
            "o default independente de 1.31 voltou: {repo}"
        );
    }

    /// `"stable"` is the one non-numeric input the resolver still special-cases
    /// (no entry point can reach it — `cluster::valid_version` rejects
    /// letters), and it must NOT leave the version unpinned.
    #[test]
    fn stable_resolves_to_the_default_version() {
        assert_eq!(resolve_k8s_version(Some("stable")), DEFAULT_K8S_VERSION);
        assert_eq!(resolve_k8s_version(None), DEFAULT_K8S_VERSION);
        assert_eq!(resolve_k8s_version(Some("1.34")), "1.34");
    }

    #[test]
    fn cinco_receitas_por_omissao() {
        assert_eq!(k8s_host_recipes(ANY, &[]).len(), 5);
    }

    #[test]
    fn config_recipes_sao_o_subconjunto_sem_rede() {
        let config = k8s_config_recipes();
        assert_eq!(config.len(), 3, "swap + módulos + sysctls");
        // The guarantee that matters to the offline build: NONE touches the network.
        for r in &config {
            let cmd = r.apply_offline();
            assert!(
                !cmd.contains("curl") && !cmd.contains("apt-get") && !cmd.contains("https://"),
                "receita '{}' precisa de rede: {cmd}",
                r.name
            );
        }
        // And they are exactly the tail of the full catalogue (they do not diverge).
        let all = k8s_host_recipes(ANY, &[]);
        assert_eq!(&all[2..], &config[..], "as 3 finais têm de ser as mesmas");
    }
}
