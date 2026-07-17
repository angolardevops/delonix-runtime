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
    /// Quantos control-planes. >1 exige um endpoint estável à frente deles.
    pub control_planes: u32,
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
            let msg = if super::output::is_pt() {
                format!("porta {DEFAULT_API_PORT} ocupada — o cluster '{cluster}' usa a {p}")
            } else {
                format!("port {DEFAULT_API_PORT} in use — cluster '{cluster}' uses {p}")
            };
            super::output::info(&msg);
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
///
/// **O stdout/stderr do comando é HERDADO** — vai direito ao terminal do
/// utilizador. Só usar quando se QUER isso; para tudo o resto,
/// [`node_exec_capture`].
fn node_exec(c: &Container, script: &str) -> Result<i32> {
    let argv = vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()];
    delonix_runtime::exec(c, &argv, false)
}

/// Como [`node_exec`], mas **captura** o output em vez de o despejar no terminal.
/// Devolve `(exit code, output combinado)`.
///
/// # Porque assim, e não com um `exec` que capture
///
/// O `delonix_runtime::exec` herda o stdio do processo pai e não tem variante de
/// captura. Em vez de mexer numa API central do motor só por causa disto,
/// redirecciona-se DENTRO do nó para um ficheiro no directório partilhado (que é
/// um bind mount, ver `cluster_dir`) e lê-se do host. Zero mudanças no motor.
///
/// Era isto que faltava para os logs: o `systemctl is-active` do `wait_in_node`
/// imprimia `inactive`/`activating`/`active` a cada sondagem, e os erros do
/// systemd do nó ("System has not been booted with systemd as init system",
/// "Failed to connect to bus") saíam no meio do output do `cluster create` como
/// se fossem falhas nossas. São ruído esperado de dentro do nó — pertencem ao
/// diagnóstico de um passo que falha, não ao caminho feliz.
fn node_exec_capture(c: &Container, script: &str) -> Result<(i32, String)> {
    // Ficheiro por-nó: os workers correm em PARALELO e partilham este directório.
    let out_rel = format!(".out-{}", c.name);
    let code = node_exec(c, &format!("{{ {script} ; }} >{NODE_SHARED}/{out_rel} 2>&1"))?;
    let path = cluster_dir_of(c).join(&out_rel);
    let out = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    Ok((code, out))
}

/// O `cluster_dir` de um nó, a partir da sua label — o `node_exec_capture` não
/// tem o `cfg` à mão.
fn cluster_dir_of(c: &Container) -> std::path::PathBuf {
    let name = c.labels.get("io.x-k8s.kind.cluster").cloned().unwrap_or_default();
    cluster_dir(&name)
}

/// Como [`node_exec_capture`], mas falha se o comando não devolver 0 — e aí (e
/// só aí) mostra o que o nó disse, que é exactamente quando isso interessa.
fn node_must(c: &Container, what: &str, script: &str) -> Result<()> {
    let (code, out) = node_exec_capture(c, script)?;
    if code == 0 {
        return Ok(());
    }
    // As últimas linhas chegam para diagnosticar e não afogam o terminal; o
    // output inteiro de um `kubeadm init` são centenas de linhas.
    let tail: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).rev().take(12).collect();
    let detalhe = tail.into_iter().rev().map(|l| format!("\n    {l}")).collect::<String>();
    Err(Error::Invalid(format!("{what} falhou no nó '{}' (exit {code}){detalhe}", c.name)))
}

/// Espera por uma condição dentro do nó (comando com exit 0), com timeout.
fn wait_in_node(c: &Container, what: &str, check: &str, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        // Captura: o `systemctl is-active` escreve o estado no stdout a cada
        // sondagem, e sem isto o utilizador via `inactive`/`activating`/`active`
        // a escorrer pelo terminal durante todo o arranque.
        if node_exec_capture(c, check).map(|(rc, _)| rc).unwrap_or(1) == 0 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Err(Error::Invalid(format!(
        "timeout à espera de {what} no nó '{}' ({}s)",
        c.name,
        timeout.as_secs()
    )))
}


/// Os dados de adesão que o control-plane emite, extraídos do
/// `kubeadm token create --print-join-command`.
#[derive(Debug, PartialEq)]
struct JoinInfo {
    endpoint: String,
    token: String,
    ca_hash: String,
}

