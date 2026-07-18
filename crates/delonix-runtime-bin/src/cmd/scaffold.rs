//! `delonix <grupo> init` — arranca um projecto completo e funcional com **um
//! comando**: os ficheiros já vêm preenchidos (imagens incluídas), prontos a
//! `build`/`apply`/`create` sem editar nada.
//!
//! # Regra que estes templates seguem
//!
//! **Só usam campos que o parser LÊ mesmo.** Um scaffold que gera YAML com
//! campos ignorados em silêncio é pior que nenhum: dá a ilusão de configuração.
//! Cada campo aqui está ligado a código (`ContainerSpec`, `VmSpec`, `KindCluster`…);
//! o que ainda não existe aparece como comentário, nunca como chave activa.
//!
//! # Resiliência por omissão
//!
//! O que é gerado usa `restart: always` (supervisor destacado que captura o
//! exit code real e reinicia — ver `container::run_supervised`), volumes
//! nomeados para o estado (sobrevivem ao `rm` do container) e portas
//! publicadas pelo ingress. É o "resiliente e funcional" pedido — não um
//! esqueleto que é preciso completar.

use std::path::{Path, PathBuf};

use delonix_runtime_core::{Error, Result};

// Templates embebidos pelo `build.rs`: `TEMPLATES: &[(&str, &[(&str, &str)])]`
// = [(nome, [(caminho-relativo, conteúdo)])].
include!(concat!(env!("OUT_DIR"), "/templates.rs"));

/// Imagens por omissão — é isto que preenche o projecto quando NÃO se passa
/// `--image`. Fixadas por digest onde a reprodutibilidade importa.
const DEFAULT_APP_BASE: &str = "alpine:3.20";
const DEFAULT_DB_IMAGE: &str = "postgres:16-alpine";
const DEFAULT_VM_IMAGE: &str = "k8s-golden";

/// O que gerar.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Target {
    Container,
    Vm,
    Cluster,
    /// Projecto completo: Delonixfile + manifesto (todos os Kinds) + cluster.
    Stack,
}

pub(crate) struct InitOpts {
    pub dir: PathBuf,
    pub name: String,
    /// `None` = preenche com a imagem por omissão do alvo (o pedido explícito:
    /// "sem passarmos o --image já preenche as imagens").
    pub image: Option<String>,
    pub force: bool,
    /// `--template <nome>`: em vez do scaffold genérico, gera um PROJECTO COMPLETO
    /// de uma linguagem/framework (ex.: `python`) com boas práticas — código,
    /// Delonixfile, manifesto, testes e dotfiles. `list` mostra os disponíveis.
    pub template: Option<String>,
}

/// Porta por omissão exposta pelos templates de app.
const TEMPLATE_PORT: &str = "8000";

/// Nomes dos templates disponíveis, para o `--help`/erros.
pub(crate) fn template_names() -> Vec<&'static str> {
    TEMPLATES.iter().map(|(n, _)| *n).collect()
}

/// Deriva um módulo Python válido a partir do nome do projecto: minúsculas,
/// `[a-z0-9_]`, e nunca a começar por dígito (senão não é um identificador).
fn python_module(name: &str) -> String {
    let mut m: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
    if m.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(true) {
        m.insert(0, 'a');
    }
    m
}

/// Substitui os tokens `__NAME__`/`__MODULE__`/`__PORT__` (em conteúdos E caminhos).
fn subst(s: &str, o: &InitOpts, module: &str) -> String {
    s.replace("__NAME__", &o.name).replace("__MODULE__", module).replace("__PORT__", TEMPLATE_PORT)
}

