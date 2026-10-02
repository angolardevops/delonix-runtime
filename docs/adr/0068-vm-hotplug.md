# ADR-0068: Hotplug de VM — CPU, memória, disco e NIC com a VM a correr

- **Estado:** Proposto (2026-10-02). Nada implementado; o spike está no Anexo A.
- **Data:** 2026-10-02
- **Decisores:** Walter Angolar
- **Relaciona-se com:** decisão D8 do plano de maturidade (`docs/discovery/65_PLANO_MATURIDADE.md`,
  PR #658), ADR-0008 (registo de backends de VM), ADR-0050 (catálogo de capacidades e evidência
  obrigatória em `supported`), ADR-0053 (`vm move`), o reconciliador de três vias
  (`crates/contexts/delonix-stack/src/reconcile.rs`), e o guarda-rio 6 da `delonix-adr` (nenhuma
  falha silenciosa).

> **Língua.** O `docs/adr/README.md` diz que os ADRs se escrevem em inglês; este está em pt-AO
> porque assim foi pedido para a D8. Se o dono preferir, traduz-se antes do merge (Questão 7).

## Contexto

Hoje uma VM só muda de forma parada: `vm resize` é o redimensionamento a frio
(`vm.resize.cold`, `VmEngine::resize` recusa `Running`/`Paused` com DX-5505), e o `kind:
VirtualMachine` não tem nenhum campo quente — `hot_fields("Vm")` é vazio, por isso mudar `vcpus`
ou `memory` num manifesto planeia `Replace`, que é recusado sem `--replace` porque deita fora o
disco overlay. A matriz de capacidades diz `vm.hotplug: not-implemented` nos três backends e
`vm.disk.resize` só `partial` no Proxmox. O dono aprovou o hotplug (D8 do plano 65): CPU, memória,
disco e NIC acrescentados ou retirados com a VM a correr, nos backends que o suportam.

O lado `container` já tem o desenho que serve de modelo: `container update` reconfigura portas,
volumes, redes e limites **a quente, com o PID inalterado**, grava cada operação no registo assim
que o dataplane a confirma, e o reconciliador tem `memory`/`cpus`/`ports`/`volumes` em
`hot_fields(CONTAINER)`.

### O que foi medido (Anexo A, 2026-10-02, neste host)

Cloud Hypervisor v53.0 com o EDK2 `CLOUDHV.fd` e um qcow2 VAZIO (sem SO convidado); libvirt
10.0.0 em `qemu:///session`, q35, domínio descartável. Sem convidado, o que se mede é o lado do
hipervisor; o que o convidado faz com a mudança (online de CPUs, plug de memória, ack de um
eject) **não é observável sem um convidado**, e o anexo di-lo em cada linha.

1. **O tecto do hotplug fixa-se no arranque, e não se muda a quente.** CH: `--cpus boot=1,max=4`
   e `--memory size=256M,hotplug_size=512M`; pedir 5 vCPUs dá `Requested vCPUs exceed maximum`,
   pedir 1 GiB dá `Not enough space in the hotplug RAM region` (ACPI) ou `new size … is bigger
   than region_size` (virtio-mem). libvirt: `<vcpu current='1'>4</vcpu>` e `<maxMemory slots='4'>`;
   `setvcpus 5` dá `5 > 4`. Uma VM criada sem margem não aceita hotplug nenhum.
2. **Acrescentar CPU funciona sem cooperação do convidado do lado do hipervisor.** CH: 1→3 dá
   204 e aparecem as threads `vcpu1`/`vcpu2`. libvirt: `setvcpus 3 --live` dá `current live 3`.
3. **Retirar CPU depende do convidado, e a resposta do hipervisor não o prova.** CH: 3→2 dá 204 e
   o `vm.info` passa a dizer `boot_vcpus 2`, mas a thread `vcpu2` continua viva (o convidado não
   ejectou), e o pedido seguinte leva **HTTP 429 «Too Many Requests»** enquanto a remoção estiver
   pendente. libvirt: `setvcpus 1 --live` é **recusado** (`device unplug request for not supported
   device type: host-x86_64-cpu`) porque os vCPUs não foram declarados `hotpluggable='yes'`.