/// Extrai `(endpoint, token, hash do CA)` da linha que o `kubeadm token create
/// --print-join-command` devolve:
///
/// ```text
/// kubeadm join 10.0.0.2:6443 --token ab.cd --discovery-token-ca-cert-hash sha256:ef…
/// ```
///
/// # Porque é preciso desmontar a linha em vez de a correr
///
/// A linha é um comando completo com argumentos posicionais. Corrê-la **e**
/// juntar-lhe `--config` é o que o kubeadm recusa:
/// `can not mix '--config' with arguments [discovery-token-ca-cert-hash token]`.
/// Como o nó rootless PRECISA de config própria, o caminho certo é o contrário:
/// tirar os dados daqui e pô-los DENTRO de uma `JoinConfiguration`, ficando o
/// `join` só com `--config`. É o que o `kind` faz.
fn parse_join_command(s: &str) -> Result<JoinInfo> {
    let toks: Vec<&str> = s.split_whitespace().collect();
    let flag = |name: &str| -> Option<String> {
        toks.iter().position(|t| *t == name).and_then(|i| toks.get(i + 1)).map(|v| v.to_string())
    };
    // O endpoint é o 1.º token depois de "join" que não seja uma flag.
    let endpoint = toks
        .iter()
        .position(|t| *t == "join")
        .and_then(|i| toks.get(i + 1))
        .filter(|t| !t.starts_with('-'))
        .map(|t| t.to_string())
        .ok_or_else(|| Error::Invalid(format!("não consegui ler o endpoint do join: {s:?}")))?;
    let token = flag("--token").ok_or_else(|| Error::Invalid(format!("join sem --token: {s:?}")))?;
    let ca_hash = flag("--discovery-token-ca-cert-hash")
        .ok_or_else(|| Error::Invalid(format!("join sem --discovery-token-ca-cert-hash: {s:?}")))?;
    Ok(JoinInfo { endpoint, token, ca_hash })
}

/// A `JoinConfiguration` de um worker.
///
/// **Só JoinConfiguration, sem KubeletConfiguration**: o `kubeadm join` puxa o
/// config do kubelet do ConfigMap `kubelet-config` do cluster, que o `init` já
/// escreveu com o `KubeletInUserNamespace` e o `failSwapOn: false`. Os workers
/// herdam a receita rootless sem a repetirem — e repeti-la aqui só criaria duas
/// fontes de verdade a divergir.
fn join_config_yaml(j: &JoinInfo) -> String {
    format!(
        "apiVersion: kubeadm.k8s.io/v1beta4\n\
         kind: JoinConfiguration\n\
         discovery:\n  bootstrapToken:\n    apiServerEndpoint: \"{ep}\"\n    token: \"{tok}\"\n    caCertHashes:\n    - \"{hash}\"\n\
         nodeRegistration:\n  criSocket: unix:///run/containerd/containerd.sock\n",
        ep = j.endpoint,
        tok = j.token,
        hash = j.ca_hash,
    )
}

/// Progresso ao estilo do `kind`, com **spinner animado**: cada passo mostra um
/// braille a girar (` ⠋ A arrancar o control-plane 🕹️ `) numa thread de fundo, e
/// a linha é reescrita com ` ✓ …` (ou ` ✗ …`) quando fecha.
///
/// # Porquê uma thread
///
/// O trabalho do passo (`node_exec_capture`) bloqueia o thread principal, às
/// vezes por minutos (`kubeadm init` puxa imagens). Sem uma thread a animar, a
/// linha ficava congelada e parecia pendurada. A thread só toca no stderr (o
/// output do passo vai para um ficheiro capturado, ver `node_exec_capture`), por
/// isso não há duas escritas a competir pela mesma linha.
///
/// **Sem TTY (pipe, CI, `2>&1 | tee`)** não há spinner nem `\r`: imprime-se só a
/// linha final, uma por passo — o que um log de CI quer.
struct Progress {
    tty: bool,
    msg: String,
    icon: String,
    spin: Option<SpinnerHandle>,
}

