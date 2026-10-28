# Perfis de rede dos clusters — matriz de compatibilidade e o que foi medido

> **Leia primeiro isto.** Os três perfis (`cilium`, `calico`, `flannel`) são
> **EXPERIMENTAIS** nesta release, e o laboratório de dois nós de 2026-10-15
> mostrou porquê: o Cilium **FALHA** neste CRI, com causa-raiz identificada
> (§3.2). Tudo até ao dataplane — golden, preflight, artefactos, instalação,
> espera por prontidão — foi medido e passa; o dataplane do Cilium não.
> Esta página diz exactamente o que foi medido, com o quê, e o que não foi. Um
> número de conformidade que ninguém revalida deixa de ser uma medição e passa
> a ser uma citação.
>
> Decisão e trade-offs: [ADR-0078](adr/0078-selectable-pod-network-profiles.md).

Medido a **2026-10-15**, contra o commit base `67a0ba97`, no host de
desenvolvimento (Linux 7.0.0-38-generic, cgroup v2, KVM). Esta é a linha **v5.1.1**
(ver o `delonix://5.1.1` da etapa L7); é o mesmo motor que hoje embarca como
**0.1.0** (rebrand de versão, sem mudança de código de rede). **Baseline histórica:
os perfis não foram re-medidos contra a tag 0.1.0** — re-medir a matriz de rede é
bancada nova (ver §5, B13).

## 1. Versões fixadas

Cada artefacto é fixado por **versão e SHA-256 do conteúdo**. Um URL que passe
a responder bytes diferentes falha fechado (DX-6704) em vez de instalar outra
coisa. `latest` não aparece em nenhum caminho reprodutível — há um teste que o
verifica por perfil.

| componente | versão | sha256 | origem do digest |
|---|---|---|---|
| `containernetworking/plugins` (amd64) | 1.9.1 | `b98f74a0f8522f0a83867178729c1aa70f2158f90c45a2ca8fa791db1c76b303` | sidecar `.sha256` do publicador + `sha256sum` local |
| `containernetworking/plugins` (arm64) | 1.9.1 | `56171987d3947707c3563db2f4001bccaf50fd63468611b9f3cbecb1375ee7ec` | sidecar `.sha256` do publicador |
| chart Cilium | 1.20.2 | `b2afd87b7f75f875f92a14559f14f59b7babbb479d968e3fd625a20bf30ec20e` | `sha256sum` do tarball obtido |
| manifesto Calico | 3.33.0 | `2de8f47595fb9c41b3f47d7b767a1f8e72ecf84057af834738ff12689a234da5` | `sha256sum` do manifesto obtido |
| `kube-flannel.yml` | 0.28.10 | `393bdbce53eb50c8eb97dd513b218518b7d6e7985dc7b889be956e112a711084` | `sha256sum` do manifesto obtido |
| Multus daemonset | 4.3.1 | `bd618d23a957729071ec81dcef740ffc03a5916102137afa6114d31a5d187843` | `sha256sum` do manifesto obtido |

**Arquitectura.** Só `amd64` está no caminho da golden (ADR-0018). O `arm64`
tem digest fixado para os plugins; qualquer outra arquitectura é **recusada**
em vez de descarregada sem verificação.

**O que os pins não cobrem.** As imagens de container *dentro* dos manifestos
do Calico e do Flannel são fixadas por **tag** pelo upstream, não por digest.
O chart do Cilium é o caso melhor — verificado no render, as três imagens dele
vêm com `@sha256:`:

```
quay.io/cilium/cilium:v1.20.2@sha256:2939231d0d3e3ebddcd80fffa168b7ddcc78fdf0dc864d1c8c126ff523c54f01
quay.io/cilium/operator-generic:v1.20.2@sha256:64d8798350e8569b8e7622563fed6e44dce2625f311e4651b774816516c744fc
quay.io/cilium/cilium-envoy:v1.37.6-...@sha256:af7382699576b9e65e9184efa52eeca0b58aea70ad6e511bf260c91d9f740463
```

Resolver as outras duas para digests é uma pendência, não um detalhe.

## 2. Matriz de capacidades por perfil