4. **Memória: o pedido não é o resultado.** CH com virtio-mem: 256→512 MiB dá 204, o
   `config.memory.hotplugged_size` passa a 256 MiB e o `memory_actual_size` **fica em 256 MiB** —
   sem driver virtio-mem no convidado nada é plugado. CH com ACPI: 512→256 dá **204 e o `vm.info`
   diz 256 MiB**, quando o ACPI não sabe retirar memória — um relato que o motor não pode
   repetir. libvirt: `setmem 128M --live` (balão) dá **rc=0 e nada muda** (`dommemstat actual`
   continua no máximo — não há driver de balão); um `<memory model='dimm'>` por `attach-device
   --live` funciona do lado do hipervisor (`Max memory` 256→512 MiB).
5. **Retirar um disco depende do convidado, e o identificador fica preso.** CH: `vm.add-disk` dá
   200 com o `bdf`; `vm.remove-device` dá 204 e o disco sai do `config.disks`, mas **continua na
   `device_tree`**, e voltar a acrescentar o mesmo id dá `Invalid identifier as it is not unique`.
   libvirt: `detach-disk --live --config` diz **«Disk detached successfully», rc=0, e o `vdb`
   continua no `domblklist`** 3 s depois (saiu só da definição persistente).
6. **Um q35 sem portas PCIe de reserva só aceita UM dispositivo a quente.** Depois do `vdb`, o
   `attach-device` de uma NIC deu `No more available PCI slots`. As portas `pcie-root-port` têm de
   ser reservadas ao definir o domínio.
7. **Uma NIC a quente no CH precisa de um tap**, e o tap das VMs vive no netns do holder: um
   `vm.add-net` sem tap preparado deu `Unable to configure tap interface: Operation not
   permitted`. É trabalho do holder (linha de controlo `vmtap`), não uma chamada à API do VMM.
8. **O registo não guarda discos nem NICs extra.** `VmConfig.extra_disks`/`extra_nics` existem e
   o `Vm` gravado não os tem: um `vm start` de hoje já perde os `extraDisks` declarados (a limitação
   documentada do `vm start`), e um disco acrescentado a quente perder-se-ia no primeiro reinício —
   a armadilha que este repositório já pagou quatro vezes («estado necessário para reconstruir o
   recurso tem de ser persistido»).

Proxmox **não foi medido** neste spike (por leitura da documentação do PVE 9): a opção `hotplug`
da VM vale por omissão `network,disk,usb`; CPU e memória a quente exigem `cpu`/`memory` nessa
lista e `numa: 1`; o tecto de CPUs é `sockets × cores` e o valor corrente é `vcpus`; uma mudança
que o nó não consegue aplicar a quente fica na secção `pending` da config em vez de falhar.

## Decisão

### D1 — Um verbo novo, `vm update`, como o `container update`; o `vm resize` fica a frio

`delonix vm update <nome> [--vcpus N] [--memory M] [--disk-add …] [--disk-rm …] [--nic-add …]
[--nic-rm …]`. Com a VM a correr aplica a quente; parada, escreve o registo (e o `resize_cold` do
backend, para CPU e memória) e o próximo arranque usa-o.

Porquê um verbo e não `vm resize --live`:

- **Um verbo, um significado.** O `vm resize` está documentado e testado como frio («a guest that
  only sees the change after its next reboot has not been resized yet»); uma flag que lhe inverte o
  contrato faz o mesmo comando querer dizer duas coisas, e um script antigo com `--live` esquecido
  passaria a pedir outra operação.
- **Disco e NIC não são «resize».** `vm resize --disk-add` lê-se mal; `update` é o verbo que a
  CLI já usa para «muda isto num recurso vivo, mantendo a identidade» (`container update`), e é o
  que um utilizador vindo do Docker (`docker update`) e do Proxmox (`qm set`) procura.
- **O reconciliador precisa de UM caminho que funcione nos dois estados**, e é o `update`.

O `vm resize` fica tal como está, sem alias. Com a VM a correr, a mensagem do DX-5505 passa a
nomear o `vm update`. Remoções correm antes de adições (`--disk-rm vdb --disk-add …` num só
comando), como no `container update`.

