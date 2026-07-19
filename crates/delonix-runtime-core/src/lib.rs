//! `delonix-runtime-core` — tipos partilhados, estado e erros do **motor**
//! (Container/Vm/Status), independentes de qualquer noção de tenant, plano,
//! licença ou consola. É a base do Delonix Runtime — pensado para viver num
//! repositório opensource próprio, sem nenhuma dependência do lado PaaS
//! (`delonix-core`, que trata tenants/licenciamento/billing, DEPENDE deste
//! crate e reexporta-o — nunca o inverso).

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub mod cred_vault;
pub mod events;
pub mod secret;
pub mod typestate;
pub mod virt;
pub mod workload_net;
mod error;
mod store;

pub use error::{Error, Result};
pub use secret::{Secret, SecretStore};
pub use store::{JsonStore, Store};

/// Formata um instante unix como data/hora LOCAL "AAAA-MM-DD HH:MM:SS".
/// Usa `localtime_r` (honra /etc/localtime|TZ); em falha, devolve o valor cru.
pub fn fmt_local_ts(unix: u64) -> String {
    let t = unix as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `t` é válido; `localtime_r` escreve em `tm` (buffer nosso, do
    // tamanho certo) e devolve NULL só em erro — tratado a seguir.
    if unsafe { libc::localtime_r(&t, &mut tm).is_null() } {
        return unix.to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec
    )
}

/// Uma montagem a injectar no container (volume nomeado ou *bind mount*).
///
/// `source` é um caminho **no host** (o `_data` de um volume, ou um caminho
/// arbitrário); `target` é o caminho **dentro** do container. É zero-copy: o
/// kernel partilha os mesmos blocos, não há cópia de dados.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Mount {
    /// Caminho de origem no host.
    pub source: String,
    /// Ponto de montagem dentro do container (começa por `/`).
    pub target: String,
    /// Se `true`, monta só-leitura.
    pub readonly: bool,
}

/// Uma regra L4 da firewall por-container (forma da UI da Console). É o tipo
/// CANÓNICO: persistido no [`Container`] e (de)serializado tanto na escrita
/// (`POST .../firewall`) como na leitura (`GET .../firewall`). O `delonix-net`
/// re-exporta-o para aplicar via nftables.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct FwRule {
    /// `in` (tráfego PARA o container) ou `out` (DO container).
    #[serde(default)]
    pub dir: String,
    /// `tcp`/`udp`/`any`.
    #[serde(default)]
    pub proto: String,
    /// porta (ou `*`/vazio = qualquer).
    #[serde(default)]
    pub port: String,
    /// CIDR do outro extremo (origem em `in`, destino em `out`); `0.0.0.0/0`/`*` = qualquer.
    #[serde(default)]
    pub src: String,
    /// `allow` (accept) ou `deny` (drop).
    #[serde(default)]
    pub action: String,
    /// Nota livre da UI (cosmética; preservada no round-trip de persistência).
    #[serde(default)]
    pub note: String,
}

/// Configuração de firewall L4 de um container, aplicada via nftables e
/// persistida no [`Container`] para que a Console possa LER as regras reais.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ContainerFw {
    #[serde(default)]
    pub enabled: bool,
    /// política por omissão de entrada: `allow` ou `deny`.
    #[serde(default, rename = "policyIn")]
    pub policy_in: String,
    #[serde(default, rename = "policyOut")]
    pub policy_out: String,
    #[serde(default)]
    pub rules: Vec<FwRule>,
    /// Namespace lógico do container (default `default`). Quando o container NÃO
    /// tem política de entrada explícita (sem `rules` de entrada e `policy_in` !=
    /// `deny`), a entrada aplica o **isolamento de namespace**: aceita a mesma
    /// namespace (`@dlxns_<ns>`) e dropa NOVAS ligações de containers de outra
    /// namespace (`@dlxall` + `ct state new`). Uma política explícita (Dependency/
    /// Ingress) é autoritativa e substitui isto (ver `fw_chain_body`).
    #[serde(default = "default_namespace")]
    pub namespace: String,
}

/// Namespace por omissão (`default`) — tudo em `default` = SDN aberta (a mesma
/// namespace contém todos), preservando o comportamento anterior a namespaces.
pub fn default_namespace() -> String {
    "default".to_string()
}

