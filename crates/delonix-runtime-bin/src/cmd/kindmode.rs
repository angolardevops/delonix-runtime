//! Modo **kind nativo** — um cluster Kubernetes local em containers, **sem
//! Docker e sem o binário `kind`**.
//!
//! O `kind` verdadeiro é um cliente do Docker: fala `docker run/ps/inspect/
//! network/exec/logs`. Suportá-lo exigiria um shim de compatibilidade Docker
//! (grande). Este módulo salta essa camada: arranca os nós `kindest/node`
//! DIRECTAMENTE no motor Delonix (`cmd::container`) e corre o `kubeadm` lá
//! dentro — o mesmo destino, sem a fachada.
//!
//! # A receita rootless (cada passo custou uma investigação)
//!
//! Um nó Kind rootless não arranca "à primeira"; estes são os passos não-óbvios,
//! todos validados de ponta a ponta (control-plane `Ready`, ver CLAUDE.md):
//!
//! 1. **`--privileged` + label `io.x-k8s.kind.*`** — activa no motor a delegação
//!    de cgroup2 dedicada (`setup_node_cgroup_ns`), o mascaramento das units
//!    systemd que falham em rootless (`mask_slow_node_units`) e o
//!    `seed_kind_nft` (sem ele o entrypoint escolhe o backend iptables *legacy*,
//!    ilegível num userns, e morre).
//! 2. **`-p <porta>:6443`** — não é só para expor o apiserver: é o que faz o
//!    container ganhar um netns PRÓPRIO com slirp4netns. É obrigatório — com
//!    `--net host` o netns é do host, não é "propriedade" do nosso userns, e
//!    então `CAP_NET_ADMIN` não vale lá: nft/iptables falham e o nó não arranca.
//! 3. **`KubeletInUserNamespace: true`** — O passo decisivo. Sem ele o kubelet
//!    morre em `open /dev/kmsg`. (Dar-lhe um `/dev/kmsg` NÃO resolve: o do host
//!    é `root:adm 0640` e um symlink p/ `/dev/console` só troca ENOENT por EIO.)
//! 4. **`--fail-swap-on=false`** — um container herda o `/proc/swaps` do HOST.
//! 5. **`conntrack.maxPerCore/min = 0`** no kube-proxy — `nf_conntrack_max` é um
//!    sysctl global, não escrevível de um userns (senão: CrashLoopBackOff).
//! 6. **CNI** — o `/kind/manifests/default-cni.yaml` da própria imagem.

use std::time::{Duration, Instant};

use delonix_image::ImageStore;
use delonix_runtime_core::{Container, Error, Result, Store};

use super::container::{self, RunOpts};

/// Imagem de nó por omissão (fixada por digest — uma tag móvel tornaria os
/// clusters irreprodutíveis entre máquinas).
pub(crate) const DEFAULT_NODE_IMAGE: &str =
    "kindest/node:v1.34.0@sha256:7416a61b42b1662ca6ca89f02028ac133a309a2a30ba309614e8ec94d976dc5a";

/// Parâmetros de um cluster em modo kind (vindos das flags ou do manifesto).
pub(crate) struct KindCluster {
    pub name: String,
    pub image: String,
    pub api_port: u16,
    pub pod_subnet: String,
    pub service_subnet: String,
    /// `default` = a CNI da imagem (kindnet); `none` = não instalar nenhuma
    /// (o nó fica `NotReady` até o utilizador aplicar a sua — comportamento
    /// do `kubeadm` puro, deliberado e documentado).
    pub cni: String,
}

/// Corre um comando dentro do nó e devolve o código de saída.
fn node_exec(c: &Container, script: &str) -> Result<i32> {
    let argv = vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()];
    delonix_runtime::exec(c, &argv, false)
}

/// Como [`node_exec`], mas falha com um erro claro se o comando não devolver 0.
fn node_must(c: &Container, what: &str, script: &str) -> Result<()> {
    match node_exec(c, script)? {
        0 => Ok(()),
        code => Err(Error::Invalid(format!("{what} falhou no nó (exit {code})"))),
    }
}

/// Espera por uma condição dentro do nó (comando com exit 0), com timeout.
fn wait_in_node(c: &Container, what: &str, check: &str, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if node_exec(c, check).unwrap_or(1) == 0 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Err(Error::Invalid(format!("timeout à espera de {what} no nó ({}s)", timeout.as_secs())))
}

