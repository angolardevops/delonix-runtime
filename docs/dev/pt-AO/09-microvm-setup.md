<!-- translated-from: 09-microvm-setup.md sha256:80e8da10a62ae255befef80de892f936bc485c1c05fc1ad7925d714a9f900918 -->
# 9. Construir microVMs

Esta página leva um contribuidor de um host Linux sem nada até construir, arrancar e testar VMs
com o Delonix, e mostra onde vive o código quando algo precisa de mudar. Assume que já leste
[01 — Preparar o ambiente](01-environment.md) e que consegues construir a árvore
([02](02-build-and-test.md)).

> **O que foi verificado para esta página.** Cada comando e flag abaixo foi verificado contra o
> `delonix <group> --help` de um binário construído a partir desta árvore, e os comandos só de
> leitura / de configuração (`vm ls`, `vm reach`, `vm default-backend`, `image vm ls`,
> `manifest validate`, `stack apply --dry-run`, os erros de selecção de backend) foram **corridos**
> com `DELONIX_ROOT` e `DELONIX_NET_RUNTIME_DIR` apontados para um directório de rascunho.
> **Arrancar uma VM, construir uma imagem e fazer pull do registo não foram executados nesta
> revisão** — o comportamento desses vem do código, dos ADRs e dos testes em script referidos abaixo.