struct SpinnerHandle {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// Frames do spinner (braille, como o `kind`/`spinnies`).
const SPIN_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

impl Progress {
    fn new() -> Self {
        // SAFETY: isatty não tem pré-condições; 2 = stderr.
        let tty = unsafe { libc::isatty(2) } == 1;
        Self { tty, msg: String::new(), icon: String::new(), spin: None }
    }

    /// Abre um passo e arranca o spinner (em TTY). `icon` é o emoji do fim.
    fn step(&mut self, msg: &str, icon: &str) {
        self.close_line('✗'); // fecha um passo anterior deixado em aberto
        self.msg = msg.to_string();
        self.icon = icon.to_string();
        if !self.tty {
            return;
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s2, msg, icon) = (stop.clone(), self.msg.clone(), self.icon.clone());
        let handle = std::thread::spawn(move || {
            use std::io::Write;
            let mut i = 0usize;
            while !s2.load(std::sync::atomic::Ordering::Relaxed) {
                // `\x1b[K` limpa até ao fim da linha (evita restos de um frame
                // mais longo). Sem `\n` — a linha é reescrita in-place.
                eprint!("\r {} {msg} {icon}\x1b[K", SPIN_FRAMES[i % SPIN_FRAMES.len()]);
                let _ = std::io::stderr().flush();
                i += 1;
                std::thread::sleep(std::time::Duration::from_millis(90));
            }
        });
        self.spin = Some(SpinnerHandle { stop, handle: Some(handle) });
    }

    /// Fecha o passo actual com `✓`.
    fn ok(&mut self) {
        self.close_line('✓');
    }