/// Cria o cluster: arranca o nó control-plane e faz o bootstrap com `kubeadm`.
pub(crate) fn create(images: &ImageStore, store: &Store, cfg: &KindCluster) -> Result<()> {
    let node = format!("{}-control-plane", cfg.name); // convenção de nomes do kind
    if store.list()?.iter().any(|c| c.name == node) {
        return Err(Error::Invalid(format!(
            "o nó '{node}' já existe — usa `delonix cluster delete --name {}` ou outro nome",
            cfg.name
        )));
    }

    eprintln!("a arrancar o nó '{node}' ({})...", cfg.image);
    container::cmd_run(
        images,
        store,
        RunOpts {
            detach: true,
            name: Some(node.clone()),
            // `--net host` + `-p`: o `-p` é que dá ao nó um netns PRÓPRIO com
            // slirp (ver nota 2 no topo) — sem isto o nft não funciona lá dentro.
            net: "host".to_string(),
            volumes: Vec::new(),
            ports: vec![format!("{}:6443", cfg.api_port)],
            privileged: true,
            entrypoint: None,
            rm: false,
            // O supervisor de restart não serve aqui: o systemd do nó é o PID 1
            // e já supervisiona o que corre lá dentro.
            restart: "no".to_string(),
            env: Vec::new(),
            labels: vec![
                "io.x-k8s.kind.role=control-plane".to_string(),
                format!("io.x-k8s.kind.cluster={}", cfg.name),
            ],
            image: cfg.image.clone(),
            command: Vec::new(),
        },
    )?;

    let c = store.list()?.into_iter().find(|c| c.name == node).ok_or_else(|| {
        Error::Invalid(format!("o nó '{node}' não ficou registado no store"))
    })?;

    eprintln!("à espera do containerd no nó...");
    wait_in_node(&c, "containerd", "systemctl is-active containerd", Duration::from_secs(90))?;

    // --- kubelet: as duas afinações sem as quais NÃO arranca em rootless ---
    eprintln!("a configurar o kubelet (KubeletInUserNamespace, swap)...");
    node_must(
        &c,
        "config do kubelet",
        "mkdir -p /etc/default && \
         grep -q fail-swap-on /etc/default/kubelet 2>/dev/null || \
         echo 'KUBELET_EXTRA_ARGS=--fail-swap-on=false' >> /etc/default/kubelet",
    )?;

    eprintln!("a correr `kubeadm init` (puxa as imagens do control-plane — pode demorar)...");
    let init = format!(
        "kubeadm init \
         --apiserver-advertise-address={ip} \
         --pod-network-cidr={pods} \
         --service-cidr={svcs} \
         --ignore-preflight-errors=Swap,SystemVerification,FileContent--proc-sys-net-bridge-bridge-nf-call-iptables,Mem,NumCPU \
         --skip-phases=addon/kube-proxy \
         2>&1 | tail -5",
        ip = delonix_net::SLIRP_IP,
        pods = cfg.pod_subnet,
        svcs = cfg.service_subnet,
    );
    // O `kubeadm init` arranca o kubelet a meio; a feature gate TEM de estar no
    // config.yaml ANTES disso. O `kubeadm` só escreve o config.yaml na fase
    // `kubelet-start`, por isso injecta-se logo a seguir e reinicia-se — é mais
    // simples (e mais robusto) que adivinhar o ficheiro antes de ele existir.
    let _ = node_exec(&c, &init);
    node_must(
        &c,
        "feature gate KubeletInUserNamespace",
        "grep -q KubeletInUserNamespace /var/lib/kubelet/config.yaml 2>/dev/null || \
         printf 'featureGates:\\n  KubeletInUserNamespace: true\\n' >> /var/lib/kubelet/config.yaml; \
         systemctl restart kubelet",
    )?;

    eprintln!("à espera do kubelet ficar saudável...");
    wait_in_node(
        &c,
        "kubelet healthz",
        "curl -sSf -m 3 http://127.0.0.1:10248/healthz >/dev/null 2>&1",
        Duration::from_secs(180),
    )?;

    // Se o `init` abortou antes das fases finais (à espera do kubelet, que só
    // agora ficou de pé), corre-as. São idempotentes.
    eprintln!("a completar as fases do kubeadm...");
    for phase in [
        "upload-config all",
        "upload-certs --upload-certs",
        "mark-control-plane",
        "bootstrap-token",
        "kubelet-finalize all",
        "addon coredns",
    ] {
        let _ = node_exec(&c, &format!("KUBECONFIG=/etc/kubernetes/admin.conf kubeadm init phase {phase} >/dev/null 2>&1"));
    }

    // --- kube-proxy: sem isto entra em CrashLoopBackOff (nf_conntrack_max) ---
    eprintln!("a instalar o kube-proxy (conntrack ajustado para rootless)...");
    node_must(
        &c,
        "addon kube-proxy",
        "KUBECONFIG=/etc/kubernetes/admin.conf kubeadm init phase addon kube-proxy >/dev/null 2>&1; \
         KUBECONFIG=/etc/kubernetes/admin.conf kubectl -n kube-system get cm kube-proxy -o yaml 2>/dev/null \
           | sed -e 's/^\\( *\\)maxPerCore: .*/\\1maxPerCore: 0/' -e 's/^\\( *\\)min: .*/\\1min: 0/' \
           | KUBECONFIG=/etc/kubernetes/admin.conf kubectl replace -f - >/dev/null 2>&1; \
         KUBECONFIG=/etc/kubernetes/admin.conf kubectl -n kube-system delete pod -l k8s-app=kube-proxy >/dev/null 2>&1; \
         true",
    )?;

    // --- CNI (senão o nó fica NotReady para sempre) ---
    if cfg.cni == "default" {
        eprintln!("a aplicar a CNI (kindnet)...");
        node_must(
            &c,
            "CNI",
            &format!(
                "sed 's|{{{{ .PodSubnet }}}}|{pods}|g; s|{{{{.PodSubnet}}}}|{pods}|g' /kind/manifests/default-cni.yaml \
                 | KUBECONFIG=/etc/kubernetes/admin.conf kubectl apply -f - >/dev/null 2>&1",
                pods = cfg.pod_subnet
            ),
        )?;
    }

    // Nó único: sem tirar a taint, nada de utilizador (nem o coredns) agenda.
    let _ = node_exec(
        &c,
        "KUBECONFIG=/etc/kubernetes/admin.conf kubectl taint nodes --all \
         node-role.kubernetes.io/control-plane- >/dev/null 2>&1",
    );

    eprintln!("à espera do nó ficar Ready...");
    wait_in_node(
        &c,
        "nó Ready",
        "KUBECONFIG=/etc/kubernetes/admin.conf kubectl get nodes --no-headers 2>/dev/null | grep -qw Ready",
        Duration::from_secs(180),
    )?;

    write_kubeconfig(&c, &cfg.name, cfg.api_port)?;
    println!("cluster '{}' pronto — kubectl --kubeconfig {} get nodes", cfg.name, kubeconfig_path(&cfg.name).display());
    Ok(())
}