impl Default for ContainerFw {
    fn default() -> Self {
        // `namespace` NUNCA vazio (o derive daria ""); tudo o resto é o zero-value.
        ContainerFw {
            enabled: false,
            policy_in: String::new(),
            policy_out: String::new(),
            rules: Vec::new(),
            namespace: default_namespace(),
        }
    }
}

/// `proto` aceite numa regra de firewall (interpolado em nft): vazio, any, tcp, udp.
pub fn fw_proto_ok(p: &str) -> bool {
    matches!(p, "" | "any" | "tcp" | "udp")
}

/// `port` seguro: vazio, `*`, número 1..=65535, ou intervalo `n-m`.
pub fn fw_port_ok(p: &str) -> bool {
    if p.is_empty() || p == "*" {
        return true;
    }
    let num_ok = |s: &str| s.parse::<u32>().map(|n| (1..=65535).contains(&n)).unwrap_or(false);
    match p.split_once('-') {
        Some((a, b)) => num_ok(a) && num_ok(b),
        None => num_ok(p),
    }
}

/// `src` seguro: vazio, `*`, `0.0.0.0/0`, ou um IP/CIDR (v4/v6) — só carateres de
/// IP/CIDR (sem espaços/`;`/`{`/`}`/newline, que injetariam sintaxe nft).
pub fn fw_src_ok(s: &str) -> bool {
    if s.is_empty() || s == "*" || s == "0.0.0.0/0" {
        return true;
    }
    if s.len() > 64 || !s.bytes().all(|b| b.is_ascii_hexdigit() || matches!(b, b'.' | b':' | b'/')) {
        return false;
    }
    let (addr, mask) = s.split_once('/').map(|(a, m)| (a, Some(m))).unwrap_or((s, None));
    if let Some(m) = mask {
        match m.parse::<u32>() {
            Ok(n) if n <= 128 => {}
            _ => return false,
        }
    }
    addr.parse::<std::net::IpAddr>().is_ok()
}

impl FwRule {
    /// Os campos interpolados no script `nft` (`src`/`proto`/`port`) são SEGUROS?
    /// Defesa contra injeção de nftables: os builders DEVEM saltar regras não-seguras.
    pub fn nft_safe(&self) -> bool {
        fw_proto_ok(&self.proto) && fw_port_ok(&self.port) && fw_src_ok(&self.src)
    }
}

/// O estado de um container/VM no seu ciclo de vida (6 estados). `Deserialize` é
/// manual (mais abaixo) para aceitar o formato legado `{"Exited": code}`.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Criado, ainda não iniciado (em transição para Running).
    Created,
    /// Em execução (tem um `pid` de init vivo).
    Running,
    /// Suspenso (freezer do cgroup / `virsh suspend`) — processos congelados.
    Paused,
    /// Parado de forma limpa (stop intencional, ou saída com código 0).
    Stopped,
    /// Terminou com código de saída ≠ 0.
    Failed(i32),
    /// Morte inesperada (morto por sinal/OOM, ou desaparecimento sem stop limpo).
    Crashed,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Created => write!(f, "created"),
            Status::Running => write!(f, "running"),
            Status::Paused => write!(f, "paused"),
            Status::Stopped => write!(f, "stopped"),
            Status::Failed(code) => write!(f, "failed ({code})"),
            Status::Crashed => write!(f, "crashed"),
        }
    }
}

impl Status {
    /// Estado terminal a partir do resultado de um `wait()` de processo:
    /// código 0 → Stopped, código ≠ 0 → Failed, morto por sinal → Crashed.
    pub fn from_wait(code: i32, signaled: bool) -> Status {
        if signaled {
            Status::Crashed
        } else if code == 0 {
            Status::Stopped
        } else {
            Status::Failed(code)
        }
    }

    /// `true` se o container/VM é listado SEM `-a`. Só `Failed`/`Crashed` exigem
    /// `-a` (ficam escondidos por omissão); Running/Created/Paused/Stopped mostram-se.
    pub fn shown_by_default(&self) -> bool {
        !matches!(self, Status::Failed(_) | Status::Crashed)
    }

    /// `true` se já terminou (não está ativo nem suspenso).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Status::Stopped | Status::Failed(_) | Status::Crashed)
    }

    /// Código de saída associado (Stopped=0, Failed=n, Crashed=137), p/ propagação.
    pub fn exit_code(&self) -> i32 {
        match self {
            Status::Failed(n) => *n,
            Status::Crashed => 137,
            _ => 0,
        }
    }
}