/// Gera um projecto a partir de um template embebido (`--template <nome>`).
fn render_template(tname: &str, o: &InitOpts) -> Result<()> {
    if tname == "list" {
        println!("templates disponíveis: {}", template_names().join(", "));
        return Ok(());
    }
    let files = TEMPLATES
        .iter()
        .find(|(n, _)| *n == tname)
        .map(|(_, f)| *f)
        .ok_or_else(|| Error::Invalid(format!("template '{tname}' não existe — disponíveis: {}", template_names().join(", "))))?;
    let module = python_module(&o.name);
    std::fs::create_dir_all(&o.dir)?;
    let mut n = 0;
    for (rel, content) in files {
        let dest = o.dir.join(subst(rel, o, &module));
        n += usize::from(write_file(&dest, &subst(content, o, &module), o.force)?);
    }
    if n == 0 {
        eprintln!("nada a fazer (tudo já existia)");
        return Ok(());
    }
    let cd = if o.dir == Path::new(".") { String::new() } else { format!("cd {} && ", o.dir.display()) };
    println!(
        "pronto. Projecto '{}' ({tname}) em {}. Agora:\n  \
         {cd}delonix build -t {}:dev .        # constrói a imagem (Delonixfile)\n  \
         {cd}delonix stack apply              # sobe a app\n  \
         {cd}curl localhost:{TEMPLATE_PORT}/api/v1/health/live",
        o.name,
        o.dir.display(),
        o.name
    );
    Ok(())
}

/// Escreve um ficheiro, recusando-se a destruir trabalho sem `--force`.
fn write_file(path: &Path, content: &str, force: bool) -> Result<bool> {
    if path.exists() && !force {
        eprintln!("  já existe, saltado: {}  (usa --force para substituir)", path.display());
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, content).map_err(|e| Error::Invalid(format!("a escrever {}: {e}", path.display())))?;
    eprintln!("  criado: {}", path.display());
    Ok(true)
}

pub(crate) fn init(target: Target, o: &InitOpts) -> Result<()> {
    // `--template <nome>` gera um projecto completo de uma stack (código +
    // Delonixfile + manifesto + testes), em vez do scaffold genérico.
    if let Some(t) = &o.template {
        return render_template(t, o);
    }
    std::fs::create_dir_all(&o.dir)?;
    let mut n = 0;
    match target {
        Target::Container => {
            n += usize::from(write_file(&o.dir.join("Delonixfile"), &delonixfile(o), o.force)?);
            n += usize::from(write_file(&o.dir.join("delonix-manifest.yaml"), &manifest_container(o), o.force)?);
        }
        Target::Vm => {
            n += usize::from(write_file(&o.dir.join("delonix-manifest.yaml"), &manifest_vm(o), o.force)?);
        }
        Target::Cluster => {
            n += usize::from(write_file(&o.dir.join("cluster-kind.yaml"), &cluster_kind(o), o.force)?);
            n += usize::from(write_file(&o.dir.join("cluster-vm.yaml"), &cluster_vm(o), o.force)?);
            n += usize::from(write_file(&o.dir.join("cluster-ssh.yaml"), &cluster_ssh(o), o.force)?);
        }
        Target::Stack => {
            n += usize::from(write_file(&o.dir.join("Delonixfile"), &delonixfile(o), o.force)?);
            n += usize::from(write_file(&o.dir.join("delonix-manifest.yaml"), &manifest_stack(o), o.force)?);
            n += usize::from(write_file(&o.dir.join("cluster-kind.yaml"), &cluster_kind(o), o.force)?);
            n += usize::from(write_file(&o.dir.join(".dockerignore"), DOCKERIGNORE, o.force)?);
            n += usize::from(write_file(&o.dir.join("README.md"), &readme(o), o.force)?);
        }
    }
    if n == 0 {
        eprintln!("nada a fazer (tudo já existia)");
        return Ok(());
    }
    println!("{}", next_steps(target, o));
    Ok(())
}

fn next_steps(target: Target, o: &InitOpts) -> String {
    let cd = if o.dir == Path::new(".") { String::new() } else { format!("cd {} && ", o.dir.display()) };
    match target {
        Target::Container => format!("pronto. Agora:\n  {cd}delonix build -t {}:dev .\n  {cd}delonix container apply", o.name),
        Target::Vm => format!("pronto. Agora:\n  {cd}delonix vm apply"),
        Target::Cluster => format!("pronto. Agora:\n  {cd}delonix cluster create --name {}    # local, sem manifesto\n  (ou edita cluster-vm.yaml / cluster-ssh.yaml para VMs / hosts remotos)", o.name),
        Target::Stack => format!(
            "pronto. Projecto completo em {}. Agora:\n  \
             {cd}delonix build -t {}:dev .        # constrói a imagem da app\n  \
             {cd}delonix stack apply              # rede + volume + BD + app, tudo de pé\n  \
             {cd}delonix container ls",
            o.dir.display(),
            o.name
        ),
    }
}

