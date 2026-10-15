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
pub(crate) fn k8s_repo_version(k8s_version: Option<&str>) -> String {
    match k8s_version {
        Some(v) if v != "stable" => format!("stable:/v{}", major_minor(v)),
        _ => "stable:/v1.31".to_string(),
    }
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
    let mut all = vec![
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
    ];
    all.extend(cni_prereq_recipes());
    all
}

/// The node prerequisites a pod-network PROFILE needs, on top of the kubelet's
/// own (swap off, `overlay`/`br_netfilter`, the three sysctls above).
///
/// Part of `k8s_config_recipes` — the no-network subset — for the one reason
/// this file exists: the golden image and a host prepared live by
/// `cluster apply` must end up IDENTICAL. A module the bootstrap loads but the
/// image lacks is a cluster that works on a re-prepared host and fails on a
/// fresh one, which is the hardest class of difference to see.
///
/// The set is the UNION over every profile and dataplane, because the image is
/// built long before a cluster chooses one. A loaded module nobody uses costs
/// a few hundred KiB; a missing one costs a dataplane.
///
/// **What it deliberately does NOT do.** It does not mount bpffs and it does
/// not move anything to cgroup v2. Both are already true on the distros this
/// image is built from (systemd auto-mounts `/sys/fs/bpf`; the unified
/// hierarchy has been the default since Ubuntu 22.04), and writing over a
/// working system to satisfy a checklist is how a build breaks something it
/// did not need to touch. They are VERIFIED instead, by the node preflight —
/// a check that fails loudly beats a change that silently did nothing.
pub(crate) fn cni_prereq_recipes() -> Vec<HostRecipe> {
    // Named explicitly rather than computed from the profile table, so this
    // file reads as what actually lands on a node. The test
    // `the_modules_cover_every_profile` proves the two agree, which is the
    // part that would otherwise rot.
    let modules = [
        "vxlan",        // cilium / calico / flannel VXLAN
        "geneve",       // cilium geneve
        "ipip",         // calico ipip
        "ip_tables",    // calico and flannel program iptables directly
        "nf_conntrack", // and both read conntrack
        // kube-proxy in `ipvs` mode needs the scheduler modules. Without them
        // it starts, logs a line nobody reads, and silently serves iptables —
        // a mode that reports itself as configured and is not.
        "ip_vs",
        "ip_vs_rr",
        "ip_vs_wrr",
        "ip_vs_sh",
    ];
    const MODULES_FILE: &str = "/etc/modules-load.d/delonix-cni.conf";
    let lines = modules.join("\\n");
    // The check reads the FILE, not `lsmod`: a module can be built into the
    // kernel (never listed by `lsmod`) and `modprobe` still succeeds, so
    // checking `lsmod` would re-run the recipe forever on such a kernel.
    let check = format!("test -f {MODULES_FILE}");
    vec![
        HostRecipe {
            name: "network-profile kernel modules persisted",
            check,
            apply: format!(
                "printf '{lines}\\n' > {MODULES_FILE} && \
                 for m in {}; do modprobe $m || true; done",
                modules.join(" ")
            ),
            // `|| true` on the modprobe and ONLY there: a kernel with one of
            // these built in, or without the ipvs schedulers, must not fail
            // the whole host preparation — the profile that needs a missing
            // module is refused by the preflight, by name, which is a better
            // place to say it than here.
            offline: Some(format!("printf '{lines}\\n' > {MODULES_FILE}")),
        },
        HostRecipe {
            name: "network-profile sysctls applied",
            // One file for every profile, so the IMAGE does not have to know
            // which one it will run. `rp_filter=0` is Calico's own documented
            // requirement (its `calico-node` refuses to start otherwise); the
            // two arp_* values are what a VXLAN/IPVS node needs to answer ARP
            // for a service address on the right interface.
            check: "test -f /etc/sysctl.d/delonix-cni.conf".to_string(),
            apply: "printf 'net.ipv4.conf.all.rp_filter=0\\n\
                    net.ipv4.conf.default.rp_filter=0\\n\
                    net.ipv4.conf.all.arp_announce=2\\n\
                    net.ipv4.conf.all.arp_ignore=1\\n' > /etc/sysctl.d/delonix-cni.conf && \
                    sysctl --system"
                .to_string(),
            offline: Some(
                "printf 'net.ipv4.conf.all.rp_filter=0\\n\
                 net.ipv4.conf.default.rp_filter=0\\n\
                 net.ipv4.conf.all.arp_announce=2\\n\
                 net.ipv4.conf.all.arp_ignore=1\\n' > /etc/sysctl.d/delonix-cni.conf"
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
    fn k8s_version_patch_e_truncada_para_major_minor_no_repo() {
        // BUG regression guard: `valid_version` (cluster.rs) explicitly
        // accepts a patch version like "1.31.2" (its own doc-comment says
        // so) but pkgs.k8s.io only ever publishes MINOR-level repo
        // directories — the untruncated version produced a 404 repo URL,
        // failing `apt-get update` on every host, well past manifest
        // validation.
        let recipes = k8s_host_recipes(Some("1.31.2"), &[]);
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
    fn seven_recipes_by_default() {
        // 2 that need the network (repo + packages) + 5 that do not (swap, the
        // kubelet's modules and sysctls, the profiles' modules and sysctls).
        assert_eq!(k8s_host_recipes(None, &[]).len(), 7);
    }

    #[test]
    fn config_recipes_sao_o_subconjunto_sem_rede() {
        let config = k8s_config_recipes();
        assert_eq!(
            config.len(),
            5,
            "swap + the kubelet's modules/sysctls + the network profiles' modules/sysctls"
        );
        // The guarantee that matters to the offline build: NONE touches the network.
        for r in &config {
            let cmd = r.apply_offline();
            assert!(
                !cmd.contains("curl") && !cmd.contains("apt-get") && !cmd.contains("https://"),
                "receita '{}' precisa de rede: {cmd}",
                r.name
            );
            // And none of them has a live effect in the virt-customize chroot,
            // whose kernel is the BUILD HOST's: a `modprobe` or a `sysctl
            // --system` there configures the wrong machine.
            assert!(
                !cmd.contains("modprobe") && !cmd.contains("sysctl --system"),
                "recipe '{}' has a live effect on the offline path: {cmd}",
                r.name
            );
        }
        // And they are exactly the tail of the full catalogue (they do not diverge).
        let all = k8s_host_recipes(None, &[]);
        assert_eq!(&all[2..], &config[..], "the last 5 have to be the same");
    }

    /// The module list is written out by hand so the file reads as what lands
    /// on a node. This is what stops it rotting: every module any profile
    /// declares it needs has to be in it, for every dataplane that profile
    /// renders.
    #[test]
    fn the_modules_cover_every_profile() {
        use super::super::net_profiles::Profile;
        let recipes = cni_prereq_recipes();
        let modules = &recipes
            .iter()
            .find(|r| r.name.contains("kernel modules"))
            .unwrap()
            .apply;
        for p in [Profile::Cilium, Profile::Calico, Profile::Flannel] {
            for dp in p.dataplanes() {
                for m in p.required_modules(dp) {
                    assert!(
                        modules.contains(m),
                        "{} needs module {m} on {dp} and the recipe does not persist it: {modules}",
                        p.name()
                    );
                }
            }
        }
        // And the ipvs schedulers, which no profile declares (they belong to
        // kube-proxy, not to a CNI) but `kubeProxy: ipvs` needs.
        for m in ["ip_vs", "ip_vs_rr", "ip_vs_wrr", "ip_vs_sh"] {
            assert!(modules.contains(m), "{m}: {modules}");
        }
    }
}