### D2 — O tecto declara-se na criação, e sem ele não há hotplug

`kind: VirtualMachine` ganha `vcpusMax` e `memoryMax` (e `vm create --vcpus-max/--memory-max`).
Omissos, **o tecto é o arranque** (zero margem) — o que é hoje, byte a byte, para toda a VM
existente — e um `vm update` acima dele é **recusado com o tecto e o comando que o muda**
(`vm stop` + `vm resize --vcpus-max …`), nunca encostado ao máximo em silêncio.

Por backend, na criação:

- **cloud-hypervisor:** `--cpus boot=N,max=M`; `--memory size=X,hotplug_method=virtio-mem,
  hotplug_size=(Max−X)`.
- **libvirt:** `<vcpu current='N'>M</vcpu>` com `<vcpus>` por vCPU (`hotpluggable='yes'` em todos
  menos o 0), `<maxMemory slots='S'>Max</maxMemory>` e uma célula NUMA (o libvirt exige-a para
  DIMMs), e **quatro `pcie-root-port` de reserva** quando há margem declarada (achado 6).
- **Proxmox:** `hotplug: network,disk,usb,cpu,memory`, `numa: 1`, `sockets×cores = M`, `vcpus = N`.

O tecto é **frio**: muda com a VM parada e vale no arranque seguinte. A margem de memória não
consome RAM do host (CH reserva espaço de endereços; libvirt reserva slots), por isso é barata —
mas não se dá por omissão, porque muda a forma do domínio para quem não a pediu.

### D3 — Memória: virtio-mem onde existe, DIMM no libvirt, e só a crescer na v1

- **cloud-hypervisor:** virtio-mem. O ACPI fica de fora: não retira memória e respondeu 204 a um
  pedido que não podia cumprir (achado 4).
- **libvirt:** DIMM por `attach-device --live` (medido). O balão (`setmem --live`) **não conta como
  hotplug**: devolveu 0 sem efeito. O virtio-mem do libvirt (≥7.9 com QEMU ≥6.2) não foi medido e
  fica para uma fatia seguinte.
- **Proxmox:** `PUT /config` com `memory`, com `hotplug` a incluir `memory`.

**Na v1 a memória e os vCPUs só crescem a quente.** Um decréscimo com a VM a correr é recusado
com o caminho a frio (`vm stop` + `vm update`/`vm resize`). Razões medidas: retirar depende do
convidado em todos os backends, o hipervisor responde «aceite» antes de o convidado obedecer
(achados 3 e 4), e um CH com uma remoção pendente responde 429 a tudo o que vier a seguir.

### D4 — A verdade é o que se relê da VM viva, não a resposta do pedido

Cada operação a quente é **pedido → releitura → registo**, e só o lido entra no registo:

| | cloud-hypervisor | libvirt | Proxmox |
|---|---|---|---|
| vCPUs | threads `vcpuN` do VMM (`/proc/<pid>/task`), não `config.boot_vcpus` | `vcpucount --live` | `GET …/status/current` `cpus` |
| memória | `vm.info` `memory_actual_size`, não `config.memory.*` | `dominfo` / `dommemstat actual` | `GET …/config?current=1` + `pending` vazio |
| disco | `vm.info` `device_tree` | `domblklist` | `GET …/config?current=1` |
| NIC | `vm.info` `device_tree` | `domiflist` | `GET …/config?current=1` |

Três desfechos, como o veredicto das tarefas Proxmox (ADR-0058): **aplicado** (lido igual ao
pedido), **pendente do convidado** (o hipervisor aceitou, o convidado ainda não plugou/ejectou —
dito ao operador, com o que falta no convidado; o registo NÃO muda), e **recusado** (erro com
classe). Uma remoção pendente não é repetida pelo reconciliador: o identificador continua ocupado
(achado 5) e o CH responde 429 (achado 3).

### D5 — Persistência: o que muda a quente entra no registo, ou perde-se no reinício

- O `Vm` gravado ganha `vcpus_max`, `memory_max`, `extra_disks` e `extra_nics`
  (`#[serde(default)]`: um registo antigo lê-se como «tecto = arranque, sem extras», que é o que
  ele era). Isto fecha também, de caminho, o `vm start` que hoje perde os `extraDisks` declarados.
