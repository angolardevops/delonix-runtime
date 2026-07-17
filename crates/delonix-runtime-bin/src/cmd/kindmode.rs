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
    /// Versão do Kubernetes (ex.: "1.34"). `None` = a que a imagem traz.
    pub k8s_version: Option<String>,
    /// Quantos workers juntar ao control-plane (0 = só control-plane, sem taint).
    pub workers: u32,
}

/// Escreve um ficheiro DENTRO do nó. Vai directo ao rootfs no host (é um
/// caminho normal do sistema de ficheiros) em vez de passar por heredocs no
/// `exec` — o conteúdo é YAML com aspas, `#` e indentação, e passá-lo por uma
/// shell seria um convite a bugs de escape.
fn write_node_file(c: &Container, path_in_node: &str, content: &str) -> Result<()> {
    let base = super::util::state_root()
        .join("containers")
        .join(&c.id)
        .join("rootfs")
        .join(path_in_node.trim_start_matches('/'));
    if let Some(dir) = base.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&base, content)
        .map_err(|e| Error::Invalid(format!("a escrever {path_in_node} no nó ({}): {e}", base.display())))?;
    Ok(())
}

/// Lê um ficheiro DE DENTRO do nó (o rootfs é um caminho do host).
fn read_node_file(c: &Container, path_in_node: &str) -> Result<String> {
    let p = super::util::state_root()
        .join("containers")
        .join(&c.id)
        .join("rootfs")
        .join(path_in_node.trim_start_matches('/'));
    std::fs::read_to_string(&p)
        .map_err(|e| Error::Invalid(format!("a ler {path_in_node} do nó ({}): {e}", p.display())))
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

/// Nome da rede do cluster. Cada cluster tem a SUA — como o `kind`, que cria
/// uma bridge por cluster. É o que permite aos nós verem-se: sem rede partilhada
/// um worker nunca alcança o apiserver (com `--net host -p`, cada nó fica num
/// netns próprio com NAT, isolado dos outros).
fn cluster_net(name: &str) -> String {
    format!("dlx-{name}")
}

/// Arranca UM nó do cluster (control-plane ou worker) na rede partilhada.
fn boot_node(
    images: &ImageStore,
    store: &Store,
    cfg: &KindCluster,
    node: &str,
    role: &str,
    publish: Vec<String>,
) -> Result<Container> {
    eprintln!("a arrancar o nó '{node}' ({role})...");
    container::cmd_run(
        images,
        store,
        RunOpts {
            detach: true,
            name: Some(node.to_string()),
            // Rede do cluster: é isto que faz os nós verem-se (ver `cluster_net`).
            net: cluster_net(&cfg.name),
            volumes: Vec::new(),
            ports: publish,
            privileged: true,
            entrypoint: None,
            rm: false,
            // O systemd do nó é o PID 1 e já supervisiona o que corre lá dentro.
            restart: "no".to_string(),
            env: Vec::new(),
            labels: vec![
                format!("io.x-k8s.kind.role={role}"),
                format!("io.x-k8s.kind.cluster={}", cfg.name),
            ],
            image: cfg.image.clone(),
            command: Vec::new(),
        },
    )?;
    let c = store
        .list()?
        .into_iter()
        .find(|c| c.name == node)
        .ok_or_else(|| Error::Invalid(format!("o nó '{node}' não ficou registado no store")))?;
    wait_in_node(&c, "containerd", "systemctl is-active containerd", Duration::from_secs(90))?;
    Ok(c)
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

    // A rede do cluster primeiro: os nós têm de nascer todos nela.
    let net = cluster_net(&cfg.name);
    let nstore = delonix_net::NetworkStore::open(super::util::state_root())?;
    if nstore.get(&net).is_err() {
        // `create_network` (e não `infra::network_create`) porque são DOIS stores
        // coordenados: o registo declarativo + o plano físico do holder, com o
        // mesmo prefixo. Só o físico deixava o `run --net` a recusar com
        // "no such container: network <x>" — apanhado a testar o multi-nó.
        super::network::create_network(&nstore, &net, "bridge", None, None, "", None, Vec::new(), None)?;
        eprintln!("rede '{net}' criada");
    }

    let c = boot_node(images, store, cfg, &node, "control-plane", vec![format!("{}:6443", cfg.api_port)])?;
    // O IP REAL do nó na rede do cluster. Com `--net host -p` era o do slirp
    // (10.0.2.100, igual em todos os nós e inalcançável de fora dele); numa rede
    // partilhada cada nó tem o seu — e é este que o apiserver anuncia e os
    // workers usam no `join`.
    let cp_ip = c.ip.clone().ok_or_else(|| {
        Error::Invalid(format!("o nó '{node}' não recebeu IP na rede '{net}'"))
    })?;


    // --- config do kubeadm: TUDO o que o rootless precisa, numa passagem ---
    //
    // Podia-se correr `kubeadm init` com flags e remendar depois, mas isso
    // obriga o init a FALHAR primeiro (fica 4min à espera de um kubelet que
    // nunca fica pronto sem a feature gate) e depois a correr as fases à mão.
    // Um ficheiro de config leva as 3 afinações ANTES de o kubelet arrancar —
    // uma só passagem, sem remendos. É também o que o `kind` faz (/kind/kubeadm.conf).
    eprintln!("a gerar o config do kubeadm (rootless)...");
    let version = cfg.k8s_version.as_deref().map(|v| format!("kubernetesVersion: v{v}\n")).unwrap_or_default();
    let kubeadm_conf = format!(
        "apiVersion: kubeadm.k8s.io/v1beta4\n\
         kind: InitConfiguration\n\
         localAPIEndpoint:\n  advertiseAddress: {ip}\n  bindPort: 6443\n\
         nodeRegistration:\n  criSocket: unix:///run/containerd/containerd.sock\n\
         ---\n\
         apiVersion: kubeadm.k8s.io/v1beta4\n\
         kind: ClusterConfiguration\n{version}\
         networking:\n  podSubnet: {pods}\n  serviceSubnet: {svcs}\n\
         apiServer:\n  certSANs:\n\
         \x20 # O kubeconfig exportado aponta para 127.0.0.1:<porta publicada> — o\n\
         \x20 # apiserver so escuta no IP do slirp (10.0.2.100), que nao existe cá\n\
         \x20 # fora. Sem estes SANs o `kubectl` do HOST rebenta com\n\
         \x20 # `x509: certificate is valid for 10.0.2.100, not 127.0.0.1`.\n\
         \x20 - \"127.0.0.1\"\n  - \"localhost\"\n\
         ---\n\
         apiVersion: kubelet.config.k8s.io/v1beta1\n\
         kind: KubeletConfiguration\n\
         cgroupDriver: systemd\n\
         # Um container herda o /proc/swaps do HOST — sem isto o kubelet recusa arrancar.\n\
         failSwapOn: false\n\
         featureGates:\n  # O passo decisivo em rootless: sem ele o kubelet morre em `open /dev/kmsg`.\n  KubeletInUserNamespace: true\n\
         ---\n\
         apiVersion: kubeproxy.config.k8s.io/v1alpha1\n\
         kind: KubeProxyConfiguration\n\
         conntrack:\n  # nf_conntrack_max e um sysctl GLOBAL: nao escrevivel de um userns.\n  maxPerCore: 0\n  min: 0\n",
        ip = cp_ip,
        pods = cfg.pod_subnet,
        svcs = cfg.service_subnet,
    );
    write_node_file(&c, "/kind/delonix-kubeadm.conf", &kubeadm_conf)?;

    eprintln!("a correr `kubeadm init` (puxa as imagens do control-plane — pode demorar)...");
    node_must(
        &c,
        "kubeadm init",
        "kubeadm init --config /kind/delonix-kubeadm.conf \
         --ignore-preflight-errors=Swap,SystemVerification,FileContent--proc-sys-net-bridge-bridge-nf-call-iptables,Mem,NumCPU \
         2>&1 | tail -3",
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
    // Com workers, a taint FICA — é para isso que eles existem (é o que o kind faz).
    if cfg.workers == 0 {
    let _ = node_exec(
        &c,
        "KUBECONFIG=/etc/kubernetes/admin.conf kubectl taint nodes --all \
         node-role.kubernetes.io/control-plane- >/dev/null 2>&1",
    );
    }

    eprintln!("à espera do nó ficar Ready...");
    wait_in_node(
        &c,
        "nó Ready",
        "KUBECONFIG=/etc/kubernetes/admin.conf kubectl get nodes --no-headers 2>/dev/null | grep -qw Ready",
        Duration::from_secs(180),
    )?;

    // --- workers: juntam-se pela rede do cluster (ver `cluster_net`) ---
    if cfg.workers > 0 {
        // O token do `join` vale 24h e vem do control-plane. `--print-join-command`
        // devolve a linha inteira (token + hash do CA) — não a construímos à mão.
        let join_file = "/kind/join.sh";
        node_must(
            &c,
            "gerar o comando de join",
            &format!(
                "KUBECONFIG=/etc/kubernetes/admin.conf kubeadm token create --print-join-command \
                 > {join_file} 2>/dev/null && chmod +x {join_file}"
            ),
        )?;
        let join_cmd = read_node_file(&c, join_file)?;
        let join_cmd = join_cmd.trim();
        for i in 1..=cfg.workers {
            let wnode = format!("{}-worker{}", cfg.name, if i == 1 { String::new() } else { i.to_string() });
            let w = boot_node(images, store, cfg, &wnode, "worker", Vec::new())?;
            // O worker precisa da MESMA receita rootless do control-plane: a
            // feature gate e o swap valem para qualquer kubelet, não só o do CP.
            write_node_file(
                &w,
                "/kind/delonix-join.conf",
                "apiVersion: kubelet.config.k8s.io/v1beta1\n\
                 kind: KubeletConfiguration\n\
                 cgroupDriver: systemd\n\
                 failSwapOn: false\n\
                 featureGates:\n  KubeletInUserNamespace: true\n",
            )?;
            eprintln!("a juntar o worker '{wnode}' ao control-plane...");
            node_must(
                &w,
                &format!("join do worker '{wnode}'"),
                &format!(
                    "{join_cmd} --config /kind/delonix-join.conf \
                     --ignore-preflight-errors=Swap,SystemVerification,FileContent--proc-sys-net-bridge-bridge-nf-call-iptables,Mem,NumCPU \
                     2>&1 | tail -3"
                ),
            )?;
        }
        eprintln!("à espera dos {} worker(s) ficarem Ready...", cfg.workers);
        wait_in_node(
            &c,
            "workers Ready",
            &format!(
                "[ \"$(KUBECONFIG=/etc/kubernetes/admin.conf kubectl get nodes --no-headers 2>/dev/null | grep -cw Ready)\" = \"{}\" ]",
                cfg.workers + 1
            ),
            Duration::from_secs(180),
        )?;
    }

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
