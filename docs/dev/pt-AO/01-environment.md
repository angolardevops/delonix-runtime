<!-- translated-from: 01-environment.md sha256:2fa9b18d23c95c5d2b01c35e9e1a3eece195a5754b6a4ba0d3143aa61cd3b9ec -->
# 1. Preparar o teu ambiente

O Delonix Runtime é **só para Linux**: todos os primitivos que usa — namespaces, cgroups v2, nftables,
`pivot_root`, a nova API de mount — vivem no kernel Linux. Consegues *compilar* a maior parte do
workspace e correr os seus testes de lógica pura em qualquer máquina Linux com a toolchain abaixo;
para *correr* containers e exercitar os caminhos ao vivo precisas de um host que cumpra os requisitos
de kernel e de pacotes desta página.

Muito do que parece um bug do motor numa máquina acabada de instalar é um pré-requisito do host. Lê a
secção [Armadilhas conhecidas do host](#known-host-traps) antes de abrires uma issue.

## Toolchain

<!-- dev-docs:begin toolchain -->
- **Toolchain de Rust:** `1.96.0` (fixada no `rust-toolchain.toml`; o `rustup` instala-a na primeira chamada ao `cargo`)
- **Componentes:** `rustfmt`, `clippy`
<!-- dev-docs:end toolchain -->

Instala o [`rustup`](https://rustup.rs/) e deixa-o apanhar o canal fixado; não o substituas por
`stable`. A CI usa exactamente o mesmo ficheiro (`rustup show` em todos os jobs).

### `protoc` (necessário para compilar)

O `crates/interfaces/delonix-cri/build.rs` compila o protobuf do CRI do Kubernetes com
`tonic-build`/`prost`, que precisa do compilador de Protocol Buffers no `PATH`. O binário `delonix`
depende do `delonix-cri`, por isso **um simples `cargo build --workspace` falha sem ele**:

```bash
# Debian / Ubuntu
sudo apt install protobuf-compiler
# Fedora / RHEL family
sudo dnf install protobuf-compiler
```

Ou descarrega uma release de <https://github.com/protocolbuffers/protobuf/releases>. A CI instala o
`protobuf-compiler` do apt em todos os jobs que compilam.

### Ferramentas opcionais, só para gates específicos

| Ferramenta | Necessária para | Onde está fixada |
|---|---|---|
| Python 3.11+ | todos os gates `scripts/*.py` (usam `tomllib`) | — |
| `buf` v1.73.0 e `protoc-gen-openapi` v0.7.1 (toolchain de Go para os instalar) | `scripts/contract_gate.py` | job `contract` em `.github/workflows/ci.yml` |
| `cargo-deny` | a verificação da cadeia de fornecimento | job `deny`, configuração em `deny.toml` |
| Módulo Python `markdown` | `docs/gen.py` (renderiza o `ARCHITECTURE.md`) | job `docs` |
| `groff` | verificar as man pages geradas | job `docs` |

Ver [02 — Clonar, compilar e testar](02-build-and-test.md) para saber como cada uma é corrida.

## Requisitos de kernel

Estas são as funcionalidades do kernel de que o motor depende. O instalador (`scripts/install.sh`) e o
`delonix system doctor` verificam a maior parte delas por ti.

| Requisito | Porquê | Como verificar |
|---|---|---|
| **cgroup v2** (hierarquia unificada) | os limites de recursos e a contabilização são escritos em `/sys/fs/cgroup` | `stat -fc %T /sys/fs/cgroup` imprime `cgroup2fs` |
| **User namespaces sem privilégio** | o modelo rootless: o motor só se torna "root" dentro do seu próprio user namespace | `unshare -r -n true` tem sucesso |
| **`/dev/net/tun`** | `slirp4netns` (rede rootless) e taps de VM | `test -e /dev/net/tun` |
| **overlayfs** com a nova API de mount e `lowerdir+` (Linux **6.5** ou mais recente) | os sistemas de ficheiros raiz dos containers são mounts overlay construídos com `fsopen`/`fsconfig`/`fsmount`, uma chamada `lowerdir+` por camada — ver [ADR-0037](../../adr/0037-overlay-mount-new-api.md) | `uname -r` |
| **`br_netfilter`** carregado, `net.bridge.bridge-nf-call-iptables=1` | o isolamento de namespace é imposto em chains `forward` do nftables; sem este módulo o tráfego entre dois containers na mesma bridge nunca lhes chega, e o isolamento fica inerte em silêncio | `delonix system doctor` |
| **KVM** (`/dev/kvm`) | só para microVMs — ver [09](09-microvm-setup.md) | `test -w /dev/kvm` |

O requisito do 6.5 **não tem verificação prévia**: um kernel mais antigo falha quando o primeiro rootfs
de container é montado, não no arranque (o ADR-0037 regista isto como uma escolha deliberada).

Em kernels Debian mais antigos os user namespaces sem privilégio estão desligados por
`kernel.unprivileged_userns_clone=0`; o instalador põe-no a `1`.

## Pacotes do host

A fonte de verdade é o [`scripts/install.sh`](../../../scripts/install.sh), que é também o
instalador oficial (é publicado como asset de release). Detecta o gestor de pacotes através de
`/etc/os-release` e suporta **apt** (Debian, Ubuntu e derivados), **dnf** (Fedora, RHEL,
CentOS Stream, Rocky, AlmaLinux), **zypper** (openSUSE, SLES) e **pacman** (Arch e
derivados). Só existem binários pré-compilados para **x86_64**; noutras arquitecturas compila a partir
do código-fonte.

Para correr containers o motor precisa de:

| Comando | Pacote (nomes apt / dnf) | Porquê |
|---|---|---|
| `slirp4netns` | `slirp4netns` | rede rootless e portas publicadas — sem ele o `run -p` falha |
| `newuidmap` / `newgidmap` | `uidmap` / `shadow-utils` | helpers setuid que mapeiam mais do que um uid no user namespace; sem eles as imagens com um utilizador não-root falham no `chown()` |
| `nft` | `nftables` | firewall da SDN, isolamento e DNAT de portas |
| `ip` | `iproute2` / `iproute` | ligação de veth, bridge e netns |
| `conntrack` (opcional) | `conntrack` / `conntrack-tools` | limpar ligações quando uma porta deixa de ser publicada |

Para VMs, `qemu-img`, `cloud-localds` (`cloud-image-utils`), `virsh` (libvirt) e/ou
Cloud Hypervisor com o seu firmware — ver [09](09-microvm-setup.md). Para construir imagens de VM,
`libguestfs-tools` (`install.sh --with-image-build`).

Precisas também de um **intervalo subordinado de uid/gid** para o teu utilizador em `/etc/subuid` e
`/etc/subgid`; sem ele o user namespace só consegue mapear um uid.

### O caminho mais rápido para um host a funcionar

Não tens de replicar o instalador à mão. Para instalar só as dependências e a configuração do host,
mantendo o binário que tu próprio compilas:

```bash
bash scripts/install.sh --no-binary
```

Lê primeiro a lista de flags no topo do script: algumas flags mudam definições de segurança de todo o
host (`--low-ports` deixa qualquer programa local ligar-se a portas a partir da 80, `--with-image-build`
torna `/boot/vmlinuz-*` legível por todos), e `--no-tune` salta os módulos de kernel e os sysctls,
incluindo o `br_netfilter`.

### Memória e disco

Não há um mínimo fixo. O que custa recursos é o que corres: imagens e camadas de containers, discos de
VM, e o próprio directório `target/` do Rust (builds de debug do workspace inteiro ocupam vários
gigabytes). Vigia o espaço livre em disco — os kubelets de um cluster local começam a despejar pods
sob pressão de disco, o que depois parece um problema do motor.

## Diagnosticar o host

Compila o binário (ver [02](02-build-and-test.md)) e pergunta-lhe. Os comandos abaixo são só de
leitura, a menos que passes `--delegate`:

```bash
./target/debug/delonix system doctor          # is every prerequisite met? says how to fix each
./target/debug/delonix system info            # rootless?, cgroup delegation, network infra, counts
./target/debug/delonix system setup           # diagnose cgroup delegation
./target/debug/delonix system resources       # which controllers are delegated, which flags are ignored
```

O `delonix system doctor --strict` sai com código diferente de zero quando uma verificação falha, o que é
útil num script de provisionamento. Se correres estes comandos numa máquina que já tem uma instalação
do Delonix em uso, isola primeiro o estado (ver [Isolar o estado do motor](02-build-and-test.md#isolating-the-engines-state)).

## Armadilhas conhecidas do host

### Ubuntu 23.10+: o AppArmor bloqueia user namespaces para o teu binário de desenvolvimento

O Ubuntu recente define `kernel.apparmor_restrict_unprivileged_userns=1`. Um binário sem perfil
AppArmor não consegue então criar um user namespace, e o motor morre no `unshare()` com `EPERM` —
o que se lê como um bug do motor.

O `install.sh` instala um perfil (`/etc/apparmor.d/delonix`, `flags=(unconfined)` com `userns`),
mas esse perfil está **preso a um caminho**: `<install dir>/delonix` (`/usr/local/bin/delonix` por
omissão, `~/.local/bin/delonix` com `--user`). Um binário que acabaste de compilar em
`target/debug/delonix` — ou que copiaste para `/tmp` — **não** está coberto.

Opções, da menos para a mais invasiva:

1. Acrescenta um segundo perfil para o teu caminho de desenvolvimento (por exemplo o
   `target/debug/delonix` do teu worktree), com a mesma forma do que o instalador escreve, e carrega-o
   com `sudo apparmor_parser -r <file>`. Isto não toca em nada do que já está a correr.
2. Instala o teu build no caminho com perfil (`sudo install -m 0755 target/debug/delonix /usr/local/bin/`)
   — **só numa máquina onde nenhum workload do Delonix esteja em uso**. O binário instalado é o que as
   units de boot (`ExecStart=<exe> container start …`) e os servidores re-executam, por isso num host
   com workloads vivos um build de debug tornar-se-ia em silêncio o motor de produção.
3. Define `kernel.apparmor_restrict_unprivileged_userns=0` — isto baixa uma fronteira de todo o host;
   só numa máquina que é tua.

### Delegação de cgroup: os limites são aceites e ignorados em silêncio

`--memory`, `--cpus` e `--pids-limit` só funcionam se a shell a partir da qual corres o motor estiver
num cgroup **delegado**. Sem delegação as flags são lidas, aceites e ficam inertes: o container
corre sem limite. Isto é uma regra do cgroup v2, não uma limitação do Delonix — o Podman rootless tem
o mesmo requisito.

O caso comum é uma **sessão SSH**: o seu `session-N.scope` é *irmão* do
`user@<uid>.service`, e mover um processo entre os dois exige escrever num cgroup que pertence ao root.
A correcção por comando não precisa de root:

```bash
systemd-run --user --scope -p Delegate=yes -- ./target/debug/delonix container run -d -m 128M alpine sleep 60
```

Para workloads de longa duração, usa uma unit systemd de **utilizador** com `Delegate=yes`. Alguns
hosts delegam às sessões de utilizador só `cpu memory pids`; `cpuset` e `io` podem nunca estar
disponíveis em rootless, e o `delonix system resources` nomeia as flags que vão ser ignoradas. O
`delonix system setup --delegate` escreve um drop-in para todo o sistema (precisa de root, faz efeito
no próximo login) quando falta o próprio controlador `cpu`.

### Um `delonix` antigo no teu `PATH`

Se o Delonix estiver instalado na máquina, o `delonix` do teu `PATH` é a release instalada, não a tua
árvore. Corre sempre `./target/debug/delonix` (ou `target/release/delonix`) quando testas uma mudança.
O `--version` mostra o commit e a distância à última tag
(`commit: <hash> (+N commits since vX.Y.Z)`), porque entre releases dois builds partilham o mesmo
número de versão.

### Outras armadilhas que podes encontrar

- **Portas abaixo de 1024** falham em rootless com `slirp_add_hostfwd failed`: a porta é ligada pelo
  `slirp4netns`, um processo sem privilégio. Usa uma porta alta, ou faz opt-in com `install.sh --low-ports`.
- **Runners de CI alojados** (alojados pelo GitHub) bloqueiam user namespaces sem privilégio. O workflow
  de caos detecta isto e reporta `skipped`, não `success`; para exercitar os caminhos ao vivo precisas
  de um host real, de um runner self-hosted ou de uma VM.
- **As armadilhas de firmware de VM e de construção de imagens** (a escolha do firmware do Cloud
  Hypervisor, um `passt` antigo nos builds do libguestfs, as permissões de `/boot/vmlinuz-*`) são
  tratadas em [09](09-microvm-setup.md).