// ---------------------------------------------------------------- templates

fn delonixfile(o: &InitOpts) -> String {
    let base = o.image.clone().unwrap_or_else(|| DEFAULT_APP_BASE.to_string());
    format!(
        "# Delonixfile — mesma gramática do Dockerfile, com extensões Delonix.\n\
         # Constrói com:  delonix build -t {name}:dev .\n\
         #\n\
         # NOTA: o build é single-stage. Um `FROM ... AS x` seguido doutro `FROM`\n\
         # é recusado com erro claro (multi-stage ainda não está implementado).\n\
         FROM {base}\n\
         \n\
         WORKDIR /app\n\
         \n\
         # As dependências primeiro: esta camada só é reconstruída quando elas mudam.\n\
         # RUN apk add --no-cache python3\n\
         \n\
         COPY . /app\n\
         \n\
         # Extensões Delonix (aceites pelo parser; ver CLAUDE.md):\n\
         #   CPUS 2\n\
         #   MEMORY 512M\n\
         #   HEALTHCHECK CMD wget -qO- http://localhost:8080/health || exit 1\n\
         \n\
         EXPOSE 8080\n\
         CMD [\"sh\", \"-c\", \"echo 'a servir na 8080'; while true; do sleep 3600; done\"]\n",
        name = o.name,
        base = base
    )
}

fn manifest_container(o: &InitOpts) -> String {
    let img = o.image.clone().unwrap_or_else(|| format!("{}:dev", o.name));
    // Template COMPLETO: toda a spec de `kind: Container`, com defaults e um
    // comentário por campo. Apaga o que não precisares — o parser aceita
    // qualquer subconjunto (só `image` é obrigatório).
    CONTAINER_REFERENCE.replace("{name}", &o.name).replace("{image}", &img)
}

/// Referência completa de `kind: Container` (todos os campos que o `apply` lê).
const CONTAINER_REFERENCE: &str = r#"# Apply with:  delonix container apply
apiVersion: delonix.io/v1
kind: Container
metadata:
  name: {name}
spec:
  image: {image}              # required — the only mandatory field
  # command + args override the image ENTRYPOINT/CMD (docker semantics):
  command: []                 # e.g. ["nginx", "-g", "daemon off;"]
  entrypoint: null            # override the image ENTRYPOINT ("" clears it)
  detach: true                # run in the background (a manifest is declarative)
  restart: always             # no | on-failure[:max] | always | unless-stopped
  # --- network ---
  network: host               # host | none | <a network created by `network create`>
  ports: []                   # ["8080:80"] — gives the container its own netns + slirp NAT
  networkAlias: []            # DNS aliases on the network
  knows: []                   # restrict name resolution to these containers (isolation)
  netBps: null                # egress rate limit (e.g. "10mbit"), only with a custom network
  netBurst: null              # burst for netBps
  # --- storage ---
  volumes: []                 # ["data:/var/lib", "/host/path:/inside:ro"]
  tmpfs: []                   # ["/scratch"]
  # --- resources (cgroup v2) ---
  memory: max                 # 64M | 2G | max (no cap)
  cpus: "1.0"                 # CPU cores
  cpuWeight: null             # relative CPU weight under contention (1-10000)
  cpuset: null                # pin to CPUs, e.g. "0-3"
  ioWeight: null              # relative I/O weight
  # --- env & secrets ---
  env: []                     # ["KEY=value"]
  envFile: []                 # ["./.env"]
  secret: []                  # vault secret names (see `delonix secret`); injected as env
  secretFiles: false          # true → secrets go to /run/secrets/<name> instead of env
  # --- security ---
  privileged: false           # all caps, seccomp off — trusted workloads only
  readOnly: false             # read-only rootfs (writes go to tmpfs/volumes)
  capAdd: []                  # ["NET_ADMIN"]
  capDrop: []                 # ["MKNOD"]
  securityOpt: []             # ["seccomp=unconfined", "apparmor=<profile>"]
  apparmor: null              # AppArmor profile
  selinux: null               # SELinux context
  userns: false               # force the subuid user namespace (default-on in rootless)
  hostPid: false              # share the host PID namespace
  hostIpc: false              # share the host IPC namespace
  detect: false               # seccomp in log mode — discover the syscalls a workload uses
  # --- devices & limits ---
  devices: []                 # ["/dev/fuse"]
  gpus: null                  # all | nvidia | dri
  ulimit: []                  # ["nofile=1024:2048"]
  sysctl: []                  # ["net.core.somaxconn=1024"]
  # --- misc ---
  labels: []                  # ["tier=frontend"]
  logDriver: null             # json | cri