    /// Pára o spinner (se houver) e escreve a linha final com `mark`. Idempotente
    /// — chamado pelo `ok`, pelo próximo `step` e pelo `Drop`.
    fn close_line(&mut self, mark: char) {
        let had_spinner = self.spin.is_some();
        if let Some(mut s) = self.spin.take() {
            s.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(h) = s.handle.take() {
                let _ = h.join();
            }
        } else if self.msg.is_empty() {
            return; // nada aberto
        }
        if self.tty {
            // `\r` + limpar a linha do spinner, depois a linha final.
            eprintln!("\r {mark} {} {}\x1b[K", self.msg, self.icon);
        } else if !self.msg.is_empty() {
            eprintln!(" {mark} {} {}", self.msg, self.icon);
        }
        let _ = had_spinner;
        self.msg.clear();
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        // Um passo deixado em aberto (erro a meio) fecha com ✗ em vez de ficar
        // com o spinner pendurado.
        self.close_line('✗');
    }
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

/// Nome do worker `i` (1-based), na convenção do `kind`: o primeiro é
/// `<cluster>-worker`, os seguintes `<cluster>-worker2`, `-worker3`, …
fn worker_name(cluster: &str, i: u32) -> String {
    if i == 1 {
        format!("{cluster}-worker")
    } else {
        format!("{cluster}-worker{i}")
    }
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
    // Sem `eprintln` aqui: o progresso é do chamador (ver `Progress`), e este é
    // chamado em PARALELO pelos workers — cada um a escrever a sua linha daria
    // output entrelaçado.
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
            // O progresso e' do `Progress`; os IDs dos nos no meio eram ruido.
            quiet: true,
            ..Default::default()
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

    if cfg.control_planes == 0 {
        return Err(Error::Invalid("--control-planes tem de ser >= 1".into()));
    }
    if cfg.control_planes > 1 {
        // Recusa-se em vez de fingir: com N control-planes e o
        // `controlPlaneEndpoint` a apontar para o IP do PRIMEIRO, todos os
        // kubelets e os outros CPs falam com esse nó — se ele morre, morre o
        // cluster. Isso não é HA, é um single point of failure com 3 nós e a
        // aparência de HA, que é pior que não ter. HA a sério precisa de um
        // load-balancer à frente (o `kind` corre um haproxy), e isso ainda não
        // está feito aqui. O `cluster kubeadm` recusa pela MESMA razão.
        let n = cfg.control_planes;
        return Err(Error::Invalid(if super::output::is_pt() {
            format!(
                "--control-planes {n} ainda não é suportado: o kubeadm em HA precisa de um endpoint \
                 estável (load-balancer/VIP) à frente dos control-planes, e o delonix ainda não o \
                 provisiona. Usa `--control-planes 1` (e `--workers N` para capacidade)."
            )
        } else {
            format!(
                "--control-planes {n} is not supported yet: kubeadm HA needs a stable endpoint \
                 (load-balancer/VIP) in front of the control-planes, which delonix does not provision \
                 yet. Use `--control-planes 1` (and `--workers N` for capacity)."
            )
        }));
    }

    super::output::info(&format!("{} \"{}\"", super::output::tr("Creating cluster", "A criar o cluster"), cfg.name));
    let mut p = Progress::new();

    // Cada nó arranca por um re-exec (processo próprio) e, em rootless sem
    // delegação de cgroup, cada um imprimia o mesmo bloco de aviso de 7 linhas —
    // 4× num cluster de 4 nós, no meio do progresso. Avisa-se UMA vez aqui (com o
    // mesmo teste que o motor faz, `cgroup_limits_apply`) e calam-se TODOS os
    // nós via env — herdada por toda a cadeia de re-exec.
    if !delonix_runtime::cgroup_limits_apply() {
        super::output::aviso(
            super::output::tr(
                "rootless without cgroup delegation: the nodes' CPU/memory/PIDs limits are not enforced \
                 (namespace/seccomp isolation still holds). For limits, run under \
                 `systemd-run --user --scope -p Delegate=yes`.",
                "rootless sem delegação de cgroup: os limites de CPU/memória/PIDs dos nós não são aplicados \
                 (o isolamento de namespaces/seccomp mantém-se). Para limites, corre sob \
                 `systemd-run --user --scope -p Delegate=yes`.",
            ),
        );
    }
    // SAFETY: single-threaded aqui (antes de qualquer thread de worker); a env
    // var é lida pelos processos-filhos do re-exec, não por esta thread.
    unsafe {
        std::env::set_var("DELONIX_NO_CGROUP_WARN", "1");
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
    }

    // A imagem do nó é garantida UMA vez, aqui, antes de qualquer paralelismo.
    // Se falta, puxa-se; se já está no store, reaproveita-se (o `resolve` aceita
    // a referência fixada por digest). **Isto não é cosmético**: os workers
    // arrancam em paralelo e, sem este passo, N threads chamariam
    // `resolve_or_pull` ao mesmo tempo e puxavam a MESMA imagem N vezes.
    let curta = cfg.image.split('@').next().unwrap_or(&cfg.image).to_string();
    p.step(&format!("{} ({curta})", super::output::tr("Ensuring node image", "A garantir a imagem do nó")), "🖼");
    super::util::resolve_or_pull(images, &cfg.image)?;
    p.ok();

    // Resolve a porta ANTES de arrancar o nó: um 2.º cluster não deve rebentar
    // só porque a 6443 está tomada.
    let api_port = pick_api_port(store, cfg.api_port, &cfg.name)?;
    p.step(&format!("{} ({})", super::output::tr("Preparing nodes", "A preparar os nós"), 1 + cfg.workers), "📦");
    let c = boot_node(images, store, cfg, &node, "control-plane", vec![format!("{api_port}:6443")])?;
    p.ok();
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
    p.step(super::output::tr("Writing configuration", "A escrever a configuração"), "📜");
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
    p.ok();

    // O `kubeadm init` puxa as imagens do control-plane cá dentro — é o passo
    // mais demorado de todos.
    p.step(super::output::tr("Starting control-plane", "A arrancar o control-plane"), "🕹️");
    node_must(
        &c,
        "kubeadm init",
        &format!(
            "kubeadm init --config {NODE_SHARED}/kubeadm.conf \
             --ignore-preflight-errors=Swap,SystemVerification,FileContent--proc-sys-net-bridge-bridge-nf-call-iptables,Mem,NumCPU \
             2>&1 | tail -3"
        ),
    )?;

    p.ok();

    // --- CNI (senão o nó fica NotReady para sempre) ---
    if cfg.cni == "default" {
        p.step(super::output::tr("Installing CNI (kindnet)", "A instalar a CNI (kindnet)"), "🔌");
        node_must(
            &c,
            "CNI",
            &format!(
                "sed 's|{{{{ .PodSubnet }}}}|{pods}|g; s|{{{{.PodSubnet}}}}|{pods}|g' /kind/manifests/default-cni.yaml \
                 | KUBECONFIG=/etc/kubernetes/admin.conf kubectl apply -f - >/dev/null 2>&1",
                pods = cfg.pod_subnet
            ),
        )?;
        p.ok();
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

    p.step(super::output::tr("Waiting for control-plane to be Ready", "À espera do control-plane ficar Ready"), "⏳");
    wait_in_node(
        &c,
        "o control-plane ficar Ready",
        "KUBECONFIG=/etc/kubernetes/admin.conf kubectl get nodes --no-headers 2>/dev/null | grep -qw Ready",
        Duration::from_secs(180),
    )?;

    p.ok();

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
        // Desmonta a linha em (endpoint, token, hash) e escreve uma
        // `JoinConfiguration` — ver `parse_join_command` para o porquê: correr a
        // linha E passar `--config` é o que o kubeadm recusa com
        // "can not mix '--config' with arguments [...]", e era isso que fazia
        // TODOS os workers falharem o join em silêncio (o `cluster create`
        // seguia e só rebentava no fim, com um "timeout à espera de workers
        // Ready" que não dizia nada sobre a causa).
        let join = parse_join_command(&join_cmd)?;
        let join_yaml = join_config_yaml(&join);

        p.step(&format!("{} {} worker(s)", super::output::tr("Joining", "A juntar"), cfg.workers), "🚜");
        // Em PARALELO: cada worker é independente (arranca, junta-se, acabou) e
        // em série o tempo somava-se. Cada thread escreve o SEU
        // `join-<nó>.conf` — um ficheiro partilhado seria uma corrida.
        let erros: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (1..=cfg.workers)
                .map(|i| {
                    let wnode = worker_name(&cfg.name, i);
                    let join_yaml = &join_yaml;
                    scope.spawn(move || -> Result<()> {
                        let conf = format!("join-{wnode}.conf");
                        std::fs::write(cluster_dir(&cfg.name).join(&conf), join_yaml)?;
                        let w = boot_node(images, store, cfg, &wnode, "worker", Vec::new())?;
                        node_must(
                            &w,
                            &format!("join do worker '{wnode}'"),
                            &format!(
                                "kubeadm join --config {NODE_SHARED}/{conf} \
                                 --ignore-preflight-errors=Swap,SystemVerification,FileContent--proc-sys-net-bridge-bridge-nf-call-iptables,Mem,NumCPU"
                            ),
                        )
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|h| match h.join() {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(e.to_string()),
                    // Um panic numa thread não pode passar por "worker ok".
                    Err(_) => Some("uma thread de worker entrou em panic".to_string()),
                })
                .collect()
        });
        if !erros.is_empty() {
            return Err(Error::Invalid(format!("{} worker(s) falharam:\n  {}", erros.len(), erros.join("\n  "))));
        }

        // Só agora se espera: os joins já devolveram OK, isto é o kubelet de
        // cada um a registar-se. O timeout escala com o nº de workers — 3 nós a
        // puxar imagens ao mesmo tempo demoram mais que 1.
        let espera = Duration::from_secs(180 + 60 * u64::from(cfg.workers));
        wait_in_node(
            &c,
            &format!("os {} worker(s) ficarem Ready", cfg.workers),
            &format!(
                "[ \"$(KUBECONFIG=/etc/kubernetes/admin.conf kubectl get nodes --no-headers 2>/dev/null | grep -cw Ready)\" = \"{}\" ]",
                cfg.workers + 1
            ),
            espera,
        )?;
        p.ok();
    }

    write_kubeconfig(&c, &cfg.name, api_port)?;

    // Instala o contexto no kubeconfig do utilizador — sem isto o cluster só
    // existe para quem passar `--kubeconfig <ficheiro>` à mão, e não aparece no
    // `kubectl config get-contexts`. É o que o `kind` faz no fim.
    let ctx = context_name(&cfg.name);
    match install_kubecontext(&cfg.name) {
        Ok(path) => {
            p.step(&format!("{} \"{ctx}\"", super::output::tr("Setting kubectl context to", "A definir o contexto kubectl para")), "📇");
            p.ok();
            let _ = path;
        }
        // Não é razão para falhar o cluster: ele ESTÁ de pé e o kubeconfig
        // próprio funciona. Diz-se o que correu mal e como usar à mesma.
        Err(e) => {
            super::output::aviso(&if super::output::is_pt() {
                format!("não consegui instalar o contexto no ~/.kube/config: {e}")
            } else {
                format!("could not install the context into ~/.kube/config: {e}")
            });
            eprintln!(
                "   {}",
                super::output::secundario(&if super::output::is_pt() {
                    format!("usa: kubectl --kubeconfig {} get nodes", kubeconfig_path(&cfg.name).display())
                } else {
                    format!("use: kubectl --kubeconfig {} get nodes", kubeconfig_path(&cfg.name).display())
                })
            );
        }
    }
    drop(p);

    println!();
    println!("{}", super::output::tr("You can now use your cluster:", "Já podes usar o teu cluster:"));
    println!();
    println!("  {}", super::output::destaque(&format!("kubectl cluster-info --context {ctx}")));
    println!();
    Ok(())
}

/// O nome do contexto/cluster/utilizador no kubeconfig. O `kind` usa
/// `kind-<nome>`; o prefixo diz de quem é o cluster e evita colidir com um
/// contexto real chamado igual.
fn context_name(cluster: &str) -> String {
    format!("delonix-{cluster}")
}

/// Caminho do kubeconfig do utilizador (`$KUBECONFIG` só com UM ficheiro, senão
/// o default).
///
/// Com `$KUBECONFIG` a listar VÁRIOS ficheiros (`a:b:c`), o kubectl funde-os e
/// escreve no primeiro — replicar essa precedência aqui era fácil de errar e
/// silenciosamente destrutivo. Nesse caso não se adivinha: usa-se o default.
fn user_kubeconfig_path() -> Option<std::path::PathBuf> {
    if let Some(kc) = std::env::var_os("KUBECONFIG") {
        let s = kc.to_string_lossy().to_string();
        if s.is_empty() {
            return home_kubeconfig();
        }
        if s.contains(':') {
            return home_kubeconfig();
        }
        return Some(std::path::PathBuf::from(s));
    }
    home_kubeconfig()
}

fn home_kubeconfig() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".kube").join("config"))
}