// Deserialize manual: aceita o formato novo E o legado `{"Exited": code}` dos
// registos antigos (mapeia para Stopped/Failed), para não perder containers/VMs.
impl<'de> Deserialize<'de> for Status {
    fn deserialize<D>(d: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        enum Repr {
            Created,
            Running,
            Paused,
            Stopped,
            Failed(i32),
            Crashed,
            Exited(i32), // legado
        }
        Ok(match Repr::deserialize(d)? {
            Repr::Created => Status::Created,
            Repr::Running => Status::Running,
            Repr::Paused => Status::Paused,
            Repr::Stopped => Status::Stopped,
            Repr::Failed(n) => Status::Failed(n),
            Repr::Crashed => Status::Crashed,
            Repr::Exited(0) => Status::Stopped,
            Repr::Exited(n) => Status::Failed(n),
        })
    }
}

/// Uma ligação de rede ADICIONAL de um container (multi-homing, `network
/// connect`): a rede, o IP atribuído e o índice da interface (`eth<idx>`, >=1).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ExtraNet {
    pub network: String,
    pub ip: String,
    pub idx: u32,
}

/// Um container: a unidade que o Delonix cria, corre, inspecciona e destrói.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Container {
    /// Identificador hexadecimal de 16 caracteres.
    pub id: String,
    /// Nome legível (também o hostname dentro do container).
    pub name: String,
    /// Imagem/rootfs de origem.
    pub image: String,
    /// O comando de init e os seus argumentos.
    pub command: Vec<String>,
    /// O PID (no host) do processo de init, enquanto vivo.
    pub pid: Option<i32>,
    /// `starttime` do init (jiffies desde o boot, campo 22 de `/proc/<pid>/stat`).
    /// Guarda contra reutilização de PID: antes de enviar sinais, confirmamos que
    /// o PID ainda tem este `starttime` — senão o kernel reciclou-o e mataríamos
    /// um processo alheio do host.
    #[serde(default)]
    pub pid_starttime: Option<u64>,
    /// O estado actual.
    pub status: Status,
    /// Instante de criação (segundos Unix).
    pub created_unix: u64,
    /// Limite de memória do cgroup (ex.: `64M`).
    pub memory_max: String,
    /// Limite de CPU em cores (ex.: `0.5`, `2`) — OBRIGATÓRIO (Fase 7+segurança).
    #[serde(default = "default_cpus")]
    pub cpus: String,
    /// Peso/prioridade de CPU (cgroup `cpu.weight`, 1–10000) — escalonamento.
    #[serde(default)]
    pub cpu_weight: Option<String>,
    /// Afinidade de cores (cgroup `cpuset.cpus`, ex.: `0-1`) — *pinning*.
    #[serde(default)]
    pub cpuset: Option<String>,
    /// Peso de I/O de disco (cgroup `io.weight`, 1–10000).
    #[serde(default)]
    pub io_weight: Option<String>,
    /// Pod a que o container pertence (partilha o network namespace).
    #[serde(default)]
    pub pod: Option<String>,
    /// Portas publicadas (`hostPort:contPort[/proto]`) — DNAT no host.
    #[serde(default)]
    pub ports: Vec<String>,
    /// Variáveis de ambiente (`KEY=value`) — imagem `ENV` + `-e`/stack `env`.
    #[serde(default)]
    pub env: Vec<String>,
    /// Segredos referenciados (`--secret <nome>`): resolvidos para env no arranque
    /// a partir do [`crate::SecretStore`]. Guardam-se os NOMES (não os valores), para
    /// re-resolver fresco a cada start (apanha atualizações do segredo). [[Secret Manager]]
    #[serde(default)]
    pub secrets: Vec<String>,
    /// `true` → injeta os segredos como **ficheiros** num tmpfs RO em `/run/secrets`
    /// **dentro do namespace do container** (`--secret-files`), em vez de variáveis de
    /// ambiente. Mais seguro: os valores ficam só em RAM (tmpfs in-ns) — nunca em
    /// `environ`/`inspect`, nem no fs do host ou do container. [[Secret Manager]]
    #[serde(default)]
    pub secret_files: bool,
    /// Diretório de trabalho do processo (Docker/OCI `WorkingDir` da imagem, ou `-w`).
    /// O runtime faz `chdir` para aqui antes do `exec`. Vazio/None = `/`. Sem isto,
    /// entrypoints que operam no CWD (redis/postgres `chown -R`) correm a partir de `/`.
    #[serde(default)]
    pub workdir: Option<String>,
    /// `true` → rootfs montado só-leitura (`--read-only`).
    #[serde(default)]
    pub read_only: bool,
    /// `true` → container **privilegiado** (`--privileged`): mantém todas as caps,
    /// seccomp unconfined, cgroup namespace (`CLONE_NEWCGROUP`) e `/sys/fs/cgroup`
    /// montado RW delegado. Necessário p/ correr systemd+containerd (nodes Kind).
    /// ⚠️ Relaxa o isolamento — só para cargas de confiança. Default `false`
    /// (containers normais ficam exatamente como antes).
    #[serde(default)]
    pub privileged: bool,
    /// Labels `chave→valor` (`docker/kubectl --label`). Persistidas para
    /// `docker ps --filter label=` e `docker inspect .Config.Labels` (Kind filtra
    /// nodes por `io.x-k8s.kind.cluster`).
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
    /// Capabilities a remover (`--cap-drop`; `ALL` remove todas).
    #[serde(default)]
    pub cap_drop: Vec<String>,
    /// Capabilities a repor (`--cap-add`), sobre a base ou sobre `cap_drop ALL`.
    #[serde(default)]
    pub cap_add: Vec<String>,
    /// Perfil seccomp: `None` = allowlist (default); `Some("unconfined")` = sem filtro.
    #[serde(default)]
    pub seccomp: Option<String>,
    /// Perfil AppArmor aplicado (`aa_change_onexec`). Persistido para que o `exec`
    /// confine também os processos que entram no container depois (sondas/`crictl`).
    #[serde(default)]
    pub apparmor: Option<String>,
    /// `true` se o container tem user namespace (root do container ≠ root do host).
    #[serde(default)]
    pub userns: bool,
    /// O IP atribuído na bridge `delonix0`, se tiver rede (Fase 3).
    #[serde(default)]
    pub ip: Option<String>,
    /// Nome da rede a que está ligado (`bridge` por omissão, ou uma rede de
    /// utilizador). `None` = sem rede.
    #[serde(default)]
    pub network: Option<String>,
    /// Namespace lógico de ISOLAMENTO (default `default`). Containers de namespaces
    /// diferentes NÃO se alcançam (mesmo na mesma rede); só um `kind: Dependency`
    /// fura a fronteira. Propaga-se ao `ContainerFw.namespace` e ao registo nos
    /// sets nft `@dlxns_<ns>`/`@dlxall` no attach. [[isolamento de namespace]]
    #[serde(default = "default_namespace")]
    pub namespace: String,
    /// Porta HTTP auto-registada no proxy L7 (`--expose`), sob o FQDN interno
    /// `<nome>.<namespace>.delonix.internal`. `None` = não exposta. Persistida para
    /// re-registar no `start` e des-registar no `rm`.
    #[serde(default)]
    pub expose: Option<u16>,
    /// Redes ADICIONAIS a que o container está ligado (multi-homing, via
    /// `network connect`). Cada uma é uma interface `eth<idx>` própria.
    #[serde(default)]
    pub extra_networks: Vec<ExtraNet>,
    /// Nomes DNS adicionais do container na sua rede (`--network-alias`), além do
    /// nome do container — resolvidos por outros containers da mesma rede.
    #[serde(default)]
    pub net_aliases: Vec<String>,
    /// Visibilidade de DNS DIRECIONADA (#2): allowlist dos peers que ESTE container
    /// resolve. `None` = vê todos (bidirecional, default). `Some([...])` = só
    /// resolve esses (ex.: app `knows=[db]` → app vê db, mas db com `knows=[]` não
    /// vê app). Permite comunicação unidirecional onde um conhece o outro mas não
    /// o contrário.
    #[serde(default)]
    pub dns_knows: Option<Vec<String>>,
    /// Sistemas de ficheiros tmpfs a montar (`--tmpfs /path[:opts]`).
    #[serde(default)]
    pub tmpfs: Vec<String>,
    /// Limites de recursos (`--ulimit nome=soft[:hard]`), aplicados antes do exec.
    #[serde(default)]
    pub ulimits: Vec<String>,
    /// `sysctl`s namespaced (`--sysctl chave=valor`), escritos em `/proc/sys`.
    #[serde(default)]
    pub sysctls: Vec<String>,
    /// Dispositivos a expor (`--device /dev/x[:/dev/y]`), ligados em `/dev`.
    #[serde(default)]
    pub devices: Vec<String>,
    /// Política de reinício (`no`|`on-failure[:max]`|`always`|`unless-stopped`).
    /// Consumida pelo supervisor de `delonix container run -d --restart` (um
    /// processo destacado por container, que fica PAI do container e por isso
    /// captura o exit code real); também usada pela unidade `systemd` gerada e
    /// pelo supervisor de stacks do lado do PaaS.
    #[serde(default)]
    pub restart_policy: Option<String>,
    /// **Estado desejado**: o utilizador pediu `stop` explicitamente. O
    /// supervisor de `--restart` NÃO ressuscita um container assim — é a
    /// semântica do docker (um `docker stop` num container `always` não o
    /// reinicia; só um `start` o traz de volta). Sem isto, `stop` e supervisor
    /// entram em guerra: o container volta sozinho e o utilizador não o
    /// consegue parar. Limpo pelo `run`/`start`.
    #[serde(default)]
    pub stopped_by_user: bool,
    /// Volumes/binds montados (persistidos para o **update zero-downtime** poder
    /// recriar o container novo com EXACTAMENTE os mesmos volumes).
    #[serde(default)]
    pub mounts: Vec<Mount>,
    /// Driver de logs (`file` por omissão, ou `journald`/`syslog`).
    #[serde(default)]
    pub log_driver: Option<String>,
    /// Limite de largura de banda da rede (`--net-bps`, ex.: `10mbit`) — `tc`
    /// TBF/police no `veth` do lado do host. `None` = sem limite (caudal livre).
    #[serde(default)]
    pub net_bps: Option<String>,
    /// Burst (bytes) do limite de banda (`--net-burst`, ex.: `256k`). `None` =
    /// ~100 ms de caudal por omissão. Só vale com [`Container::net_bps`].
    #[serde(default)]
    pub net_burst: Option<String>,
    /// Prioridade de CPU (valor `nice`, -20..19; menor = mais prioritário),
    /// aplicada por `renice` à árvore de processos. `None` = nice 0 (normal).
    /// Persistida para o arranque reaplicar. `--priority high|normal|low` mapeia
    /// para -5/0/10; `--nice N` define o valor cru.
    #[serde(default)]
    pub nice: Option<i32>,
    /// Firewall L4 ACTUALMENTE aplicada (nftables) ao container, persistida pelo
    /// `POST /api/containers/:id/firewall`. `None` = nunca foi aplicada nenhuma
    /// (a Console mostra vazio/fallback). Permite a LEITURA das regras reais via
    /// `GET /api/containers/:id/firewall`, em vez de regras hardcoded.
    #[serde(default)]
    pub firewall: Option<ContainerFw>,
}