| capacidade | cilium | calico | flannel | default | none |
|---|---|---|---|---|---|
| nível de suporte | **EXPERIMENTAL — FALHA neste CRI** (§3.2) | **EXPERIMENTAL** | **EXPERIMENTAL** | suportado | suportado |
| nós | vários | vários | vários | **um só** | — |
| dataplanes renderizados | `vxlan` (omissão), `geneve` | `vxlan` (omissão), `ipip` | `vxlan` | — | — |
| modos de kube-proxy | `iptables` (omissão), `ipvs`, `none` | `iptables` (omissão), `ipvs` | `iptables` (omissão), `ipvs` | `iptables` | `iptables` |
| `NetworkPolicy` **implementada pelo perfil** | sim | sim | **não** | não | não |
| `NetworkPolicy` **testada aqui** | **não** | **não** | n/a | n/a | n/a |
| MTU automática (1500 → ) | 1450 | 1450 (`vxlan`) / 1480 (`ipip`) | 1450 | — | — |
| plugins que a cadeia executa | `loopback`, `portmap` | `loopback`, `portmap`, `bandwidth` | `loopback`, `bridge`, `host-local`, `portmap` | `loopback`, `bridge`, `host-local`, `portmap` | — |
| módulos de kernel | `vxlan` | `vxlan`/`ipip`, `ip_tables`, `nf_conntrack` | `vxlan`, `ip_tables`, `nf_conntrack` | — | — |
| imagens fixadas por digest | **sim** | não (tag) | não (tag) | n/a | n/a |
| Multus como extensão | sim | sim | sim | não | não |
| Hubble | sim | n/a | n/a | n/a | n/a |

> **O Flannel não entrega isolamento.** Uma `NetworkPolicy` num cluster Flannel
> é aceite pelo apiserver e **aplicada por ninguém**. É um perfil de
> laboratório e de diagnóstico do caminho CRI/CNI, e não é equivalente aos
> outros dois. O `delonix cluster network profiles` di-lo em cada execução.

### Versões de Kubernetes

`DEFAULT_K8S_VERSION` = **1.36**. A validação do motor aceita qualquer versão
numérica (`1.36`, `1.36.2`) e trunca para `major.minor` só no caminho do
repositório apt. **Não existe aqui uma matriz medida de perfil × versão de
Kubernetes**: o que está declarado acima é a versão do perfil que este binário
instala, não uma afirmação de que ela foi exercida contra cada versão de
Kubernetes suportada. Construí-la exige o laboratório da §5.

## 3. O que foi medido — e com o quê

### PASS — medido nesta sessão

| # | o que | como | resultado |
|---|---|---|---|
| 1 | Digest de cada artefacto | `delonix cluster network fetch` + `sha256sum` independente | **PASS** — 4 artefactos obtidos e verificados em 3,0 s; os quatro digests confirmados por `sha256sum` fora do motor |
| 2 | Digest dos plugins CNI | `curl` do sidecar `.sha256` + `sha256sum` local do tarball | **PASS** — `b98f74a0…b303` nos dois |
| 3 | Conteúdo do tarball de plugins | `tar tzf` | **PASS** — os 10 plugins de referência presentes; 18 membros no total |
| 4 | Render do chart Cilium com os valores do motor | `helm template` com a lista exacta de `cilium_helm_values` | **PASS** — 22 objectos; `routing-mode: tunnel`, `tunnel-protocol: vxlan`, `mtu: 1450`, `cluster-pool-ipv4-cidr: 10.244.0.0/16`, `kube-proxy-replacement: false`, `/opt/cni/bin` e `/etc/cni/net.d` — todos presentes no ConfigMap |
| 5 | Substituição nos manifests **reais** | teste `the_substitution_lands_on_the_real_upstream_manifests` sobre os artefactos em cache | **PASS** — e **encontrou um bug real**: ver §4 |
| 6 | Preflight de nó, os 5 perfis | `sudo sh dist/delonix-node-preflight.sh <perfil>` no host real | **PASS** — cada perfil produz veredictos distintos e correctos; detectou swap activa, ausência do `delonix-cri`, e confirmou bpffs/cgroup v2/kernel 7.0/vxlan |
| 7 | Preflight falha fechado sem root | execução como uid 1000 | **PASS** — `FAIL` + rc=1, nunca rc=0 |
| 8 | Preflight recusa uso incorrecto | argumento inválido | **PASS** — rc=2 |
| 9 | `shellcheck` do preflight | `shellcheck dist/delonix-node-preflight.sh` | **PASS** — limpo |
| 10 | As 10 recusas, ponta a ponta pela CLI real | `delonix cluster apply -f` com cada manifesto inválido | **PASS** — DX-1701/1702/1703/1704/1705/1706 e o aviso de campo desconhecido, todos com rc=1 |
| 11 | Exemplo novo válido | `delonix manifest validate -f examples/cluster/kubernetescluster.yaml` | **PASS** |
| 12 | Testes unitários e de contrato | `cargo test -p delonix-runtime-bin --bin delonix` (suites afectadas) | **PASS** — 212 testes |
| 13 | Requisitos de CRI do agente Cilium | inspecção do DaemonSet renderizado | **PASS por leitura** — ver §3.1 |
| 14 | `fmt`, gate da superfície da CLI, schema publicado, traduções PT | `cargo fmt --check`, `scripts/cli-tree.sh --gate`, os testes de deriva | **PASS** — 273 folhas batem com a linha de base |
| 15 | **Build da golden a sério** (era B1) | `delonix vm image build -t k8s-golden --k8s-version 1.34 --offline`, raiz isolada `/var/tmp/delonix-lab`, binários release deste ramo, sob um piso de disco de 8 GiB | **PASS** — `rc=0`, 608 MiB, 49 passos de `virt-customize`, nunca chegou ao piso |
| 16 | `/etc/machine-id` da golden | `virt-ls -l -a … /etc/` | **PASS** — **0 bytes**. A golden de 2026-07-17 tinha **33** (`6bd6963d409a14986c04b3b6eed4c818`) — é o DX-6702 medido no artefacto |
| 17 | Os 10 plugins de referência, e só eles | `virt-ls -a … /opt/cni/bin` | **PASS** — `bandwidth bridge firewall host-device host-local ipvlan loopback macvlan portmap tuning`. Os 8 excluídos e o `LICENSE`/`README.md` ausentes — ver §4.1, porque à primeira NÃO foi |
| 18 | Preflight instalado como COMANDO | `virt-ls -l -a … /usr/local/bin` | **PASS** — `delonix-node-preflight`, `0755`, ao lado de `delonix` e `delonix-cri` |
| 19 | Proveniência na imagem | `virt-cat -a … /etc/delonix-image-release` | **PASS** — `DELONIX_CNI_PLUGINS="v1.9.1 (amd64, sha256:b98f74a0f8522f0a)"` e `DELONIX_NET_PROFILES="cilium:1.20.2,calico:3.33.0,flannel:0.28.10,multus:4.3.1"` |
| 20 | Módulos e sysctls dos perfis persistidos | `virt-cat` de `/etc/modules-load.d/delonix-cni.conf` e `/etc/sysctl.d/delonix-cni.conf` | **PASS** — os 9 módulos (`vxlan geneve ipip ip_tables nf_conntrack ip_vs ip_vs_rr ip_vs_wrr ip_vs_sh`) e os 4 sysctls |
| 21 | Diagnóstico de rede na golden | `virt-ls` de `/usr/bin` e `/usr/sbin` | **PASS** — `tcpdump`, `ethtool`, `conntrack` |
| 22 | A golden não leva conflist | `virt-ls -l -a … /etc/cni/net.d` | **PASS** — só `.kubernetes-cni-keep`, 0 bytes, do `.deb`. Não é `*.conflist`/`*.conf`, e o `cni::list_conf_files` filtra por extensão, logo é invisível à camada CNI |