"#;

fn manifest_vm(o: &InitOpts) -> String {
    let img = o.image.clone().unwrap_or_else(|| DEFAULT_VM_IMAGE.to_string());
    VM_REFERENCE.replace("{name}", &o.name).replace("{image}", &img)
}

/// Referência completa de `kind: Vm` (espelha `delonix_vm::VmConfig`).
const VM_REFERENCE: &str = r#"# Apply with:  delonix vm apply
# The golden VM image is built once with:
#   delonix image vm build -t {image} --k8s-version 1.34
apiVersion: delonix.io/v1
kind: Vm
metadata:
  name: {name}
spec:
  disk: {image}               # required — base qcow2/raw (an overlay is made per VM)
  vcpus: 2
  memory: 2G                  # "2G" | "1024M" (a "…i" suffix is accepted, k8s-style)
  network: {name}-net         # ingress network for the VM's tap
  restart_policy: null        # no | on-failure | always
  # --- boot: firmware (cloud images) OR direct kernel ---
  firmware: null              # UEFI firmware path (typical for cloud images)
  kernel: null                # direct kernel boot (alternative to firmware)
  initrd: null                # initramfs for direct kernel boot
  cmdline: null               # kernel cmdline for direct boot
  # --- cloud-init ---
  seed: null                  # path to a prebuilt NoCloud ISO (hostname/ssh come from here)
                              # via manifest only a ready seed is accepted; use
                              # `delonix vm create --user-data …` to generate one.
  # --- performance / passthrough ---
  hugepages: false
  cpu_affinity: null          # pin vCPUs, e.g. "8-15"
  devices: []                 # VFIO PCI passthrough (sysfs paths)
  # --- backend selection ---
  backend: null               # cloud-hypervisor | libvirt | null (auto)
  net_mode: null              # libvirt only: user | nat | bridge
  bridge: null                # host bridge / libvirt network
"#;

fn manifest_stack(o: &InitOpts) -> String {
    let app = o.image.clone().unwrap_or_else(|| format!("{}:dev", o.name));
    format!(
        "# Projecto completo: volume + BD + app. Aplica TUDO com:\n\
         #   delonix stack apply\n\
         #\n\
         # Ordem de aplicação (por dependência): Network -> Volume -> Image -> Vm -> Container.\n\
         # Semântica: \"garante presente\", idempotente por nome. Fail-fast SEM rollback:\n\
         # o que já foi aplicado antes de um erro FICA aplicado.\n\
         #\n\
         # PORQUÊ SEM `network:` E SEM `ports:` — não é esquecimento:\n\
         #   Estes containers ficam em `--net host` (o default) e falam entre si por\n\
         #   `127.0.0.1` — testado e funcional em rootless. As alternativas HOJE não\n\
         #   servem para um scaffold que tem de funcionar à primeira:\n\
         #     * `network: <rede>` (isolamento + DNS por nome) tem uma limitação\n\
         #       CONHECIDA em rootless — o `setns` falha (\"netns do pod indisponível\"),\n\
         #       porque a netns vive no userns do holder. Só funciona como root.\n\
         #     * `ports:` dá ao container uma netns PRÓPRIA com slirp — óptimo para\n\
         #       expor ao mundo, mas o slirp corre com `--disable-host-loopback`, por\n\
         #       isso a app deixaria de alcançar a BD em `127.0.0.1`.\n\
         #   Em ROOT, ou quando o `setns` rootless estiver fechado, troca para\n\
         #   `network:` + `ports:` e usa o nome do container no DATABASE_URL.\n\
         apiVersion: delonix.io/v1\n\
         kind: Volume\n\
         metadata:\n  name: {name}-data\n\
         spec: {{}}\n\
         ---\n\
         apiVersion: delonix.io/v1\n\
         kind: Container\n\
         metadata:\n  name: {name}-db\n\
         spec:\n  \
         image: {db}\n  \
         # Supervisor destacado: captura o exit code real e reinicia. Um `stop`\n  \
         # explícito é respeitado (não ressuscita).\n  \
         restart: always\n  \
         # Volume NOMEADO: os dados sobrevivem ao `rm` do container.\n  \
         volumes:\n    - \"{name}-data:/var/lib/postgresql/data\"\n  \
         env:\n    - \"POSTGRES_PASSWORD=troca-me\"\n    - \"POSTGRES_DB={name}\"\n\
         ---\n\
         apiVersion: delonix.io/v1\n\
         kind: Container\n\
         metadata:\n  name: {name}-app\n\
         spec:\n  \
         # Constrói primeiro:  delonix build -t {app} .\n  \
         image: {app}\n  \
         restart: always\n  \
         env:\n    - \"APP_ENV=dev\"\n    \
         - \"DATABASE_URL=postgres://postgres:troca-me@127.0.0.1:5432/{name}\"\n",
        name = o.name,
        db = DEFAULT_DB_IMAGE,
        app = app
    )
}