- A ordem é a do `resize`: **hipervisor primeiro, releitura, registo em último**, uma operação de
  cada vez (`JsonStore::update` com flock), como o `container update` — sem transacção: se a
  terceira falhar, as duas primeiras já estão no hipervisor e no registo.
- libvirt e CH: o registo é a definição inteira e o próximo arranque reconstrói-a dele, por isso
  basta `--live` (o `--config` do libvirt é irrelevante enquanto o `stop` desfizer o domínio).
- Proxmox: a config do nó é persistente por natureza; uma mudança que caia em `pending` é tratada
  como **não aplicada** (lida e reportada), nunca como sucesso.
- **Gate obrigatório por campo:** mudar a quente, `vm stop`, `vm start`, e reler a VM — o valor
  tem de sobreviver. É o check que apanha a quinta ocorrência da armadilha.

### D6 — Disco e NIC

- **Disco:** CH `vm.add-disk` com `id` determinístico (`dlx-<alvo>`) e `vm.remove-device`;
  libvirt `attach-disk`/`detach-disk --live`; Proxmox `PUT /config` com `scsiN`/`virtioN` e
  `DELETE` do volume. O caminho do ficheiro passa pela mesma validação de nome/confinamento que o
  `extraDisks` já tem. O **redimensionamento** de um disco existente (`vm.disk.resize`) não faz
  parte desta decisão.
- **NIC:** libvirt `attach-device` de um `<interface>` (exige porta PCIe livre, D2); Proxmox
  `netN`. **No CH a NIC a quente fica para uma fatia própria**: precisa de o holder criar o tap e
  ligá-lo à bridge (uma linha de controlo nova, com a mesma compatibilidade de holder do `vmtap`),
  e com ele o anti-spoof e o isolamento de namespace do tap novo (ADR-0055) — não é uma chamada à
  API do VMM. Até lá, `--nic-add` no CH é recusado por nome (DX-1501).

### D7 — No `kind: VirtualMachine`, `vcpus`/`memory` passam a crescer a quente, os extras a quente

- `grow_only_fields(Vm)` = `vcpus`, `memory` (a memória comparada em MiB normalizados, porque o
  `is_hot_change` compara números e `2G` contra `2048M` é a mesma coisa). Crescer planeia
  `Update`; decrescer continua a planear `Replace` (Questão 2).
- `hot_fields(Vm)` ganha `extraDisks` e `extraNics`, convergidos **item a item** (os que saem
  primeiro, os que entram depois), nunca como lista substituída.
- `vcpusMax`/`memoryMax` são frios.
- `is_hot_change` é a mesma função no `plan` e no `apply` (como no `SystemContainer`), para os dois
  nunca discordarem sobre o que exige recriar.

### D8 — Catálogo: `vm.hotplug` parte-se em quatro

`vm.hotplug` dá lugar a `vm.hotplug.cpu`, `vm.hotplug.memory`, `vm.hotplug.disk` e
`vm.hotplug.nic` (catálogo minor+1). Cada backend declara cada um com a razão; `supported` só com
evidência (ADR-0050). Declaração inicial à saída da primeira fatia:

| | cloud-hypervisor | libvirt | proxmox |
|---|---|---|---|
| `cpu` | partial — cresce; o online é do convidado | partial — idem | partial — sem caso ao vivo |
| `memory` | partial — virtio-mem; plug é do convidado | partial — DIMM; online é do convidado | partial — sem caso ao vivo |
| `disk` | partial | partial | partial |
| `nic` | not-implemented — tap no holder (D6) | partial | partial |

Passam a `supported` só com o check de bateria da D9 a correr contra um convidado que online.

### D9 — A bateria lê a VM viva, e um convidado real

Os checks de `scripts/e2e.sh` (e o caso `live.rs` do Proxmox) lêem o hipervisor pela tabela da D4,
**nunca** o que o `delonix` imprimiu — e, para `supported`, lêem também DENTRO do convidado
(`nproc`, `/proc/meminfo`, `lsblk`, `ip link`) pela consola série. Isso precisa de uma imagem que
faça online sozinha: kernel com `memhp_default_state=online` (ou a regra udev
`40-vm-hotadd.rules`), driver virtio-mem (Linux ≥5.8 x86_64), e CPUs onlinadas por udev. A
`delonix-vm-base` tem de o garantir e dizê-lo (Questão 4). Cada campo tem o seu check de
persistência (D5).