fn default_cpus() -> String {
    "1.0".to_string()
}

impl Container {
    /// Constrói um container no estado [`Status::Created`].
    pub fn new(
        id: String,
        name: String,
        image: String,
        command: Vec<String>,
        memory_max: String,
    ) -> Self {
        let created_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            id,
            name,
            image,
            command,
            pid: None,
            pid_starttime: None,
            status: Status::Created,
            created_unix,
            memory_max,
            cpus: default_cpus(),
            cpu_weight: None,
            cpuset: None,
            io_weight: None,
            pod: None,
            ports: Vec::new(),
            env: Vec::new(),
            secrets: Vec::new(),
            secret_files: false,
            workdir: None,
            read_only: false,
            privileged: false,
            labels: std::collections::BTreeMap::new(),
            cap_drop: Vec::new(),
            cap_add: Vec::new(),
            seccomp: None,
            apparmor: None,
            userns: false,
            ip: None,
            network: None,
            namespace: default_namespace(),
            expose: None,
            extra_networks: Vec::new(),
            net_aliases: Vec::new(),
            dns_knows: None,
            tmpfs: Vec::new(),
            ulimits: Vec::new(),
            sysctls: Vec::new(),
            devices: Vec::new(),
            restart_policy: None,
            stopped_by_user: false,
            mounts: Vec::new(),
            log_driver: None,
            net_bps: None,
            net_burst: None,
            nice: None,
            firewall: None,
        }
    }

    /// Os primeiros 12 caracteres do id (como o Docker mostra).
    pub fn short_id(&self) -> &str {
        let n = self.id.len().min(12);
        &self.id[..n]
    }

    /// O caminho do cgroup dedicado deste container. Fica ANINHADO sob a
    /// `delonix.slice` (o cgroup-pai com os limites AGREGADOS de todo o Delonix),
    /// para que a soma de todos os containers nunca esgote o host.
    pub fn cgroup(&self) -> String {
        format!("{}/delonix-{}", DELONIX_SLICE, self.id)
    }
}

