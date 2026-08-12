---
name: delonix-runtime-vm
description: Domínio do motor de VMs/microVMs do delonix — o trait `VmBackend` e os seus backends (Cloud Hypervisor, libvirt, Proxmox), o ciclo `create_with`/`boot`/`stop`, imagens VM douradas e appliances, cloud-init, e as armadilhas de firmware/consola/rede que já custaram horas. Usa quando mexeres em `crates/delonix-vm`, `crates/delonix-proxmox`, no grupo `delonix vm`/`image vm` da CLI, ou quando construíres/corrigires uma imagem VM.
---

# Motor de VMs — o que já foi medido, e o que não se volta a adivinhar

## Onde as coisas estão

- `crates/delonix-vm/src/lib.rs` — o trait `VmBackend`, o registo `BACKENDS`,
  `create_with`, e os backends locais (Cloud Hypervisor, libvirt).
- `crates/delonix-proxmox` — backend REMOTO, o primeiro. Ver ADR-0008.
- `crates/delonix-runtime-bin/src/cmd/{vm,vmimage,vmfile}.rs` — CLI e imagens.
- `scripts/appliances/` — OPNsense, os quatro Proxmox, TrueNAS.

## O trait, e a fronteira local↔remoto

`backend_for` resolve pelo registo e **um nome desconhecido é ERRO** — acabava
em `_ => CloudHypervisorBackend`, e um default silencioso ali fazia um `stop` de
uma VM libvirt passar pelo caminho errado e deixar o domínio órfão a reportar
sucesso.

Dois métodos com default separam local de remoto, e existem porque o motor
tornava um backend remoto impossível de escrever:

- **`manages_own_storage()`** — o `create_with` resolve `cfg.disk` no filesystem
  LOCAL e constrói um overlay qcow2 **antes** de consultar backend nenhum. Para
  um hypervisor noutra máquina os três passos são sem sentido, e o
  `canonicalize` falhava aqui antes de o backend ser perguntado.
- **`auto_selectable()`** — a única resposta honesta de um backend remoto a
  «estás disponível?» custa uma ida à rede, e a auto-detecção não é sítio para
  pedidos HTTP.

**Não se publica um backend que nunca se viu arrancar uma VM.** É a regra do
spike GO/NO-GO do kind. Enquanto não for exercitado, o `boot` RECUSA-SE com erro
que diz porquê — melhor que existir por escrever.

## As armadilhas medidas

**Um domínio sem `<video>` não arranca certos convidados.** VNC é acesso remoto
a um ecrã; VGA é a máquina TER um. O `<video>` só saía com `--vnc`, e TODAS as
imagens Proxmox — incluindo as originais do fabricante — entram num ciclo
`SeaBIOS → GRUB → reset` sob `qemu -vga none`, sem imprimirem uma linha de
kernel. Isto mascarou-se durante horas porque validar com `--vnc` era cómodo
para depurar.

**`vm stop` faz *undefine* do domínio libvirt** (para não deixar órfãos), logo um
`vm snapshot` exige a VM a correr. E `undefine` precisa de `--managed-save
--snapshots-metadata --nvram`, senão recusa.

**Mas o `--snapshots-metadata` não apaga o snapshot — apaga só o que aponta para
ele.** O estado fica no qcow2 (medido com `qemu-img snapshot -l`), por isso o
`stop` guarda o `snapshot-dumpxml` de cada um em `vms/<vm>/snapshots/` e o `boot`
devolve-os com `snapshot-create --redefine` — que **RECUSA** um XML cujo uuid de
domínio não seja o actual, e o uuid é novo em cada `define`. Antes disto, um
`stop`+`start` deixava `vm snapshots` vazio com rc=0. Ao mexer aqui: nunca
perguntar ao libvirt por uma VM parada — ele só sabe de domínios definidos.

**O `Vm` nunca persistiu tudo o que a `VmConfig` tem** — kernel/initrd/firmware,
seed próprio, volumes 9p, VNC, campos avançados de libvirt só existem como flags
do `vm create`. `vm start`/`restart` reconstroem do registo e cobrem o caso
comum, não substituem o `create` para o resto. Não prometas que substituem.

**Namespace só é aplicável onde a VM está na NOSSA dataplane** — uma VM libvirt
vive na `virbr0`, noutro netns. `--namespace` aí é RECUSADO, nunca ignorado.

**O IP de uma VM CH vem por DHCP** e o lease é determinístico do MAC
(`dhcp_lease_ip`) — a aritmética já esteve duplicada em três sítios; há UMA
função e três consumidores.

## Imagens VM e appliances

**Um appliance não corre cloud-init** (`VmImage.cloud_init: Some(false)`): o
`vm create` salta o seed e RECUSA `--hostname`/`--ssh-key`/`--user-data` a
nomeá-las. `None` (metadados antigos) conta como `true`.

**`source = "from-dhcp"` de um answer file Proxmox significa «obtém por DHCP
DURANTE a instalação e grava como ESTÁTICA»** — não «usa DHCP no boot». As
quatro imagens publicadas carregavam o endereço do slirp do build.

**Só DHCP não chega**: a bridge nomeia uma porta física, e `ens18` num
hypervisor é `enp0s3` no seguinte. `net.ifnames=0` faz dela `eth0` em todo o
lado, que é o único nome verdadeiro.

**`inet dhcp` num guest sem `dhclient` dá zero IPv4 e o `networking.service`
reporta `Finished`.** PBS/PMG/PDM não trazem cliente; o PVE traz por ser
hypervisor — validar um dos quatro e generalizar foi erro.

**O SSH a fechar é o convidado a despedir-se, não a máquina a parar.** Capturar
o disco por tempo depois de um `poweroff` lê um filesystem a meio de ser
escrito, e produz imagens que arrancam e nunca chegam à rede — para correcções
que estavam certas. Espera pelo PROCESSO.

## Método, e é o que custou mais

**O teste que vale é o comando que o utilizador escreve**, não o que é cómodo
para quem investiga. E **quando não souberes o estado, OLHA para ele**:
`delonix vm create --vnc` + `virsh screenshot` dá a resposta em 30 segundos onde
cinco hipóteses deram uma hora. A consola série (`console=ttyS0`, sem `tty0`)
é o que torna um convidado sem rede observável de todo.

Corre `delonix-vm-backend` para acrescentar um backend, e `delonix-testing` para
a disciplina de validação.

## No roteiro de auditoria

Cobre os pontos **1 e 2** no domínio das VMs. A secção «Método» acima é o ponto
**10** em forma de hábito — quando não souberes o estado, OLHA para ele — e as
VMs são hoje a lacuna conhecida da recuperação após falha do plano de controlo
(containers e pods recuperam, VMs não): ver `delonix-producao`. Para acrescentar
um backend, `delonix-vm-backend`. Ordem e relatório em `delonix-auditoria`.