### 3.2 O laboratório de dois nós — o que correu, e onde parou

Corrido a **2026-10-15**, raiz isolada `/var/tmp/delonix-lab`, golden construída
nesta sessão, `cluster kubeadm --control-plane 1 --workers 1 --cni cilium
--experimental-cni` em libvirt/NAT (virbr0).

| # | etapa | resultado |
|---|---|---|
| L1 | Provisionamento das 2 VMs | **PASS** — 16,1 s + 19,4 s |
| L2 | **Endereços DISTINTOS** (o DX-6702) | **PASS** — `labcil-cp1` 192.168.122.114, `labcil-w1` 192.168.122.15, com Client-IDs diferentes nos leases do dnsmasq. A golden de Julho dava **um** lease partilhado |
| L3 | Preparação dos 2 hosts (receitas + CRI + CLI + preflight) | **PASS** — 3,6 s; detectou e substituiu o CLI mais antigo que a golden trazia |
| L4 | `kubeadm init` | **PASS** — 32,4 s, depois de corrigido o bug da versão (§4.0) |
| L5 | **Instalação do perfil** (o passo que não existia) | **PASS** — 1,0 s, entre o `init` e o join, como desenhado |
| L6 | `kubeadm join` do worker | **PASS** — 6,3 s |
| L7 | **Os dois nós registam-se no cluster** | **PASS** — `kubectl get nodes` mostra os 2, `v1.34.12`, e **`CONTAINER-RUNTIME: delonix://5.1.1`** — este CRI a servir um kubelet real em dois nós |
| L8 | O DaemonSet do Cilium agenda | **PASS** — `DESIRED 2, CURRENT 2` |
| L9 | **O agente Cilium arranca** | **FAIL** — `CrashLoopBackOff` nos dois nós |
| L10 | A espera por prontidão **falha honestamente** | **PASS** — `✗ … 7m00s` e `DX-6704`: «its agent is Ready on 0/0 node(s) and 0/2 node(s) report Ready … nothing was deleted or rolled back». Não reportou sucesso, não apagou a prova, e nomeou os comandos de diagnóstico — mas **esses comandos não corriam**, ver a correcção a seguir à tabela |
| L11 | Cross-node, DNS, ClusterIP, MTU, NetworkPolicy | **NOT RUN** — dependem de L9 |

