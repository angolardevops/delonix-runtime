//! Catálogo partilhado das "receitas" de preparação de um host para
//! `kubeadm`/`kubelet`/`kubectl` — a parte tecnicamente sensível (repositório
//! apt assinado, pacotes certos, swap, módulos kernel, sysctls) que TEM de
//! ficar idêntica entre `cmd::vmimage::build` (aplica-as offline, uma vez,
//! via `virt-customize`, na imagem dourada) e `cmd::cluster::apply` (aplica-as
//! ao vivo, via SSH, em hosts arbitrários) — nunca diverge entre os dois
//! pipelines. Conta (`delonix` user/root password) e instalação do binário
//! `delonix-cri` ficam FORA daqui, deliberadamente: usam mecanismos nativos
//! diferentes em cada transporte (flags `--root-password` do virt-customize
//! vs `chpasswd` por SSH; `--copy-in` vs `scp`) — forçar um shell-command
//! comum aí não traria benefício real, só risco.

/// Uma receita idempotente: `check` (comando shell; código de saída 0 = já
/// satisfeita) + `apply` (comando shell que a satisfaz). `cmd::cluster::apply`
/// usa `check`+`apply` via SSH em hosts VIVOS; `cmd::vmimage::build` usa
/// `apply_offline()` — a variante sem efeitos-vivos (`modprobe`/`sysctl
/// --system`/`swapoff` não fazem sentido no chroot do virt-customize, cujo
/// kernel é o do HOST de build: só a persistência em /etc interessa, os
/// efeitos acontecem no primeiro boot da VM).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HostRecipe {
    pub name: &'static str,
    pub check: String,
    pub apply: String,
    /// Variante para customização offline de imagem (None = igual a `apply`).
    pub offline: Option<String>,
}

impl HostRecipe {
    /// O comando a usar num build de imagem (offline): só persistência.
    pub fn apply_offline(&self) -> &str {
        self.offline.as_deref().unwrap_or(&self.apply)
    }
}

/// A versão do repositório `pkgs.k8s.io` (`stable:/v1.31` por omissão).
fn k8s_repo_version(k8s_version: Option<&str>) -> String {
    match k8s_version {
        Some(v) if v != "stable" => format!("stable:/v{v}"),
        _ => "stable:/v1.31".to_string(),
    }
}

/// Constrói o catálogo — `extra_packages` estende a lista de pacotes
/// instalados na receita `kubeadm/kubelet/kubectl` sem tocar nesta função
/// (é a parte "100% parametrizada" do pipeline).
pub(crate) fn k8s_host_recipes(k8s_version: Option<&str>, extra_packages: &[String]) -> Vec<HostRecipe> {
    let repo = k8s_repo_version(k8s_version);
    let mut packages = vec!["kubeadm".to_string(), "kubelet".to_string(), "kubectl".to_string()];
    packages.extend(extra_packages.iter().cloned());
    let packages_str = packages.join(" ");

    vec![
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
            offline: Some("printf 'overlay\\nbr_netfilter\\n' > /etc/modules-load.d/k8s.conf".to_string()),
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
        let install = recipes.iter().find(|r| r.name.contains("instalados")).unwrap();
        assert!(install.apply.contains("ipvsadm"));
        assert!(install.apply.contains("kubeadm"));
    }

    #[test]
    fn k8s_version_explicita_entra_no_repo() {
        let recipes = k8s_host_recipes(Some("1.30"), &[]);
        let repo_step = recipes.iter().find(|r| r.name.contains("repositório")).unwrap();
        assert!(repo_step.apply.contains("stable:/v1.30"));
    }

    #[test]
    fn sem_versao_usa_stable_v1_31() {
        let recipes = k8s_host_recipes(None, &[]);
        let repo_step = recipes.iter().find(|r| r.name.contains("repositório")).unwrap();
        assert!(repo_step.apply.contains("stable:/v1.31"));
    }

    #[test]
    fn cinco_receitas_por_omissao() {
        assert_eq!(k8s_host_recipes(None, &[]).len(), 5);
    }
}
