# Requisitos de laboratório — o que falta, por folhas que desbloqueia

> **Como ler.** Cada linha é uma peça de infra-estrutura que **não existe neste
> host** e as folhas classe `E` que ela liberta. Ordenado pelo número de folhas,
> não pela dificuldade: a decisão de investir é do dono, e o que ela precisa é
> do retorno, não da minha opinião sobre o esforço.
>
> **Medido** a 2026-10-18, commit `a24731e3`, com `scripts/lab_preflight.sh`
> (**rc=1**, duas peças a faltar) e sondagem directa de cada ferramenta.

## O que este host JÁ tem

O `lab_preflight.sh` passa em 25 das 27 verificações. Está tudo cá para a parte
difícil: user namespaces sem privilégio, `/etc/subuid`+`/etc/subgid`
(`walter:100000:65536`), `br_netfilter` com `bridge-nf-call-iptables=1`,
`/dev/kvm` com leitura e escrita, `virsh`, `qemu-img`, `qemu-system-x86_64`,
`cloud-hypervisor`, `cloud-localds`, `virt-customize`, o EDK2 `CLOUDHV.fd`,
`qemu:///system` acessível, `slirp4netns`, `nft`, `ip`, `conntrack`,
`newuidmap`, `protoc`, `wg`, e 138 GiB livres.

Também presentes, e importam: `critest` e `crictl` **v1.36.0** — exactamente a
versão dos dois baselines gravados —, `kubeadm`, `kubectl` e `kind`.

Logo o laboratório local cobre containers, rede rootless, e os **dois** backends
de VM. O que falta é tudo externo ao nó, mais duas peças de cgroup.

---

## A tabela, por retorno

| # | peça que falta | folhas que desbloqueia | quem a pode pôr |
|---|---|---:|---|
| 0 | **`socat`** (um pacote) | **5** folhas da CLI, **sem uma linha de código** | dono (`apt install socat`) |
| 1 | **Registry OCI local** | **5** folhas da CLI | eu, se autorizares |
| 2 | **`docker` + `podman` numa VM de laboratório** | a **Fase 5 inteira** | dono (instalação) |
| 3 | **Nó Proxmox VE** (API + credenciais) | **~10** | dono |
| 4 | **Delegação de `cpuset` e `io`** | os limites de cpuset/io | dono (precisa de root) |
| 5 | **Appliance OPNsense que arranque** | **4** células de Kind + o `live.rs` | dono |
| 6 | **Imagem de VM arrancável no store isolado** | **2** folhas da CLI | eu, se autorizares |
| 7 | **`buf` + `protoc-gen-openapi`** | destranca o `contract_gate.py` | dono (instalação) |
| 8 | **Imagem `kindest/node`** | **1** folha | eu, se autorizares |
| 9 | **Imagem golden + VMs reais** | **1** folha | eu, se autorizares |
| 10 | **VG LVM / zpool / sistema btrfs reais** | **33** operações de provider | dono |
| 11 | **Appliance TrueNAS** | o `live.rs` do TrueNAS (5 ops) | dono |
| 12 | **Servidor PowerDNS + zona** | o `live.rs` do PowerDNS (2 ops) | dono |
| 13 | **NAS com NFS** | os volumes de share | dono |
| 14 | **`cosign` / `syft`** | só interoperabilidade | dono (instalação) |
| 15 | **2.º nó Proxmox + storage partilhado** | **4** casos live que nunca passaram | dono |

---

## 0. `socat` — 5 folhas por um pacote, e é o melhor retorno de todos

```bash
sudo apt install socat
```

Isto não é uma lacuna de cobertura: a bateria **já tem** os cinco checks de
`net netns exec/detach/firewall/publish/unpublish`
(`scripts/e2e.sh` ~7200-7250), e julgam no **fio** — dois netns, A responde um
token por `socat`, B consome-o com `nc`. O bloco salta num
`command -v socat`, e o `socat` não está no host; o `nc` está.

Medido a 2026-10-18: o bloco entrou em `5c62db13` (2026-10-11), **antes** do
trace de `e5be0903` (2026-10-16). Logo as cinco folhas contaram zero por o
bloco ter saltado, não por não haver checks. Instalado o pacote, uma corrida
leva a CLI de 254 para **259** sem uma linha de código novo.

A alternativa — reescrever o servidor do fixture com o `nc` que já existe —
mexe num check que funciona para contornar uma falta do host, e trocaria um
`socat` com `fork,reuseaddr` por um `nc -k` com semântica diferente. Não vale
o risco por um `apt install`.

## 1. Registry OCI local — 5 folhas, e é o melhor retorno do conjunto

Desbloqueia `image push`, `image sign`, `vm image pull`, `vm image push` e
`vm image ls-remote` — cinco das dezanove folhas que nenhuma corrida executa.

Nem `registry` nem `skopeo` estão no host. Basta um registry local sem TLS no
`127.0.0.1`, levantado e derrubado pela própria bateria; não precisa de ser um
serviço permanente nem de credenciais reais.

**E `image sign` NÃO precisa de `cosign`**: o motor tem ECDSA-P256 próprio, com
a chave gerada à primeira utilização em `<DELONIX_ROOT>/keys/image-signing.key`.
O `cosign` só serviria para provar interoperabilidade da assinatura (linha 14).

## 2. `docker` + `podman` numa VM — a Fase 5 inteira

Nenhum dos dois existe aqui, e instalá-los **neste** host está fora de âmbito:
é a máquina de desenvolvimento, e a própria `comparacao-medida.md` registou em
2026-08-13 que não se mexe nela por isso mesmo. A comparação correu numa VM
`libvirt` criada pelo próprio motor, e é esse o caminho a repetir.

