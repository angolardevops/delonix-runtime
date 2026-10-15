# Perfis de rede dos clusters — matriz de compatibilidade e o que foi medido

> **Leia primeiro isto.** Os três perfis (`cilium`, `calico`, `flannel`) são
> **EXPERIMENTAIS** nesta release. Estão implementados, os artefactos estão
> fixados por conteúdo e verificados, e o caminho de instalação existe — mas
> **nenhum foi validado de ponta a ponta contra o CRI deste motor numa
> topologia de vários nós**. Esta página diz exactamente o que foi medido, com
> o quê, e o que não foi. Um número de conformidade que ninguém revalida deixa
> de ser uma medição e passa a ser uma citação.
>
> Decisão e trade-offs: [ADR-0078](adr/0078-selectable-pod-network-profiles.md).

Medido a **2026-10-15**, contra o commit base `67a0ba97`, no host de
desenvolvimento (Linux 7.0.0-38-generic, cgroup v2, KVM).

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
| nível de suporte | **EXPERIMENTAL** | **EXPERIMENTAL** | **EXPERIMENTAL** | suportado | suportado |
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
| 11 | Exemplo novo válido | `delonix manifest validate -f examples/cluster-cilium.yaml` | **PASS** |
| 12 | Testes unitários e de contrato | `cargo test -p delonix-runtime-bin --bin delonix` (suites afectadas) | **PASS** — 212 testes |
| 13 | Requisitos de CRI do agente Cilium | inspecção do DaemonSet renderizado | **PASS por leitura** — ver §3.1 |
| 14 | `fmt`, gate da superfície da CLI, schema publicado, traduções PT | `cargo fmt --check`, `scripts/cli-tree.sh --gate`, os testes de deriva | **PASS** — 273 folhas batem com a linha de base |

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
| B2 | Cluster 1 CP + 2 workers, por perfil | o mesmo, mais 3 VMs | §5 |
| B3 | Criação/remoção de pods, DNS, `ClusterIP`/`NodePort`/`hostPort`, saída externa | precisa de B2 | §5 |
| B4 | Comunicação no mesmo nó e **entre nós** | precisa de B2 — e é o teste que decide o nível de suporte | §5 |
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

## 4. Bug encontrado por estes testes

**A substituição da MTU do Flannel não casava nada.** O
`kube-flannel.yml` v0.28.10 **não tem chave `MTU`** — o `net-conf.json` dele é
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
delonix cluster apply -f examples/cluster-cilium.yaml
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
10. **O `critest` não foi remedido** nesta sessão; o `docs/cri-conformance.md`
    continua a ser do 2026-08-25 e a listar 3 falhas de mount propagation que o
    código actual já trata (`Bidirectional`→`rshared`). Remedir é pendência.

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
