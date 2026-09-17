<!-- translated-from: cloud-native-primer.md sha256:e1b5728ca5d6149e326d548d4760188c5664ceea69767235b709a0b9f6c7eb34 -->
# Introdução ao cloud native

O motor é uma camada fina e cuidadosa sobre funcionalidades do kernel Linux e um punhado de
especificações abertas. Esta página dá-te o suficiente de cada conceito para leres o código, e diz
**onde vive** neste repositório. Para aprofundar, segue os links oficiais — são melhores do que
qualquer resumo aqui.

Cada secção tem três partes: o conceito, **No Delonix** (ficheiros e símbolos que podes procurar com
`grep`), e **Ler mais**. Os caminhos são relativos à raiz do repositório.

Para te orientares no ecossistema mais amplo, o [CNCF Landscape](https://landscape.cncf.io/) e o
[CNCF Glossary](https://glossary.cncf.io/) são bons mapas. As comparações com runc, crun, containerd
ou Podman só aparecem onde ajudam a explicar uma escolha de desenho.

---

## 4.1 Namespaces do Linux e funcionamento rootless

Um **namespace** dá a um processo a sua própria vista de um tipo de recurso global. Um container é,
no fundo, um processo arrancado num conjunto novo deles: **mount** (a sua própria árvore de sistema de
ficheiros), **PID** (a sua própria numeração de processos, com ele próprio como PID 1), **network** (as
suas próprias interfaces e rotas), **IPC**, **UTS** (hostname), **cgroup** (a sua própria vista da árvore
de cgroups) e **user**.

O **user namespace** é o que torna possíveis os containers rootless. Lá dentro, um processo pode ser
uid 0 com capabilities completas *sobre os recursos que pertencem a esse namespace*, enquanto no host é
um utilizador comum. O mapeamento entre os uids de dentro e de fora é escrito em
`/proc/<pid>/uid_map` e `gid_map`. Um utilizador sem privilégio só consegue mapear o seu próprio uid;
mapear um *intervalo* exige os helpers setuid `newuidmap`/`newgidmap`, que verificam `/etc/subuid` e
`/etc/subgid`. Algumas distribuições restringem além disso os user namespaces sem privilégio através do
AppArmor — ver [Ambiente](environment.md) para as consequências práticas.

**No Delonix**

- A `fn spawn` em `crates/adapters/delonix-linux/src/lib.rs` constrói as `CloneFlags`
  (`CLONE_NEWNS`, `CLONE_NEWPID`, `CLONE_NEWNET`, `CLONE_NEWUSER`, …) e chama `nix::sched::clone`.
  A partilha de IPC/UTS entre membros de um pod é tratada por `setns` no `container_init`.
- O `write_userns_maps` no mesmo ficheiro escreve os mapas a partir do pai: um mapa de um só uid
  (`0 <euid> 1`) em rootless, ou um intervalo de subuid através de `newuidmap`/`newgidmap` quando
  `have_subid_helpers()` diz que estão disponíveis. `USERNS_UID_BASE`/`USERNS_RANGE` definem o intervalo
  usado quando se corre como root.
- O `setup_rootfs` monta a raiz do container e chama `pivot_root`; o `container_init` é o código que
  corre dentro dos novos namespaces antes do `execvp`.
- As operações rootless sobre ficheiros que pertencem a subuids mapeados re-executam o binário dentro de
  um user namespace mapeado: `reexec_mapped`, `reexec_mapped_hold`, `remove_tree_mapped`.

**Ler mais:** [`namespaces(7)`](https://man7.org/linux/man-pages/man7/namespaces.7.html),
[`user_namespaces(7)`](https://man7.org/linux/man-pages/man7/user_namespaces.7.html),
[`newuidmap(1)`](https://man7.org/linux/man-pages/man1/newuidmap.1.html),
[`subuid(5)`](https://man7.org/linux/man-pages/man5/subuid.5.html),
[`pivot_root(2)`](https://man7.org/linux/man-pages/man2/pivot_root.2.html).

---

## 4.2 cgroups v2 e delegação

Os **control groups** limitam e contabilizam recursos (memória, CPU, PIDs, I/O) para um conjunto de
processos. O cgroup v2 é uma árvore única montada em `/sys/fs/cgroup`; um directório é um grupo, e
ficheiros como `memory.max`, `cpu.max`, `pids.max` e `memory.events` são a sua interface. Um
controlador só está disponível para um filho se o pai o listar em `cgroup.subtree_control`.

Um utilizador sem privilégio só consegue gerir uma subárvore se ela lhe tiver sido **delegada**. Em
hosts com systemd, o `user@<uid>.service` delega tipicamente alguns controladores, e o
`systemd-run --user --scope -p Delegate=yes` cria um scope delegado a pedido. Um processo num scope de
sessão SSH fica normalmente *fora* da subárvore delegada, e a regra «sem processos internos» impede de o
mover para lá — por isso os limites podem, em silêncio, não se aplicar aí. O motor é só v2.

**No Delonix**

- O modo root coloca os containers debaixo de `delonix_runtime_core::DELONIX_SLICE`
  (`/sys/fs/cgroup/delonix.slice`).
- O modo rootless encontra o cgroup do serviço do utilizador e cria folhas debaixo de
  `<user@uid.service>/dlx-containers` — ver `user_service_base` e `try_delegated_base` em
  `crates/adapters/delonix-linux/src/lib.rs`. O `cgroup_limits_apply` responde a «os limites vão
  aplicar-se neste host?» sem arrancar um container.
- A decisão de desenho sobre o nível intermédio é o
  [ADR-0015](../../adr/0015-intermediate-cgroup-level.md); a forma como o CRI segue a hierarquia de cgroups
  do kubelet é o [ADR-0038](../../adr/0038-cri-follows-kubelet-resource-model.md), com o pai vindo do
  kubelet validado por `KubeCgroupParent::parse` em
  `crates/foundation/delonix-runtime-core/src/lib.rs`.

**Ler mais:** [kernel Linux — Control Group v2](https://docs.kernel.org/admin-guide/cgroup-v2.html),
[systemd — Control Group APIs and Delegation](https://systemd.io/CGROUP_DELEGATION/),
[`cgroups(7)`](https://man7.org/linux/man-pages/man7/cgroups.7.html).

---

## 4.3 Capabilities, seccomp, AppArmor, caminhos mascarados

O poder do root está dividido em **capabilities** (`CAP_NET_ADMIN`, `CAP_SYS_ADMIN`, …). Um container
mantém um pequeno conjunto por omissão e larga o resto. O **seccomp** instala um filtro BPF que permite
ou nega chamadas de sistema, reduzindo a superfície de ataque ao kernel. O **AppArmor** (e o SELinux
noutras distribuições) são Linux Security Modules que confinam um processo por perfil. Por fim, os
runtimes **mascaram** caminhos sensíveis de `/proc` e `/sys` (fazem bind de algo vazio por cima deles) e
tornam outros só de leitura, porque esses ficheiros deixam escapar informação do host ou permitem
controlá-lo.

Um ponto subtil que o código documenta: o `clone3` passa as suas flags através de um ponteiro que um
filtro seccomp não consegue inspeccionar, por isso um filtro que bloqueie `clone(CLONE_NEWUSER)` tem
também de fazer o `clone3` falhar com `ENOSYS`, para obrigar a libc a voltar ao `clone` filtrável.

**No Delonix**

- Capabilities: `KEPT_CAPS` e `resolve_cap_keep` em
  `crates/adapters/delonix-linux/src/capabilities.rs`; `drop_capabilities` em `lib.rs`.
- seccomp: `apply_seccomp` em `crates/adapters/delonix-linux/src/lib.rs` (construído com o crate
  [`seccompiler`](https://docs.rs/seccompiler), incluindo o pré-filtro `clone3` → `ENOSYS`); os perfis
  JSON personalizados são lidos e compilados em `seccomp_profile.rs` (`parse`, `compile`).
- AppArmor: `apply_apparmor` em `lib.rs`.
- Caminhos mascarados e só de leitura: `DEFAULT_MASKED_PATHS`, `DEFAULT_READONLY_PATHS`,
  `apply_masked_paths`, `apply_readonly_paths`, `mask_proc_paths` em `lib.rs`.
- As decisões de segurança ao nível do nó (política, admissão, eventos, score) são um crate puro à parte:
  `evaluate` em `crates/contexts/delonix-security-runtime/src/admission.rs`
  ([ADR-0026](../../adr/0026-security-runtime-decision-crate.md)).

**Ler mais:** [`capabilities(7)`](https://man7.org/linux/man-pages/man7/capabilities.7.html),
[kernel — Seccomp BPF](https://docs.kernel.org/userspace-api/seccomp_filter.html),
[documentação do AppArmor](https://gitlab.com/apparmor/apparmor/-/wikis/Documentation),
[OCI runtime spec — configuração Linux](https://github.com/opencontainers/runtime-spec/blob/main/config-linux.md)
(os campos `maskedPaths`/`readonlyPaths`/`seccomp` que outros runtimes consomem).

---

## 4.4 Imagens OCI, armazenamento endereçado por conteúdo e overlayfs

A **Open Container Initiative** publica três especificações:

- a **image spec** — uma imagem é um *manifesto* (JSON) que aponta para uma *config* e para uma lista
  ordenada de *camadas* (tarballs), e possivelmente um *índice* que aponta para um manifesto por
  plataforma;
- a **distribution spec** — a API HTTP que os registos servem (`/v2/<name>/manifests/<ref>`,
  `/v2/<name>/blobs/<digest>`, autenticação por token);
- a **runtime spec** — como se diz a um runtime como o runc ou o crun para correr um bundle de sistema
  de ficheiros.

Tudo é **endereçado por conteúdo**: um blob é nomeado pelo digest SHA-256 dos seus bytes, por isso um
cliente verifica o que descarregou calculando o hash. Fazer pull por digest (`name@sha256:…`) só é uma
garantia se o *manifesto* for verificado contra esse digest, além de cada blob contra o manifesto.

Em tempo de execução, as camadas são empilhadas com **overlayfs**: `lowerdir`s só de leitura, um
`upperdir` gravável para onde as alterações são copiadas, e um `workdir`. Muitos containers podem
partilhar as mesmas camadas inferiores.

**No Delonix**

- Cliente de registo (distribution spec): `crates/adapters/delonix-oci/src/registry.rs` —
  funções `pull_from_registry*`, os media types de `ACCEPT_MANIFEST`, e
  `verify_manifest_digest`. Os tipos vêm do crate [`oci-spec`](https://docs.rs/oci-spec).
- Store de blobs endereçado por conteúdo: `Cas` em `crates/adapters/delonix-oci/src/cas.rs`.
- Escrever um arquivo de image layout OCI: `write_oci_archive` em `save.rs`.
- Preparação do overlay: o `ImageStore::prepare_overlay` em `overlay.rs` escreve um marcador
  `overlay-lowers` (`LOWERS_FILE`); o mount propriamente dito acontece dentro dos user e mount
  namespaces do container em `mount_overlay_if_marked` (`crates/adapters/delonix-linux/src/lib.rs`),
  usando a nova API de mount — ver [ADR-0016](../../adr/0016-filesystem-under-the-state-root.md) e
  [ADR-0037](../../adr/0037-overlay-mount-new-api.md).
- O motor corre ele próprio os containers, em vez de entregar um bundle de runtime OCI ao runc/crun.

**Ler mais:** [OCI image spec](https://github.com/opencontainers/image-spec),
[OCI distribution spec](https://github.com/opencontainers/distribution-spec),
[OCI runtime spec](https://github.com/opencontainers/runtime-spec),
[kernel — Overlay Filesystem](https://docs.kernel.org/filesystems/overlayfs.html).

---

## 4.5 Rede de containers

Blocos de construção da rede no Linux:

- um **network namespace** tem as suas próprias interfaces, rotas e firewall;
- um **par veth** é um cabo virtual com uma ponta em cada namespace;
- uma **bridge** é um switch virtual que junta muitas pontas veth;
- o **nftables** é o filtro de pacotes e o motor de NAT do kernel; o **DNAT** reescreve um destino
  (é assim que uma porta publicada chega a um container), e o **conntrack** segue os fluxos para que o
  tráfego de resposta de uma ligação permitida passe (`ct state established,related`);
- o **slirp4netns** dá a um network namespace sem privilégio conectividade de saída, emulando uma pilha
  TCP/IP em espaço de utilizador, e encaminha portas do host para dentro dele;
- o **VXLAN** transporta frames L2 sobre UDP entre hosts, e o **WireGuard** cifra um túnel.

O **CNI** (Container Network Interface) é uma especificação em que um runtime executa binários de plugin
(`bridge`, `host-local`, `portmap`, …) com comandos `ADD`/`DEL` e uma configuração JSON de
`/etc/cni/net.d`. Os runtimes de Kubernetes usam-no para a rede dos pods.

**No Delonix**

- A rede rootless não consegue criar interfaces no host, por isso o motor mantém um network namespace
  **holder** de longa duração: um processo *pin* mínimo é dono dos namespaces, e um processo *control*
  reiniciável serve um socket Unix. Ver `start_pin`, `start_control` e `ensure_up` em
  `crates/adapters/delonix-sdn/src/infra.rs`.
- Ligar um workload: `attach_container` (IPAM + comando de controlo) e `do_attach` (veth para dentro da
  bridge, dentro do holder). Os nomes das bridges vêm do `bridge_name` no
  `crates/foundation/delonix-net-rules/src/lib.rs`, que não tem dependências.
- Saída e encaminhamento de portas: `slirp_attach` e `slirp_add_hostfwd` em
  `crates/adapters/delonix-sdn/src/lib.rs` (que lançam o `slirp4netns`); publicação dentro do holder em
  `publish_port`/`do_publish` (`infra.rs`).
- Firewall: `table ip dlxing` com as base chains `fwguard`, `fwdeny`, `fwcont` e o verdict map
  `fwmap` (`FWMAP`), gerados em `infra.rs` (`do_firewall`, `apply_firewall_all`,
  `ns_set_join` para os sets de isolamento de namespace).
- DNS interno (`<name>.<namespace>.delonix.internal`): `dns_server_main`, `handle_dns`,
  `dns_resolve_for`, `dns_resolve_multi_for` em `infra.rs`.
- Redes overlay: `set_vxlan` (`infra.rs`) e os helpers de WireGuard em
  `crates/adapters/delonix-sdn/src/wg.rs`, orquestrados pelo `realize_overlay` em
  `bins/delonix-runtime-bin/src/cmd/network.rs`.
- CNI: `crates/adapters/delonix-sdn/src/cni.rs` — `add`, `del`, `readiness`,
  `attach_named_netns`. O uso rootless é opt-in (`enabled_conf` verifica `DELONIX_CNI=1`); o caminho
  CRI em root usa a cadeia CNI do nó (`root_cni_readiness` em
  `crates/interfaces/delonix-cri/src/runtime_svc.rs`).
- Decisões de topologia: [ADR-0013](../../adr/0013-network-topology.md),
  [ADR-0014](../../adr/0014-runtime-dir-per-root.md).

**Ler mais:** [`network_namespaces(7)`](https://man7.org/linux/man-pages/man7/network_namespaces.7.html),
[`veth(4)`](https://man7.org/linux/man-pages/man4/veth.4.html),
[wiki do nftables](https://wiki.nftables.org/),
[slirp4netns](https://github.com/rootless-containers/slirp4netns),
[kernel — VXLAN](https://docs.kernel.org/networking/vxlan.html),
[WireGuard](https://www.wireguard.com/),
[CNI](https://www.cni.dev/) e a sua [especificação](https://www.cni.dev/docs/spec/).

---

## 4.6 Kubernetes: CRI, kubelet, kubeadm e kind

O **kubelet** é o agente de nó do Kubernetes. Não corre containers ele próprio; fala com um runtime de
containers através da **Container Runtime Interface**, uma API gRPC (`RuntimeService`,
`ImageService`) servida num socket Unix local. O kubelet cria um *pod sandbox*
(`RunPodSandbox`) e depois containers dentro dele. Tem também uma definição de **cgroup driver**
(`systemd` ou `cgroupfs`) que tem de corresponder à forma como o runtime gere os cgroups, senão os
cgroups dos pods e os cgroups dos containers divergem.

O **kubeadm** faz o bootstrap de um cluster em máquinas existentes (`kubeadm init`, `kubeadm join`).
O **kind** corre nós Kubernetes como containers construídos a partir da imagem `kindest/node`.

**No Delonix**

- O `crates/interfaces/delonix-cri` é um servidor CRI `runtime.v1`. O protobuf é
  `proto/api.proto` dentro desse crate, compilado pelo `build.rs` com `tonic-build`.
  O binário é o `src/bin/delonix-cri.rs`.
- O cgroup driver reportado ao kubelet: `engine_cgroup_driver` em `runtime_svc.rs`; o seu doc comment
  regista porque é que a resposta é a que é e o que teria de mudar para ser a outra.
- Um round-trip sobre gRPC real é testado em `crates/interfaces/delonix-cri/tests/grpc_status.rs`.
- Comandos de bootstrap de clusters: kubeadm sobre SSH em `bins/delonix-runtime-bin/src/cmd/cluster.rs`
  (com `kubeadm_config.rs`, `etcd.rs`, `lb.rs`), e clusters locais ao estilo kind em `kindmode.rs`.

**Ler mais:** [Kubernetes — Container Runtime Interface](https://kubernetes.io/docs/concepts/architecture/cri/),
[repositório cri-api](https://github.com/kubernetes/cri-api),
[Configuring a cgroup driver](https://kubernetes.io/docs/tasks/administer-cluster/kubeadm/configure-cgroup-driver/),
[kubeadm](https://kubernetes.io/docs/reference/setup-tools/kubeadm/),
[kind](https://kind.sigs.k8s.io/).

---

## 4.7 Virtualização: KVM, virtio, Cloud Hypervisor, libvirt, cloud-init

O **KVM** é o hypervisor do kernel, exposto como `/dev/kvm`; um **VMM** em espaço de utilizador (QEMU,
Cloud Hypervisor) usa-o para correr convidados. O **virtio** é a família de dispositivos paravirtuais
(disco, rede, partilha de sistema de ficheiros 9p) que os convidados usam para I/O eficiente. O **Cloud
Hypervisor** é um VMM em Rust focado em cargas cloud que pode correr sem privilégio com acesso a
`/dev/kvm`; arranca os convidados através de um firmware (um build UEFI do EDK2 ou o
`rust-hypervisor-firmware`) ou directamente a partir de uma imagem de kernel.
O **libvirt** gere domínios QEMU/KVM descritos em XML, através do `virsh` e do `libvirtd`.

As cloud images são genéricas; a configuração por instância (hostname, chaves SSH, utilizadores, rede)
vem do **cloud-init**, que lê um datasource. O datasource **NoCloud** é um pequeno ISO com a etiqueta
`cidata` que contém `user-data`, `meta-data` e opcionalmente `network-config`.

**No Delonix**

- A porta é o `VmBackend` em `crates/adapters/delonix-vm/src/lib.rs`, implementado por
  `CloudHypervisorBackend` e `LibvirtBackend` aí e por `ProxmoxBackend` em
  `crates/providers/delonix-proxmox` ([ADR-0008](../../adr/0008-proxmox-vm-backend.md)).
- Ordem de procura do firmware do Cloud Hypervisor: `DEFAULT_CH_FIRMWARES` (o `CLOUDHV.fd` do EDK2 antes
  do `hypervisor-fw`); a linha de comandos do VMM é construída no `boot_ch`.
- XML de domínio do libvirt: `libvirt_domain_xml`.
- Geração do seed NoCloud: `generate_seed_iso` em `bins/delonix-runtime-bin/src/cmd/vm.rs`.
- As VMs na SDN rootless recebem um lease DHCP derivado do seu MAC: `dhcp_lease_ip` em
  `crates/adapters/delonix-sdn/src/infra.rs`.
- Configuração prática e armadilhas conhecidas do host: [Configurar microVMs](microvm-setup.md).

**Ler mais:** [kernel — KVM](https://docs.kernel.org/virt/kvm/index.html),
[especificação virtio (OASIS)](https://docs.oasis-open.org/virtio/virtio/),
[Cloud Hypervisor](https://www.cloudhypervisor.org/) e a sua
[documentação](https://github.com/cloud-hypervisor/cloud-hypervisor/tree/main/docs),
[libvirt](https://libvirt.org/docs.html),
[cloud-init NoCloud](https://docs.cloud-init.io/en/latest/reference/datasources/nocloud.html).

---

## 4.8 Reconciliação declarativa

O Kubernetes popularizou um modelo em que os utilizadores submetem o **estado desejado** como objectos
tipados (`apiVersion`, `kind`, `metadata`, `spec`), e os controladores comparam-no repetidamente com o
**estado real** e agem para convergir. O `kubectl apply` acrescenta um **diff de três vias**: guarda a
última configuração aplicada no próprio objecto, para conseguir distinguir «tiraste este campo do teu
ficheiro» (reverte-o) de «alguém definiu este campo à mão» (deixa-o estar).

**No Delonix**

- O motor tem os seus próprios Kinds em grupos de API (o `delonix api-resources` lista-os). Os factos
  sobre cada Kind (domínio, se converge, teardown, namespacing) vivem numa só tabela: `KindFacts`
  em `crates/contexts/delonix-stack/src/kinds.rs`.
- O planeador é puro: `plan(desired, actual, stack)` em
  `crates/contexts/delonix-stack/src/reconcile.rs`. A documentação do módulo tem a tabela de verdade de
  três vias, e o último spec aplicado é guardado no próprio recurso debaixo da anotação `LAST_APPLIED`
  (`delonix.io/last-applied`) — não há um ficheiro de estado à parte.
- A posse é uma label no recurso; o histórico de revisões está em `revision.rs`
  ([ADR-0019](../../adr/0019-stack-revision-history.md)).
- Os manifestos são lidos em `bins/delonix-runtime-bin/src/cmd/manifest.rs`; `stack plan`/`apply`
  vivem em `cmd/stack.rs`.
- Não há nenhum ciclo de controlador a correr em segundo plano: a reconciliação acontece quando um
  comando corre (daemonless). O reconciliador pull proposto mantém essa propriedade por ser um timer do
  systemd que invoca o mesmo apply, e não um processo residente
  ([ADR-0021](../../adr/0021-gitops-pull-reconciler.md), estado *Proposed*).

**Ler mais:** [Kubernetes — Objects](https://kubernetes.io/docs/concepts/overview/working-with-objects/),
[Controllers](https://kubernetes.io/docs/concepts/architecture/controller/),
[Gestão declarativa com `kubectl apply`](https://kubernetes.io/docs/tasks/manage-kubernetes-objects/declarative-config/).

---

## 4.9 Observabilidade e a interface MCP

O **OpenTelemetry** é um padrão da CNCF para traces, métricas e logs, exportados por **OTLP** para um
collector. O **Prometheus** recolhe métricas de um endpoint HTTP `/metrics` num formato de exposição em
texto. O **Model Context Protocol** é um protocolo aberto que permite a clientes de IA descobrir e chamar
ferramentas expostas por um servidor, normalmente sobre stdio.

**No Delonix**

- Logging estruturado e spans OTLP opcionais: `init` em
  `crates/adapters/delonix-telemetry/src/telemetry.rs` (os spans só são exportados quando
  `DELONIX_OTLP_ENDPOINT` está definido; o exportador corre na sua própria thread, para que a CLI
  síncrona não precise de runtime async).
- Registo Prometheus (prefixo `delonix`) e codificação em texto: `crates/adapters/delonix-telemetry/src/metrics.rs`
  (`encode`). A API de gestão local serve `/metrics` em `crates/interfaces/delonix-mgmt/src/lib.rs`.
- MCP: `crates/interfaces/delonix-mcp` (construído sobre [`rmcp`](https://docs.rs/rmcp), transporte stdio),
  com o binário em `bins/delonix-mcp-bin`. Âmbito e limites:
  [ADR-0025](../../adr/0025-mcp-local-ai-control-surface.md).

**Ler mais:** [documentação do OpenTelemetry](https://opentelemetry.io/docs/),
[especificação OTLP](https://opentelemetry.io/docs/specs/otlp/),
[Prometheus — formatos de exposição](https://prometheus.io/docs/instrumenting/exposition_formats/),
[Model Context Protocol](https://modelcontextprotocol.io/).

---

## 4.10 Daemonless, num parágrafo

O containerd e o Docker Engine mantêm um daemon residente que é dono do estado dos containers; o Podman
mostrou que um runtime pode, em vez disso, ser um comando que sai, com processos auxiliares por container
e o systemd para tudo o que tenha de persistir. O Delonix segue o segundo modelo: a CLI faz o trabalho e
sai, o estado são ficheiros debaixo da raiz de estado protegidos por `flock` (ver
[Introdução ao Rust §3.8](rust-primer.md#38-concurrency-and-shared-state)), existe um processo
supervisor por container destacado, o holder de rede só existe enquanto algo precisa dele, e a
persistência no arranque são units do systemd (`bins/delonix-runtime-bin/src/cmd/boot.rs`). As
consequências — boas e más — são discutidas em [Arquitectura](architecture.md) e
[System design interview](system-design-interview.md).