## Alternativas consideradas

- **`vm resize --live`.** Rejeitada pela D1: inverte o contrato de um verbo publicado e não cobre
  disco/NIC.
- **Tecto por omissão generoso (ex.: `max = 2× boot`).** Rejeitada: muda a forma de todas as VMs
  (NUMA, slots, portas PCIe) para quem não pediu hotplug, e no libvirt a célula NUMA obrigatória
  pode alterar o comportamento do convidado.
- **Balão como «memória a quente» no libvirt.** Rejeitada: medido a devolver 0 sem efeito sem
  driver; e não aumenta acima do `<memory>`, só baixa.
- **ACPI no CH.** Rejeitada: não retira, e respondeu 204 a uma retirada (achado 4).
- **Retirar a quente na v1.** Adiada: depende do convidado em todos os backends, e o hipervisor
  responde «aceite» antes de o convidado obedecer; com a D4 e um convidado de teste pode entrar
  numa fatia seguinte.
- **Gravar o registo com o valor pedido e reconciliar depois.** Rejeitada: é o relato desonesto
  que este repositório persegue — um `vm describe` a dizer 4 vCPUs de uma VM com 2.

## Consequências

- O registo `Vm` cresce quatro campos; os registos antigos continuam válidos.
- Uma VM nova com margem declarada tem uma forma de domínio diferente no libvirt (NUMA, `<vcpus>`,
  portas PCIe) — os checks de XML existentes têm de passar a cobri-la.
- O `vm start` deixa de perder os `extraDisks`/`extraNics` declarados (efeito colateral da D5).
- O reconciliador ganha o primeiro Kind com campos «só a crescer» que não são números crus
  (memória normalizada).
- Fatias: (1) registo + tecto na criação + `vm update` de CPU/memória a crescer nos três backends;
  (2) disco; (3) NIC libvirt/Proxmox; (4) NIC no CH com o holder; (5) retirar a quente, se o dono
  quiser, depois de um convidado de teste.

## Questões em aberto para o dono

1. **Tecto por omissão = arranque (sem hotplug) — confirma?** A alternativa é uma margem pequena
   por omissão para as VMs NOVAS, com o custo da D2.
2. **Decréscimo no manifesto:** hoje planeia `Replace` (recusado sem `--replace`). Quer uma acção
   de plano nova, «reiniciar» (stop + aplicar + start, sem destruir o overlay), em vez de
   `Replace`?
3. **Mudar o tecto** (`vcpusMax`/`memoryMax`) de uma VM existente: só com a VM parada pelo
   `vm resize`, ou também pela acção «reiniciar» da questão 2?
4. **Imagem de teste:** a `delonix-vm-base` passa a garantir o online automático (kernel cmdline +
   regra udev) — e isso vale para as quatro distros?
5. **NIC no CH (D6):** entra nesta série ou fica para depois do programa NaaS?
6. **Proxmox:** há janela no laboratório (`pve`/`pve2`) para medir `hotplug` + `numa` + `pending`
   antes de declarar, como os outros casos `live.rs`?
7. **Língua deste ADR** (o README pede inglês).

## Anexo A — Spike (2026-10-02, isolado)

Directório temporário em `/tmp/dlxhp.*`, removido no fim; nenhuma VM existente, nenhum
`DELONIX_ROOT`, só `qemu:///session`. Domínio `dlx-hp-spike-<pid>` destruído e `undefine`d;
`virsh list --all | grep dlx-hp` → 0; nenhum `cloud-hypervisor` do spike vivo no fim.

### A.1 Cloud Hypervisor v53.0, ACPI

```
cloud-hypervisor --api-socket path=$D/api.sock --firmware /usr/local/share/delonix/CLOUDHV.fd \
  --disk path=$D/boot.qcow2,image_type=qcow2 --cpus boot=1,max=4 \
  --memory size=256M,hotplug_method=acpi,hotplug_size=512M --serial file=$D/serial.log --console off
curl --unix-socket $D/api.sock -X PUT -d '<json>' http://localhost/api/v1/<verb>
```