/// Uma microVM (Cloud Hypervisor) — a unidade do `kind: VM`. Modelo IRMÃO do
/// [`Container`]: uma VM não tem rootfs/cgroup/seccomp/init-pid, logo não faz
/// sentido sobrecarregar o `Container`. Persistida via [`store::JsonStore`]
/// (um JSON por nome, sob `$DELONIX_ROOT/vms`).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Vm {
    /// Nome da VM (chave de persistência).
    pub name: String,
    /// Disco base (qcow2/raw) indicado no manifesto.
    pub disk: String,
    /// Overlay qcow2 por-VM, criado sobre o disco base.
    pub overlay: String,
    /// Número de vCPUs.
    pub vcpus: u32,
    /// Memória (ex.: `"2G"`).
    pub memory: String,
    /// Rede usada para o *tap*.
    pub network: String,
    /// Nome da interface *tap* na bridge.
    pub tap: String,
    /// MAC derivado do nome.
    pub mac: String,
    /// PID do processo `cloud-hypervisor` (se vivo).
    pub pid: Option<i32>,
    /// Caminho do socket da API do Cloud Hypervisor.
    pub api_socket: String,
    /// Estado no ciclo de vida (reusa [`Status`]).
    pub status: Status,
    /// Timestamp Unix de criação.
    pub created_unix: u64,
    /// Política de reinício normalizada (`"no"`|`"on-failure"`|`"always"`).
    #[serde(default)]
    pub restart_policy: Option<String>,
    /// IP atribuído por DHCP (resolvido a partir do MAC), quando conhecido.
    #[serde(default)]
    pub ip: Option<String>,
    /// Backend de virtualização que arrancou esta VM (`"cloud-hypervisor"` ou
    /// `"libvirt"`). Determina como reconciliar liveness/parar. Default p/ registos
    /// antigos = `cloud-hypervisor` (o único backend antes do trait VmBackend).
    #[serde(default = "default_vm_backend")]
    pub backend: String,
}