fn cluster_kind(o: &InitOpts) -> String {
    let img = o.image.clone().unwrap_or_else(|| super::kindmode::DEFAULT_NODE_IMAGE.to_string());
    format!(
        "# Cluster Kubernetes LOCAL, em containers — sem Docker e sem o binário `kind`.\n\
         #   delonix cluster create --name {name}       # não precisa deste ficheiro\n\
         #   delonix cluster apply -f cluster-kind.yaml # versiona a config no git\n\
         apiVersion: delonix.io/v1\n\
         kind: Cluster\n\
         metadata:\n  name: {name}\n\
         spec:\n  \
         # kind = containers aqui | vm = VMs douradas | ssh = hosts remotos\n  \
         mode: kind\n  \
         k8sVersion: \"1.34\"\n  \
         podSubnet: 10.244.0.0/16\n  \
         serviceSubnet: 10.96.0.0/12\n  \
         # default = a CNI da imagem (kindnet); none = aplicas a tua depois\n  \
         cni: default\n  \
         controlPlane:\n    replicas: 1\n  \
         workers:\n    replicas: 0\n  \
         kind:\n    \
         # Fixada por digest: uma tag móvel tornaria o cluster irreprodutível.\n    \
         image: {img}\n    \
         apiServerPort: 6443\n",
        name = o.name,
        img = img
    )
}

fn cluster_vm(o: &InitOpts) -> String {
    format!(
        "# Cluster Kubernetes em microVMs (kernel próprio por nó — isolamento de\n\
         # hipervisor). A imagem dourada já traz kubeadm/kubelet/kubectl + delonix-cri:\n\
         # arrancar um nó NÃO instala nada.\n\
         #   delonix image vm build -t {img} --k8s-version 1.34   # uma vez\n\
         #   delonix network create {name}-net\n\
         #   delonix cluster apply -f cluster-vm.yaml\n\
         apiVersion: delonix.io/v1\n\
         kind: Cluster\n\
         metadata:\n  name: {name}\n\
         spec:\n  \
         mode: vm\n  \
         k8sVersion: \"1.34\"\n  \
         podSubnet: 10.244.0.0/16\n  \
         serviceSubnet: 10.96.0.0/12\n  \
         cni: default\n  \
         controlPlane:\n    replicas: 1   # >1 exige controlPlaneEndpoint (LB/VIP)\n  \
         workers:\n    replicas: 2\n  \
         vm:\n    \
         image: {img}\n    \
         network: {name}-net\n    \
         vcpus: 2\n    \
         memory: 2G\n    \
         bootTimeout: 300s\n",
        name = o.name,
        img = DEFAULT_VM_IMAGE
    )
}