```
  boot_vcpus 1 mem.size 268435456 actual 268435456 disks ['_disk0']
== vm.resize {"desired_vcpus":5}         → HTTP500 "Requested vCPUs exceed maximum"
== vm.resize {"desired_vcpus":3}         → HTTP204   boot_vcpus 3   (threads vcpu0..vcpu2)
== vm.resize {"desired_vcpus":2}         → HTTP204   boot_vcpus 2   (threads: vcpu0, vcpu1, vcpu2 — vcpu2 NÃO saiu)
== vm.resize {"desired_vcpus":4}         → HTTP429 "Too Many Requests"   (remoção pendente)
== vm.resize {"desired_ram":1073741824}  → HTTP500 "Not enough space in the hotplug RAM region"
== vm.resize {"desired_ram":536870912}   → HTTP204   mem.size 536870912 actual 536870912
== vm.resize {"desired_ram":268435456}   → HTTP204   mem.size 268435456 actual 268435456   (ACPI não retira)
== vm.add-disk {"path":".../extra.raw","id":"hp0"} → HTTP200 {"id":"hp0","bdf":"0000:00:03.0"}
                                           disks ['_disk0','hp0'], device_tree com hp0
== vm.remove-device {"id":"hp0"}         → HTTP204   disks ['_disk0'], device_tree AINDA com hp0 (também 5 s depois)
== vm.add-disk ... id hp0                → HTTP500 "Invalid identifier as it is not unique: hp0"
== vm.add-net {"id":"net9"}              → HTTP500 "Unable to configure tap interface … Operation not permitted"
```

### A.2 Cloud Hypervisor v53.0, virtio-mem

`--memory size=256M,hotplug_method=virtio-mem,hotplug_size=512M`

```
  mem.size 268435456 hotplugged_size None actual 268435456
== vm.resize {"desired_ram":536870912}   → HTTP204   hotplugged_size 268435456 actual 268435456 (também 3 s depois)
== vm.resize {"desired_ram":268435456}   → HTTP204   hotplugged_size None      actual 268435456
== vm.resize {"desired_ram":1073741824}  → HTTP500 "new size 0x30000000 is bigger than region_size 0x20000000"
```

Sem convidado, o `memory_actual_size` não se mexe: é o convidado que pluga os blocos.

### A.3 libvirt 10.0.0, `qemu:///session`, q35

Domínio com `<maxMemory slots='4' unit='MiB'>2048</maxMemory>`, `<memory>256</memory>`,
`<vcpu placement='static' current='1'>4</vcpu>`, uma célula NUMA de 256 MiB, um disco virtio,
`<memballoon model='virtio'/>`.

```
== setvcpus 3 --live        rc=0   current live 3, current config 1
== setvcpus 1 --live        rc=1   "acpi: device unplug request for not supported device type: host-x86_64-cpu"
== setvcpus 5 --live        rc=1   "requested vcpus is greater than max allowable vcpus for the live domain: 5 > 4"
== setmem 128M --live       rc=0   dominfo Used memory 262144 KiB (inalterado); dommemstat actual = máximo
== setmem 512M --live       rc=1   "cannot set memory higher than max memory"
== attach-device <memory model='dimm'> 256 MiB --live   rc=0   Max/Used memory 524288 KiB, 1 dimm no XML vivo
== attach-disk vdb --live --config   rc=0   domblklist: vda vdb
== detach-disk vdb --live --config   rc=0   "Disk detached successfully"; domblklist: vda vdb (3 s depois);
                                            XML inactivo: 1 disco, XML vivo: 2
== attach-device <interface type='user'> --live --config   rc=1   "No more available PCI slots"
```

### A.4 O que não se mediu

- Nada dentro de um convidado (online de CPU/memória, ack de eject): o qcow2 do spike está vazio.
- Proxmox: nenhuma chamada; os factos da secção «Contexto» são por leitura.
- virtio-mem no libvirt, vCPUs `hotpluggable='yes'` no libvirt, NIC a quente no CH com tap.