/// Backend por omissão para VMs persistidas antes do suporte multi-backend.
fn default_vm_backend() -> String {
    "cloud-hypervisor".to_string()
}

impl Vm {
    /// Constrói uma VM no estado [`Status::Created`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        disk: String,
        overlay: String,
        vcpus: u32,
        memory: String,
        network: String,
        tap: String,
        mac: String,
        api_socket: String,
    ) -> Self {
        let created_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            name,
            disk,
            overlay,
            vcpus,
            memory,
            network,
            tap,
            mac,
            pid: None,
            api_socket,
            status: Status::Created,
            created_unix,
            restart_policy: None,
            ip: None,
            backend: default_vm_backend(),
        }
    }
}

/// O cgroup-pai de TODOS os containers do Delonix. Tem limites agregados
/// (memória/CPU/PIDs) = uma fracção do host, para o host nunca morrer por
/// excesso de containers (protecção de robustez).
pub const DELONIX_SLICE: &str = "/sys/fs/cgroup/delonix.slice";

/// Gera um id de container: 16 caracteres hexadecimais.
pub fn generate_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let mixed = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ pid.rotate_left(32);
    format!("{mixed:016x}")
}

/// Caminho do binário `delonix` a relançar para delegar operações (exec, run,
/// mutações da API…). Prefere o próprio executável, mas é **robusto a
/// substituição do binário** enquanto o servidor corre (install/upgrade): nesse
/// caso o `/proc/self/exe` fica marcado `" (deleted)"` e `current_exe()` devolve
/// um caminho inexistente — o que fazia spawns falharem com `os error 2`. Tenta,
/// por ordem: exe atual se existir → caminho sem o sufixo `(deleted)` → `delonix`
/// no `PATH` → o nome simples.
pub fn self_bin() -> std::path::PathBuf {
    use std::path::{Path, PathBuf};
    if let Ok(p) = std::env::current_exe() {
        if p.exists() {
            return p;
        }
        let s = p.to_string_lossy();
        if let Some(real) = s.strip_suffix(" (deleted)") {
            let pb = PathBuf::from(real);
            if pb.exists() {
                return pb;
            }
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let cand = Path::new(dir).join("delonix");
            if cand.exists() {
                return cand;
            }
        }
    }
    PathBuf::from("delonix")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fw_fields_reject_nft_injection() {
        // proto
        assert!(fw_proto_ok("tcp") && fw_proto_ok("udp") && fw_proto_ok("any") && fw_proto_ok(""));
        assert!(!fw_proto_ok("tcp drop; }"));
        assert!(!fw_proto_ok("tcp\n\t\taccept"));
        // port
        assert!(fw_port_ok("8080") && fw_port_ok("1000-2000") && fw_port_ok("*") && fw_port_ok(""));
        assert!(!fw_port_ok("80; flush ruleset"));
        assert!(!fw_port_ok("99999"));
        // src (o vetor crítico)
        assert!(fw_src_ok("10.0.0.0/16") && fw_src_ok("192.168.1.1") && fw_src_ok("0.0.0.0/0") && fw_src_ok("*"));
        assert!(!fw_src_ok("1.2.3.4 accept; }; chain forward { policy drop; }"));
        assert!(!fw_src_ok("1.2.3.4\n\t\taccept"));
        assert!(!fw_src_ok("$(reboot)"));
        // regra completa
        let bad = FwRule { src: "x; flush ruleset".into(), proto: "tcp".into(), port: "80".into(), ..Default::default() };
        assert!(!bad.nft_safe());
        let good = FwRule { src: "10.0.0.0/16".into(), proto: "tcp".into(), port: "443".into(), dir: "in".into(), action: "allow".into(), note: String::new() };
        assert!(good.nft_safe());
    }

    fn sample(id: &str, name: &str) -> Container {
        Container::new(
            id.to_string(),
            name.to_string(),
            "/tmp/rootfs".to_string(),
            vec!["/bin/sh".to_string()],
            "64M".to_string(),
        )
    }

    #[test]
    fn id_has_16_hex_chars() {
        let id = generate_id();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn short_id_and_cgroup() {
        let c = sample("0123456789abcdef", "web");
        assert_eq!(c.short_id(), "0123456789ab");
        // aninhado sob a delonix.slice (limites agregados — protecção do host).
        assert_eq!(c.cgroup(), "/sys/fs/cgroup/delonix.slice/delonix-0123456789abcdef");
        assert_eq!(c.status, Status::Created);
    }

    #[test]
    fn status_displays_human_readably() {
        assert_eq!(Status::Running.to_string(), "running");
        assert_eq!(Status::Failed(137).to_string(), "failed (137)");
        assert_eq!(Status::Stopped.to_string(), "stopped");
        assert_eq!(Status::Crashed.to_string(), "crashed");
        // retrocompat: registos legados `{"Exited": n}` desserializam p/ Stopped/Failed.
        assert_eq!(
            serde_json::from_str::<Status>(r#"{"Exited":0}"#).unwrap(),
            Status::Stopped
        );
        assert_eq!(
            serde_json::from_str::<Status>(r#"{"Exited":3}"#).unwrap(),
            Status::Failed(3)
        );
    }

    #[test]
    fn store_round_trip_and_lookup() {
        let dir = std::env::temp_dir().join(format!("delonix-test-{}", generate_id()));
        let store = Store::open(&dir).unwrap();

        let mut c = sample("aaaa1111bbbb2222", "web");
        c.pid = Some(4242);
        c.status = Status::Running;
        store.save(&c).unwrap();

        assert_eq!(store.load("aaaa1111bbbb2222").unwrap().pid, Some(4242));
        assert_eq!(store.load("aaaa1111").unwrap().name, "web");
        assert_eq!(store.load("web").unwrap().id, "aaaa1111bbbb2222");

        assert_eq!(store.list().unwrap().len(), 1);
        store.remove("aaaa1111bbbb2222").unwrap();
        assert!(store.load("web").is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