/// Funde o kubeconfig do cluster no do utilizador, com as entradas renomeadas
/// para `delonix-<cluster>`, e passa a ser o contexto actual.
///
/// **Não é destrutivo**: lê o que lá está, substitui só as entradas com o NOSSO
/// nome (um `cluster create` repetido actualiza em vez de duplicar) e mantém
/// tudo o resto. Escreve em `.tmp` + `rename` — um crash a meio não deixa o
/// utilizador sem `~/.kube/config`, que seria um estrago sério.
fn install_kubecontext(cluster: &str) -> Result<std::path::PathBuf> {
    use serde_yaml::Value;
    let name = context_name(cluster);
    let src = kubeconfig_path(cluster);
    let raw = std::fs::read_to_string(&src).map_err(|e| Error::Invalid(format!("a ler {}: {e}", src.display())))?;
    let novo: Value = serde_yaml::from_str(&raw).map_err(|e| Error::Invalid(format!("kubeconfig do cluster inválido: {e}")))?;

    let dest = user_kubeconfig_path().ok_or_else(|| Error::Invalid("sem $HOME nem $KUBECONFIG".into()))?;
    let mut cfg: Value = match std::fs::read_to_string(&dest) {
        Ok(t) if !t.trim().is_empty() => {
            serde_yaml::from_str(&t).map_err(|e| Error::Invalid(format!("o {} existente não é YAML válido: {e}", dest.display())))?
        }
        // Não existe (ou está vazio): começa-se um kubeconfig do zero.
        _ => serde_yaml::from_str("apiVersion: v1\nkind: Config\nclusters: []\nusers: []\ncontexts: []\n").unwrap(),
    };

    // Tira do kubeconfig do cluster o 1.º de cada lista e renomeia-o.
    let pega = |v: &Value, chave: &str| -> Option<Value> { v.get(chave)?.as_sequence()?.first().cloned() };
    let mut cl = pega(&novo, "clusters").ok_or_else(|| Error::Invalid("kubeconfig sem clusters".into()))?;
    let mut us = pega(&novo, "users").ok_or_else(|| Error::Invalid("kubeconfig sem users".into()))?;
    if let Some(m) = cl.as_mapping_mut() {
        m.insert("name".into(), name.clone().into());
    }
    if let Some(m) = us.as_mapping_mut() {
        m.insert("name".into(), name.clone().into());
    }
    let ctx: Value = serde_yaml::from_str(&format!(
        "name: {name}\ncontext:\n  cluster: {name}\n  user: {name}\n"
    ))
    .map_err(|e| Error::Invalid(e.to_string()))?;

    // Substitui-se a entrada com o nosso nome, se já lá estiver; senão junta-se.
    let upsert = |cfg: &mut Value, chave: &str, item: Value| {
        let seq = cfg
            .as_mapping_mut()
            .unwrap()
            .entry(chave.into())
            .or_insert_with(|| Value::Sequence(vec![]));
        if !seq.is_sequence() {
            *seq = Value::Sequence(vec![]);
        }
        let s = seq.as_sequence_mut().unwrap();
        let nome = item.get("name").cloned();
        if let Some(pos) = s.iter().position(|e| e.get("name") == nome.as_ref()) {
            s[pos] = item;
        } else {
            s.push(item);
        }
    };
    upsert(&mut cfg, "clusters", cl);
    upsert(&mut cfg, "users", us);
    upsert(&mut cfg, "contexts", ctx);
    if let Some(m) = cfg.as_mapping_mut() {
        m.insert("current-context".into(), name.clone().into());
        m.entry("apiVersion".into()).or_insert_with(|| "v1".into());
        m.entry("kind".into()).or_insert_with(|| "Config".into());
    }

    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = serde_yaml::to_string(&cfg).map_err(|e| Error::Invalid(e.to_string()))?;
    let tmp = dest.with_extension("delonix.tmp");
    std::fs::write(&tmp, out)?;
    // 0600: o kubeconfig traz credenciais de admin do cluster.
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
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
    eprintln!("kubeconfig: {}", path.display());  // rótulo universal
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
        eprintln!("{} '{}'...", super::output::tr("removing node", "a remover o nó"), n.name);
        container::remove_container(images, store, n, true)?;
    }
    // A rede do cluster (`dlx-<nome>`) foi criada PARA este cluster — some com ele
    // (ao contrário de uma rede de utilizador, que um `container rm` nunca apaga).
    // Assim a sub-rede/bridge ficam livres para reutilizar. Volumes NÃO se tocam:
    // são explícitos, como no docker.
    let net = cluster_net(name);
    if let Ok(nstore) = delonix_net::NetworkStore::open(super::util::state_root()) {
        if nstore.get(&net).is_ok() {
            let _ = nstore.remove(&net);
            delonix_net::infra::network_remove(&net);
        }
    }
    let _ = std::fs::remove_file(kubeconfig_path(name));
    let _ = std::fs::remove_dir_all(cluster_dir(name));
    // Tira o contexto do ~/.kube/config — senão o `kubectl config get-contexts`
    // ficava a listar um cluster que já não existe, e um `kubectl` distraído
    // apontava para uma porta que entretanto pode ser de OUTRA coisa.
    if let Err(e) = remove_kubecontext(name) {
        super::output::aviso(&if super::output::is_pt() {
            format!("não consegui tirar o contexto '{}' do kubeconfig: {e}", context_name(name))
        } else {
            format!("could not remove context '{}' from kubeconfig: {e}", context_name(name))
        });
    }
    println!("{}", if super::output::is_pt() { format!("cluster '{name}' removido ({} nó(s))", nodes.len()) } else { format!("cluster '{name}' removed ({} node(s))", nodes.len()) });
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