**Correcção ao L10, medida a 2026-10-20 e já corrigida no código.** O que o
L10 observou continua verdade: a espera falha, com as contagens, sem apagar
nada. O que ninguém experimentou nessa sessão foi **correr** os dois comandos
que o DX-6704 nomeia. Não corriam: `delonix cluster <nome> -n <ns> describe
daemonset <ds>` resolve o kubeconfig em cache
(`<root>/clusters/<nome>-kubeconfig.yaml`), e o `apply_ssh` ia buscá-lo no
**último** passo — depois da instalação do perfil e depois da espera. A única
corrida que imprime o conselho era, por construção, a corrida que regressava
antes de o ficheiro existir. Reproduzido a 2026-10-20 num cluster de 3
control-planes + 2 workers com `--cni flannel`: `DX-6704` aos 420s e depois
`error[DX-1000] … no kubeconfig for cluster 'ngolacloud'`, com a directoria do
cluster a conter apenas `id_ed25519`, `id_ed25519.pub` e
`network-profile.json`.

O kubeconfig passa a ser guardado **logo a seguir ao `kubeadm init`**, que é o
primeiro momento em que o `admin.conf` existe; no caminho de sucesso não custa
nada, e nos de falha é a diferença entre um conselho e um segundo erro. Dois
testes seguram a ordem: a do plano (`bootstrap_plan`) e a das chamadas reais
dentro do `apply_ssh` (`tests/architecture.rs`).

**A lição, que é a mesma do §4:** nomear comandos de diagnóstico não é o
critério — corrê-los é. Um «PASS» dado à redacção de uma mensagem não viu
metade do que a mensagem promete.

**A causa-raiz de L9, medida:**

```
level=info  msg="Mounting BPF filesystem" bpffsRoot=/sys/fs/bpf
level=fatal msg="Unable to mount BPF filesystem" error="multiple mount points detected at /sys/fs/bpf"
```

E o host do nó tem **exactamente um** bpffs, já `shared` (bom para propagação):

```
35 24 0:30 / /sys/fs/bpf rw,nosuid,nodev,noexec,relatime shared:11 - bpf bpf rw,mode=700
```

Logo a duplicação é **construída dentro do container**, por este motor. A
hipótese que o código suporta: um container `privileged` recebe `/sys` por bind
recursivo do host (`delonix-linux`, o caminho que cai no bind quando um `sysfs`
fresco é recusado numa userns) **e** o pod pede `/sys/fs/bpf` como hostPath —
duas entradas no mesmo ponto, que é precisamente o que o Cilium recusa. O
registo do container confirma o par `"source": "/sys/fs/bpf"` /
`"target": "/sys/fs/bpf"`.

**Isto é um defeito do motor, no CRI, não do perfil.** Está registado como
pendência própria: é a primeira coisa a corrigir para o Cilium sair de
experimental, e não se corrige nesta mudança porque vive no caminho de mounts
de containers privilegiados, que serve muito mais do que CNIs.

**O que a falha PROVA, e que não se podia provar por leitura:** o portão
`EXPERIMENTAL` estava certo. Um perfil cujo artefacto está fixado, verificado,
renderizado com os valores certos e aplicado com sucesso **ainda não funciona**
— e a única razão por que isso se soube é ter-se corrido. Declarar o Cilium
suportado com base no código a parecer certo teria sido exactamente o relato
que o DX-1705 existe para impedir.

**Isolamento com flannel: NOT RUN.** A tentativa (mesmo par de VMs, perfil que
não precisa de bpffs) parou porque o `kubeadm reset` deixou o apiserver a
segurar a porta 6443 no control-plane, e a limpeza mais funda foi bloqueada
pelo ambiente desta sessão. É o próximo passo e isolaria a falha do Cilium do
resto da cadeia.

### 3.1 Requisitos de CRI do agente Cilium — PASS por leitura, não medido ao vivo

Extraído do DaemonSet renderizado (o que o agente realmente pede):

| o agente pede | este motor | onde |
|---|---|---|
| `privileged: true` (×1) | implementado, e o bootstrap **levanta o tecto** nos seus nós | `cluster.rs` (`DELONIX_CRI_CAP_CEILING=all`, drop-in) |
| `SYS_ADMIN` ×5, `NET_ADMIN` ×4, `SYS_MODULE`, `SYS_RESOURCE`, `SYS_PTRACE`, `SYS_CHROOT`, `NET_RAW` | cobertos pelo tecto `all` | `cap_ceiling.rs` |
| `mountPropagation: Bidirectional` (×1) | implementado → `rshared` | `lifecycle.rs` → `delonix-linux` (`MS_SHARED\|MS_REC`, `mount()` próprio) |
| `mountPropagation: HostToContainer` (×4) | implementado → `rslave` | o mesmo |
| `hostNetwork: true` (×3) | implementado; o CNI é saltado | `lifecycle.rs` |
| bpffs em `/sys/fs/bpf` | **verificado** pelo preflight (não configurado) | `dist/delonix-node-preflight.sh` |
| cgroup v2 unificado | **verificado** pelo preflight | o mesmo |