**Há um segundo bloqueio, e é independente.** O `scripts/bench_gate.py` é
**por máquina** por desenho, e a única entrada da `bench_baseline.json` é de uma
**AMD Ryzen 9 8940HX, 32 threads** (2026-09-23, delonix 4.3.0). Este host é um
**Intel i9-13900H, 20 threads** — máquina desconhecida, e o gate **recusa julgar**
(sai 3, nunca um verde silencioso). Logo a bancada tem de correr na VM e gravar
a sua própria entrada, ou a Fase 5 não tem veredicto.

## 3. Nó Proxmox VE — ~10 folhas

Com credenciais (`DELONIX_PROXMOX_TEST_URL` + token ou utilizador/senha)
destranca: as 4 células de Kind do `NetworkZone` (`apply`, `plan`, `diff`,
`drift`), o `apply` do `SystemContainer` (hoje só recusas), e as 5 rotas que a
matriz do Proxmox marca `supported+untested`.

## 4. `cpuset` e `io` delegados — precisa de root

As duas únicas falhas do preflight:

```
MISS  cgroup controller cpuset delegated — missing from
      /sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/cgroup.subtree_control
MISS  cgroup controller io delegated — idem
```

```bash
sudo mkdir -p /etc/systemd/system/user@.service.d
printf '[Service]\nDelegate=cpu cpuset io memory pids\n' \
  | sudo tee /etc/systemd/system/user@.service.d/delegate.conf
sudo systemctl daemon-reload && sudo systemctl restart user@1000.service
```

O `daemon-reload` sozinho **não chega**: um `user@` a correr não muda de
controladores. Está medido no `AGENTS.md`, e o `docs/lab.md` pede os cinco.

## 5. Appliance OPNsense que arranque — 4 células de Kind

O ADR-0071 já o diz por escrito: *«Not proven live: the lab OPNsense VM does not
boot»*. Enquanto não arrancar, o `NetworkGateway` fica com `apply`, `plan`,
`diff` e `drift` em `E`, e o `live.rs` do OPNsense (4 casos, 15 operações) não
corre.

## 6. Imagem de VM arrancável no store isolado — 2 folhas

`vm pause` e `vm unpause` são reais nos **três** backends
(cloud-hypervisor `:847`/`:851`, libvirt `:3254`/`:3268`, proxmox
`:6488`/`:6497`) e recusam uma VM que não esteja a correr. Falta só uma VM a
correr: o trace de 2026-10-16 diz «no VM images in the isolated store».

O `vm image import` **já é executado** pela bateria, logo o caminho de entrada
funciona — falta o ficheiro. Com `virt-customize` presente, a alternativa é
construí-la (linha 9).

## 7. `buf` + `protoc-gen-openapi` — destranca um gate que hoje chumba

`python3 scripts/contract_gate.py` dá **rc=1** aqui:

```
FAIL  buf is not on PATH — the gate cannot check what it guards
FAIL  protoc-gen-openapi is not on PATH — the gate cannot check what it guards
```

**O comportamento está correcto** — falha fechada, não finge medir. Mas
significa que o contrato do nó (formato, lint, mudanças incompatíveis,
mapeamentos HTTP, OpenAPI) **não é verificável neste host**, e as Fases 4/5
mexem-lhe.

## 10. VG LVM / zpool / btrfs reais — 33 operações sem cobertura alguma

Os três crates (`delonix-provider-lvm`, `-zfs`, `-btrfs`) **não têm `tests/`**.
As ~11 operações de cada (`probe`, `required`, `allocate`, `release`, `usage`,
`create_pool`, `created_here`, `contents`, `destroy_pool`) só têm unidade com um
`Run` falso — nunca correram contra um VG, um zpool ou um sistema btrfs.

E são enviadas e **ligadas** (`delonix-storage-helper/src/main.rs:120-133`,
`cmd/storage_pool.rs:125-134`) enquanto o `capability-matrix.md:196-197` declara
`storage.lvm-thin` e `storage.zfs-btrfs` **`not-implemented`**: a matriz e o
código discordam, e nenhum dos dois está provado.

Um ficheiro-loopback (`losetup`) serve para LVM e btrfs sem hardware dedicado;
ZFS precisa do módulo.

## 15. 2.º nó Proxmox + storage partilhado — 4 casos que nunca passaram

O `delonix-proxmox/tests/live.rs:33-50` declara-o: nove casos nunca passaram
contra um nó. Quatro precisam de um segundo nó e de storage partilhado — os três
`move` de VM e o `move_to` do system container. Logo
`VmBackend::move_to_node` e `SystemContainerProvider::move_to` estão
implementados e **não provados**.

Os outros cinco precisam de: um convidado com guest agent (IP, vCPUs/memória),
uma zona DNS (`DnsProvider::prepare_zone`/`observe`), e um gateway libertado à
mão (a reparação).

---

## Uma nota sobre a etiqueta que a missão pede

O briefing exige que «recursos de teste levem a label
`delonix-test-run=<uuid>`». **Não existe**: `grep -rn 'delonix-test-run'` no
repositório dá **0 ocorrências**, nos nove providers. Não é um requisito de
laboratório — é trabalho de código, e entra no plano da Fase 4, não aqui. O que
existe hoje são nomes derivados do PID (TrueNAS, PowerDNS, libvirt), um nome
fixo (`dlxhp-live`, Cloud Hypervisor), nomes por caso (Proxmox) e o
`fresh_mark()` do OPNsense — que é a marca de posse **de produção** (ADR-0059
D1.5), não uma etiqueta de teste.
