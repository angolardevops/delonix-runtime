# `delonix vm` — guia do administrador, e um laboratório de rede completo

**Para quem administra sistemas e redes.** Cada comando aqui foi executado contra o
binário real e a saída mostrada é a que se obteve. O que não foi possível provar
está identificado como tal, com a razão — um guia que promete o que não verificou é
pior do que um guia incompleto.

- **Versão do motor:** `delonix 0.52.0` (as correcções assinaladas entraram na 0.51.x)
- **Host de validação:** Ubuntu, 32 vCPU, 30 GiB RAM, rootless, libvirt + Cloud
  Hypervisor instalados
- **Data:** 12 de Agosto de 2026

Índice:

1. [Antes de começar](#1-antes-de-começar)
2. [Cheatsheet: o ciclo de vida](#2-cheatsheet-o-ciclo-de-vida)
3. [Cheatsheet: inspecção](#3-cheatsheet-inspecção)
4. [Cheatsheet: acesso](#4-cheatsheet-acesso)
5. [Cheatsheet: imagens](#5-cheatsheet-imagens)
6. [Cheatsheet: instantâneos](#6-cheatsheet-instantâneos)
7. [`kind: VirtualMachine` — a VM declarada](#7-kind-virtualmachine--a-vm-declarada)
8. [`kind: Network` — e a fronteira que engana toda a gente](#8-kind-network--e-a-fronteira-que-engana-toda-a-gente)
9. [O laboratório completo](#9-o-laboratório-completo)
10. [Armadilhas medidas](#10-armadilhas-medidas)

---

## 1. Antes de começar

### O que o host precisa de ter

```bash
delonix system info
```

```
Delonix Runtime 0.51.0
  state root:         /home/walter/.local/share/delonix
  mode:               rootless (daemonless)
  cgroup2 delegated:  yes
  network infra:      up (holder pid 71944)
```

Para VMs, além disto: `libvirt` (com o utilizador no grupo `libvirt`), `qemu-img` e
`cloud-localds`. Sem acesso ao libvirt de sistema, uma VM cai em modo *user* (SLIRP)
e **nunca ganha um endereço visível** — o `vm create` avisa quando isso acontece.

### Os dois motores, e quando cada um serve

| | **libvirt** | **Cloud Hypervisor** |
|---|---|---|
| Onde vive a VM | `virbr*`, na rede do host | SDN do motor, dentro do *holder* |
| Endereço | DHCP do libvirt, **observado** | calculado do MAC, **previsto** |
| Consola gráfica (`--vnc`) | sim | não |
| Instantâneos | sim | não (recusa explícita) |
| Appliances (OPNsense, TrueNAS, Proxmox) | sim | **não arrancam** |
| Imagens `delonix-vm-base:*` | sim | **não arrancam** neste host |

**Para trabalho a sério com VMs, use libvirt.** É o que este guia usa em todo o
laboratório. O Cloud Hypervisor serve microVMs Linux que precisem de estar na mesma
rede dos contentores — e traz a distinção descrita em
[Armadilhas](#em-cloud-hypervisor-o-endereço-é-calculado--não-observado).

```bash
delonix vm default-backend --set libvirt   # fixa a escolha, uma vez
delonix vm default-backend                 # consulta
```

Precedência efectiva, da mais forte para a mais fraca: `--backend` explícito → o
`HYPERVISOR` gravado na imagem → `DELONIX_VM_BACKEND` → `vm default-backend` →
detecção automática.

---

## 2. Cheatsheet: o ciclo de vida

### Criar

```bash
delonix vm create dev \
  --disk delonix-vm-base:ubuntu-24.04 \
  --backend libvirt --net-mode nat \
  --vcpus 1 --memory 1G \
  --hostname dev --ssh-key @$HOME/.ssh/id_ed25519.pub \
  --wait --boot-timeout 180
```

```
Creating VM 'dev'…
 ✓ preparing the overlay disk 💽 0.0s
 ✓ configuring the network 🌐 0.1s
 ✓ defining the domain 📋 0.0s
 ✓ starting the VM ▶ 0.8s
✓ VM 'dev' is up.
info vm 'dev' is up — ip 192.168.122.190
```

**Medido: 11,5 s** do comando ao endereço atribuído; o SSH aceitou ligação poucos
segundos depois, já com o *hostname* aplicado e a chave instalada pelo cloud-init.

O disco base torna-se um *overlay* por VM — a imagem original nunca é escrita, o que
permite dezenas de VMs a partir de uma cópia só.

**Há admissão de recursos**, e é uma boa notícia para quem corre isto a sério:

```
error system call `VM admission` failed: host protection: VM 'lab-nas' asks for
6144 MiB but the host only has 7181 MiB available (reserve 2048 MiB). Stop
VMs/containers, reduce the memory, or lower DELONIX_VM_RESERVE_MIB (at your own risk).
```

A recusa acontece **antes de criar seja o que for** e reserva 2 GiB para o host. Foi
accionada duas vezes durante a montagem do laboratório e evitou levar a máquina a
*swap*.

### Parar, arrancar, reiniciar, remover

```bash
delonix vm stop dev       # 0,4 s — preserva disco e registo
delonix vm start dev      # 0,9 s — mesmo overlay, disco preservado
delonix vm restart dev    # 1,3 s — sempre um arranque real
delonix vm rm dev         # pára e apaga overlay + registo
delonix vm rm dev --force # descarta o registo local mesmo se o libvirt falhar
```

> **`stop` faz `undefine` do domínio libvirt** — para não deixar domínios órfãos.
> Uma VM parada não tem, portanto, domínio a quem o `virsh` possa perguntar seja o
> que for. Os instantâneos **sobrevivem** (desde a v0.51.x — ver
> [Armadilhas](#stop-apagava-os-instantâneos-sem-o-dizer--corrigido-na-v051x)).

`start` reconstrói a VM a partir do registo persistido, mas **só recupera** disco,
vCPU, memória, rede e motor. Kernel próprio, *seed* de cloud-init, volumes 9p,
endereço fixo, VNC e as opções avançadas de libvirt só existem como argumentos do
`vm create` e **não sobrevivem**. Uma VM que dependa deles volta pelo `vm create`
original — que também é idempotente.

---

## 3. Cheatsheet: inspecção

```bash
delonix vm ls          # tabela
delonix vm ls --ports  # + sonda TCP real a 22/6443/10250/80/443
delonix vm ls -o json  # para script
delonix vm status [nome]
delonix image vm describe <nome>...  # detalhe de uma IMAGEM de VM
delonix vm dash --once # KPIs + tabela
```

```
NAME      IMAGE                          BACKEND   VCPUS  MEMORY  STATUS   IP             UPTIME        PORTS OPEN
lab-dns   delonix-vm-base_ubuntu-24.04   libvirt       1  1G      Running  192.168.122.2  Up 2 minutes  22
```

`--ports` faz ligações TCP verdadeiras e por isso está desligado por omissão —
distingue *«a VM tem endereço»* de *«a VM serve alguma coisa»*, que não é a mesma
pergunta.

> A tabela mede as colunas pelo conteúdo e **omite as que ficam totalmente vazias**.
> Se nenhuma VM tiver nenhuma daquelas portas aberta, a coluna `PORTS OPEN`
> desaparece — não é a flag a falhar. Para uma saída de forma estável, use
> `-o json`, onde o campo `ports_open` está sempre presente quando se pede
> `--ports`.

`status` reconcilia com o motor em vez de reler o registo; `describe` mostra o
registo inteiro, incluindo o caminho do *overlay* e o seu tamanho real em disco.

---

## 4. Cheatsheet: acesso

```bash
delonix vm ssh dev                        # o nome basta — o IP vem do registo
delonix vm ssh dev -- systemctl status    # um comando e sai
delonix vm ssh 192.168.122.50 -l root     # directo a um endereço
delonix vm console dev                    # consola série (voltar: Ctrl+])
delonix vm vnc dev                        # só para VMs criadas com --vnc
```

O utilizador por omissão é `delonix` numa imagem cloud-init e `root` num
*appliance* — o comando escolhe sozinho.

`console` precisa de um terminal real; sem ele diz-lo em vez de bloquear:

```
error: Cannot run interactive console without a controlling TTY
```

**A consola é o que salva quando não há rede.** Foi ela que permitiu configurar o
OPNsense do laboratório, que arranca sem endereço utilizável.

---

## 5. Cheatsheet: imagens

```bash
delonix image vm ls          # o que está local
delonix vm ls-remote         # o que há publicado, sem descarregar nada
delonix vm pull <ref>        # trazer
delonix image vm build -t <tag> .  # construir a partir de um VMfile
delonix vm convert <src> --to qcow2|raw|vmdk|vdi|vhdx|vhd
```

`ls-remote` lê só manifestos (poucos KB) e mostra os três repositórios oficiais:
`delonix-vm-k8s` (nós Kubernetes), `delonix-vm-base` (Ubuntu 24.04/26.04, Debian
bookworm, Rocky 9, Fedora 42) e `delonix-vm-appliances` (OPNsense, TrueNAS, os
quatro produtos Proxmox).

**`convert` — os seis formatos foram convertidos e verificados com `file(1)`:**

| `--to` | `file(1)` confirmou |
|---|---|
| `raw` | `data` |
| `vmdk` | `VMware4 disk image` |
| `vdi` | `VirtualBox Disk Image` |
| `vhdx` | `Microsoft Disk Image eXtended` |
| `vhd` | `Microsoft Disk Image, Virtual Server` |

Uma imagem construída aqui é importável por VMware, VirtualBox e Hyper-V — sem que
este motor tenha um backend para nenhum deles.

### `image vm build` — imagem a partir de um `VMfile`

```bash
delonix image vm init --vmfile   # gera VMfile + cloud-init/user-data.yaml
delonix image vm build -t app:1.0 .
```

```
[1/1] stage-1: FROM ubuntu:24.04
FROM ubuntu:24.04: official delonix base delonix-vm-base:ubuntu-24.04 (local)
 ✓ preparing the disk 📦 1.7s
 ✓ 2 steps inside the guest 🔧 2.5s
 ✓ compacting the image 🗜 7.2s
```

**Medido: 12 s.** O `VMfile` tem gramática de Dockerfile (`FROM`, `RUN`, `COPY`,
`ENV`) mais o que só faz sentido numa VM: `SIZE`, `VCPUS`, `MEMORY`, `HYPERVISOR`,
`SSHKEY`, `USER`. Suporta multi-estágio, onde cada estágio é um disco inteiro.

**`RUN` corre sem rede por omissão**, de propósito: um build que vai à internet dá
uma imagem diferente conforme o dia em que correu. Para instalar pacotes há
`--network` — que **neste host não funciona** (ver
[Armadilhas](#vm-build---network-esbarra-no-host)).

---

## 6. Cheatsheet: instantâneos

Os quatro verbos vivem no grupo `vm snapshot`, e funcionam com a VM **a correr ou
parada**:

```bash
delonix vm snapshot create  dev antes-da-actualizacao
delonix vm snapshot ls      dev
delonix vm snapshot restore dev antes-da-actualizacao
delonix vm snapshot rm      dev antes-da-actualizacao
```

> Até à v0.51.0 estes eram três comandos planos (`vm snapshot`, `vm snapshots`,
> `vm restore`), e não havia forma de apagar um instantâneo pela CLI. A forma
> antiga foi removida sem alias: falha com `unrecognized subcommand`.

Com a VM **a correr** é um ponto de restauro de sistema: memória *e* disco. Com ela
**parada** é só do disco — e a VM continua parada. Restaurar um instantâneo tirado a
correr traz a VM de volta a correr, memória incluída, e o comando di-lo.

No **Cloud Hypervisor** os instantâneos são do disco e exigem a VM parada: o vmm a
correr segura o `qcow2` em exclusivo e o CH não tem API de instantâneo de disco ao
vivo. Os verbos que escrevem recusam com erro que diz o que fazer; o `ls` responde
sempre.

Provado escrevendo um ficheiro depois do instantâneo e restaurando:

```
--- restore para base-limpa
real 0m1.394s
--- a marca ainda existe?
cat: /root/marca.txt: No such file or directory
AUSENTE (restore reverteu)
```

**1,4 s** para reverter uma VM inteira.

---

## 7. `kind: VirtualMachine` — a VM declarada

```yaml
apiVersion: delonix.io/v1
kind: VirtualMachine
metadata:
  name: dev
spec:
  disk: delonix-vm-base:ubuntu-24.04   # nome de imagem OU caminho para um qcow2
  vcpus: 1
  memory: 1G
  backend: libvirt
  netMode: nat
  hostname: dev
  sshKeys:
    - "@/home/walter/.ssh/id_ed25519.pub"
```

```bash
delonix stack validate -f vm.yaml            # referências resolvem?
delonix stack apply -f vm.yaml --dry-run     # o spec com todos os defaults
delonix stack apply -f vm.yaml               # aplicar
delonix stack plan -f vm.yaml                # o que mudaria
delonix explain Vm                           # todos os campos, gerados do código
```

O ciclo declarativo foi exercitado de ponta a ponta:

```
$ delonix stack apply -f vm.yaml            # 2.ª vez
vm/lab-mani: ensured                        # idempotente

$ delonix stack plan -f vm.yaml --detailed-exitcode
  =   Vm/lab-mani
Summary: 1 unchanged                        # rc=0

# memória 1G → 2G no ficheiro
$ delonix stack plan -f vm.yaml
  -/+ Vm/lab-mani  — does not converge live: memory
        memory: 1G → 2G
Summary: 1 to replace                       # rc=2 com --detailed-exitcode

$ delonix stack apply -f vm.yaml
  ✗ Vm/lab-mani: does not converge live: memory — pass `--replace Vm/lab-mani`
error: stack apply refused: 1 resource(s) need an explicit decision (nothing was changed)

$ delonix stack apply -f vm.yaml --replace Vm/lab-mani
Vm/lab-mani: recreating                     # e a memória passou mesmo a 2G
```

Repare em **«nothing was changed»**: a recusa acontece antes da primeira alteração,
não a meio. É o que permite pôr `stack plan --detailed-exitcode` num *pipeline* como
detector de desvio de configuração.

### O que o reconciliador compara — e o que não

O motor **diz-lho**, em vez de deixar descobrir:

```
Vm 'lab-nas': Converged=False (FieldsNotCompared) — declared but NOT applied to an
existing VM: extraNics, libvirtXmlOverlay, netMode — the reconciler compares only
disk, vcpus, memory, network, backend.
```

Mudar placas de rede, XML injectado ou modo de rede num manifesto **não altera uma
VM que já existe**. Ou se recria (`--replace`, que deita fora o disco), ou se usa
`vm create`. Vale a pena repetir porque é silencioso na prática: o `apply` diz
`ensured` e a VM continua como estava.

### Campos que valem a pena conhecer

| Campo | Para quê |
|---|---|
| `extraNics[]` | placas adicionais — `type` (`network`/`bridge`/`user`), `source`, `mac` |
| `extraDisks[]` | discos além do overlay |
| `volumes[]` | montar um `kind: Volume` dentro da VM, por virtio-9p |
| `ip` | reserva DHCP na rede libvirt (só `netMode: nat`) |
| `userData` | cloud-init próprio — **substitui** o gerado |
| `vnc`, `video`, `tpm`, `cpuModel`, `cpuTopology`, `bootOrder`, `machine` | equivalentes tipados do XML libvirt |
| `libvirtXmlOverlay[]` | XML cru antes de `</devices>` |
| `libvirtXml` | substitui o `<domain>` inteiro |

Os dois últimos **não são validados** e podem nomear qualquer caminho do host — só
para manifestos de confiança. O laboratório usa `libvirtXmlOverlay` uma vez, e a
[razão](#o-nas-precisa-de-um-disco-com-número-de-série) é instrutiva.

**`mac` fixo é mais do que uma conveniência.** Dentro do sistema, `enp1s0` e `enp2s0`
dependem da ordem em que o PCI enumera as placas, e isso não é um contrato. Todas as
VMs do laboratório casam a configuração de rede por MAC:

```yaml
network:
  version: 2
  ethernets:
    lab0:
      match: { macaddress: "52:54:00:5a:b0:10" }
      set-name: lab0
      addresses: [10.50.0.10/24]
```

---

## 8. `kind: Network` — e a fronteira que engana toda a gente

```yaml
apiVersion: delonix.io/v1
kind: Network
metadata:
  name: lab-net
spec:
  driver: bridge
  subnet: 10.233.0.0/16
```

```
$ delonix network describe lab-net
Name:           lab-net
Driver:         bridge
Bridge:         dlxne9623e
Subnet:         10.233.0.0/16
Gateway:        10.233.0.1
```

### Uma rede `kind: Network` **não** é uma rede de VM

Esta é a confusão que mais tempo custa, por isso vale medi-la:

```
$ delonix network ls
NAME        DRIVER   BRIDGE       SUBNET
lab-net     bridge   dlxne9623e   10.233.0.0/16

$ virsh -c qemu:///system net-list --all
 Name      State    Autostart
 default   active   yes
 labnet    active   yes            ← a rede que as VMs libvirt usam
                                     (lab-net NÃO aparece)
```

Usar uma na outra falha, e a mensagem do delonix não ajuda:

```
$ delonix vm create x --backend libvirt --net-mode nat --bridge lab-net
error system call `vm` failed: failed to start the libvirt domain (KVM/permissions/image?)

$ virsh start x     # a causa verdadeira
error: Network not found: no network with matching name 'lab-net'
```

**A regra, em duas linhas:**

- `delonix network create` / `kind: Network` → redes de **contentores** (SDN do
  motor, rootless, dentro do *holder*).
- VMs **libvirt** usam redes **libvirt**, criadas com `virsh net-define`. O
  `--bridge`/`spec.bridge` nomeia uma delas.

Uma VM **Cloud Hypervisor** é a excepção: entra mesmo na SDN dos contentores
(`--network lab-net` deu-lhe `10.233.254.141`). Só que esses endereços vivem dentro
do *holder* e não são alcançáveis a partir do host — só de outro contentor da mesma
rede.

Para atravessar a fronteira há `delonix vm bridge`, que **exige root** e diz o que
vai fazer antes de o fazer:

```
$ delonix vm bridge lab-net          # sem --apply: só imprime o plano
warning DRY-RUN — the plan below needs root. Review it, then re-run with `--apply`:
  ip link add vbh72a57c1d type veth peer name vbs72a57c1d
  ip link set vbs72a57c1d netns 71944
  nsenter -t 71944 -n -- ip link set vbs72a57c1d master dlxn0536623e up
  ip addr add 10.233.255.254/16 dev vbh72a57c1d
  sysctl -w net.ipv4.ip_forward=1
  iptables -I FORWARD -s 192.168.122.0/24 -d 10.233.0.0/16 -j ACCEPT
  ...
```

`delonix vm unbridge <rede>` imprime o desfazer, regra a regra. Um comando
privilegiado que mostra o plano e sabe reverter é raro; use o *dry-run* como
documentação.

Sem privilégio, `delonix vm reach` responde à pergunta prática — que portas
publicadas é que as VMs alcançam de facto:

```
$ delonix vm reach
VM network gateway(s): 192.168.122.1, 10.10.100.1
warning Published on loopback only — NOT reachable from VMs:
CONTAINER             HOST PORT   BOUND TO
kaeso-db18            5433        127.0.0.1 (host only)
  fix: DELONIX_PUBLISH_ADDR=192.168.122.1 delonix net ingress publish <c> <port>
```

---

## 9. O laboratório completo

Uma rede de empresa em miniatura: DNS autoritativo, DHCP, autenticação de
utilizadores, um posto de trabalho, armazenamento em NAS e uma firewall.

Ficheiros em [`examples/lab-rede/`](../examples/lab-rede/). Tudo o que se segue foi
executado; as saídas são as reais.

### Topologia

```
                    ┌───────────────────────── labnet — 10.50.0.0/24 ──┐
                    │  (rede libvirt isolada, SEM DHCP próprio)         │
                    │                                                  │
  ┌──────────┐      │  ┌────────────┐  ┌────────────┐  ┌────────────┐  │
  │ internet │──NAT─┼──│ lab-dns    │  │ lab-dhcp   │  │ lab-samba  │  │
  └──────────┘ default│ 10.50.0.10  │  │ 10.50.0.11 │  │ 10.50.0.12 │  │
                    │  │ BIND 9     │  │ ISC DHCP   │  │ Samba      │  │
                    │  └────────────┘  └────────────┘  └────────────┘  │
                    │                                                  │
                    │  ┌────────────┐  ┌────────────┐  ┌────────────┐  │
                    │  │ lab-cli    │  │ lab-nas    │  │ lab-fw     │  │
                    │  │ 10.50.0.50 │  │ 10.50.0.20 │  │10.50.0.254 │  │
                    │  │ (por DHCP) │  │ TrueNAS    │  │ OPNsense   │  │
                    │  └────────────┘  └────────────┘  └────────────┘  │
                    └──────────────────────────────────────────────────┘
```

Cada VM tem **duas placas**, e a razão é prática: a `default` dá saída para a
internet, que o cloud-init precisa para instalar os pacotes no primeiro arranque; a
`labnet` é a LAN do laboratório, isolada, para o `lab-dhcp` poder servir sem
competir com o DHCP do libvirt. **Dois servidores de DHCP no mesmo domínio de
difusão entregam a resposta de quem chegar primeiro** — é das avarias mais difíceis
de diagnosticar, e vale a pena não a construir de propósito.

| Nome | Papel | Endereço | Memória |
|---|---|---|---|
| `lab-dns` | BIND 9, zona `lab.ngola` | 10.50.0.10 | 1 GiB |
| `lab-dhcp` | ISC DHCP | 10.50.0.11 | 1 GiB |
| `lab-samba` | Samba, autenticação e partilha | 10.50.0.12 | 1 GiB |
| `lab-cli` | posto de trabalho | 10.50.0.50 (por DHCP) | 1 GiB |
| `lab-nas` | TrueNAS SCALE | 10.50.0.20 | 4 GiB |
| `lab-fw` | OPNsense | 10.50.0.254 | 2 GiB |

### Passo 0 — a rede do laboratório

O `kind: Network` não serve aqui (secção 8): é preciso uma rede **libvirt**, e
**sem DHCP**, para o `lab-dhcp` ser o único a servir.

```bash
cat > /tmp/labnet.xml <<'EOF'
<network>
  <name>labnet</name>
  <bridge name='virbrlab' stp='on' delay='0'/>
  <ip address='10.50.0.1' netmask='255.255.255.0'>
  </ip>
</network>
EOF
virsh -c qemu:///system net-define /tmp/labnet.xml
virsh -c qemu:///system net-start labnet
virsh -c qemu:///system net-autostart labnet
```

Sem `<forward>` a rede é **isolada**: as VMs falam entre si e com o host, e não saem
por esta placa. Sem bloco `<dhcp>` o libvirt não serve endereços nela.

> O host fica **membro desta LAN** — tem `10.50.0.1` na `virbrlab`. É cómodo para
> diagnosticar e convém saber, porque significa que a firewall do laboratório não
> está entre o host e as VMs.

### Passo 1 — preparar e aplicar

Os manifestos trazem `SSH_KEY_PLACEHOLDER`, para poderem ser partilhados sem a chave
de ninguém lá dentro:

```bash
cd examples/lab-rede
mkdir -p /tmp/labrun/cloud-init
KEY=$(cat ~/.ssh/id_ed25519.pub)
for n in dns dhcp samba cliente; do
  sed "s|SSH_KEY_PLACEHOLDER|$KEY|" cloud-init/$n.yaml > /tmp/labrun/cloud-init/$n.yaml
done
for f in 10-dns 20-dhcp 30-samba 40-cliente; do
  sed "s|./cloud-init/|/tmp/labrun/cloud-init/|" $f.yaml > /tmp/labrun/$f.yaml
done

delonix stack apply -f /tmp/labrun/10-dns.yaml
delonix stack apply -f /tmp/labrun/20-dhcp.yaml
delonix stack apply -f /tmp/labrun/30-samba.yaml
delonix stack apply -f /tmp/labrun/40-cliente.yaml
```

Cada `apply` responde em cerca de **1 s**; o cloud-init leva depois **2 a 4 minutos**
a instalar e configurar os serviços. Acompanhar com:

```bash
delonix vm ssh lab-dns -- cloud-init status
```

> **Não use `--prune` com estes ficheiros.** O nome do *stack* vem do **directório**
> do manifesto, portanto os seis ficheiros são o mesmo *stack*: um `apply --prune`
> de um deles vê as outras cinco VMs como órfãs e apaga-as. Ou se junta tudo num
> ficheiro multi-documento, ou se usa `--name`, ou não se usa `--prune`.

### Passo 2 — DNS

```
$ delonix vm ssh lab-dns -- 'ip -4 -o addr show | grep -v " lo "; systemctl is-active named'
2: enp1s0  inet 192.168.122.2/24 ...      ← saída para a internet
3: lab0    inet 10.50.0.10/24 ...         ← a LAN, por MAC
active

$ dig +short @10.50.0.10 nas.lab.ngola A       →  10.50.0.20
$ dig +short @10.50.0.10 -x 10.50.0.20         →  nas.lab.ngola.
$ dig +short @10.50.0.10 one.one.one.one A     →  1.1.1.1   (recursão)
```

Zona directa, zona inversa e reencaminhamento. O BIND escuta **só** em `127.0.0.1` e
`10.50.0.10` e só aceita consultas de `10.50.0.0/24` — um resolvedor aberto na
internet é dos contributos mais fáceis de dar, sem querer, a um ataque de
amplificação.

### Passo 3 — DHCP

```
$ delonix vm ssh lab-dhcp -- 'systemctl is-active isc-dhcp-server; sudo ss -ulnp | grep -c ":67"'
active
1
```

O `INTERFACESv4="lab0"` limita o serviço à placa do laboratório. A reserva por MAC
dá ao cliente sempre o mesmo endereço, o que mantém estáveis os registos de DNS e as
regras de firewall.

### Passo 4 — Samba

```
$ delonix vm ssh lab-samba -- 'systemctl is-active smbd; sudo pdbedit -L'
active
ana:1001:
```

Modo *standalone*, não controlador de domínio Active Directory — um AD DC exige ser
também o DNS da zona, e aqui o DNS é o `lab-dns`. Trocar isso obriga a desligar o
BIND e entregar a zona ao Samba: uma decisão de arquitectura que merece ser tomada de
propósito, e não por arrasto.

### Passo 5 — o cliente prova tudo junto

```
$ delonix vm ssh lab-cli -- '...'

===== 1. DHCP: o endereço veio do lab-dhcp?
10.50.0.50/24                          ← exactamente a reserva por MAC
ADDRESS=192.168.122.14                 ← a outra placa, servida pelo libvirt
ADDRESS=10.50.0.50

===== 2. DNS: o lab-dns resolve a zona?
10.50.0.12
dc.lab.ngola.

===== 3. SAMBA: autenticação real
	Sharename       Type      Comment
	publico         Disk
	IPC$            IPC       IPC Service (Samba do lab.ngola)
```

E escrita autenticada de facto, ponta a ponta:

```
$ smbclient //10.50.0.12/publico -U ana%... -c "put /tmp/prova.txt prova.txt; ls"
putting file /tmp/prova.txt as \prova.txt (56.6 kb/s)
  prova.txt    A    58   Wed Aug 12 15:13:52 2026

# no servidor:
-rwxr--r-- 1 ana labusers 58 Aug 12 15:13 prova.txt
ficheiro criado pelo lab-cli em 2026-08-12T15:13:52+00:00
```

O ficheiro chegou com dono `ana` e grupo `labusers` — a autenticação foi mesmo
exercida, não contornada por acesso de convidado (`map to guest = never`).

### Passo 6 — o NAS

```bash
# o disco de dados tem de existir antes
mkdir -p ~/.local/share/delonix/vms/labdata
qemu-img create -f qcow2 ~/.local/share/delonix/vms/labdata/nas-dados.qcow2 10G

delonix stack apply -f 50-nas.yaml
```

O TrueNAS serviu a interface **80 s** depois do arranque. A API responde com a conta
`truenas_admin` (não `root`):

```
$ curl -sk -u truenas_admin:... https://<nas>/api/v2.0/system/info
version: 25.10.5   hostname: truenas   memória: 5.8 GiB   cores: 2
```

#### O NAS precisa de um disco com número de série

A primeira tentativa de criar a *pool* falhou:

```
[EINVAL] pool_create.topology: Disks have duplicate serial numbers: None (vda, vdb).
```

Os discos virtio saem sem número de série e o TrueNAS recusa juntar dois discos
indistinguíveis. O `extraDisks` não tem campo para o definir — é o caso para que o
escape-hatch de XML existe:

```yaml
libvirtXmlOverlay:
  - |
    <disk type='file' device='disk'>
      <driver name='qemu' type='qcow2'/>
      <source file='.../nas-dados.qcow2'/>
      <target dev='vdb' bus='virtio'/>
      <serial>LABDATA01</serial>
    </disk>
```

Criar a *pool* é um passo único, feito na interface web ou pela API:

```bash
curl -sk -u truenas_admin:<pass> -H 'Content-Type: application/json' \
  -X POST https://<nas>/api/v2.0/pool \
  -d '{"name":"labpool","topology":{"data":[{"type":"STRIPE","disks":["vdb"]}]}}'
# → labpool  ONLINE  9.5 GiB
```

#### Provisionar armazenamento a partir do manifesto

A partir daqui o `kind: Volume` faz o resto **sem ninguém abrir a interface web**:

```yaml
kind: Volume
metadata: { name: lab-dados }
spec:
  provision:
    truenas:
      url: https://<nas>
      username: truenas_admin
      passwordSecret: nas-cred
      insecureTLS: true
      dataset: labpool/lab-dados
      quota: 2G
      owner: { uid: 1000, gid: 1000, mode: "0770" }
      share:
        networks: [10.50.0.0/24, 192.168.122.0/24]
        maprootUser: root
```

```
$ delonix stack apply -f 60-storage.yaml
volume/lab-dados: provisioned 192.168.122.116:/mnt/labpool/lab-dados (quota 2.00 GiB, 2.00 GiB free)
```

Confirmado do lado da NAS: `quota: 2 GiB`, export NFS activo com exactamente as duas
redes declaradas. E montado a partir do cliente:

```
$ delonix vm ssh lab-cli -- 'sudo mount -t nfs <nas>:/mnt/labpool/lab-dados /mnt/nas; df -h /mnt/nas'
<nas>:/mnt/labpool/lab-dados  2.0G  0  2.0G  0%  /mnt/nas    ← a quota é real

$ ... | sudo tee /mnt/nas/do-cliente.txt
# e no dataset ZFS da NAS:
do-cliente.txt  58B  uid=0     used: 96 KiB   quota: 2 GiB
```

Três notas que valem o tempo que custaram:

1. **O `apply` termina com erro no host, e o provisionamento está feito.** O
   `mount -t nfs` local precisa de `CAP_SYS_ADMIN`, que uma sessão *rootless* não
   tem. O relatório do provisionamento sai **antes** do erro, de propósito — um
   `apply` que morresse calado deixaria o administrador sem saber que já existe um
   dataset, uma quota e um export na NAS.
2. **Sem `nfs-common` no cliente, o `mount` falha e a escrita segue para o disco
   local**, com ar de sucesso. Foi o que aconteceu na primeira tentativa. O
   manifesto do cliente instala-o.
3. **NFS faz *root-squash*.** Sem `owner:` e `maprootUser`, monta-se e não se
   escreve. O `maprootUser: root` é o `no_root_squash` clássico: cómodo num
   laboratório, e uma cedência real fora dele.

**Os dados sobrevivem à recriação da VM.** A dada altura foi preciso reduzir a
memória do NAS, o que obriga a `--replace` — que deita fora o disco do sistema. A
*pool* estava no disco de dados, que é externo:

```
$ curl ... /api/v2.0/pool/import_find
[{"name": "labpool", "guid": "10428492264621109666", "status": "ONLINE"}]
```

Importada e o `stack apply` do armazenamento voltou a correr tal e qual. É um bom
argumento para manter os dados fora do disco do sistema.

### Passo 7 — a firewall

```bash
delonix stack apply -f 70-firewall.yaml
```

**A ordem das placas é o que decide os papéis**, e é fácil trocá-la sem dar por isso:
o OPNsense chama LAN à primeira (`vtnet0`) e WAN à segunda (`vtnet1`). Por isso, ao
contrário das outras VMs, aqui a placa primária é a `labnet`:

```yaml
netMode: nat
bridge: labnet          # primária → vtnet0 → LAN
extraNics:
  - { type: network, source: default }   # segunda → vtnet1 → WAN
```

Uma firewall com a WAN em `vtnet0` fica com as regras invertidas — a proteger a
internet do laboratório.

O OPNsense é um *appliance*: não corre cloud-init, e o motor **recusa**
`hostname`/`sshKeys` em vez de os aceitar e ignorar. Configura-se pela consola:

```
$ delonix vm console lab-fw
 LAN (vtnet0)    -> v4: 192.168.1.1/24        ← o valor de fábrica
 WAN (vtnet1)    -> v4/DHCP4: 192.168.122.5/24

  2) Set interface IP address
```

Opção `2` → LAN → sem DHCP → `10.50.0.254` → `24` → sem gateway → **não** activar o
servidor DHCP (quem serve DHCP neste laboratório é o `lab-dhcp`).

Feito isto, a partir da LAN:

```
$ delonix vm ssh lab-dns -- 'ping -c3 10.50.0.254; curl -sk -o /dev/null -w "%{http_code}" https://10.50.0.254/'
3 packets transmitted, 3 received, 0% packet loss
rtt min/avg/max/mdev = 0.276/0.306/0.366/0.042 ms
200
```

E pela WAN, como deve ser:

```
$ curl -sk --max-time 8 https://192.168.122.5/     → HTTP 000
```

A administração responde na LAN e não responde na WAN. Que é, afinal, o que se pede
a uma firewall.

### Executar por fases

O laboratório inteiro pede cerca de **10 GiB** de RAM. Num host partilhado, arranque
por grupos — a admissão de recursos do motor recusa antes de criar seja o que for,
mas mais vale não chegar lá:

```bash
# `vm start` leva UMA VM de cada vez — `start a b` dá "unexpected argument".
# Fase A — infra-estrutura de rede (2 GiB)
for v in lab-dns lab-dhcp; do delonix vm start $v; done

# Fase B — identidade e posto de trabalho (+2 GiB)
for v in lab-samba lab-cli; do delonix vm start $v; done

# Fase C — armazenamento (4 GiB)
delonix vm stop lab-samba && delonix vm start lab-nas

# Fase D — perímetro (2 GiB)
delonix vm start lab-fw

delonix vm ls                            # o que está de pé
delonix vm dash                          # painel interactivo
```

Para desmontar tudo:

```bash
for v in lab-dns lab-dhcp lab-samba lab-cli lab-nas lab-fw; do delonix vm rm $v; done
virsh -c qemu:///system net-destroy labnet
virsh -c qemu:///system net-undefine labnet
```

---

## 10. Armadilhas medidas

Cada uma custou tempo real durante a montagem deste laboratório.

### `stop` apagava os instantâneos, sem o dizer — corrigido na v0.51.x

Era isto, e foi medido aqui — **os comandos abaixo são os da versão antiga** (hoje
são `vm snapshot create`/`ls`/`restore`), ficam só para registo:

```
$ delonix vm snapshot lab-probe base-limpa
base-limpa
$ delonix vm stop lab-probe && delonix vm start lab-probe
$ delonix vm snapshots lab-probe
                                  ← vazio, rc=0
$ delonix vm restore lab-probe base-limpa
error: Domain snapshot not found: no domain snapshot with matching name 'base-limpa'
```

`stop` faz `virsh undefine --managed-save --snapshots-metadata --nvram` para não
deixar domínios órfãos, e o `--snapshots-metadata` levava a contabilidade dos
instantâneos. A medição que resolveu isto: **o `qemu-img snapshot -l` mostrava-os
intactos no `qcow2` depois do `stop`** — o `undefine` não apaga o instantâneo, apaga
só o que aponta para ele.

Hoje o `stop` guarda o `snapshot-dumpxml` de cada um em `vms/<vm>/snapshots/` e o
arranque seguinte devolve-os ao libvirt. **Um instantâneo sobrevive a `stop`/`start`**,
e o `vm snapshot ls` de uma VM parada lista-os. O que fica da armadilha é a regra:
nunca perguntar ao libvirt por uma VM parada — ele só conhece domínios definidos.

### Em Cloud Hypervisor, o endereço é calculado — não observado

Nesse motor o endereço vem de aritmética sobre o MAC, antes de o sistema arrancar
(é o que permite pôr a VM debaixo do isolamento de *namespaces* antes de ela
existir). Em libvirt vem de um *lease* real, que só é entregue a um convidado que
arrancou o suficiente para o pedir.

A diferença importa quando o convidado **não** arranca. Até à v0.51.0 o `--wait`
devolvia em 62 ms a anunciar `is up` sobre uma VM cujo firmware falhava antes do
kernel. Hoje espera e diz o que sabe:

```
$ delonix vm create prova-ch --backend cloud-hypervisor --network lab-net \
      --wait --boot-timeout 60
prova-ch
warning vm 'prova-ch' is running but never answered at 10.233.254.240 — that
address is computed from the MAC, not observed, so it exists whether or not the
guest booted; `delonix vm console prova-ch` to watch the boot
real 1m0.322s
```

O aviso é a informação toda: o processo corre, o endereço existe, e **ninguém
respondeu nele**. A VM daquele teste tinha o *overlay* com 448 KiB — não escreveu um
byte — e `ip neigh` a dar `FAILED` a partir de um contentor da mesma rede.

**Um `--wait` que esgota o tempo continua a sair 0**, portanto num *script* verifique
o aviso ou sonde por si (`delonix vm ls --ports`, um `ssh`).

Sobre a imagem, com um controlo: a golden `delonix-vm-k8s:1.34` **arranca** o kernel
no mesmo motor, rede e firmware — logo a rede está boa e o que varia é a imagem. As
`delonix-vm-base:*` não arrancam com o `hypervisor-fw` que o instalador coloca.
**Para VMs em Cloud Hypervisor, use a golden.**

### `image vm build --network` esbarra no host

```
virt-customize: error: libguestfs error: passt exited with status 1
```

O erro traz o remédio, e é meio caminho:

```bash
mkdir -p /tmp/delonix-run && chmod 700 /tmp/delonix-run
XDG_RUNTIME_DIR=/tmp/delonix-run delonix image vm build --network -t img:1.0 .
```

Passa o problema de AppArmor, mas neste host o `passt` continua sem dar resolução de
nomes ao *appliance*:

```
W: Failed to fetch http://archive.ubuntu.com/... Temporary failure resolving
E: Unable to locate package bind9-utils
```

**Alternativa usada no laboratório, e melhor por si só:** instalar pelo cloud-init no
primeiro arranque. A VM já tem a rede NAT do libvirt, com internet e DNS a funcionar
(verificado: `HTTP/1.1 200 OK` de `archive.ubuntu.com`), e a configuração fica
declarada no manifesto, à vista de quem o ler.

### `vm pull` numa ligação que cai a meio *(corrigido na v0.51.0)*

Durante a montagem deste laboratório, um `vm pull` de 276 MiB morreu assim:

```
$ time delonix vm pull ghcr.io/angolardevops/delonix-vm-base:debian-bookworm
error registry error: blob read: request or response body error
real 8m19s
```

Não era o relógio — o tecto é de 4 horas desde a v0.47.1 — era a ligação a cair. O
que tornava isto grave é que **não havia retomada**: a tentativa seguinte recomeçava
no byte zero, portanto numa ligação má a imagem podia nunca acabar, por mais vezes
que se tentasse.

Hoje o descarregamento de um blob **retoma com `Range: bytes=<n>-`**, com 5
tentativas e uma linha por retomada. Costurar dois intervalos é seguro pela mesma
razão de sempre: o digest do manifesto é verificado no fim, portanto ou os bytes
batem certo ou são descartados. Coberto por
`blob_retoma_uma_ligacao_cortada_a_meio` e por
`blob_recomeca_quando_o_servidor_ignora_o_range`.

**A retomada é dentro da mesma invocação** — cobre a ligação a cair, não o processo a
ser morto. Matar um `vm pull` a meio e voltar a lançá-lo continua a começar do
princípio.

> Uma nota de honestidade sobre a medição original: quando isto falhou, a ligação
> deste host ao `ghcr.io` dava **416 KB/s**; horas depois, a mesma medição deu
> **9,7 MB/s** e a imagem inteira desceu em menos de 75 s. O defeito era real e a
> correcção é a certa, mas os 416 KB/s eram um mau momento da rede — não uma
> característica do host.

`delonix vm ls-remote` funciona bem mesmo em ligações fracas, porque lê só
manifestos. Se a rede for genuinamente má, traga as imagens por outro meio e
registe-as com `delonix image vm import`.

### Um `apply` por ficheiro, um *stack* por directório

O nome do *stack* vem do **directório** do manifesto (a menos que haja um `kind:
Stack` com nome, ou `--name`). Os seis ficheiros do laboratório são, para o motor, o
mesmo *stack*:

```
$ delonix stack plan -f 50-nas.yaml
  -/+ Vm/lab-nas    — does not converge live: memory
  -   Vm/lab-samba  — no longer declared in the manifest
  -   Vm/lab-dns    — no longer declared in the manifest
  -   Vm/lab-cli    — no longer declared in the manifest
```

Sem `--prune` nada é removido — e é por isso que essa opção existe e nunca é o
comportamento por omissão. Mas o plano mostra bem o risco.

### `--replace` deita fora o disco

Está escrito na recusa (`--replace, which discards the disk`) e vale a pena repetir,
porque a alteração que o desencadeia costuma ser inócua — mudar `memory: 6G` para
`4G` recria a VM e perde o disco do sistema. Dados importantes ficam num disco
separado (foi o que salvou a *pool* do NAS) ou num volume.

### Um `apply` que diz `ensured` pode não ter mudado nada

Mudar `extraNics`, `libvirtXmlOverlay` ou `netMode` num manifesto de uma VM **que já
existe** não tem efeito: o reconciliador compara apenas `disk`, `vcpus`, `memory`,
`network` e `backend`. O motor diz-lo no bloco `Conditions` do fim — que é fácil não
ler quando a linha anterior diz `ensured`.

### `delonix vm ssh` logo a seguir a `start` recusa ligação

```
ssh: connect to host 192.168.122.14 port 22: Connection refused
```

O `vm start` devolve em menos de um segundo — define o domínio e arranca-o. O sistema
lá dentro leva **30 a 60 s** a ter o SSH a aceitar ligações. Não é avaria; num
*script*, espere pela condição em vez de por um tempo fixo:

```bash
until delonix vm ssh lab-dns -- true 2>/dev/null; do sleep 5; done
```

### Nomeie a imagem, em vez de apontar ao ficheiro

Durante a montagem deste laboratório, um *appliance* declarado pelo **caminho** do
`qcow2` escapava à recusa de cloud-init que o mesmo *appliance* declarado pelo
**nome** recebia — e levava um CD-ROM de *seed* que o convidado nunca lê. Hoje as
duas formas são recusadas do mesmo modo:

```
# por nome                    spec.disk: opnsense:26.1
# pelo caminho do ficheiro    spec.disk: /.../opnsense_26.1.qcow2
error invalid argument: hostname, sshKeys: this image is an appliance and does not
run cloud-init, so these would be silently ignored — configure it on first boot
(console or web UI), or pass your own `--seed` if you know the guest reads one
```

Continua a valer a pena **nomear a imagem**: é ao nome que estão associados os
metadados que o motor usa sem lhos pedir — se é *appliance*, e que vCPU, memória e
motor a imagem recomenda (o `resolve_vm_defaults`). `delonix image vm ls` mostra os
nomes disponíveis.

---

## Referência rápida

```bash
# ciclo de vida
delonix vm create <n> --disk <img> --backend libvirt --net-mode nat \
                      --vcpus N --memory NG --hostname <h> \
                      --ssh-key @~/.ssh/id_ed25519.pub --wait
delonix vm start|stop|restart|rm <n>          # rm -f força

# inspecção
delonix vm ls [--ports] [-o json] · status [n] · describe <n>... · dash [--once]

# acesso
delonix vm ssh <n> [-l user] [-- cmd] · console <n> · vnc <n>

# imagens
delonix image vm ls · vm ls-remote · vm pull <ref> · vm push <n> <alvo>
delonix image vm build -t <tag> [--network] . · vm convert <src> --to <fmt>
delonix vm init --vmfile

# instantâneos (libvirt, VM a correr)
delonix vm snapshot create|ls|restore|rm <n> [<s>]

# declarativo
delonix stack validate|plan|apply -f <f> [--dry-run] [--replace K/n] [--detailed-exitcode]
delonix explain Vm · delonix manifest schema

# rede
delonix network create|ls|describe|rm · vm reach
delonix vm bridge <rede> [--apply] · vm unbridge <rede>    # root
```

**Códigos de saída:** `0` sucesso · `1` erro genérico · `2` argumentos inválidos (ou
«há alterações» no `plan --detailed-exitcode`) · `4` não existe · `5` conflito.