**Todas as peças existem. Nenhuma foi exercida em conjunto.** É isto, e só
isto, que o nível `EXPERIMENTAL` significa.

### BLOCKED — não executado, com a razão

| # | o que | porquê | desbloqueia com |
|---|---|---|---|
| B1 | Build da golden a sério | **5 GiB livres em 938 GiB (100% ocupado)**. O `virt-resize`/`virt-customize` expande o qcow2 e não cabe; encher a raiz de uma máquina partilhada não é uma troca aceitável | ~40 GiB livres |
| B2 | Cluster 1 CP + 1 worker | **JÁ CORRIDO** para o cilium — ver §3.2. O cluster sobe, os nós registam-se, o agente não arranca | corrigir o bpffs duplicado |
| B3 | Criação/remoção de pods, DNS, `ClusterIP`/`NodePort`/`hostPort`, saída externa | precisa de B2 | §5 |
| B4 | Comunicação no mesmo nó e **entre nós** | **NOT RUN** — depende de um agente que arranque (§3.2 L9) | corrigir o bpffs duplicado, ou isolar com flannel |
| B5 | MTU com pacotes grandes | precisa de B2 | §5 |
| B6 | `NetworkPolicy` ingress e egress | precisa de B2 | §5 |
| B7 | Reinício de agentes e de nós | precisa de B2 | §5 |
| B8 | Falha parcial no `ADD` e limpeza pelo `DEL` | precisa de B2 + `crictl` no nó | §5 |
| B9 | Ausência de fugas de IPAM/interfaces/regras após ciclos | precisa de B2 | §5 |
| B10 | Modo offline ponta a ponta | precisa de B2 (a **obtenção** verificada já está PASS, #1) | §5 |
| B11 | Multus: interfaces adicionais e **isolamento** delas | precisa de B2 | §5 |
| B12 | Repetição do bootstrap (idempotência ao vivo) | precisa de B2 | §5 |
| B13 | `critest` remedido | não corrido nesta sessão | `scripts/critest.sh` |

### NOT RUN

- **Matriz perfil × versão de Kubernetes.** Só a versão por omissão (1.36) é
  nomeada; nenhuma combinação foi exercida.
- **Providers além do libvirt.** Nada aqui foi corrido contra Proxmox ou Cloud
  Hypervisor, e **não se extrapola** de um provider para outro.
- **Políticas de rede.** Ver B6. Instalar o Cilium **não é** entregar
  isolamento, e esta página não o afirma.

## 4. Bugs encontrados por estes testes

### 4.0. `--kubernetes-version=v1.34` — o kubeadm recusa, e TODOS os exemplos usavam isso

Encontrado na **primeira corrida real de dois nós**, depois de as VMs existirem e
os hosts estarem preparados:

```
kubeadm init --kubernetes-version=v1.34
error: version "v1.34" doesn't match patterns for neither semantic version nor
labels (stable, latest, ...)
```

O motor construía a flag como `v{spec.k8sVersion}` tal e qual. O kubeadm exige
**semver completo**. O `valid_version` aceita major.minor, a ajuda da flag diz
«minor version», e os cinco exemplos entregues usam `k8sVersion: "1.34"` —
logo **todas as invocações documentadas deste caminho morriam no `kubeadm
init`**.

É o **espelho exacto** do bug que o `k8s_repo_version` já registra: lá uma
versão com patch quebrava o URL do repo apt, que só existe a nível minor, e a
correcção truncava. Ninguém verificou a outra direcção — os dois consumidores
do mesmo campo querem formas opostas.

**Porque se omite a flag em vez de inventar `.0`:** medido na golden que esta
sessão construiu, o kubeadm instalado é `1.34.12` e o `preseed_k8s_images`
cacheou imagens `v1.34.12`. Pedir `v1.34.0` faria o kubeadm querer imagens que
o nó **não tem** — transformando um nó offline a funcionar num que vai calado à
rede, ou falha. Omitir faz o kubeadm usar a versão dele próprio, que por
construção é a que a golden instalou e a que foi pré-semeada. Um semver
completo passa intacto: quem fixou um patch quis fixá-lo.

### 4.1. Os 8 plugins que eu dizia não ir, iam

A primeira golden construída trouxe **todos os 18** plugins mais `LICENSE` e
`README.md`. O `.deb kubernetes-cni` chega como dependência do kubelet e é dono
de 21 caminhos em `/opt/cni/bin`, por isso extrair dez binários verificados por
cima dos dele deixava os outros oito intactos — enquanto o cabeçalho do módulo
e o ADR-0078 afirmavam que não iam, com superfície de ataque como razão.

**Nenhum teste unitário o podia apanhar:** os testes verificam o conteúdo de
`REFERENCE_PLUGINS`, e os ficheiros extra vêm de um `.deb` que teste nenhum
corre. A imagem era o único sítio onde aquela afirmação era verificável.

Corrigido com um prune, e verificado numa golden reconstruída. O custo está
escrito no código: o `dpkg` continua a achar-se dono dos ficheiros removidos.

### 4.2. A substituição da MTU do Flannel não casava nada

O `kube-flannel.yml` v0.28.10 **não tem chave `MTU`** — o `net-conf.json` dele é
`Network`, `EnableNFTables` e `Backend`. O código substituía uma linha `"MTU":`
que não existe, por isso o Flannel teria autodetectado a MTU do **nó** e cada
pod pagaria o cabeçalho VXLAN de 50 bytes duas vezes: pacotes grandes
descartados em silêncio, que se lê como lentidão inexplicada.

Era exactamente a falha que o cálculo de overhead existe para impedir,
derrotada por uma substituição escrita a partir de uma suposição. **Os testes
de excerto passaram sempre**, porque o excerto foi escrito com a mesma
suposição. Corrigido: a MTU é **inserida**, ancorada na linha `"Network":`, e o
teste passou a fazer parse do documento real como YAML e do `net-conf.json`
como JSON, em vez de procurar uma substring.

## 5. Procedimento de laboratório — como desbloquear B1-B12

Pré-requisitos: ~40 GiB livres, `/dev/kvm`, `libvirt`, `virt-customize`,
`helm`, e `scripts/lab_preflight.sh` a passar.

```bash
# 0. Raiz isolada, para não tocar no estado partilhado do host
export DELONIX_ROOT=/var/tmp/delonix-lab
export DELONIX_NET_RUNTIME_DIR=/var/tmp/delonix-lab-net

# 1. A golden, com os plugins fixados e o preflight dentro (B1)
delonix vm image build -t k8s-golden --k8s-version 1.36 --offline
virt-cat -a "$DELONIX_ROOT/vm-images/k8s-golden.qcow2" /etc/delonix-image-release
#   confirmar: DELONIX_CNI_PLUGINS=v1.9.1 (amd64, sha256:b98f74a0f8522f0a)
#              DELONIX_NET_PROFILES=cilium:1.20.2,calico:3.33.0,flannel:0.28.10,multus:4.3.1
#   e que /etc/machine-id está VAZIO (0 bytes) — identidade de cluster na imagem é DX-6702

# 2. Artefactos verificados (já PASS; repetir aqui prova a cache offline)
delonix cluster network fetch

# 3. Um cluster por perfil (B2). Repetir com --cni calico e --cni flannel.
delonix network create lab-net
delonix cluster kubeadm --name lab-cilium --control-plane 1 --workers 2 \
  --network lab-net --cni cilium --experimental-cni --copy-kubeconfig
#   CRITÉRIO: termina sozinho, sem passo manual, e a última linha diz
#   "agente Ready em 3/3 nós; 3/3 nós Ready". Um timeout é FAIL, não "lento".

export KUBECONFIG="$DELONIX_ROOT/clusters/lab-cilium-kubeconfig.yaml"

# 4. Estado e deriva (operação)
delonix cluster network status lab-cilium
#   CRITÉRIO: desired e observed iguais; rc=0.

# 5. Pods, mesmo nó e ENTRE NÓS (B3, B4) — o teste que decide o suporte
kubectl create deployment web --image=nginx:1.27 --replicas=4
kubectl rollout status deployment/web --timeout=180s
kubectl get pods -o wide        # confirmar que caíram em nós DIFERENTES
A=$(kubectl get pod -l app=web -o jsonpath='{.items[0].metadata.name}')
B_IP=$(kubectl get pod -l app=web -o jsonpath='{.items[-1:].status.podIP}')
kubectl exec "$A" -- curl -sS --max-time 5 "http://$B_IP" >/dev/null && echo "PASS cross-node"

# 6. DNS, ClusterIP, NodePort, hostPort (B3)
kubectl expose deployment web --port 80 --name web-cip
kubectl run probe --rm -i --restart=Never --image=busybox:1.36 -- \
  sh -c 'nslookup web-cip.default.svc.cluster.local && wget -qO- http://web-cip'
kubectl expose deployment web --port 80 --type NodePort --name web-np
#   e um pod com hostPort, que exercita o portmap (ADR-0074) sob este perfil

# 7. Saída externa (B3)
kubectl run egress --rm -i --restart=Never --image=busybox:1.36 -- \
  sh -c 'wget -qO- --timeout=5 http://example.com >/dev/null && echo PASS egress'

# 8. MTU com pacotes grandes (B5)
kubectl exec "$A" -- ip link show eth0 | grep -o 'mtu [0-9]*'   # esperado: mtu 1450
kubectl exec "$A" -- ping -c3 -M do -s 1422 "$B_IP"             # 1422+28 = 1450
kubectl exec "$A" -- ping -c1 -M do -s 1500 "$B_IP" && echo "FAIL: devia fragmentar"

# 9. NetworkPolicy ingress E egress (B6) — cilium e calico
kubectl create ns locked
kubectl -n locked run target --image=nginx:1.27
kubectl -n locked apply -f - <<'EOF'
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: deny-all, namespace: locked}
spec: {podSelector: {}, policyTypes: [Ingress, Egress]}
EOF
#   CRITÉRIO: o acesso que passava passa a ser REJEITADO. Se continuar a passar,
#   o perfil NÃO entrega isolamento e isso tem de ficar escrito na matriz.

# 10. Reinícios (B7)
kubectl -n kube-system rollout restart ds/cilium
kubectl -n kube-system rollout status ds/cilium --timeout=180s
delonix vm stop lab-cilium-w1 && delonix vm start lab-cilium-w1
kubectl wait --for=condition=Ready node/lab-cilium-w1 --timeout=300s

# 11. Falha parcial no ADD e limpeza pelo DEL (B8), no nó
#   crictl runp com uma conflist cuja cadeia falha no 2.º plugin; confirmar que
#   nenhum lease de IPAM nem netns sobrevive (`ls /var/lib/cni/networks`,
#   `ip netns list`).

# 12. Fugas após ciclos (B9)
for i in $(seq 1 20); do kubectl run churn-$i --image=busybox:1.36 --restart=Never -- true; done
kubectl delete pod -l run --all --wait
#   CRITÉRIO: contagens de netns, veth, leases de IPAM e regras nft voltam ao
#   valor inicial. Método: scripts/net-lifecycle-leaks.sh.

# 13. Offline (B10)
#   Num host SEM alcance: copiar $DELONIX_ROOT/net-profiles/ e correr o passo 3
#   com --cni-offline. CRITÉRIO: instala sem rede; sem a cache, DX-6704.

# 14. Idempotência (B12)
delonix cluster apply -f examples/cluster/kubernetescluster.yaml
#   CRITÉRIO: converge sem duplicar nada, e rc=0. Com OUTRO perfil: DX-6703, e
#   o cluster fica intocado.

# 15. Multus (B11)
#   --multus, uma NetworkAttachmentDefinition macvlan, um pod com duas
#   interfaces. CRITÉRIO CRÍTICO: confirmar se a interface secundária permite
#   CONTORNAR a NetworkPolicy da principal. Se permitir, documentar como
#   limitação antes de qualquer uso.

# 16. Limpeza
delonix vm rm -f lab-cilium-cp1 lab-cilium-w1 lab-cilium-w2
delonix network rm lab-net
```

**Condição de saída de `EXPERIMENTAL`, por perfil:** B2-B9 e B12 em PASS, mais
B6 para os perfis que declaram implementar `NetworkPolicy`. Qualquer FAIL
mantém o perfil experimental e fica escrito nesta página com o resultado.

## 6. Limitações conhecidas

1. **Nenhum perfil está provado contra este CRI em vários nós.** É o item 1 das
   pendências do ADR-0078.
2. **`ipFamily` só `ipv4`.** `ipv6` e dual-stack são recusados (DX-1706) como
   **não implementados**, com o ADR-0065 nomeado. Para dual-stack hoje:
   `cni: none` e a tua própria CNI.
3. **Dataplanes encaminhados ausentes.** `native` e BGP exigem que a rede em
   volta transporte as rotas dos pods; este motor não a configura.
4. **`kubeProxy: none` é uma decisão de criação.** Muda o `kubeadm init`
   (`skipPhases: addon/kube-proxy`) e não se liga depois.
5. **Mudar de CNI num cluster vivo não é suportado** (DX-6703). O cluster fica
   intocado; a saída é criar outro cluster e migrar as cargas.
6. **O `helm` é pré-requisito do host para o perfil Cilium.** A ausência dele é
   uma recusa que nomeia as alternativas, nunca um fallback silencioso.
7. **As imagens do Calico e do Flannel são fixadas por tag**, não por digest.
8. **O `cni: default` serve um nó.** Não foi alargado: uma bridge local ao nó
   não tem como encaminhar entre nós.
9. **LoadBalancer, BGP e anúncios L2 estão fora desta entrega.** O ADR-0066
   (balanceador L4) está aceite e não construído; nada aqui liga BGP ou
   anúncios L2, e nada aqui cria um segundo dono de um pool de endereços. A
   fronteira mantém-se: rede dos pods (esta página), rede e firewall das VMs
   (`guia-rede.md`), VIP do apiserver (o HAProxy que o `cluster kubeadm`
   provisiona), e Services `LoadBalancer` (não entregue).
10. **O `critest` não foi remedido** nesta sessão (2026-10-15). **Entretanto
    remedido a 2026-10-24** na v5.1.1 → **84/20** (`84 Passed · 20 Failed · 18
    Skipped`, critest v1.36.0); o `docs/cri-conformance.md` foi actualizado e já
    não lista o 79/103 de 2026-08-25 como número corrente. Ver
    `reports/cri-conformance-2026-10-24/`.

## 6.1. Nota de método: porque é que estas conclusões não dependem do binário em disco

Uma sessão paralela avisou — e confirmou-se — que um `CARGO_TARGET_DIR`
PARTILHADO entre worktrees em commits diferentes produz binários **mal
rotulados**: a fingerprint do build-script não inclui o git HEAD, por isso o
`commit:` que o `delonix version` imprime pode ser de outro commit que não o
código que está dentro. Medido às 21:12 de 2026-10-15: o binário em
`target/release/delonix` dizia `commit: ce3582329` (o desta série) e **não
continha a string `cilium`** — era de outro ramo, escrito por cima 4 minutos
depois de a medição do §3.2 terminar.

**O que isso invalida: nada do §3.2 — e a razão importa.** As conclusões não
se apoiam no `delonix version` nem no ficheiro que hoje está naquele caminho,
apoiam-se no COMPORTAMENTO que ficou registado no log da corrida, e esse só
pode vir deste código:

- `Installing the cilium pod network` — passo que só existe aqui;
- `Waiting for the cilium pod network …` e o `DX-6704` com a redacção
  «nothing was deleted or rolled back»;
- `--experimental-cni` aceite como flag;
- e o próprio fix da versão a mudar o observável: `--kubernetes-version=v1.34`
  presente na 1ª corrida, **ausente** na seguinte, com o kubeadm a responder
  `Using Kubernetes version: v1.34.12` por si.

**A lição, que vale mais do que o episódio:** um `cargo build` que termina
`exit 0` não prova que o binário é do commit que se pensa, quando o `target/`
é partilhado. Qualquer medição deste repositório deve ser ancorada em
comportamento observável (uma string que só aquele código emite, uma flag que
só ele aceita) e não na etiqueta de versão. Para medir com o `target/` próprio:
`CARGO_TARGET_DIR=<caminho só teu> cargo build --release` — custa ~10 GB e um
build do zero, e o disco deste host esteve a 100% três vezes no mesmo dia.

**Uma variável que o §3.2 pôde excluir de graça.** Até ao commit `f945b1c7`
(outro ramo), o `--network <nome>` era aceite, gravado e **descartado** no
provider libvirt: todos os nós caíam na rede `default`. A corrida do §3.2 usou
`--network default`, logo o destino era o mesmo com ou sem esse bug — e os dois
nós receberam, medido, **endereços e client-ids distintos**
(`192.168.122.114` e `.15`). Mais: o agente do Cilium morre em
`Mounting BPF filesystem`, a PRIMEIRA coisa que faz, antes de tocar na rede.
Endereçamento, escolha de rede libvirt e partilha de leases não podem explicar
uma falha de construção de mount namespace.

## 7. Diagnóstico e recuperação

**O cluster não fica `Ready`.** O bootstrap já falhou com as contagens
(`agente Ready em A/D nós; N/E nós Ready`) e **não apagou nada**:

```bash
delonix cluster network status <nome>              # desired vs observed
delonix cluster <nome> -n kube-system get pods -o wide
delonix cluster <nome> -n kube-system describe ds <agente>
delonix cluster <nome> -n kube-system logs -l k8s-app=<agente> --tail=100
ssh delonix@<nó> sudo delonix-node-preflight <perfil>   # o que falta NO NÓ
```

**Repetir é a forma de retomar.** O `kubectl apply` é idempotente e a
instalação converge; `delonix cluster apply -f <manifesto>` com o **mesmo**
perfil retoma de onde parou. Com outro perfil, DX-6703 e nada muda.

**Pods `Pending` sem endereço.** Quase sempre um CIDR de pods pequeno demais
para os nós — recusado hoje na validação (DX-1704), mas um cluster criado antes
desta release pode estar nesse estado.

**Lentidão inexplicada com pacotes grandes.** É MTU. `ip link show eth0` dentro
de um pod tem de mostrar a MTU do perfil (1450 em VXLAN sobre 1500), e
`ping -M do -s <mtu-28>` tem de passar enquanto `-s 1500` fragmenta.

**Actualizar um perfil.** Não há caminho suportado para trocar a versão de um
perfil num cluster vivo nesta release. O procedimento é o do upstream (o
`helm upgrade`/`kubectl apply` do próprio projecto), fora do motor, e fica
**fora** do que esta página garante.