Usa sempre o binário que construíste (`./target/debug/delonix`), não um que esteja no teu `PATH`, e
isola sempre **os dois** state roots enquanto fazes experiências — ver [Isolamento](#isolation-first).

---

## 1. Pré-requisitos do host

### KVM

Os dois backends locais precisam de virtualização por hardware:

```bash
ls -l /dev/kvm            # must exist; missing = VT-x/AMD-V off in firmware, or no nested virt
id -nG | tr ' ' '\n' | grep -x kvm   # your user must be in the kvm group
```

O `scripts/install.sh` (por omissão, ou seja sem `--no-vm`) acrescenta-te aos grupos `kvm` e
`libvirt` e avisa quando falta o `/dev/kvm`. As mudanças de grupo precisam de uma sessão de login
nova.

### Cloud Hypervisor e o seu firmware

O backend está disponível quando o `cloud-hypervisor` está no `PATH`
(`CloudHypervisorBackend::available` em `crates/adapters/delonix-vm/src/lib.rs`). Onde a
distribuição não o empacota, o instalador descarrega o binário **estático** de upstream para
`/usr/local/bin/cloud-hypervisor`, fixado a uma versão **e** a um SHA-256 em `scripts/install.sh`.

Para arrancar uma cloud image sem `--kernel`, o CH precisa de firmware UEFI. O `default_ch_firmware`
usa `$DELONIX_HYPERVISOR_FW` se estiver definida, caso contrário o primeiro ficheiro que exista em
`DEFAULT_CH_FIRMWARES`:

```text
/usr/local/share/delonix/CLOUDHV.fd       ← EDK2 build from cloud-hypervisor/edk2 (preferred)
/usr/share/delonix/CLOUDHV.fd
/usr/local/share/delonix/hypervisor-fw    ← rust-hypervisor-firmware (fallback)
/usr/share/delonix/hypervisor-fw
```

**A ordem importa.** Medido e registado no doc comment da constante: com o `rust-hypervisor-fw`
nenhuma das imagens que este projecto constrói arranca em CH; com o EDK2 `CLOUDHV.fd` arrancam. O
`hypervisor-fw` fica como recurso para hosts que só o têm a ele. O instalador obtém os dois (cada um
fixado por tag e SHA-256). O teste unitário `o_edk2_vem_antes_do_hypervisor_fw_na_procura_de_firmware`
guarda a ordem.

### libvirt / QEMU

O backend libvirt está disponível quando `virsh` e `qemu-system-x86_64` estão ambos no `PATH`
(`LibvirtBackend::available`). O instalador instala o QEMU e um pacote do daemon libvirt e activa o
`libvirtd` (por socket activation, onde for suportado).

Que ligação libvirt é usada importa mais do que parece (`libvirt_uri_for`):

| Situação | Ligação | Consequência |
|---|---|---|
| `--net-mode nat` ou `bridge` | `qemu:///system` | IP alcançável; precisa do grupo `libvirt` (ou de root) |
| sem `--net-mode`, ligação de sistema utilizável | `qemu:///system`, **`nat` escolhido automaticamente** | IP por DHCP a partir da rede libvirt (`virbr0`) |
| sem `--net-mode`, ligação de sistema **não** utilizável, rootless | `qemu:///session`, user-mode | **nenhum IP visível nem alcançável**; `vm create` avisa |

Em `qemu:///system` o QEMU corre como o utilizador do serviço libvirt, que não consegue ler um disco
debaixo de uma home 0700. Para um chamador rootless, o XML do domínio recebe um `seclabel` DAC
estático que fixa o QEMU ao teu uid/gid com `relabel='no'`, para que o teu próprio overlay arranque
sem lhe mudarem o dono.

### Ferramentas que o código de VM invoca

| Ferramenta | Pacote (Debian / Fedora) | Usada por |
|---|---|---|
| `qemu-img` | `qemu-utils` / `qemu-img` | overlays por VM, `vm convert`, snapshots em CH, builds de imagem |
| `cloud-localds` | `cloud-image-utils` / `cloud-utils` | ISO de seed NoCloud — gerado em **cada** `vm create` de uma imagem cloud-init, a menos que seja dado `--seed` (`crates/adapters/delonix-vm/src/cloudinit.rs`) |
| `virsh` | `libvirt-clients` / `libvirt-client` | backend libvirt |
| `virt-customize`, `virt-sparsify`, `virt-copy-out` | `libguestfs-tools` / `guestfs-tools` | só `image vm build` |

O `vmimage::tool_package` faz corresponder um binário em falta ao seu pacote, para que uma
ferramenta em falta seja reportada pelo nome e não como um `No such file or directory` seco.

### Só para construir imagens: `--with-image-build`

O `image vm build` corre o `virt-customize`, que constrói um pequeno appliance com o supermin. Três
problemas do host partem-no de maneiras que não parecem problemas do host; o
`scripts/install.sh --with-image-build` trata deles, e o `tool_failure_hint` (`cmd/vmimage.rs`)
nomeia-os quando um build falha:

1. **Nenhum cliente DHCP no host.** O supermin *copia* pacotes do host para o appliance; sem
   `isc-dhcp-client` o appliance não tem rede e o build morre em
   `Temporary failure resolving …`. O instalador instala-o.
2. **O `/boot/vmlinuz-*` está a 0600** (Debian/Ubuntu). O supermin copia o kernel do host e falha com
   `Permission denied`. O instalador corre `chmod 0644 /boot/vmlinuz-*` — isto **baixa uma fronteira
   de segurança do host** (qualquer utilizador local consegue ler a imagem do kernel), por isso é
   opt-in e imprime como reverter (`sudo chmod 0600 /boot/vmlinuz-*`). A dica de falha mostra também
   como fazer com que sobreviva às actualizações do kernel.
3. **passt**, só com `image vm build --network`. O libguestfs dá rede ao appliance através do passt.
   O perfil AppArmor dele (Debian/Ubuntu) proíbe o directório de runtime que o libguestfs usa, e o
   passt empacotado no Ubuntu 24.04 arranca mas nunca entrega um lease — o `dhclient` espera ~300 s e
   o build continua **sem** rede, falhando mais tarde num mirror de pacotes. Remédios:

   ```bash
   mkdir -p /tmp/delonix-run && chmod 700 /tmp/delonix-run
   XDG_RUNTIME_DIR=/tmp/delonix-run ./target/debug/delonix image vm build --network …
   ```

   e, se ainda falhar, um passt actual **primeiro no `PATH`** (o instalador constrói um em
   `/usr/local/bin`). Não «desligues» o passt com um stub que falha: o libguestfs passa então a usar o
   stub e morre nele.

O modo `--offline` da receita dourada evita a armadilha 3 por completo: obtém e verifica os pacotes
no host e corre o convidado com `--no-network`.

### Isolamento primeiro

Esta máquina pode estar a correr outras cargas. Antes de qualquer comando `vm` para além de `--help`:

```bash
export DELONIX_ROOT=$HOME/dlx-dev/root
export DELONIX_NET_RUNTIME_DIR=/tmp/dlx-dev-run     # keep it SHORT (AF_UNIX sun_path is 108 bytes)
export TMPDIR=$HOME/dlx-dev/tmp                     # VMfile builds put whole disks here
mkdir -p "$DELONIX_ROOT" "$DELONIX_NET_RUNTIME_DIR" "$TMPDIR"
```

Os dois roots, sempre: isolar só o `DELONIX_ROOT` deixa os sockets de rede partilhados com o estado
real e já reiniciou cargas reais antes (ver [02](02-build-and-test.md)). Uma VM Cloud Hypervisor
também recusa logo à partida quando `<root>/vms/<name>.sock` não cabe em `sun_path`
(`ch_socket_paths_fit`), por isso evita caminhos de `DELONIX_ROOT` muito profundos.

---

## 2. Backends e como um é escolhido

### A porta e o registo

O `VmBackend` (`crates/adapters/delonix-vm/src/lib.rs`) é a porta que cada hypervisor implementa:
`id`, `available`, `boot`, `is_running`, `ip`, `stop`, mais métodos com implementação por omissão
(`destroy`, `pause`, `unpause`, `resume`, `snapshot`/`restore`/`snapshots`/`delete_snapshot`,
`preserve_snapshots`, `ip_is_predicted`, `manages_own_storage`, `auto_selectable`, `disk_health`).
Um default que não pode ser honrado **falha fechado** com uma mensagem, nunca é um no-op silencioso.

Os backends vivem num registo (`BACKENDS`), não num `match`:

| Backend | Crate | Aliases | Auto-seleccionável |
|---|---|---|---|
| `cloud-hypervisor` | `delonix-vm` (embutido) | `ch`, `cloudhypervisor` | sim |
| `libvirt` | `delonix-vm` (embutido) | `kvm`, `qemu` | sim |
| `proxmox` | `delonix-proxmox` (registado pela CLI) | — | **não** — seleccionado só pelo nome |

O `register_backend` recusa um id/alias que pertença a outro backend, e recusa
`auto_selectable: true` para tudo o que não seja embutido: a auto-detecção pergunta `available()` a
cada candidato, e um backend remoto só consegue responder a isso pela rede. Registar não faz I/O; a
factory corre na primeira vez que o backend é seleccionado. O `backend_for` numa VM existente
resolve o backend registado, e um nome desconhecido é um **erro** (antes caía para o CH).

### Precedência de selecção para uma VM nova

A partir de `delonix_vm::create_with` e `resolve_vm_defaults` (`cmd/vm.rs`), ganha a primeira que
corresponder:

1. `--backend` (ou `backend:` no manifesto).
2. O `HYPERVISOR` da imagem (registado por um build de VMfile), quando `--disk` nomeia uma imagem
   local.
3. `DELONIX_VM_BACKEND` (para toda a sessão).
4. `delonix vm default-backend --set <backend>` (para toda a máquina, guardado em
   `<DELONIX_ROOT>/vm-default-backend`).
5. Heurística de capacidade: `volumes` presentes ⇒ `libvirt` (só o libvirt faz virtio-9p); uma cloud
   image sem `--kernel` ⇒ `libvirt` **se o libvirt estiver disponível**; caso contrário
   auto-detecção — o primeiro backend registado auto-seleccionável que esteja instalado (CH, depois
   libvirt).

Por isso, num host com os dois hypervisors, um `vm create` simples de uma cloud image vai parar ao
**libvirt**; passa `--backend cloud-hypervisor` (ou define um default) para teres uma microVM na SDN.

```text
$ delonix vm default-backend
none (auto-detection: cloud-hypervisor if installed, else libvirt)
$ delonix vm default-backend --set ch
default backend set to cloud-hypervisor
$ delonix vm default-backend --set bogus
error invalid argument: unknown VM backend: 'bogus' (use 'cloud-hypervisor', 'libvirt')
$ delonix vm default-backend --clear
default backend cleared (falls back to auto-detection)
```

### Proxmox VE (remoto)

O `bins/delonix-runtime-bin/src/cmd/vmbackends.rs::register_configured` regista o backend Proxmox
no arranque quando configurado através do ambiente (uma má configuração é um aviso, nunca fatal para
comandos sem relação):

| Variável | Significado |
|---|---|
| `DELONIX_PROXMOX_URL` | URL base da API, p. ex. `https://pve.example:8006`. Não definida = backend não registado. |
| `DELONIX_PROXMOX_NODE` | Obrigatória. O único nó a que este backend se dirige (tal como `GET /nodes` o nomeia). |
| `DELONIX_PROXMOX_SECRET` | Credencial preferida: nome de um `kind: Secret` com `tokenId`+`tokenSecret` (ou `username`+`password`). |
| `DELONIX_PROXMOX_TOKEN_ID` + `DELONIX_PROXMOX_TOKEN` | API token a partir do ambiente. |
| `DELONIX_PROXMOX_USER` + `DELONIX_PROXMOX_PASSWORD` | Login por password (ticket, reautenticado num 401). |
| `DELONIX_PROXMOX_INSECURE_TLS` | `1`/`true`/`yes` para saltar a verificação do certificado. Opt-in, nunca um recurso. |
| `DELONIX_PROXMOX_BRIDGE` | Bridge por omissão no nó (um `bridge` por VM ganha). |
| `DELONIX_PROXMOX_VLAN` | Tag VLAN por omissão, 1–4094; fora da gama é um erro, não uma NIC sem tag em silêncio. |

Sem configuração, `--backend proxmox` responde que o backend «não está disponível nesta build» e diz
o que definir (*corrido*). O backend é dono do seu armazenamento (`manages_own_storage`), por isso
não é feito nenhum overlay local nem seed NoCloud; `--hostname`/`--ssh-key` vão para o cloud-init do
nó, e `--user-data` é recusado. Desenho e limites: [ADR-0008](../../adr/0008-proxmox-vm-backend.md).
Um backend OpenStack está **só proposto** ([ADR-0039](../../adr/0039-openstack-vm-backend.md)); não
há código para ele.

---

## 3. Imagens

As imagens de VM vivem no `VmImageStore` (`cmd/vmimage.rs`) em `<DELONIX_ROOT>/vm-images/`: um qcow2
mais um registo JSON de metadados (`VmImage`) por imagem. O `delonix image vm ls` lista-as com `TYPE`
(cloud-init / appliance) e `DEFAULTS` (vCPU/memória registados).

### Imagens oficiais

`OFFICIAL_REPOS` em `cmd/vmimage.rs`:

| Chave | Repositório | Conteúdo |
|---|---|---|
| `k8s` | `ghcr.io/angolardevops/delonix-vm-k8s` | nó Kubernetes (kubeadm/kubelet/kubectl + `delonix-cri`) |
| `base` | `ghcr.io/angolardevops/delonix-vm-base` | SO base com o motor `delonix`, sem Kubernetes (p. ex. `ubuntu-24.04`) |
| `appliances` | `ghcr.io/angolardevops/delonix-vm-appliances` | appliances de fabricantes, sem cloud-init |

```bash
delonix vm ls-remote                 # tags of the Kubernetes golden repo
delonix vm ls-remote --no-k8s        # tags of the base repo
delonix vm pull                      # the official Kubernetes golden
delonix vm pull --no-k8s             # the official base image
delonix vm pull <oci-ref> --name <local-name>
```

As imagens são artefactos OCI de blob único; o pull verifica os digests do manifesto e do blob e
repõe os metadados a partir das annotations do manifesto. Os mesmos verbos existem como
`delonix image vm pull/ls-remote/push`.
**Nota:** um `vm create` sem `--disk` e sem imagem local descarrega a imagem dourada oficial, por
isso precisa de rede.

### Construir a receita dourada

`delonix image vm build -t <tag>` sem `VMfile` no contexto corre a receita embutida:

```bash
# Kubernetes node, packages fetched and verified on the HOST, guest offline
delonix image vm build --offline --k8s-version 1.34 -t delonix-vm-k8s:1.34
# no Kubernetes: just the engine, rootless-ready
delonix image vm build --no-k8s --distro debian --debian-release bookworm -t delonix-vm-base:debian-bookworm
```

Flags relevantes (consulta `image vm build --help` para os defaults): `--distro ubuntu|debian|rocky|fedora`,
`--ubuntu-release`, `--debian-release`, `--rocky-release`, `--fedora-release` (release **e** build,
p. ex. `42-1.1`), `--k8s-version`, `--offline`, `--no-k8s`, `--extra-package`, `--extra-run`,
`--cri-bin`, `--delonix-bin`, `--root-password` (sem ela nenhuma conta tem password),
`--node-exporter[=<addr>]`, `--no-compress`. A tua própria receita é um `VMfile` — ver
[08 — Delonixfile e VMfile](08-delonixfile-and-vmfile.md). Os builds de imagem são só amd64
([ADR-0018](../../adr/0018-vm-images-stay-amd64.md)).

### Converter e importar

```bash
delonix vm convert <image-or-path> --to raw|qcow2|vmdk|vdi|vhdx|vhd [-o out] [--compress]
delonix image vm import disk.qcow2 -t opnsense:26.1 --appliance --default-vcpus 2 --default-memory 2G
```

O `vm convert` achata (sem cadeia de backing); `--compress` só é aceite para `qcow2` e `vmdk`. O
`import --appliance` regista `cloud_init: false`: o `vm create` passa então a não anexar **nenhum**
seed e recusa `--hostname`/`--ssh-key`/`--user-data` pelo nome, porque o convidado nunca os leria.
Os scripts de build de appliances vivem em `scripts/appliances/`.

---

## 4. Criar e correr

### `vm create`

```bash
delonix vm create dev --disk delonix-vm-base:ubuntu-24.04 \
  --backend cloud-hypervisor --vcpus 2 --memory 2G \
  --ssh-key @$HOME/.ssh/id_ed25519.pub --hostname dev --wait
```

O que acontece (`cmd/vm.rs` → `delonix_vm::create_with`):

1. A **política do nó** é imposta antes de qualquer imagem ser resolvida (`policy::enforce`).
2. **Resolução do disco** (`resolve_image_ref`): `--url-img` ganha (descarregado, em cache,
   verificado contra `<url>.sha256` quando disponível); senão `--disk` é procurado como nome de
   imagem local, depois como caminho para o qcow2 de uma imagem guardada; senão é usado como caminho
   simples; sem `--disk`, a única imagem dourada local, ou é feito pull da oficial.
3. Os **defaults** dos metadados da imagem preenchem `--vcpus`/`--memory`/`--backend` só onde não
   estão definidos.
4. **Seed**: é gerado um ISO NoCloud (configuração de rede por MAC, hostname, chaves) a menos que
   seja dado `--seed`, que a imagem seja um appliance, ou que o backend seja remoto. `--user-data`
   substitui o user-data gerado.
5. **Backend** escolhido (secção 2); uma verificação de admissão recusa quando o host não tem RAM
   suficiente; um `--namespace` diferente de `default` é recusado em libvirt (a VM vive na `virbr0`,
   fora da SDN do Delonix).
6. **Overlay**: `<root>/vms/<name>.qcow2`, um qcow2 fino sobre a base (`prepare_local_overlay`);
   `--disk-size <GiB>` fá-lo crescer e não pode ser menor do que a base.
7. **Boot**: o `boot` do backend. O `create` é idempotente: uma VM existente e a correr é devolvida
   tal como está.

As imagens publicadas não definem password em nenhuma conta (ver `--root-password` acima), por isso
passa `--ssh-key` se quiseres entrar por SSH.

**`--wait` e o IP previsto.** Em libvirt o IP vem de um lease DHCP real, por isso ter um é evidência
de que o convidado arrancou. Em Cloud Hypervisor o IP é **calculado a partir do MAC** antes de o
convidado correr (`ip_is_predicted`), por isso o `--wait` também sonda o endereço por ARP de dentro
do holder de rede (`delonix_sdn::infra::sdn_reachable`) até ao `--boot-timeout` (por omissão 120 s).
Três desfechos: de pé; «which could not be verified from here» (não foi possível fazer a pergunta à
sonda); «is running but never answered … computed from the MAC, not observed». Usa `vm console` para
ver porquê.

### Verbos do dia-2

| Comando | Notas |
|---|---|
| `delonix vm ls [--namespace <ns>] [--ports] [-o json]` | lista as VMs |
| `delonix describe vm <name>` / `delonix delete vm <name>` | não há `vm describe`/`vm rm`; os verbos genéricos substituem-nos |
| `delonix vm console <name> [-e ^X]` | consola série; desliga com `Ctrl-]` por omissão (`$DELONIX_CONSOLE_ESCAPE`). libvirt: `virsh console` como processo filho; CH: o socket de consola `<root>/vms/<name>.console` |
| `delonix vm ssh <name|ip> [-l user] [-i key] [-- cmd]` | IP a partir do registo; utilizador por omissão `delonix` em imagens cloud-init, `root` em appliances |
| `delonix vm vnc <name>` | só para VMs libvirt criadas com `--vnc` (o CH não tem ecrã) |
| `delonix vm stop <name>` | mantém o disco, o registo e os snapshots. libvirt: o domínio é undefined (os metadados dos snapshots são preservados antes) |
| `delonix vm start <name>` / `restart <name>` | reconstroem o boot a partir do registo e reaproveitam o overlay; `start` numa VM a correr é um no-op, `restart` reinicia sempre |
| `delonix vm pause` / `unpause <name>` | suspendem as vCPUs, a memória fica em RAM; CH e libvirt |
| `delonix vm migrate <name> --host <h> --network <n>` | stop-copy-start por SSH para outro host; tempo de indisponibilidade real (o [ADR-0031](../../adr/0031-live-vm-migration-no-go.md) explica porque é que a migração ao vivo está fora de âmbito) |
| `delonix vm prune` | recupera estado que nenhum registo de VM justifica |

### Snapshots

`delonix vm snapshot create|ls|rm|restore <vm> [<snapshot>]`:

| Backend | VM a correr | VM parada |
|---|---|---|
| libvirt | `create` é um checkpoint de sistema (memória + disco) | só disco; os quatro verbos definem o domínio só durante o comando |
| cloud-hypervisor | só `ls` (`qemu-img info -U`); `create`/`restore`/`rm` **recusados** — o VMM a correr bloqueia o disco e o CH não tem API de snapshot de disco ao vivo | os quatro, via `qemu-img snapshot` |
| proxmox | `create` (com estado da VM), `ls`, `restore` | igual; `rm` não implementado (falha fechado) |

Os snapshots libvirt sobrevivem a `vm stop`/`vm start`: o `undefine --snapshots-metadata` remove só a
contabilidade do libvirt, por isso o `preserve_snapshots` despeja o XML de cada snapshot para
`<root>/vms/<vm>/snapshots/` antes de parar e o `boot` redefine-os (reescrevendo o uuid do domínio,
que muda a cada define).

### Declarativo

O `kind: VirtualMachine` (`compute.delonix.io/v1alpha1`) espelha o `vm create`; um exemplo completo
e anotado é o `examples/vm.yaml`. Um `kind: Workload` com `type: microvm` baixa para uma
`VirtualMachine` com o backend **forçado** a `cloud-hypervisor`; pedir outro backend é um erro
([ADR-0006](../../adr/0006-workload-type-microvm.md)). Os dois foram *corridos* num root de rascunho:

```yaml
apiVersion: compute.delonix.io/v1alpha1
kind: VirtualMachine
metadata: { name: dev }
spec:
  disk: delonix-vm-base:ubuntu-24.04
  resources: { vcpus: 2, memory: 2G }
  cloudInit:
    hostname: dev
    sshKeys: ["@~/.ssh/id_ed25519.pub"]
---
apiVersion: compute.delonix.io/v1alpha1
kind: Workload
metadata: { name: fast }
spec:
  type: microvm
  microvm: { disk: delonix-vm-base:ubuntu-24.04, vcpus: 1, memory: 1G }
```

```text
$ delonix manifest validate -f vm.yaml
stack validate: OK — 2 document(s), all references resolved
$ delonix stack apply -f vm.yaml --dry-run | grep backend
  backend: null
  backend: cloud-hypervisor
```

Aplica com `delonix vm apply -f vm.yaml` ou `delonix stack apply -f vm.yaml` (não executado aqui).

---

## 5. Rede para VMs

| | Cloud Hypervisor | libvirt |
|---|---|---|
| Onde vive a NIC | um tap numa rede Delonix (`--network`, por omissão `ingress`) **dentro do holder de rede** — a mesma SDN dos containers | `virbr0` (nat) ou uma bridge do host, no network namespace do host |
| IP | lease pelo DHCP do holder, determinístico a partir do MAC | DHCP do libvirt; `--ip` reserva um (só nat) |
| Isolamento de namespace | sim (`--namespace`) | recusado |
| VM ↔ container por IP | directo | container → VM funciona através do host; VM → container precisa de uma porta publicada ou de `vm bridge` |

O **`vm reach`** (só leitura, sem privilégio) lista os gateways libvirt e, para cada porta publicada
por um container a correr, se uma VM a consegue alcançar. Uma porta publicada no `127.0.0.1` por
omissão é invisível para as VMs; o comando imprime a republicação exacta, p. ex.
`DELONIX_PUBLISH_ADDR=<gateway> delonix net ingress publish <c> <port>` — alcançável a partir das
VMs dessa rede, não a partir da LAN externa.

O **`vm bridge <network> [--vm-subnet <cidr>] [--apply]`** é **experimental e precisa de root**: um
veth do host para dentro do holder mais rotas, dando às VMs libvirt alcançabilidade IP directa a uma
rede de containers. Sem `--apply` só imprime o plano. O `vm unbridge <network>` desfaz (também um
dry-run sem `--apply`). É a única excepção deliberada ao rootless no código de VM
(`cmd/vmbridge.rs`).

---

## 6. Resolução de problemas

| Sintoma | Causa | Correcção |
|---|---|---|
| `/dev/kvm does not exist` / a VM não arranca | virtualização desligada, ou sem virtualização aninhada | activa VT-x/AMD-V; numa VM, activa a virtualização aninhada |
| `no VM backend available` | nem `cloud-hypervisor` nem `virsh`+`qemu-system-x86_64` no `PATH` | instala um; o `scripts/install.sh` fá-lo |
| VM CH «a correr» mas nunca responde; o overlay fica minúsculo | o firmware não consegue arrancar a imagem (p. ex. só o `hypervisor-fw` instalado) | instala o EDK2 `CLOUDHV.fd` em `/usr/local/share/delonix/`, ou define `DELONIX_HYPERVISOR_FW`, ou usa `--backend libvirt` |
| `vm ls` não mostra IP numa VM libvirt | caiu para `qemu:///session` em user-mode | entra no grupo `libvirt` e volta a fazer login, ou `--net-mode nat` |
| `warning: cannot reach qemu:///system for NAT networking` | não está no grupo `libvirt` | `sudo usermod -aG libvirt $USER`, sessão nova |
| `vm console` em libvirt: «Active console session exists» | uma sessão anterior morreu de forma não limpa | o código actual passa `--force` ao `virsh console`; actualiza o teu binário |
| VM CH recusada antes do boot a mencionar o caminho do socket | `<root>/vms/<name>.sock` excede 108 bytes | `DELONIX_ROOT` ou nome da VM mais curtos |
| `namespace '…' is not enforceable on the 'libvirt' backend` | as VMs libvirt ficam fora da SDN | `--backend cloud-hypervisor`, ou tira o `--namespace` |
| `vm snapshot create` recusado numa VM CH a correr | o CH não tem snapshot de disco ao vivo | `vm stop` primeiro, ou usa libvirt |
| `--hostname`/`--ssh-key` recusados | a imagem é um appliance (`cloud_init: false`) | configura o appliance pela sua própria consola/UI |
| `cloud-localds not found` | falta `cloud-image-utils` | instala-o (necessário para cada `vm create` com cloud-init) |
| `image vm build`: `cp: cannot open '/boot/vmlinuz-…'` | o kernel do host está a 0600 | `install.sh --with-image-build`, ou `sudo chmod 0644 /boot/vmlinuz-*` (baixa uma fronteira) |
| `image vm build`: `Temporary failure resolving …` | o appliance não tem rede: falta o cliente DHCP do host, ou o passt | instala `isc-dhcp-client`; para `--network` usa `XDG_RUNTIME_DIR=/tmp/delonix-run` e um passt actual primeiro no `PATH`; ou constrói com `--offline` |
| um passo pára ~300 s e depois as instalações de pacotes falham | o passt nunca deu lease; o `dhclient` esgotou o tempo | igual ao anterior |
| `--offline belong to the built-in golden recipe …` | flag da receita dourada usada com um VMfile | tira-a; o VMfile descreve o build |
| `` `--network` is for VMfile builds `` | `--network` sem VMfile | usa `--offline` para a receita dourada |
| o build do VMfile enche o `/tmp` | cada estágio é um disco achatado completo em `$TMPDIR` | define `TMPDIR` para um sistema de ficheiros grande; remove os directórios `delonix-vmfile-*` que sobrarem depois de falhas |
| `VM backend 'proxmox' is not available in this build` | o backend não está configurado | define as variáveis `DELONIX_PROXMOX_*` (secção 2) |

---

## 7. Para contribuidores

### Onde vive o código

| Área | Caminho |
|---|---|
| Porta, registo, backends CH e libvirt, `create_with`, snapshots, procura de firmware | `crates/adapters/delonix-vm/src/lib.rs` |
| Geração do seed NoCloud | `crates/adapters/delonix-vm/src/cloudinit.rs` |
| Backend Proxmox | `crates/providers/delonix-proxmox/` |
| CLI `vm`, `kind: VirtualMachine`, `vm reach` | `bins/delonix-runtime-bin/src/cmd/vm.rs` |
| Store de imagens, receita dourada, pull/push/import/convert, dicas de falha | `bins/delonix-runtime-bin/src/cmd/vmimage.rs` |
| VMfile | `bins/delonix-runtime-bin/src/cmd/vmfile.rs` |
| Registo de backends a partir do ambiente | `bins/delonix-runtime-bin/src/cmd/vmbackends.rs` |
| `vm bridge` | `bins/delonix-runtime-bin/src/cmd/vmbridge.rs` |
| Redução do `kind: Workload` | `bins/delonix-runtime-bin/src/cmd/workload.rs` |
| Builds de appliances | `scripts/appliances/` |

### Acrescentar um backend

Lê primeiro o [ADR-0008](../../adr/0008-proxmox-vm-backend.md); é o modelo. Em resumo:

- Implementa o `VmBackend` **no seu próprio crate** se ele falar com uma API remota (o crate do motor
  fica livre de clientes HTTP), colocado no directório da sua camada e listado em
  `scripts/arch_fitness.py` (ver [05 — Arquitectura](05-architecture.md)).
- Regista-o com `register_backend` a partir do processo que conhece a sua configuração, com
  `auto_selectable: false` a menos que seja um backend local, sem configuração, embutido no
  `delonix-vm`.
- Sobrepõe `manages_own_storage`, `destroy`, `resume` e `ip_is_predicted` de forma deliberada: para
  um backend remoto `stop` e `destroy` **não** são a mesma operação, e o `boot` numa VM parada não
  pode criar uma segunda.
- Deixa os verbos não suportados nos seus defaults que falham fechado; recusa os campos de `VmConfig`
  não suportados pelo nome antes de criares seja o que for.
- Não publiques um backend que nunca foi visto a arrancar uma VM. Backends novos e fronteiras de
  hypervisor passam por um ADR ([docs/adr/](../../adr/)).

### Testes

- **Testes unitários puros** — os construtores de argv e de XML são funções puras, por isso
  testam-se sem hypervisor: p. ex. `libvirt_snapshot_argv_uses_flags_not_positional`,
  `snapshot_xml_with_uuid`, `libvirt_domain_xml`, o teste de ordem do firmware acima, os testes do
  registo e de `auto_detect`, e os testes de parser/scaffold em `cmd/vmfile.rs`. Corre
  `cargo test -p delonix-vm` e `cargo test -p delonix-runtime-bin vmfile` (ver
  [02](02-build-and-test.md) para o `protoc` e o directório de target).
- **`scripts/e2e.sh`** — as secções `vm` correm sem hypervisor (listagem, recusas) e, quando
  disponível, exercitam snapshots através de stop/start em libvirt (precisa de `virsh`, `qemu-img` e
  de um `qemu:///system` utilizável) e em Cloud Hypervisor. Isola os dois state roots por omissão; a
  secção CH recusa-se a correr se só o `DELONIX_ROOT` estiver isolado. As secções sem o seu
  hypervisor são reportadas como saltadas, não como passadas.
- **Teste ao vivo do Proxmox** — `crates/providers/delonix-proxmox/tests/live.rs` cria e destrói uma
  VM contra um nó real e é saltado a menos que `DELONIX_PROXMOX_TEST_URL` (mais `_NODE`, `_USER`,
  `_PASS`) esteja definida: `cargo test -p delonix-proxmox --test live -- --nocapture`. Usa um nó
  descartável.
- **Ciclo de vida do provider** — ao mudar o `delonix-vm` ou um provider, prova o ciclo de vida
  inteiro (create, stop, start, snapshot, destroy) **só através da CLI `delonix`**, observando o
  hypervisor em modo só-leitura (`virsh -r`, `GET`s da API), nunca o reparando à mão entre passos.
- **Olha, não adivinhes.** Quando um convidado não sobe, a consola série (`vm console`) ou um
  screenshot do libvirt respondem em segundos ao que as hipóteses levam horas a encontrar — e valida
  com o comando que um utilizador escreveria, não com as flags que são convenientes para depurar (o
  `--vnc` já mascarou uma falha de boot que só acontecia sem dispositivo de vídeo).
