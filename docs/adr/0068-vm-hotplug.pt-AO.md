# ADR-0068: Hotplug de VM — CPU, memória, disco e NIC com a VM a correr

- **Estado:** Proposed (2026-10-02). Nada implementado; o spike está no Anexo A. As respostas do
  dono de 2026-10-02 estão registadas como decisões (D2, D4, D8, D10); as questões que deixou em
  aberto continuam abertas, com recomendação.
- **Data:** 2026-10-02
- **Decisores:** Walter Angolar
- **Relaciona-se com:** decisão D8 do plano de maturidade (`docs/discovery/65_PLANO_MATURIDADE.md`,
  PR #658), ADR-0008 (registo de backends de VM), ADR-0043 (o dicionário `DX-CDNN`), ADR-0050
  (catálogo de capacidades, evidência obrigatória em `supported`), ADR-0053 (`vm move`), ADR-0055
  (anti-spoof por VM), o reconciliador de três vias
  (`crates/contexts/delonix-stack/src/reconcile.rs`), e o guarda-rio 6 da `delonix-adr` (nenhuma
  falha silenciosa).
- **Cópias:** o canónico é `0068-vm-hotplug.md`, em inglês. Este ficheiro é a cópia interna de
  revisão em pt-AO; os dois têm o mesmo ID, estado, decisões e referências.

## Contexto

Hoje uma VM só muda de forma parada: `vm resize` é o redimensionamento a frio (`vm.resize.cold`;
`VmEngine::resize` recusa `Running`/`Paused` com DX-5505), e o `kind: VirtualMachine` não tem
nenhum campo quente — `hot_fields("Vm")` é vazio, por isso mudar `vcpus` ou `memory` num manifesto
planeia `Replace`, que é recusado sem `--replace` porque deita fora o disco overlay. A matriz de
capacidades diz `vm.hotplug: not-implemented` nos três backends e `vm.disk.resize` só `partial` no
Proxmox. O dono aprovou o hotplug (D8 do plano 65): CPU, memória, disco e NIC acrescentados ou
retirados com a VM a correr, nos backends que o suportam.

O lado `container` já tem o desenho que serve de modelo: `container update` reconfigura portas,
volumes, redes e limites **a quente, com o PID inalterado**, grava cada operação no registo assim
que o dataplane a confirma, e o reconciliador tem `memory`/`cpus`/`ports`/`volumes` em
`hot_fields(CONTAINER)`.

### O que foi medido (Anexo A, 2026-10-02, neste host)

Cloud Hypervisor v53.0 com o EDK2 `CLOUDHV.fd` e um qcow2 VAZIO (sem SO convidado); libvirt 10.0.0
em `qemu:///session`, q35, domínio descartável. Sem convidado, o que se mede é o lado do
hipervisor; o que o convidado faz com a mudança (online de CPUs, plug de memória, ack de um eject)
**não é observável sem um convidado**, e o anexo di-lo em cada linha.

1. **O tecto do hotplug fixa-se no arranque, e não se muda a quente.** CH: `--cpus boot=1,max=4`
   e `--memory size=256M,hotplug_size=512M`; pedir 5 vCPUs dá `Requested vCPUs exceed maximum`,
   pedir 1 GiB dá `Not enough space in the hotplug RAM region` (ACPI) ou `new size … is bigger
   than region_size` (virtio-mem). libvirt: `<vcpu current='1'>4</vcpu>` e `<maxMemory slots='4'>`;
   `setvcpus 5` dá `5 > 4`. Uma VM criada sem margem não aceita hotplug nenhum.
2. **Acrescentar CPU funciona do lado do hipervisor sem o convidado.** CH: 1→3 dá 204 e aparecem
   as threads `vcpu1`/`vcpu2`. libvirt: `setvcpus 3 --live` dá `current live 3`.
3. **Retirar CPU depende do convidado, e a resposta do hipervisor não o prova.** CH: 3→2 dá 204 e
   o `vm.info` passa a dizer `boot_vcpus 2`, mas a thread `vcpu2` continua viva (o convidado não
   ejectou), e o pedido seguinte leva **HTTP 429 «Too Many Requests»** enquanto a remoção estiver
   pendente. libvirt: `setvcpus 1 --live` é **recusado** (`device unplug request for not supported
   device type: host-x86_64-cpu`) porque os vCPUs não foram declarados `hotpluggable='yes'`.
4. **Memória: o pedido não é o resultado.** CH com virtio-mem: 256→512 MiB dá 204, o
   `config.memory.hotplugged_size` passa a 256 MiB e o `memory_actual_size` **fica em 256 MiB** —
   sem driver virtio-mem no convidado nada é plugado. CH com ACPI: 512→256 dá **204 e o `vm.info`
   diz 256 MiB**, quando o ACPI não sabe retirar memória — um relato que o motor não pode repetir.
   libvirt: `setmem 128M --live` (balão) dá **rc=0 e nada muda** (`dommemstat actual` continua no
   máximo — não há driver de balão); um `<memory model='dimm'>` por `attach-device --live`
   funciona do lado do hipervisor (`Max memory` 256→512 MiB).
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
   o `Vm` gravado não os tem: um `vm start` de hoje já perde os `extraDisks` declarados (a
   limitação documentada do `vm start`), e um disco acrescentado a quente perder-se-ia no primeiro
   reinício — a armadilha que este repositório já pagou quatro vezes («estado necessário para
   reconstruir o recurso tem de ser persistido»).

Proxmox **não foi medido** neste spike (por leitura da documentação do PVE 9): a opção `hotplug`
da VM vale por omissão `network,disk,usb`; CPU e memória a quente exigem `cpu`/`memory` nessa
lista e `numa: 1`; o tecto de CPUs é `sockets × cores` e o valor corrente é `vcpus`; uma mudança
que o nó não consegue aplicar a quente fica na secção `pending` da config em vez de falhar.

O que o convidado precisa está no guia de administração de memory-hotplug do kernel
(`Documentation/admin-guide/mm/memory-hotplug.rst`): os blocos de memória acrescentados só ficam
online se a política de auto-online o disser — o parâmetro de linha de comandos
`memhp_default_state=online|online_kernel|online_movable`, o `CONFIG_MEMHP_DEFAULT_ONLINE_TYPE`, ou
uma escrita em `/sys/devices/system/memory/auto_online_blocks` — senão uma regra udev (ou um
agente) tem de escrever `online` em cada `memoryN/state` novo. Os CPUs x86 acrescentados ficam
offline a não ser que uma regra udev escreva `1` em `cpuN/online`. O virtio-mem pluga pelo seu
próprio driver (Linux ≥ 5.8 em x86_64) e fica online pela mesma política.

## Decisão

### D1 — Um verbo novo, `vm update`, como o `container update`; o `vm resize` fica a frio

`delonix vm update <nome> [--vcpus N] [--memory M] [--disk-add …] [--disk-rm …] [--nic-add …]
[--nic-rm …]`. Com a VM a correr aplica a quente; parada, escreve o registo (e o `resize_cold` do
backend, para CPU e memória) e o próximo arranque usa-o.

Porquê um verbo e não `vm resize --live`:

- **Um verbo, um significado.** O `vm resize` está documentado e testado como frio; uma flag que
  lhe inverte o contrato faz o mesmo comando querer dizer duas coisas, e um script antigo com
  `--live` esquecido passaria a pedir outra operação.
- **Disco e NIC não são «resize».** `update` é o verbo que a CLI já usa para «muda isto num
  recurso vivo, mantendo a identidade» (`container update`), e é o que um utilizador vindo do
  Docker (`docker update`) e do Proxmox (`qm set`) procura.
- **O reconciliador precisa de UM caminho que funcione nos dois estados**, e é o `update`.

O `vm resize` fica tal como está, sem alias. Com a VM a correr, a mensagem do DX-5505 passa a
nomear o `vm update`. Remoções correm antes de adições, como no `container update` — e as
remoções em si estão sujeitas à D10.

### D2 — Três quantidades, separadas: inicial, máximo, atribuído (dono, 2026-10-02)

O modelo e toda a saída separam três quantidades, para CPU e para memória:

- **inicial** — com o que a VM arrancou (gravado no arranque; informativo);
- **máximo** — o tecto do hotplug, fixado no arranque (D3);
- **atribuído** — o que a VM tem agora, relido da VM viva (D5), com a parte activa no convidado
  ao lado.

`vm describe`, `vm ls -o json`, a API de nó e o dashboard imprimem as três, p. ex.
`CPUs: initial 2 · maximum 4 · assigned 3 (guest online 3)`. **O máximo não é consumo**: reserva
espaço de endereços ou slots, nunca RAM nem CPU do host, e nada que meça ou reporte uso
(`dashstats`, as gauges Prometheus, um showback) o pode contar — só conta o atribuído. O registo
ganha `vcpus_max` e `memory_max`; `vcpus`/`memory` continuam a querer dizer «com o que o próximo
arranque começa» e acompanham o atribuído confirmado (D6).

### D3 — O tecto declara-se na criação, e sem ele não há hotplug (dono, 2026-10-02)

`kind: VirtualMachine` ganha `vcpusMax` e `memoryMax` (e `vm create --vcpus-max/--memory-max`).
**Por omissão o máximo é o tamanho de arranque** (zero margem) — o que é hoje, byte a byte, para
toda a VM existente. O hotplug de CPU ou memória exige AMBOS um máximo declarado acima do arranque
E o backend a declarar a capacidade (D9); faltando um deles, um `vm update` acima do arranque é
**recusado com o tecto e o comando que o muda**, nunca encostado ao máximo em silêncio.

Por backend, na criação:

- **cloud-hypervisor:** `--cpus boot=N,max=M`; `--memory size=X,hotplug_method=virtio-mem,
  hotplug_size=(Max−X)`.
- **libvirt:** `<vcpu current='N'>M</vcpu>` com `<vcpus>` por vCPU (`hotpluggable='yes'` em todos
  menos o 0), `<maxMemory slots='S'>Max</maxMemory>` e uma célula NUMA (o libvirt exige-a para
  DIMMs), e **quatro `pcie-root-port` de reserva** quando há margem declarada (achado 6).
- **Proxmox:** `hotplug: network,disk,usb,cpu,memory`, `numa: 1`, `sockets×cores = M`, `vcpus = N`.

O tecto é **frio**: muda com a VM parada e vale no arranque seguinte. Não se dá por omissão
porque muda a forma do domínio (NUMA, slots, portas PCIe) para quem não pediu hotplug.

### D4 — Uma operação só está completa quando o CONVIDADO usa o recurso; senão é PARCIAL (dono, 2026-10-02)

Uma operação a quente tem três desfechos, e só o primeiro é sucesso:

- **completa** — o hipervisor atribuiu o recurso E o convidado usa-o (CPUs online, blocos de
  memória online, o disco/NIC presente no convidado);
- **parcial** — o hipervisor atribuiu-o e o convidado não o activou dentro do prazo (`--wait`,
  60 s por omissão), ou o backend não consegue ver dentro do convidado. Reportado como o estado
  `Partial`, com o que falta no convidado, sob um código novo **`DX-8503 vm.hotplug_partial`**
  (classe *timeout*, domínio *vm*, **saída 124**: «o prazo passou com o trabalho por acabar»; o
  número livre a seguir a DX-8501/8502, atribuído na implementação pelo gate do dicionário).
  **Nunca** é reportado como sucesso, e o `-o json` leva `"state": "partial"` com o atribuído e o
  activo no convidado;
- **recusada** — um erro com a sua classe, nada mudou.

Como se lê o lado do convidado: em libvirt e Proxmox pelo agente que as imagens base já trazem
(`guest-get-vcpus`, `guest-get-memory-blocks`, `guest-get-disks`,
`guest-network-get-interfaces`). Sem agente a responder, o resultado é `partial` com a razão
«activação no convidado não observável», nunca presumido. No Cloud Hypervisor o
`memory_actual_size` do virtio-mem mostra o driver do convidado a plugar; a activação de CPU não
tem canal hoje, por isso um acrescento de CPU no CH acaba `partial` até existir um (Questão 5).

### D5 — A verdade é o que se relê da VM viva, não a resposta do pedido

Cada operação a quente é **pedido → releitura (hipervisor, depois convidado) → registo**:

| | cloud-hypervisor | libvirt | Proxmox |
|---|---|---|---|
| vCPUs | threads `vcpuN` do VMM (`/proc/<pid>/task`), não `config.boot_vcpus` | `vcpucount --live` | `GET …/status/current` `cpus` |
| memória | `vm.info` `memory_actual_size`, não `config.memory.*` | `dominfo` / `dommemstat actual` | `GET …/config?current=1` + `pending` vazio |
| disco | `vm.info` `device_tree` | `domblklist` | `GET …/config?current=1` |
| NIC | `vm.info` `device_tree` | `domiflist` | `GET …/config?current=1` |
| convidado | só o tamanho plugado do virtio-mem | agente do convidado | agente do convidado |

### D6 — Persistência: o que muda a quente entra no registo, ou perde-se no reinício

- O `Vm` gravado ganha `vcpus_max`, `memory_max`, `extra_disks` e `extra_nics` (com
  `#[serde(default)]`: um registo antigo lê-se como «máximo = arranque, sem extras», que é o que
  ele era). Isto fecha também o `vm start` que hoje perde os `extraDisks` declarados.
- A ordem é a do `resize`: **hipervisor primeiro, releitura, registo em último**, uma operação de
  cada vez (`JsonStore::update` com flock), como no `container update` — sem transacção.
- O que o registo leva é o valor **atribuído pelo hipervisor**, também num `partial` (o host
  segura-o, e um reinício não o pode largar em silêncio; o convidado vê-o no arranque seguinte),
  junto de uma condição `HotplugPartial` a nomear o que o convidado não activou. Uma operação
  recusada deixa o registo como estava.
- libvirt e CH: o registo é a definição inteira e o próximo arranque reconstrói-a dele, por isso
  basta `--live` (o `--config` do libvirt é irrelevante enquanto o `stop` desfizer o domínio).
- Proxmox: a config do nó é persistente por natureza; uma mudança que caia em `pending` **não foi
  aplicada** (lida e reportada como recusada), nunca um sucesso.
- **Gate obrigatório por campo:** mudar a quente, `vm stop`, `vm start`, e reler a VM — o valor
  tem de sobreviver.

### D7 — Disco e NIC

- **Acrescentar disco:** CH `vm.add-disk` com `id` determinístico (`dlx-<alvo>`); libvirt
  `attach-disk --live`; Proxmox `PUT /config` com `scsiN`/`virtioN`. O caminho do ficheiro passa
  pela mesma validação de nome/confinamento que o `extraDisks` já tem. O **redimensionamento** de
  um disco existente (`vm.disk.resize`) não faz parte desta decisão.
- **Acrescentar NIC:** libvirt `attach-device` de um `<interface>` (exige porta PCIe livre, D3);
  Proxmox `netN`. **No CH a NIC a quente fica para uma fatia própria**: o holder tem de criar o
  tap e ligá-lo à bridge (uma linha de controlo nova, com a mesma compatibilidade de holder do
  `vmtap`), e com ele o anti-spoof e o isolamento de namespace do tap novo (ADR-0055) — não é uma
  chamada à API do VMM. Até lá, `--nic-add` no CH é recusado por nome (DX-1501).
- Retirar disco e NIC segue a D10.

### D8 — As imagens base geridas pelo Delonix põem online o que se acrescenta (dono, 2026-10-02)

As imagens construídas pelo `vm-image` (`delonix-vm-base` nas quatro distros — Ubuntu, Debian,
Rocky, Fedora — e a golden k8s) têm de pôr online os CPUs e a memória acrescentados onde o kernel
o suporta. Uma fase dedicada muda as receitas de build (`rootless_customization_steps` /
`shared_account_steps` e a receita k8s), escrevendo ficheiros e não comandos de runtime, porque o
`virt-customize` corre contra um convidado offline:

- memória: a política de auto-online, pela linha de comandos do kernel
  (`memhp_default_state=online_movable`, movable para que uma remoção futura possa resultar) ou,
  onde o bootloader da distro o torne incómodo, uma regra udev
  `SUBSYSTEM=="memory", ACTION=="add", ATTR{state}=="offline", ATTR{state}="online_movable"` —
  segundo o guia de memory-hotplug do kernel;
- CPU: uma regra udev `SUBSYSTEM=="cpu", ACTION=="add", ATTR{online}=="0", ATTR{online}="1"`, a não
  ser que a distro já traga uma equivalente (a medir por distro, não a presumir);
- o agente do convidado continua instalado e activo (a D4 lê através dele);
- o ficheiro de proveniência da imagem (`/etc/delonix-image-release`) regista `hotplug-online: yes`.

**O check:** um teste por distro a exigir que a receita escreve a política, e um caso da bateria
que arranca cada imagem publicada com margem, acrescenta um vCPU e 256 MiB, e lê `nproc` e
`/proc/meminfo` dentro do convidado — `complete`, não `partial`. As imagens que não são
construídas pelo Delonix são do utilizador: aí o resultado é o que a D4 observar.

### D9 — No `kind: VirtualMachine`, `vcpus`/`memory` crescem a quente, os extras a quente

- `grow_only_fields(Vm)` = `vcpus`, `memory` (a memória comparada em MiB normalizados, porque o
  `is_hot_change` compara números e `2G` contra `2048M` é a mesma coisa). Crescer planeia
  `Update`; decrescer continua a planear `Replace` (Questão 1).
- `hot_fields(Vm)` ganha `extraDisks` e `extraNics` para **acrescentos**, convergidos item a item;
  um item retirado do manifesto não é retirado a quente enquanto a remoção estiver fora de âmbito
  (D10) — o plano di-lo em vez de planear uma remoção que não vai fazer.
- `vcpusMax`/`memoryMax` são frios.
- `is_hot_change` é a mesma função no `plan` e no `apply` (como no `SystemContainer`).

### D10 — Retirar é uma capacidade à parte, fora das primeiras fases (dono, 2026-10-02)

Suportar o acrescento a quente não prova uma remoção a quente segura: os achados 3, 4 e 5 mostram
o hipervisor a responder «feito» a uma remoção que o convidado não executou, uma remoção no CH a
bloquear com 429 todos os pedidos seguintes, e um identificador que fica ocupado. Por isso:

- cada capacidade parte-se em **`.add`** e **`.remove`** (D11);
- a remoção (CPU, memória, disco, NIC) fica **fora das primeiras fases** e é recusada a quente por
  nome, com o caminho a frio (`vm stop` + `vm update`/`vm resize`);
- só entra por backend depois de provada lá, com um convidado que ejecte, por um check da bateria
  que lê o hipervisor e o convidado depois da remoção e depois de um reinício.

### D11 — Catálogo: `vm.hotplug` parte-se em oito células

`vm.hotplug` dá lugar a `vm.hotplug.{cpu,memory,disk,nic}.{add,remove}` (catálogo minor+1). Cada
backend declara cada uma com a razão; `supported` só com evidência (ADR-0050). Declaração à saída
da primeira fase:

| | cloud-hypervisor | libvirt | proxmox |
|---|---|---|---|
| `cpu.add` | partial — sem canal para a activação de CPU (D4) | partial — até ao check da D8 | partial — sem caso ao vivo |
| `memory.add` | partial — virtio-mem; até ao check da D8 | partial — DIMM; até ao check da D8 | partial — sem caso ao vivo |
| `disk.add` | partial | partial | partial |
| `nic.add` | not-implemented — tap no holder (D7) | partial | partial |
| `*.remove` | not-implemented — D10 | not-implemented — D10 | not-implemented — D10 |

Uma célula passa a `supported` só com o check da bateria da D12 a correr contra um convidado que
põe online.

### D12 — A bateria lê a VM viva, e um convidado real

Os checks de `scripts/e2e.sh` (e o caso `live.rs` do Proxmox) lêem o hipervisor pela tabela da D5,
**nunca** o que o `delonix` imprimiu — e, para `supported` e para `complete`, lêem também DENTRO
do convidado (`nproc`, `/proc/meminfo`, `lsblk`, `ip link`) pela consola série ou pelo agente,
com uma imagem da D8. Cada campo tem o seu check de persistência (D6), e um caso `partial` exige
a saída 124 e o `DX-8503` numa imagem sem a política da D8.

## Fases

1. Campos do registo + tecto na criação + `vm update` para **acrescentar** CPU/memória nos três
   backends, com as três quantidades da D2 na saída e o `partial` da D4 (DX-8503).
2. Imagens base que põem online (D8), com o check por distro.
3. Acrescentar disco.
4. Acrescentar NIC em libvirt/Proxmox.
5. Acrescentar NIC no CH, com o holder.
6. Remoção, por backend, só quando provada (D10).

## Alternativas consideradas

- **`vm resize --live`.** Rejeitada pela D1: inverte o contrato de um verbo publicado e não cobre
  disco/NIC.
- **Um tecto generoso por omissão (ex.: `max = 2× boot`).** Rejeitada pelo dono (D3).
- **Balão como «memória a quente» no libvirt.** Rejeitada: medido a devolver 0 sem efeito sem
  driver; e não sobe acima do `<memory>`, só desce.
- **ACPI no CH.** Rejeitada: não retira, e respondeu 204 a uma retirada (achado 4).
- **Sucesso assim que o hipervisor aceita.** Rejeitada pelo dono (D4): o hipervisor responde
  antes de o convidado agir.
- **Gravar no registo o valor pedido e reconciliar depois.** Rejeitada: é o relato desonesto que
  este repositório persegue — um `vm describe` a dizer 4 vCPUs de uma VM com 2.

## Consequências

- O registo `Vm` cresce quatro campos; os registos antigos continuam válidos.
- Uma VM nova com margem declarada tem uma forma de domínio diferente no libvirt (NUMA, `<vcpus>`,
  portas PCIe) — os checks de XML existentes têm de passar a cobri-la.
- O `vm start` deixa de perder os `extraDisks`/`extraNics` declarados (efeito colateral da D6).
- O reconciliador ganha o primeiro Kind cujos campos «só a crescer» não são números crus
  (memória normalizada).
- Um código novo, DX-8503, e um estado `Partial` novo na saída de VM.
- As receitas do vm-image mudam para as quatro distros (D8), e as imagens publicadas são
  reconstruídas.

## Questões em aberto (sem resposta do dono; com recomendação)

1. **Um decréscimo no manifesto** planeia hoje `Replace` (recusado sem `--replace`).
   *Recomendo:* uma acção de plano nova, «reiniciar» (stop + aplicar + start, mantendo o overlay),
   para um decréscimo e uma mudança de tecto convergirem sem destruir o disco.
2. **Mudar o tecto** de uma VM existente. *Recomendo:* só com a VM parada, pelo
   `vm resize --vcpus-max/--memory-max` e, se a Questão 1 for aceite, pela acção «reiniciar».
3. **NIC a quente no CH** (D7). *Recomendo:* depois da fatia actual do programa NaaS, porque mexe
   na linha de controlo do holder e no anti-spoof que o trabalho de rede também está a mudar.
4. **Janela no laboratório Proxmox** (`pve`/`pve2`). *Recomendo:* uma janela antes de a fase 1
   sair, para medir `hotplug` + `numa` + `pending` + `guest-get-vcpus` num caso `live.rs` antes de
   declarar.
5. **A activação de CPU no CH** não tem canal no convidado. *Recomendo:* ficar `partial` no CH e
   avaliar um agente por vsock num ADR posterior, em vez de declarar completo um acrescento de CPU
   no CH com base na palavra do hipervisor.

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
- Se cada distro já traz uma regra de online de CPU/memória (a D8 mede-o).
