# Laboratório de rede em VMs

Uma rede de empresa em miniatura, declarada em manifestos: DNS autoritativo, DHCP,
autenticação de utilizadores, um posto de trabalho, armazenamento em NAS e uma
firewall de perímetro.

**O guia completo, com a saída medida de cada passo e as armadilhas encontradas,
está em [`docs/guia-vm-lab.md`](../../docs/guia-vm-lab.md).** Este ficheiro é só o
mínimo para pôr o laboratório de pé.

| Ficheiro | VM | Papel | Endereço na LAN | Memória |
|---|---|---|---|---|
| `10-dns.yaml` | `lab-dns` | BIND 9, zona `lab.ngola` | 10.50.0.10 | 1 GiB |
| `20-dhcp.yaml` | `lab-dhcp` | ISC DHCP | 10.50.0.11 | 1 GiB |
| `30-samba.yaml` | `lab-samba` | Samba (autenticação + partilha) | 10.50.0.12 | 1 GiB |
| `40-cliente.yaml` | `lab-cli` | posto de trabalho | 10.50.0.50 (por DHCP) | 1 GiB |
| `50-nas.yaml` | `lab-nas` | TrueNAS SCALE | 10.50.0.20 | 4 GiB |
| `60-storage.yaml` | — | dataset + quota + export NFS na NAS | — | — |
| `70-firewall.yaml` | `lab-fw` | OPNsense | 10.50.0.254 | 2 GiB |

Tudo junto pede ~10 GiB de RAM. O motor recusa criar uma VM que não caiba (reserva
2 GiB para o host), por isso arranque por fases se a máquina for partilhada.

## Pré-requisitos

- `delonix` 0.51.0 ou superior, com libvirt acessível (utilizador no grupo `libvirt`)
- a imagem base e as dos *appliances*:

```bash
delonix image vm ls        # ver o que já existe
delonix vm ls-remote       # ver o que há publicado
```

O laboratório usa `delonix-vm-base:ubuntu-24.04`, `truenas-scale:25.10` e
`opnsense:26.1`.

## Passo 0 — a rede do laboratório

Uma rede **libvirt**, isolada e **sem DHCP** (quem serve DHCP aqui é o `lab-dhcp`).
Note que `kind: Network` **não** serve para isto: cria redes de contentores, não de
VMs — ver a secção 8 do guia.

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

## Passo 1 — a sua chave SSH

Os ficheiros de cloud-init trazem `SSH_KEY_PLACEHOLDER` para poderem ser partilhados
sem a chave de ninguém lá dentro. Substitua-o pela sua:

```bash
mkdir -p /tmp/labrun/cloud-init
KEY=$(cat ~/.ssh/id_ed25519.pub)
for n in dns dhcp samba cliente; do
  sed "s|SSH_KEY_PLACEHOLDER|$KEY|" cloud-init/$n.yaml > /tmp/labrun/cloud-init/$n.yaml
done
for f in 10-dns 20-dhcp 30-samba 40-cliente; do
  sed "s|./cloud-init/|/tmp/labrun/cloud-init/|" $f.yaml > /tmp/labrun/$f.yaml
done
```

## Passo 2 — aplicar

```bash
for f in 10-dns 20-dhcp 30-samba 40-cliente; do
  delonix stack apply -f /tmp/labrun/$f.yaml
done
```

Cada `apply` responde em cerca de 1 s; o cloud-init leva depois **2 a 4 minutos** a
instalar e configurar os serviços dentro de cada VM:

```bash
delonix vm ssh lab-dns -- cloud-init status     # → status: done
```

> **Nunca use `--prune` com estes ficheiros.** O nome do *stack* vem do
> **directório**, portanto os sete ficheiros são o mesmo *stack*: um `--prune` num
> deles vê as outras VMs como órfãs e apaga-as.

## Passo 3 — verificar

```bash
delonix vm ssh lab-cli -- '
  ip -4 -o addr show lab0            # 10.50.0.50 — veio do lab-dhcp
  dig +short @10.50.0.10 dc.lab.ngola A
  smbclient -L //10.50.0.12 -U ana%Lab#2026Ngola
'
```

## NAS e firewall

Estes dois têm passos próprios (criar o disco de dados e a *pool* no TrueNAS,
configurar a LAN do OPNsense pela consola). Estão descritos, com a saída real de
cada comando, nos passos 6 e 7 do [guia](../../docs/guia-vm-lab.md#passo-6--o-nas).

## Desmontar

```bash
for v in lab-dns lab-dhcp lab-samba lab-cli lab-nas lab-fw; do delonix vm rm $v; done
virsh -c qemu:///system net-destroy labnet
virsh -c qemu:///system net-undefine labnet
```

## Credenciais

São de laboratório e estão nos manifestos de propósito, para arrancar sem passos
manuais. **Num ambiente a sério vêm de um `kind: Secret`**, nunca de um ficheiro em
git — e o motor avisa disso a cada `apply`:

| Serviço | Utilizador | Password |
|---|---|---|
| VMs Linux (SSH) | `delonix` | só chave |
| Samba | `ana` | `Lab#2026Ngola` |
| TrueNAS (API e web) | `truenas_admin` | `delonix-admin` |
| OPNsense (consola e web) | `root` | `opnsense` |
