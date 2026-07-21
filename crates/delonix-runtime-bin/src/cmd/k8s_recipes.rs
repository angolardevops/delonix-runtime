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

/// The version of the `pkgs.k8s.io` repository (`stable:/v1.31` by default).
pub(crate) fn k8s_repo_version(k8s_version: Option<&str>) -> String {
    match k8s_version {
        Some(v) if v != "stable" => format!("stable:/v{v}"),
        _ => "stable:/v1.31".to_string(),
    }
}

/// Builds the catalogue — `extra_packages` extends the list of packages
/// installed in the `kubeadm/kubelet/kubectl` recipe without touching this function
/// (it is the "100% parameterized" part of the pipeline).
pub(crate) fn k8s_host_recipes(
    k8s_version: Option<&str>,
    extra_packages: &[String],
) -> Vec<HostRecipe> {
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

    #[test]
    fn extra_packages_entram_na_receita_de_instalacao() {
        let recipes = k8s_host_recipes(None, &["ipvsadm".to_string()]);
        let install = recipes
            .iter()
            .find(|r| r.name.contains("instalados"))
            .unwrap();
        assert!(install.apply.contains("ipvsadm"));
        assert!(install.apply.contains("kubeadm"));
    }

    #[test]
    fn k8s_version_explicita_entra_no_repo() {
        let recipes = k8s_host_recipes(Some("1.30"), &[]);
        let repo_step = recipes
            .iter()
            .find(|r| r.name.contains("repositório"))
            .unwrap();
        assert!(repo_step.apply.contains("stable:/v1.30"));
    }

    #[test]
    fn sem_versao_usa_stable_v1_31() {
        let recipes = k8s_host_recipes(None, &[]);
        let repo_step = recipes
            .iter()
            .find(|r| r.name.contains("repositório"))
            .unwrap();
        assert!(repo_step.apply.contains("stable:/v1.31"));
    }

    #[test]
    fn cinco_receitas_por_omissao() {
        assert_eq!(k8s_host_recipes(None, &[]).len(), 5);
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
        let all = k8s_host_recipes(None, &[]);
        assert_eq!(&all[2..], &config[..], "as 3 finais têm de ser as mesmas");
    }
}