/// Tira do kubeconfig do utilizador as entradas deste cluster. Best-effort e
/// idempotente: um cluster que nunca chegou a instalar contexto não é erro.
fn remove_kubecontext(cluster: &str) -> Result<()> {
    use serde_yaml::Value;
    let name = context_name(cluster);
    let Some(dest) = user_kubeconfig_path() else { return Ok(()) };
    let Ok(txt) = std::fs::read_to_string(&dest) else { return Ok(()) };
    if txt.trim().is_empty() {
        return Ok(());
    }
    let mut cfg: Value = serde_yaml::from_str(&txt).map_err(|e| Error::Invalid(format!("{} não é YAML válido: {e}", dest.display())))?;
    let mut mexeu = false;
    for chave in ["clusters", "users", "contexts"] {
        if let Some(seq) = cfg.get_mut(chave).and_then(|v| v.as_sequence_mut()) {
            let antes = seq.len();
            seq.retain(|e| e.get("name").and_then(|n| n.as_str()) != Some(name.as_str()));
            mexeu |= seq.len() != antes;
        }
    }
    // Se o contexto actual era o nosso, deixá-lo apontar para um contexto que já
    // não existe faria o kubectl falhar em TUDO — tira-se.
    if cfg.get("current-context").and_then(|v| v.as_str()) == Some(name.as_str()) {
        if let Some(m) = cfg.as_mapping_mut() {
            m.remove(Value::from("current-context"));
        }
        mexeu = true;
    }
    if !mexeu {
        return Ok(());
    }
    let out = serde_yaml::to_string(&cfg).map_err(|e| Error::Invalid(e.to_string()))?;
    let tmp = dest.with_extension("delonix.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, &dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nomes_de_worker_seguem_a_convencao_do_kind() {
        // O primeiro NÃO leva número — é `-worker`, não `-worker1`.
        assert_eq!(worker_name("c", 1), "c-worker");
        assert_eq!(worker_name("c", 2), "c-worker2");
        assert_eq!(worker_name("c", 3), "c-worker3");
    }

    #[test]
    fn parse_do_join_extrai_endpoint_token_e_hash() {
        let linha = "kubeadm join 10.217.227.44:6443 --token d9rxyc.0nlg8oq9xc53v4r1 \
                     --discovery-token-ca-cert-hash sha256:4ff6978e882b82bba8ec70ca603c13db";
        let j = parse_join_command(linha).unwrap();
        assert_eq!(j.endpoint, "10.217.227.44:6443");
        assert_eq!(j.token, "d9rxyc.0nlg8oq9xc53v4r1");
        assert_eq!(j.ca_hash, "sha256:4ff6978e882b82bba8ec70ca603c13db");
    }

    #[test]
    fn parse_do_join_tolera_espacos_e_quebras() {
        // O `--print-join-command` real vem com `\` e newline pelo meio.
        let linha = "kubeadm join 10.0.0.2:6443 --token ab.cd \\\n\t--discovery-token-ca-cert-hash sha256:ef \n";
        let j = parse_join_command(linha).unwrap();
        assert_eq!(j.endpoint, "10.0.0.2:6443");
        assert_eq!(j.ca_hash, "sha256:ef");
    }

    #[test]
    fn parse_do_join_recusa_linha_incompleta() {
        assert!(parse_join_command("kubeadm join 1.2.3.4:6443 --token ab.cd").is_err());
        assert!(parse_join_command("kubeadm join --token ab.cd --discovery-token-ca-cert-hash x").is_err());
        assert!(parse_join_command("").is_err());
    }

    #[test]
    fn join_config_nao_leva_kubelet_config() {
        // O kubelet config vem do ConfigMap do cluster (escrito pelo `init`);
        // repeti-lo aqui criava duas fontes de verdade a divergir.
        let j = JoinInfo {
            endpoint: "10.0.0.2:6443".into(),
            token: "ab.cd".into(),
            ca_hash: "sha256:ef".into(),
        };
        let y = join_config_yaml(&j);
        assert!(y.contains("kind: JoinConfiguration"));
        assert!(y.contains("apiServerEndpoint: \"10.0.0.2:6443\""));
        assert!(y.contains("- \"sha256:ef\""));
        assert!(!y.contains("KubeletConfiguration"), "o join não deve trazer KubeletConfiguration");
    }

    #[test]
    fn contexto_tem_prefixo_do_produto() {
        assert_eq!(context_name("njinga-huila-65"), "delonix-njinga-huila-65");
    }
}
