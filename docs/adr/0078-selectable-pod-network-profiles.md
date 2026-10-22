# ADR-0078: A rede dos pods de um cluster kubeadm é um PERFIL seleccionável, fixado por conteúdo e recusado antes de existir máquina

- **Status:** Accepted (2026-10-15). **D1 a D6 implementados**; **D7 (validação
  ao vivo multi-nó) EXECUTADO no mesmo dia** — e o Cilium **falhou**, com
  causa-raiz medida (bpffs duplicado dentro do container, um defeito do CRI).
  Tudo até ao dataplane passou. Os três perfis ficam `EXPERIMENTAL`, agora por
  medição e não por precaução.
- **Date:** 2026-10-15
- **Deciders:** Walter Angolar
- **Relates to:** ADR-0074 (publicação de `hostPort` no CRI — a sua própria
  secção «o que não foi medido» é a razão de D2), ADR-0038 (o CRI segue o
  modelo de recursos do kubelet), ADR-0065 (dataplane IPv6 — a razão por que
  `ipFamily` é «não implementado» e não «impossível»), ADR-0069 D3 (os plugins
  CNI têm prazo e tecto) e D7 (`KubernetesCluster` fica fora do `stack apply`),
  ADR-0018 (as imagens VM ficam amd64), `docs/discovery/kaas-matrix-2026-10-08.md`
  (gap #1).

## Contexto

O `spec.cni` de um `kind: KubernetesCluster` aceitava exactamente dois valores:

| valor | o que faz | consequência |
|---|---|---|
| `default` | uma conflist `bridge`+`host-local`+`portmap` local ao nó | **recusada acima de UM nó** (DX-6701): não encaminha pods entre nós |
| `none` | não instala nada | todos os nós ficam `NotReady` até alguém aplicar uma CNI à mão |

Ou seja: **este motor não conseguia criar um cluster kubeadm multi-nó
funcional.** A recusa era honesta — o DX-6701 di-lo por extenso — mas era uma
recusa. O gap #1 da auditoria de KaaS (2026-10-08) nomeia-o como o mais grave
da matriz, e nota que o estado «nó `Ready`» era *estruturalmente inalcançável*
por este caminho.

Três factos medidos antes de desenhar, todos contra o código e não contra a
documentação:

1. **Nada na build nomeava os plugins CNI.** O único fornecedor de
   `/opt/cni/bin` era o `.deb` `kubernetes-cni`, a chegar como dependência do
   `kubelet` — e a versão dele **não é fixada de propósito**
   (`download_k8s_debs`, cujo `VERSIONED` cobre só
   `kubeadm`/`kubelet`/`kubectl`, porque o `kubernetes-cni` tem versionamento
   próprio, 1.7.x). O conjunto de binários num nó era o que o `pkgs.k8s.io`
   publicasse no dia da build. A única evidência no repositório de que o
   `portmap` chega a existir é uma medição do ADR-0074 e um teste do
   `scripts/e2e.sh` que **salta** quando o ficheiro falta.

2. **O `valid_cidr` era uma verificação de CONJUNTO DE CARACTERES.** Aceitava
   `9`, `1.2.3.4`, `///` e `0.0.0.0/99` como `podSubnet`, por uma razão sólida
   (o valor é interpolado num `bash -c` remoto, e a defesa contra injecção é
   essa) — mas a falha chegava depois, do `kubeadm`, num host.

3. **Os requisitos de CRI do agente de uma CNI real estão todos implementados,
   e nunca foram exercidos juntos.** Medido a 2026-10-15 renderizando o chart
   Cilium 1.20.2 fixado: o agente pede `privileged: true` (×1),
   `SYS_ADMIN` (×5), `NET_ADMIN` (×4), `SYS_MODULE`, `SYS_RESOURCE`,
   `SYS_PTRACE`, `SYS_CHROOT`, `NET_RAW`, `mountPropagation: Bidirectional`
   (×1) e `HostToContainer` (×4), e `hostNetwork: true` (×3). O bootstrap deste
   motor **já** levanta o tecto de capabilities nos seus nós
   (`DELONIX_CRI_CAP_CEILING=all`, num drop-in visível), o CRI **já** traduz
   `Bidirectional`→`rshared` e `HostToContainer`→`rslave`, e **já** salta o CNI
   em `hostNetwork`. Nenhuma dessas peças foi provada em conjunto.

## Decisão

### D1 — O perfil é um valor do contrato, com uma tabela como única fonte

`spec.cni` passa a aceitar `default | none | cilium | calico | flannel`, e um
bloco `spec.network` novo exprime *como* esse perfil é configurado
(`dataplane`, `kubeProxy`, `mtu`, `nodeMtu`, `ipFamily`, `interface`, `multus`,
`hubble`, `experimental`, `offline`).

Tudo o que se pode decidir sem tocar num host decide-se em
`cmd/net_profiles.rs` — puro, testado sem host — e o lado que toca em coisas
vive em `cmd/net_profiles_install.rs`. **A divisão não é cosmética:** é o que
permite recusar um cluster mau antes de existir uma máquina, e é por isso que
o `cluster kubeadm` (o caminho que CRIA VMs) valida o pedido de rede inteiro
na primeira linha, antes de resolver a imagem.

Nenhum valor por omissão vive em dois sítios. O `k8s_recipes::resolve_k8s_version`
deste repositório tem registado o custo de ter dois: um default independente
derivou **quatro versões menores** do outro, e a build descarregava de um
repositório que a documentação não mencionava.

### D2 — Um perfil não é «suportado» por o manifesto aplicar. Os três nascem EXPERIMENTAL

Cada perfil carrega um nível de suporte **com a razão**, e um `Experimental` é
**recusado** (DX-1705) a não ser que o manifesto o diga pelo nome
(`spec.network.experimental: true` / `--experimental-cni`).

Os três nascem experimentais porque a secção «o que não foi medido» do
ADR-0074 o diz por escrito: *«A real cluster CNI (Calico, Cilium) declaring the
capability differently, and more than one node, were not measured»*. O Cilium é
o perfil que este motor **quer** — e é precisamente por isso que não se declara
suportado antes de o medir. Entregá-los como suportados seria a falha «diz que
sim sobre trabalho que não fez» que este projecto persegue no código dos
outros.

O portão é **declarativo e visível**, não uma variável de ambiente: um operador
que escolheu um perfil não medido tem de o ter escrito.

### D3 — Sem fallback silencioso. Uma combinação que o motor não entrega é uma recusa que nomeia o campo

Sete recusas novas, cada uma com código próprio e cada uma antes de qualquer
máquina existir:

| código | recusa |
|---|---|
| DX-1701 | um `cni` que não existe — e a mensagem **lista os que existem** |
| DX-1702 | um botão que o perfil escolhido não lê (incluindo qualquer botão em `cni: default`/`none`) |
| DX-1703 | uma MTU que não cabe no encapsulamento |
| DX-1704 | CIDR mal formado, com bits fora do prefixo, sobrepostos entre si, sobrepostos a uma rede deste host, ou pequenos demais para os nós |
| DX-1705 | um perfil experimental sem consentimento explícito |
| DX-1706 | `ipFamily` que não `ipv4` — dito como **não implementado**, com o ADR-0065 nomeado |
| DX-6703 | mudar a CNI de um cluster vivo (o cluster fica **intocado**) |
| DX-6704 | artefacto que não se obteve, ou cujos bytes não batem com o digest |

Um cluster que pediu Cilium e recebeu Flannel é pior do que um cluster que
nunca foi criado.

**Porque a MTU é uma recusa e não um aviso:** uma MTU que não cabe não falha.
O dataplane sobe e descarta os pacotes grandes em silêncio, o que se lê como
lentidão inexplicada e não como erro. O `docs/guia-rede.md` já registra esta
armadilha para o overlay cifrado do próprio motor — 1500 − 50 (VXLAN) − 60
(WireGuard) ≈ 1390 — e a lição é a mesma.

**Porque os bits fora do prefixo são recusados e não mascarados:**
`10.244.0.1/16` é quase sempre um erro de escrita de `10.244.0.0/16`.
Aceitá-lo depois de mascarar significa que o manifesto e o cluster discordam
sobre o que foi pedido.

**Modos encaminhados ausentes de propósito.** `native` e BGP exigem que a rede
em volta do cluster transporte as rotas dos pods. Este motor não configura essa
rede e não a vai assumir, por isso esses modos estão **ausentes**, não
aceites-e-partidos. O VXLAN é o primeiro de cada lista por ser o único modo que
não pede nada ao que está entre os nós.

### D4 — O pin é o CONTEÚDO, não a versão

Cada perfil nomeia uma versão **e o SHA-256 do artefacto exacto**. Um URL que
comece a responder bytes diferentes falha fechado em vez de instalar outra
coisa. `latest` não aparece em nenhum caminho reprodutível, e há um teste que o
verifica para cada perfil.

Fixado a 2026-10-15, cada digest obtido do sidecar `.sha256` do publicador **e**
reconfirmado com um `sha256sum` local do ficheiro que chegou:

| artefacto | versão | sha256 (prefixo) |
|---|---|---|
| `containernetworking/plugins` | 1.9.1 | `b98f74a0f8522f0a` (amd64), `56171987d3947707` (arm64) |
| chart Cilium | 1.20.2 | `b2afd87b7f75f875` |
| manifesto Calico | 3.33.0 | `2de8f47595fb9c41` |
| `kube-flannel.yml` | 0.28.10 | `393bdbce53eb50c8` |
| Multus daemonset | 4.3.1 | `bd618d23a9577290` |

**O que estes pins NÃO cobrem, dito sem rodeios:** as imagens de container
*referenciadas dentro* dos manifestos do Calico e do Flannel são fixadas por
TAG pelo upstream, não por digest — `docker.io/calico/node:v3.33.0` é tão
reprodutível quanto o Docker Hub decidir. O chart do Cilium é o caso melhor:
verificado no render, as três imagens dele vêm com `@sha256:`. Resolver as
outras duas para digests exige uma ida ao registry por imagem e **fica como
pendência**, registada como tal.

### D5 — Instalar é esperar. O `kubectl apply` a devolver 0 não é o critério

Um perfil considera-se instalado quando **o agente dele está `Ready` em todos
os nós E todos os nós estão `Ready`**, observado por polling, com prazo
limitado, e com o estado observado registado. O `kubectl apply` a sair 0 só
diz que o apiserver aceitou os objectos.

Um perfil **espera sempre**, independentemente do `--copy-kubeconfig`: a flag
existe para preservar a temporização histórica de um caminho que não instalava
CNI nenhuma, e um caminho que acabou de instalar uma e não olhou é a falha na
sua forma mais pura. Um timeout é **erro** com as contagens
(`agente Ready em A/D nós; N/E nós Ready`), não um aviso — e **não apaga nem
reaplica nada**: uma CNI meio instalada é a única prova que explica a falha.

A instalação acontece **entre o `kubeadm init` e os joins**, como a
documentação do kubeadm pede: um nó que entra num cluster cuja rede já está
instalada recebe o agente ao entrar, em vez de ficar `NotReady` até alguém
notar.

**Idempotência:** não é um `check` que salta, é um apply que converge. Repetir
o mesmo perfil é seguro e é como se retoma uma instalação a meio. Repetir com
OUTRO perfil é DX-6703, antes de obter o artefacto.

### D6 — A imagem leva pré-requisitos; o que ela não configura, verifica-se

**Na golden:** o subconjunto de plugins de referência fixado e verificado
(dez, cada um com a razão escrita, e os oito que nenhum perfil alcança —
`dhcp`, `static`, `vlan`, `vrf`, `ptp`, `sbr`, `tap`, `dummy` — **não vão**:
código inalcançável numa máquina entregue a terceiros é superfície de ataque, o
mesmo raciocínio que o `k8s_node_debloat_steps` já aplica); os módulos e
sysctls dos perfis, no catálogo **partilhado** `k8s_recipes` para a golden e um
host preparado ao vivo não poderem divergir; ferramentas de diagnóstico que a
cloud image não traz (`ethtool`, `conntrack`, `tcpdump`); e proveniência
(`DELONIX_CNI_PLUGINS`, `DELONIX_NET_PROFILES`) no
`/etc/delonix-image-release`.

**A golden continua a não levar nenhuma conflist.** É deliberado e importa: o
kubelet escolhe o primeiro ficheiro de `/etc/cni/net.d` por ordem lexical, e
uma bridge genérica lá dentro seria escolhida **antes** da CNI pretendida. A
ausência é a decisão.

**O que a imagem deliberadamente NÃO configura:** não monta o bpffs e não mexe
na hierarquia de cgroup. As duas coisas já são verdade nestas distribuições (o
systemd monta `/sys/fs/bpf`; a hierarquia unificada é o default desde a 22.04),
e escrever por cima de um sistema que funciona para satisfazer uma checklist é
a maneira de partir o que não era preciso tocar.

**Em vez disso, verifica-se.** O `dist/delonix-node-preflight.sh` mede, por
perfil: swap, módulos, sysctls, o serviço e o socket do CRI, os binários que a
cadeia daquele perfil executa, o vxlan, e — para o Cilium — bpffs, cgroup v2 e
kernel ≥ 5.10; para o Calico, o `rp_filter` e os módulos de iptables. Reporta
**todas** as falhas (a mesma razão do `scripts/lab_preflight.sh`: nomear só a
primeira é a mesma mentira um nível acima), e **nunca sai 0 por não ter
conseguido medir** — um «não sei» conta como falha. O `prepare_host` corre-o
antes do kubeadm e **empurra-o** para um nó cuja golden é anterior a ele.

### D7 — A validação ao vivo multi-nó CORREU, e o Cilium falhou

Corrida a 2026-10-15, dois nós (1 control-plane + 1 worker) em libvirt/NAT, com
a golden construída na mesma sessão. O detalhe está em
`docs/network-compatibility.md` §3.2; o resumo:

**Passou tudo até ao dataplane.** As VMs com endereços distintos (o DX-6702, que
a golden anterior falhava), os dois hosts preparados, `kubeadm init` em 32,4 s,
**o passo de instalação do perfil em 1,0 s** no lugar desenhado (entre o `init`
e o join), o worker a juntar-se em 6,3 s, e os dois nós registados no cluster
com `CONTAINER-RUNTIME: delonix://5.1.1` — este CRI a servir um kubelet real em
dois nós. O DaemonSet do Cilium agendou 2/2.

**O agente do Cilium não arranca:**

```
level=fatal msg="Unable to mount BPF filesystem"
            error="multiple mount points detected at /sys/fs/bpf"
```

O host do nó tem **exactamente um** bpffs, já `shared`. A duplicação é
construída **dentro do container**, por este motor: um container `privileged`
recebe `/sys` por bind recursivo e o pod pede `/sys/fs/bpf` como hostPath — duas
entradas no mesmo ponto. É um **defeito do CRI**, no caminho de mounts de
containers privilegiados, que serve muito mais do que CNIs — por isso não se
corrige nesta mudança, e é a pendência nº1.

**A espera por prontidão falhou honestamente**, que é a decisão D5 a ser
exercida: `✗ … 7m00s` e DX-6704 com as contagens («agent Ready on 0/0 node(s)
and 0/2 node(s) report Ready»), sem apagar nada e nomeando os comandos de
diagnóstico. Um `kubectl apply` com êxito não foi tratado como sucesso.

> **Correcção, 2026-10-20.** Os comandos que o DX-6704 nomeia **não corriam**
> no momento em que eram impressos: resolvem o kubeconfig em cache, e o
> `apply_ssh` só o ia buscar no último passo — depois do perfil e depois da
> espera. Reproduzido num cluster de 3+2 com `--cni flannel`: DX-6704 aos
> 420s, e logo a seguir `DX-1000 … no kubeconfig for cluster`. O D5 continua
> inteiro — o que falhava era a metade accionável da mensagem, não o critério.
> O kubeconfig passa a ser guardado a seguir ao `kubeadm init`; ver
> `docs/network-compatibility.md`, correcção ao L10.

**O que isto prova, e que nenhuma leitura provava:** o portão `EXPERIMENTAL`
estava certo. Um perfil com artefacto fixado, verificado, renderizado com os
valores certos e aplicado com êxito **ainda não funciona**. Declarar o Cilium
suportado porque o código parecia certo teria sido exactamente o relato que o
DX-1705 existe para impedir.

**O Calico e o Flannel continuam NOT RUN.** A tentativa de isolar com flannel
(mesmo par de VMs, perfil sem bpffs) parou porque o `kubeadm reset` deixou o
apiserver a segurar a porta 6443. É o próximo passo, e separa a falha do Cilium
do resto da cadeia.

## Alternativas consideradas

**Instalar com `helm install` em vez de `helm template`.** Rejeitado: deixaria
estado de release dentro do cluster, de onde uma segunda execução pode
divergir, e acrescentaria uma coisa residente a um motor que não tem nenhuma.
O `helm template` corre **no host**, produz um manifesto comum, e o
`kubectl apply` torna-o idempotente. A ausência do `helm` é uma recusa que
nomeia a alternativa (`--cni calico`, `--cni flannel`), nunca um fallback
silencioso para outra CNI.

**Embutir os manifestos no binário.** Rejeitado: amarraria a versão da CNI à
versão do motor, e uma correcção de segurança numa CNI passaria a exigir uma
release do motor.

**Fixar só a versão e confiar no URL.** Rejeitado: é o que o `kubernetes-cni`
já fazia, e é como o conjunto de plugins de um nó passou a ser «o que o
publicador tinha nesse dia».

**Aceitar `native`/BGP porque o upstream os suporta.** Rejeitado: exigem
configuração da rede em volta do cluster, que este motor não faz. Aceitar um
valor que não se consegue entregar é a definição do problema que este ADR
resolve.

**Deixar o `cni: default` crescer para multi-nó.** Rejeitado: uma bridge local
ao nó não tem mecanismo para encaminhar entre nós. Alargá-la seria trocar uma
promessa falhada por outra, mais silenciosa.

## Consequências

**Ganha-se:** um cluster kubeadm multi-nó funcional pela primeira vez; recusas
antes de existir máquina em vez de falhas num host; artefactos reprodutíveis
por conteúdo; e um estado observável (`cluster network status`) que lê o
cluster e não o nosso próprio registo — por isso consegue detectar deriva.

**Custa:** o `helm` passa a ser um pré-requisito do **host** para o perfil
Cilium (na mesma família do `ssh`, `scp`, `qemu-img`, `virt-customize` e
`gpgv`, a que este motor já recorre); a golden ganha ~70 MiB de plugins e
diagnóstico; e três perfis ficam atrás de um consentimento explícito, o que é
atrito deliberado.

**Não se ganha, e não se afirma:** nenhum perfil está provado contra o CRI
deste motor em vários nós. As políticas de rede **não** foram testadas — o
`enforces_network_policy` descreve o que o perfil implementa, não o que foi
medido aqui, e o Flannel diz explicitamente que não entrega nenhuma. O Multus
está implementado e as interfaces secundárias dele não foram exercidas; o
isolamento de uma interface secundária face às políticas da principal **não
foi verificado**, e é o primeiro item do laboratório.

## Pendências, por ordem de importância

1. **O bpffs duplicado dentro de um container privilegiado** — é o que impede o
   Cilium de arrancar (D7). `/sys` entra por bind recursivo E o pod pede
   `/sys/fs/bpf` como hostPath; o agente recusa com «multiple mount points
   detected». Vive no caminho de mounts de containers privilegiados, que serve
   muito mais do que CNIs, por isso é trabalho próprio e não um remendo aqui.
2. **Correr D7 para o Calico e o Flannel**, e promover ou bloquear cada um com
   o resultado. O flannel é o que isola: não precisa de bpffs, por isso um
   cross-node a funcionar nele separa a falha do Cilium do resto da cadeia.
3. **Políticas de rede testadas** (ingress e egress) nos perfis que as
   implementam. Instalar o Cilium não é entregar isolamento.
4. **Resolver as imagens do Calico e do Flannel para digests** (D4).
5. **Isolamento das interfaces secundárias do Multus** — e quem pode criar uma
   `NetworkAttachmentDefinition`.
6. **`prevResult` no `DEL` do CNI e conflist persistida no caminho rootless**:
   o `cni::del` passa `None`, e o `DEL` rootless relê o `/etc/cni/net.d` actual
   em vez do `sb.cni_conf` que o `ADD` gravou. É o ADR-0069 Pendente #8, e toca
   a limpeza do `portmap`.
7. **`--extra-package` é descartado em silêncio no caminho offline** da build
   da golden (o `k8s_customization_steps_offline` não recebe `extra_packages`),
   e a build regista-o no `/etc/delonix-image-release` como se tivesse sido
   instalado. Achado por leitura nesta sessão, fora do âmbito desta mudança.
8. **`k8s_repo_version` tem um default independente** (`stable:/v1.31`) que
   discorda do `DEFAULT_K8S_VERSION` (1.36). Há correcção numa árvore de
   trabalho de outra sessão; não foi duplicada aqui para não conflituar.