fn kubeconfig_path(name: &str) -> std::path::PathBuf {
    super::util::state_root().join("clusters").join(format!("{name}-kubeconfig.yaml"))
}

/// Traz o `admin.conf` do nó para o host, com o endereço reescrito para a porta
/// publicada (dentro do nó aponta para o IP do slirp, que não existe cá fora).
fn write_kubeconfig(c: &Container, name: &str, api_port: u16) -> Result<()> {
    let path = kubeconfig_path(name);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = format!("/tmp/delonix-kubeconfig-{name}");
    node_must(
        c,
        "exportar o kubeconfig",
        &format!(
            "sed 's|server: https://.*:6443|server: https://127.0.0.1:{api_port}|' \
             /etc/kubernetes/admin.conf > {tmp}"
        ),
    )?;
    // O rootfs do nó é um caminho do host — lê-se directamente, sem `cp` remoto.
    let src = super::util::state_root().join("containers").join(&c.id).join("rootfs").join(tmp.trim_start_matches('/'));
    let data = std::fs::read(&src)
        .map_err(|e| Error::Invalid(format!("a ler o kubeconfig do nó ({}): {e}", src.display())))?;
    std::fs::write(&path, data)?;
    eprintln!("kubeconfig: {}", path.display());
    Ok(())
}

/// Remove um cluster kind: pára e apaga os nós com a label do cluster.
pub(crate) fn delete(images: &ImageStore, store: &Store, name: &str) -> Result<()> {
    let label = format!("io.x-k8s.kind.cluster={name}");
    let (k, v) = label.split_once('=').unwrap();
    let nodes: Vec<Container> = store
        .list()?
        .into_iter()
        .filter(|c| c.labels.get(k).map(|x| x == v).unwrap_or(false))
        .collect();
    if nodes.is_empty() {
        return Err(Error::NotFound(format!("cluster kind '{name}'")));
    }
    for n in &nodes {
        eprintln!("a remover o nó '{}'...", n.name);
        container::remove_container(images, store, n, true)?;
    }
    let _ = std::fs::remove_file(kubeconfig_path(name));
    println!("cluster '{name}' removido ({} nó(s))", nodes.len());
    Ok(())
}
