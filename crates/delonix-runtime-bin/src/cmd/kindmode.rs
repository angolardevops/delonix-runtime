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
    /// `None` = o delonix escolhe uma livre (ver `pick_api_port`).
    pub api_port: Option<u16>,
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

/// A porta está livre? Verifica os DOIS sítios que importam: nenhum container
/// vivo a publica (o nosso store) e nada no host a tem presa (bind de teste).
/// Só o store não chega — um processo qualquer da máquina pode estar lá.
fn port_free(store: &Store, port: u16) -> bool {
    if super::container::port_owner(store, &port.to_string()).ok().flatten().is_some() {
        return false;
    }
    // O bind de teste liberta-se logo a seguir (o listener cai). Há uma janela
    // de corrida até o slirp a tomar — inevitável sem um alocador central, e
    // benigna: no pior caso o `run` falha com o erro claro de porta ocupada.
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Escolhe a porta do apiserver.
///
/// **Explícito manda; default é esperto.** Com `--api-port` dado, respeita-se e
/// falha-se claro se estiver ocupada (o utilizador pediu AQUELA porta — dar-lhe
/// outra em silêncio seria pior que o erro). Sem flag, tenta a 6443 (a
/// convenção) e, se estiver tomada — tipicamente por outro cluster já a correr —
/// escolhe uma alta livre em vez de chatear: criar um 2.º cluster não devia
/// obrigar a inventar portas à mão.
fn pick_api_port(store: &Store, preferred: Option<u16>, cluster: &str) -> Result<u16> {
    if let Some(p) = preferred {
        if !port_free(store, p) {
            let by = super::container::port_owner(store, &p.to_string())
                .ok()
                .flatten()
                .map(|n| format!(" (pelo container '{n}')"))
                .unwrap_or_default();
            return Err(Error::Invalid(format!(
                "a porta {p} já está em uso{by} — escolhe outra com `--api-port` ou omite a flag \
                 para o delonix escolher uma livre"
            )));
        }
        return Ok(p);
    }
    if port_free(store, DEFAULT_API_PORT) {
        return Ok(DEFAULT_API_PORT);
    }
    // 6443 tomada: procura uma alta livre. O intervalo é dos efémeros altos,
    // longe das portas de serviço.
    for p in 36443..36543 {
        if port_free(store, p) {
            eprintln!("porta {DEFAULT_API_PORT} ocupada — o cluster '{cluster}' usa a {p}");
            return Ok(p);
        }
    }
    Err(Error::Invalid(
        "não encontrei nenhuma porta livre para o apiserver (6443 e 36443-36542 ocupadas)".into(),
    ))
}

/// A porta convencional do apiserver — a primeira escolha.
pub(crate) const DEFAULT_API_PORT: u16 = 6443;

/// Directório do cluster no HOST, montado em `/kind/delonix` dentro de cada nó.
///
/// **Porquê um bind mount e não escrever no rootfs**: com `--net <rede>` o
/// container é criado pelo 2.º passo do re-exec, que corre dentro do MOUNT
/// namespace do holder — o overlay do rootfs é montado LÁ e é invisível daqui.
/// Do host, o `merged/` aparece vazio e qualquer ficheiro que lá escrevêssemos
/// (ou lêssemos) nunca chegaria ao container: foi assim que o kubeadm ficou a
/// dizer `unable to read config from /kind/delonix-kubeadm.conf` com o ficheiro
/// a existir no disco. Nenhuma resolução de caminho resolve isto — a montagem
/// simplesmente não está no nosso namespace.
/// Um bind mount é montado PELO RUNTIME durante a criação do container, dentro
/// do namespace certo, e o mesmo directório fica visível dos dois lados: é a
/// ponte que faltava (e o mecanismo já existia, `-v /host:/dest`).
fn cluster_dir(name: &str) -> std::path::PathBuf {
    super::util::state_root().join("clusters").join(name)
}

/// Onde o `cluster_dir` aparece DENTRO do nó.
const NODE_SHARED: &str = "/kind/delonix";

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


/// Reis/rainhas de Angola — Ndongo, Kongo, Matamba, Bailundo.
const REIS: &[&str] = &[
    "njinga", "mandume", "ekuikui", "nzinga", "kiluanji", "ngola", "mbandi",
    "kitamba", "katyavala", "samakaka", "kalandula", "mutu", "hoolo", "soba",
];

/// Províncias, municípios e comunas de Angola.
const LUGARES: &[&str] = &[
    "luanda", "benguela", "huambo", "huila", "bie", "malanje", "uige", "zaire",
    "cunene", "namibe", "moxico", "bengo", "cuando", "cubango", "viana",
    "cacuaco", "belas", "talatona", "kilamba", "catumbela", "lobito", "lubango",
    "chibia", "cazenga", "sumbe", "ndalatando", "menongue", "saurimo", "dundo",
    "ondjiva", "caxito", "gabela", "quibala", "camacupa", "andulo", "chinguar",
];

/// Inventa um nome de cluster (rei + lugar + sufixo), evitando os já usados.
///
/// Sem isto, o `create` sem `--name` usava sempre "delonix" e colidia à segunda
/// invocação ("o nó 'delonix-control-plane' já existe"), obrigando o utilizador
/// a inventar nomes à mão. Um nome legível é melhor que um hash: aparece no
/// `cluster ls`, nos nós e no kubeconfig — e estes lêem-se e dizem-se.
///
/// Aleatoriedade sem dependências novas: nanos do relógio + pid. Não é
/// criptográfico nem precisa de ser; o que interessa é não colidir, e isso é
/// garantido pela verificação contra os nomes existentes (o espaço é de ~50k
/// combinações, a colisão é improvável e, se acontecer, tenta outro).
pub(crate) fn random_cluster_name(store: &Store) -> Result<String> {
    let existing: Vec<String> = store
        .list()?
        .iter()
        .filter_map(|c| c.labels.get("io.x-k8s.kind.cluster").cloned())
        .collect();
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64) << 20;
    for _ in 0..50 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); // LCG
        let r = (seed >> 33) as usize;
        let name = format!(
            "{}-{}-{:02}",
            REIS[r % REIS.len()],
            LUGARES[(r / REIS.len()) % LUGARES.len()],
            (r / (REIS.len() * LUGARES.len())) % 100
        );
        if !existing.contains(&name) {
            return Ok(name);
        }
    }
    Err(Error::Invalid("não consegui inventar um nome livre — passa `--name`".into()))
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
            // A ponte host<->nó (ver `cluster_dir`): sem isto não há como pôr o
            // kubeadm.conf lá dentro nem trazer o join/kubeconfig cá para fora.
            volumes: vec![format!("{}:{NODE_SHARED}", cluster_dir(&cfg.name).display())],
            // `/dev/fuse`: o entrypoint do Kind escolhe o snapshotter
            // `fuse-overlayfs` em userns, e sem este device o
            // `containerd-fuse-overlayfs` morre em ciclo ("fuse: device not
            // found") — o containerd fica sem conseguir extrair UMA imagem e o
            // kubeadm falha no preflight com `[ERROR ImagePull]`. O
            // `--privileged` do Docker expõe o /dev do host inteiro e traz o
            // fuse de graça; o nosso /dev é um tmpfs com uma lista curada, por
            // isso pede-se explicitamente. É seguro em rootless: no host o
            // /dev/fuse é crw-rw-rw-.
            devices: vec!["/dev/fuse".to_string()],
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

    // O dir partilhado tem de existir ANTES do 1.º nó (é o alvo do bind mount).
    std::fs::create_dir_all(cluster_dir(&cfg.name))?;
    // A rede do cluster: os nós têm de nascer todos nela.
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

    // Resolve a porta ANTES de arrancar o nó: um 2.º cluster não deve rebentar
    // só porque a 6443 está tomada.
    let api_port = pick_api_port(store, cfg.api_port, &cfg.name)?;
    let c = boot_node(images, store, cfg, &node, "control-plane", vec![format!("{api_port}:6443")])?;
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
    std::fs::write(cluster_dir(&cfg.name).join("kubeadm.conf"), &kubeadm_conf)?;

    eprintln!("a correr `kubeadm init` (puxa as imagens do control-plane — pode demorar)...");
    node_must(
        &c,
        "kubeadm init",
        &format!(
            "kubeadm init --config {NODE_SHARED}/kubeadm.conf \
             --ignore-preflight-errors=Swap,SystemVerification,FileContent--proc-sys-net-bridge-bridge-nf-call-iptables,Mem,NumCPU \
             2>&1 | tail -3"
        ),
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
        // O CP escreve o join no dir PARTILHADO — e o host lê-o de lá. Ler do
        // rootfs não funcionaria (ver `cluster_dir`).
        node_must(
            &c,
            "gerar o comando de join",
            &format!(
                "KUBECONFIG=/etc/kubernetes/admin.conf kubeadm token create --print-join-command \
                 > {NODE_SHARED}/join.sh 2>/dev/null"
            ),
        )?;
        let join_cmd = std::fs::read_to_string(cluster_dir(&cfg.name).join("join.sh"))
            .map_err(|e| Error::Invalid(format!("a ler o comando de join: {e}")))?;
        let join_cmd = join_cmd.trim().to_string();
        for i in 1..=cfg.workers {
            let wnode = format!("{}-worker{}", cfg.name, if i == 1 { String::new() } else { i.to_string() });
            let w = boot_node(images, store, cfg, &wnode, "worker", Vec::new())?;
            // O worker precisa da MESMA receita rootless do control-plane: a
            // feature gate e o swap valem para qualquer kubelet, não só o do CP.
            let _ = &w; // o config do worker vai pela ponte partilhada
            std::fs::write(
                cluster_dir(&cfg.name).join("join-kubelet.conf"),
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
                    "{join_cmd} --config {NODE_SHARED}/join-kubelet.conf \
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

    write_kubeconfig(&c, &cfg.name, api_port)?;
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
    // O nó escreve no dir PARTILHADO; o host lê de lá (ver `cluster_dir`).
    node_must(
        c,
        "exportar o kubeconfig",
        &format!(
            "sed 's|server: https://.*:6443|server: https://127.0.0.1:{api_port}|' \
             /etc/kubernetes/admin.conf > {NODE_SHARED}/kubeconfig.yaml"
        ),
    )?;
    let src = cluster_dir(name).join("kubeconfig.yaml");
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

/// Há quanto tempo o processo do nó está de pé (segundos). Vem do
/// `/proc/<pid>/stat` (campo 22: arranque em ticks desde o boot) cruzado com o
/// `/proc/uptime` — e NÃO do `created_unix` do registo, que é a hora de criação
/// e não muda num restart (daria "uptime" a crescer para sempre).
fn node_uptime_secs(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // O comm pode ter espaços/parênteses; os campos contam-se DEPOIS do ')'.
    let after = stat.rsplit_once(')')?.1;
    let start_ticks: u64 = after.split_whitespace().nth(19)?.parse().ok()?;
    let hz = 100u64; // USER_HZ é 100 em Linux/x86-64
    let up: f64 = std::fs::read_to_string("/proc/uptime").ok()?.split_whitespace().next()?.parse().ok()?;
    Some((up as u64).saturating_sub(start_ticks / hz))
}

fn fmt_dur(mut s: u64) -> String {
    let d = s / 86400;
    s %= 86400;
    let h = s / 3600;
    s %= 3600;
    let m = s / 60;
    if d > 0 {
        format!("{d}d{h}h")
    } else if h > 0 {
        format!("{h}h{m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

/// `cluster ls` — os clusters do modo kind, agrupados pela label
/// `io.x-k8s.kind.cluster` dos nós (é ela a fonte de verdade: não há registo de
/// "cluster" à parte, e inventá-lo criaria estado a dessincronizar).
pub(crate) fn list(store: &Store) -> Result<()> {
    use std::collections::BTreeMap;
    let mut clusters: BTreeMap<String, Vec<Container>> = BTreeMap::new();
    for mut c in store.list()? {
        let Some(name) = c.labels.get("io.x-k8s.kind.cluster").cloned() else { continue };
        if delonix_runtime::reconcile_status(&mut c) {
            let _ = store.update(&c.id, |cur| delonix_runtime::reconcile_status(cur));
        }
        clusters.entry(name).or_default().push(c);
    }
    if clusters.is_empty() {
        println!("(nenhum cluster — cria um com `delonix cluster create`)");
        return Ok(());
    }

    // O `last restart` sai do LOG DE EVENTOS (o `Container` não conta reinícios):
    // o `start`/`die` mais recente de cada nó. É a prova de que o log serve para
    // mais que `system events`.
    let evs = delonix_runtime_core::events::read(&super::util::state_root());

    println!(
        "{:<12}  {:<9}  {:>2}  {:>7}  {:<10}  {:<8}  {:<16}  {}",
        "NOME", "ESTADO", "CP", "WORKERS", "PORTA API", "UPTIME", "ÚLTIMO REINÍCIO", "CRI SOCKET"
    );
    for (name, nodes) in clusters {
        let cp: Vec<&Container> = nodes
            .iter()
            .filter(|c| c.labels.get("io.x-k8s.kind.role").map(|r| r == "control-plane").unwrap_or(false))
            .collect();
        let workers = nodes.len() - cp.len();
        let running = nodes.iter().filter(|c| matches!(c.status, delonix_runtime_core::Status::Running)).count();
        let estado = if running == nodes.len() {
            "up".to_string()
        } else {
            format!("{running}/{} up", nodes.len())
        };

        // Porta do apiserver: a publicada pelo control-plane.
        let api = cp
            .first()
            .and_then(|c| c.ports.first())
            .and_then(|p| delonix_net::parse_publish(p).ok())
            .map(|(hp, _, _)| hp)
            .unwrap_or_else(|| "-".into());

        let uptime = cp
            .first()
            .and_then(|c| c.pid)
            .and_then(node_uptime_secs)
            .map(fmt_dur)
            .unwrap_or_else(|| "-".into());

        // O reinício mais recente de QUALQUER nó do cluster.
        let ids: Vec<&str> = nodes.iter().map(|c| c.id.as_str()).collect();
        let last = evs
            .iter()
            .filter(|e| ids.contains(&e.id.as_str()) && (e.action == "start" || e.action == "die"))
            .map(|e| e.ts)
            .max()
            .map(|ts| delonix_runtime_core::fmt_local_ts(ts))
            .unwrap_or_else(|| "—".into());

        // O socket do CRI é o que ESCREVEMOS no kubeadm.conf do cluster — lê-se
        // de lá (o dir partilhado), em vez de adivinhar ou de pagar um `exec`.
        let cri = std::fs::read_to_string(cluster_dir(&name).join("kubeadm.conf"))
            .ok()
            .and_then(|t| {
                t.lines()
                    .find(|l| l.trim_start().starts_with("criSocket:"))
                    .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()))
            })
            .unwrap_or_else(|| "-".into());

        println!("{name:<12}  {estado:<9}  {:>2}  {workers:>7}  {api:<10}  {uptime:<8}  {last:<16}  {cri}", cp.len());
    }
    Ok(())
}