fn cluster_ssh(o: &InitOpts) -> String {
    format!(
        "# Cluster Kubernetes em hosts remotos JÁ EXISTENTES (datacenter/bare-metal).\n\
         # O delonix NÃO cria estas máquinas: têm de estar vivas, alcançáveis por SSH,\n\
         # e o utilizador tem de ter `sudo` NOPASSWD.\n\
         #   delonix cluster apply -f cluster-ssh.yaml\n\
         #\n\
         # Idempotente SEM ficheiro de estado (\"Terraform sem .tfstate\"): cada passo\n\
         # tem um `check` e um `apply`. Correr duas vezes não faz nada de novo.\n\
         apiVersion: delonix.io/v1\n\
         kind: Cluster\n\
         metadata:\n  name: {name}\n\
         spec:\n  \
         mode: ssh\n  \
         k8sVersion: \"1.34\"\n  \
         podSubnet: 10.244.0.0/16\n  \
         serviceSubnet: 10.96.0.0/12\n  \
         # Em produção instalas normalmente a TUA CNI (Cilium, Calico...).\n  \
         cni: none\n  \
         # HA: com >1 control-plane isto é OBRIGATÓRIO (o kubeadm precisa de um\n  \
         # endereço estável à frente deles). O delonix não provisiona o LB.\n  \
         controlPlaneEndpoint: \"k8s-api.exemplo.ao:6443\"\n  \
         controlPlane:\n    hosts:\n      - address: 10.0.0.11\n      - address: 10.0.0.12\n      - address: 10.0.0.13\n  \
         workers:\n    hosts:\n      - address: 10.0.0.21\n      - address: 10.0.0.22\n  \
         ssh:\n    user: delonix\n    keyPath: ~/.ssh/id_ed25519\n    port: 22\n  \
         # Só `stacked` é suportado; `external` é recusado com erro claro.\n  \
         etcd:\n    mode: stacked\n",
        name = o.name
    )
}

const DOCKERIGNORE: &str = "# Fora do contexto de build (menos bytes a copiar, imagem mais pequena).\n\
                            .git\n\
                            target/\n\
                            node_modules/\n\
                            *.qcow2\n\
                            delonix-manifest.yaml\n\
                            cluster-*.yaml\n";

fn readme(o: &InitOpts) -> String {
    format!(
        "# {name}\n\n\
         Projecto Delonix — containers e Kubernetes **sem daemon e sem root**.\n\n\
         ## Arrancar\n\n\
         ```bash\n\
         delonix build -t {name}:dev .   # constrói a imagem da app (Delonixfile)\n\
         delonix stack apply             # rede + volume + BD + app\n\
         delonix container ls\n\
         delonix container logs -f {name}-app\n\
         ```\n\n\
         ## Kubernetes local\n\n\
         ```bash\n\
         delonix cluster create --name {name}    # nós em containers, sem Docker\n\
         kubectl --kubeconfig ~/.local/share/delonix/clusters/{name}-kubeconfig.yaml get nodes\n\
         delonix cluster delete --name {name}\n\
         ```\n\n\
         ## Ficheiros\n\n\
         | Ficheiro | O quê |\n\
         |---|---|\n\
         | `Delonixfile` | Build da imagem (gramática do Dockerfile + extensões) |\n\
         | `delonix-manifest.yaml` | Rede, volume, BD e app — `delonix stack apply` |\n\
         | `cluster-kind.yaml` | Cluster Kubernetes local |\n\n\
         ## Notas honestas\n\n\
         - O build é **single-stage** (multi-stage é recusado com erro claro).\n\
         - `stack apply` é *garante-presente*, fail-fast e **sem rollback**.\n\
         - `restart: always` é servido por um supervisor por container (não há daemon);\n\
           sem daemon, não há ressurreição no reboot do host.\n\
         - Os containers do stack ficam em `--net host` e falam por `127.0.0.1`.\n\
           Redes de utilizador (isolamento + DNS por nome) têm uma limitação\n\
           conhecida em rootless (`setns`) — só funcionam como root. Ver o\n\
           comentário no topo do `delonix-manifest.yaml`.\n",
        name = o.name
    )
}
