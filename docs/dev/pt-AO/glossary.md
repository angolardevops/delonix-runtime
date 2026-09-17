<!-- translated-from: glossary.md sha256:b3639c47dba48ec2f15561c5c42ff3a9e50f62de763f290882cdc887ce8a4991 -->
# Glossário

As palavras que quem chega encontra neste repositório, com o significado que têm **no Delonix** — que
às vezes é mais estreito do que o significado geral no cloud native. Cada entrada aponta para o sítio
onde o termo é explicado ou implementado. Os caminhos são relativos à raiz do repositório;
`file.rs::symbol` nomeia um símbolo dentro desse ficheiro.

Os termos estão por ordem alfabética. Para o contexto geral (namespaces, cgroups,
OCI, CRI, CNI, KVM), começa por [Introdução ao cloud native](cloud-native-primer.md).

---

**Adapter** — Um crate em `crates/adapters/` que implementa uma porta contra um mecanismo local: o
kernel (`delonix-linux`), o dataplane de rede (`delonix-sdn`), a loja OCI (`delonix-oci`), os
hypervisors locais de VM (`delonix-vm`). Um adapter pode depender de crates de fundação e de
contexto, nunca de um crate de interface. Ver: [Camadas](architecture.md#layers-and-the-allowed-direction),
`scripts/arch_fitness.py::LAYERS`.

**ADR (Architecture Decision Record)** — Um ficheiro Markdown por decisão estrutural, em `docs/adr/`,
com o nome `NNNN-title.md`, escrito em inglês, antes do código. Um ADR aceite nunca é reescrito; um
ADR novo sucede-lhe. Ver: [`docs/adr/README.md`](../../adr/README.md),
[Quando escrever um ADR](contributing-workflow.md#when-to-write-an-adr).

**Apply / plan / prune** — Os três verbos da convergência declarativa. `delonix plan` mostra o que um
apply mudaria e não muda nada (`--detailed-exitcode` sai com 2 quando há alterações);
`delonix apply` faz convergir o manifesto; `--prune` remove também o que a stack possui e o manifesto
já não declara, e nunca corre por omissão. Ver: `crates/contexts/delonix-stack/src/reconcile.rs::plan`,
[Reconciliação declarativa](cloud-native-primer.md#48-declarative-reconciliation).

**CAS (content-addressed storage)** — A loja de blobs de imagens: cada blob vive em
`blobs/sha256/<hex>` no state root, endereçado pelo seu digest, por isso conteúdo idêntico é guardado
uma só vez. A integridade é verificada quando o conteúdo **entra** na loja, não quando é lido: um pull
compara o manifesto, a config e cada layer com o digest esperado (`verify_manifest_digest` e as
comparações de digest em `crates/adapters/delonix-oci/src/registry.rs`). `Cas::read` é uma leitura
simples de ficheiro e não volta a calcular o hash; `Cas::verify` volta a calculá-lo a pedido. Ver:
`crates/adapters/delonix-oci/src/cas.rs::Cas`,
[Estado em disco](architecture.md#state-on-disk).

**CDI (Container Device Interface)** — Uma especificação da CNCF que descreve como expor um
dispositivo (tipicamente uma GPU) a um container. O Delonix só **consome** specs já geradas por uma
ferramenta do fabricante e transforma-as nos mesmos mounts e nós de dispositivo que `-v`/`--device`
produzem; nunca descobre drivers sozinho. Ver: `crates/adapters/delonix-linux/src/cdi.rs`.

**cgroup delegation** — O mecanismo do cgroup v2 que deixa um utilizador sem privilégios gerir uma
sub-árvore de cgroups. Sem ele, os limites de recursos rootless (`-m`, `--cpus`) não podem ser
impostos; o motor detecta isto e recusa um limite que não conseguiria aplicar, em vez de o aceitar em
silêncio. Uma shell aberta por SSH muitas vezes não está delegada; `systemd-run --user --scope -p
Delegate=yes` dá uma que está. `delonix system info` mostra a resposta como `cgroup2 delegated`. Ver:
`crates/adapters/delonix-linux/src/lib.rs::cgroup_limits_apply`,
[cgroups v2 e delegação](cloud-native-primer.md#42-cgroups-v2-and-delegation),
[Delegação de cgroup](environment.md#cgroup-delegation-some-limits-are-refused-others-are-not-enforced).

**CNI (Container Network Interface)** — O padrão de plugins que o Kubernetes usa para dar rede a um
pod. O Delonix consegue correr a cadeia de plugins CNI de um nó contra um network namespace nomeado,
que é como um sandbox de pod do CRI recebe a sua rede. Ver:
`crates/adapters/delonix-sdn/src/cni.rs::attach_named_netns`,
[Rede de containers](cloud-native-primer.md#45-container-networking).

**Contract (node)** — A API de um nó, definida em Protocol Buffers em `proto/delonix/node/v1/`
(pacote `delonix.node.v1`), com um documento OpenAPI `docs/api/openapi.yaml` gerado a partir dela.
O `scripts/contract_gate.py` guarda a formatação, o lint, a compatibilidade com a última tag e o
OpenAPI gerado. É um contrato publicado; nenhum servidor o implementa ainda. Ver:
[Em que ponto está a reestruturação](architecture.md#where-the-restructuring-stands),
[ADR-0040](../../adr/0040-engine-restructuring-layers-ports-node-contract.md).

**Control process** — A metade reiniciável da infra de rede rootless: o binário do motor arrancado
com os argumentos internos `netns control` (não é um comando para o utilizador) corre dentro dos
namespaces segurados pelo pin, escuta num socket unix de controlo `0600` (aceitando só o uid do
próprio motor) e faz os attaches, os publishes, as mudanças de firewall, o DNS e o DHCP, um pedido de
cada vez. Matá-lo não perturba os workloads a correr; o comando seguinte reinicia-o. Ver:
`crates/adapters/delonix-sdn/src/infra.rs::start_control`, `control_loop`; **Holder / pin**.

**CRI (Container Runtime Interface)** — A API gRPC que o kubelet usa para correr pods. O crate
`delonix-cri` (binário `delonix-cri`) implementa-a por cima do motor, para que um nó Kubernetes possa
usar o Delonix em vez de outro runtime. Ver: `crates/interfaces/delonix-cri/`,
[Kubernetes](cloud-native-primer.md#46-kubernetes-cri-kubelet-kubeadm-and-kind).

**Daemonless** — Não é preciso nenhum processo residente para o motor funcionar: cada comando da CLI
faz o seu trabalho e sai. O que tem de persistir pertence ao systemd (units, timers) ou a um processo
por workload com um dono claro (o supervisor de um container, o pin de rede). Um processo residente
novo precisa de um ADR. Ver: *«Identidade e fronteira do motor»* no [`AGENTS.md`](../../../AGENTS.md),
[Daemonless](cloud-native-primer.md#410-daemonless-in-one-paragraph).

**Delegated cgroup** — Ver **cgroup delegation**.

**`DX_*` exit class** — Cada erro do motor tem uma string de código estável (`DX_NOT_FOUND`,
`DX_INVALID_ARGUMENT`, …) e corresponde a um código de saída do processo decidido a partir do **tipo**
do erro, nunca da sua mensagem (traduzida): por exemplo 4 = recurso inexistente, 5 = conflito. Os
scripts e os reconciliadores decidem pelo número, não pelo texto. Ver:
`crates/foundation/delonix-model/src/error.rs::Error::code`,
`crates/foundation/delonix-model/src/exitcode.rs::for_error`,
[Erros](rust-primer.md#32-errors-one-error-and-exit-codes-derived-from-its-type).

**Fitness function** — Uma verificação automática de que a arquitectura ainda tem a forma que foi
decidida. Aqui é o `scripts/arch_fitness.py` (job de CI `arch`): direcção das camadas, directório =
camada, versões das dependências só na raiz, nenhum nome de consumidor no código, e os ratchets de
dívida. Ver:
[Identidade e fronteiras do motor](architecture.md#engine-identity-and-boundaries),
[Regras de arquitectura](contributing-workflow.md#architecture-rules-the-gates-enforce).

**Holder / pin** — A metade de longa duração da infra de rede rootless. O binário do motor arrancado
com os argumentos internos `netns pin` cria um user, network e mount namespace e depois só dorme,
segurando-os; o seu pidfile mantém o nome histórico `holder.pid`, e todo o `nsenter -t <pid>` para a
infra aponta para ele. Antes da divisão em pin e processo de controlo, um único «holder» fazia os dois
trabalhos, e é por isso que as duas palavras aparecem no código. Ver:
`crates/adapters/delonix-sdn/src/infra.rs::start_pin`, `pin_main`,
`crates/adapters/delonix-sdn/src/pin_userns.rs`; **Control process**.

**IPAM (IP address management)** — Atribuição dos endereços dos workloads dentro do prefixo de uma
rede. O Delonix mantém um ficheiro de leases por prefixo em `ipam/` no state root, e um ceifador que
só reclama um lease depois de o ter visto órfão duas vezes, separadas por um período de graça.
`delonix network ipam ls` lista os leases. Ver: `crates/adapters/delonix-sdn/src/ipam.rs::allocate`,
`reap_orphan_leases`; a aritmética pura de endereços está em
`crates/foundation/delonix-net-rules/src/lib.rs`.

**Kind** — O tipo de um recurso declarativo num manifesto (`kind: Network`, `kind: Pod`,
`kind: VirtualMachine`, …), agrupado por `apiVersion` (`core`, `compute`, `networking`, `gateway`,
`storage`, `artifact`, `infrastructure`). Os factos de cada Kind — o seu grupo, se tem namespace, se
converge, a sua forma — vivem numa só tabela. `delonix api-resources` imprime-a;
`delonix explain <Kind>` documenta os seus campos. Ver:
`crates/contexts/delonix-stack/src/kinds.rs::FACTS`, `KindFacts`.

**LANG-01** — A regra de língua do código: identificadores, comentários e mensagens visíveis ao
utilizador escrevem-se em inglês; o português chega ao operador só através do catálogo de tradução
(`bins/delonix-runtime-bin/data/pt.po`, escolhido com `--l18n pt`). O `scripts/lang_ratchet.py` conta
o português que ainda está no código, como um ratchet. Ver:
[Língua](contributing-workflow.md#language-english-in-the-code-lang-01).

**Layer (ADR-0040)** — Um dos anéis arquitecturais a que cada crate pertence: fundação, contextos,
adapters, providers, interfaces, binários. As dependências apontam para dentro, e o directório do
crate (`crates/<layer>/`) tem de bater certo com a camada declarada. Não confundir com uma **layer de
imagem** (ver **Overlay / lowerdir**). Ver: [Camadas](architecture.md#layers-and-the-allowed-direction),
[ADR-0040](../../adr/0040-engine-restructuring-layers-ports-node-contract.md).

**Lowering (sugar Kinds)** — Reescrever um Kind de conveniência no Kind que faz de facto o trabalho,
enquanto o manifesto é carregado, para que o resto do motor nunca o veja. `Workload` baixa para
`Container`/`Pod`/`VirtualMachine`, `Dependency` para `NetworkPolicy`. A coluna `FORM` de
`delonix api-resources` diz em que se torna cada Kind: `primary`, `sugar → X` (baixado),
`compat → X` (um schema estrangeiro mantido mas compilado para X), `sunset → X` (ainda aplicado como
ele próprio, com sucessor anunciado), `aggregate` (expande-se nos documentos que contém, como o
`Stack`). Ver: `crates/contexts/delonix-stack/src/kinds.rs::Form`,
`bins/delonix-runtime-bin/src/cmd/manifest.rs::load`.

**MCP (Model Context Protocol)** — Um protocolo através do qual um cliente de IA chama ferramentas.
`delonix mcp serve` (crate `delonix-mcp`) é uma superfície de controlo **local** e sem inquilino,
sobre stdio: um processo em primeiro plano arrancado pelo cliente para uma sessão, confiado como o uid
local que o corre — não uma API de gestão remota. Ver: `crates/interfaces/delonix-mcp/src/lib.rs`,
[ADR-0025](../../adr/0025-mcp-local-ai-control-surface.md).

**microVM** — Uma máquina virtual leve com um modelo de dispositivos mínimo, que arranca depressa,
usada onde um workload precisa do seu próprio kernel. No Delonix o hypervisor de microVMs é o Cloud
Hypervisor; o `libvirt` (QEMU/KVM) é o outro backend local. Um `Workload` com `type: microvm` força o
backend Cloud Hypervisor. Ver: [Construir microVMs](microvm-setup.md),
[ADR-0006](../../adr/0006-workload-type-microvm.md).

**NoCloud seed** — Um ISO pequeno com o `user-data`, o `meta-data` e o `network-config` do cloud-init,
anexado a uma VM para o seu primeiro arranque aplicar o hostname, as chaves SSH e a rede. O Delonix
gera um por VM, a não ser que a imagem seja um appliance que não corre cloud-init. Ver:
`crates/adapters/delonix-vm/src/cloudinit.rs::generate_seed_iso`,
[Virtualização](cloud-native-primer.md#47-virtualization-kvm-virtio-cloud-hypervisor-libvirt-cloud-init).

**OCI (Open Container Initiative)** — Os padrões para imagens de containers (image spec), para as
distribuir a partir de registos (distribution spec) e para as correr (runtime spec). O Delonix faz ele
próprio o pull, o build, o armazenamento e o push de imagens OCI. Ver: `crates/adapters/delonix-oci/`,
[Imagens OCI](cloud-native-primer.md#44-oci-images-content-addressed-storage-and-overlayfs).

**Overlay / lowerdir** — O overlayfs empilha as layers só de leitura da imagem (os `lowerdir`s) por
baixo de um directório `upper` gravável por container. O Delonix desempacota cada layer de imagem uma
só vez em `layers/` e todos os containers dessa imagem partilham-nas; o directório do container tem
`upper/`, `work/`, `merged/` e um ficheiro `overlay-lowers` que lista as layers. O mount é feito dentro
do próprio user namespace do container com a nova API de mount, uma chamada `lowerdir+` por layer,
para que imagens com muitas layers não esbarrem no limite de comprimento das opções do `mount(2)`
clássico. Ver: `crates/adapters/delonix-oci/src/overlay.rs::prepare_overlay`,
`crates/adapters/delonix-linux/src/lib.rs::mount_overlay_if_marked`,
[ADR-0037](../../adr/0037-overlay-mount-new-api.md).

**Port (hexagonal)** — Um trait de que um caso de uso precisa e que um adapter ou provider
implementa, para que o domínio nunca nomeie um mecanismo concreto. Exemplos: `VmBackend`, e as portas
de compute `ImageStore`, `StorageProvider`, `NetworkProvider`, `WorkloadRuntime`. Ver:
`crates/contexts/delonix-compute/src/ports.rs`, `launch.rs`,
[Traits como portas](rust-primer.md#33-traits-as-ports-vmbackend-and-the-backend-registry).

**Provider** — Um crate em `crates/providers/` que implementa uma porta contra **uma API de gestão
remota** (hoje Proxmox VE e TrueNAS), trazendo o seu próprio cliente HTTP. Um provider novo entra como
implementação de uma porta, registado na raiz de composição — nunca como um `if provider == …` no
código — e precisa de um ADR. Ver: [Providers](crates.md#providers),
[ADR-0008](../../adr/0008-proxmox-vm-backend.md), [ADR-0009](../../adr/0009-truenas-storage-provisioner.md).

**Ratchet** — Um gate sobre um contador de dívida que falha quando o número **sobe** e também quando
**desce** sem a linha de base registada ter sido baixada no mesmo commit, para que o progresso fique
registado e nunca se perca. O `scripts/lang_ratchet.py` (português no código) e os ratchets de dívida
do `scripts/arch_fitness.py` funcionam assim; os dois têm `--list` e `--update`. Ver:
[Regras de arquitectura](contributing-workflow.md#architecture-rules-the-gates-enforce).

**Reconcile (3-way)** — Como o `plan`/`apply` decidem o que mudar sem um ficheiro de estado. Os três
lados são o manifesto (o desejado), o que é observado no nó (o real), e a última spec aplicada,
guardada no próprio recurso na anotação `delonix.io/last-applied`. O terceiro lado separa «tiraste
este campo do ficheiro» (reverte-o) de «alguém definiu isto à mão» (deixa-o). Ver:
`crates/contexts/delonix-stack/src/reconcile.rs`,
[O reconciliador declarativo](system-design-interview.md#55-the-declarative-reconciler-without-a-state-file).

**Rootless** — Correr como um utilizador sem privilégios, com privilégios só dentro dos user
namespaces que o motor cria. É o caminho por omissão no Delonix («rootless-first»); root é um opt-in
explícito. Ver:
[Namespaces Linux e operação rootless](cloud-native-primer.md#41-linux-namespaces-and-rootless-operation).

**slirp4netns** — Uma pilha de rede em espaço de utilizador que liga um network namespace rootless à
rede do host sem privilégios, e encaminha portas do host para dentro dele. O Delonix corre um único
`slirp4netns` para toda a infra de rede rootless, com o NAT e a publicação de portas feitos pelo
nftables dentro do namespace da infra. Ver: `crates/adapters/delonix-sdn/src/infra.rs`,
[Rede](system-design-interview.md#52-networking-pin-control-slirp-nftables).

**Stack** — O conjunto de recursos que um manifesto possui. A posse é uma label em cada recurso,
`delonix.io/stack`, e não um registo à parte: `apply --prune` e `stack destroy` só tocam em recursos
que a levam, e um recurso criado à mão nunca é removido por eles. `Stack` é também um Kind agregado
que agrupa recursos num só documento. Ver:
`crates/contexts/delonix-stack/src/reconcile.rs::STACK_LABEL`.

**State root** — O directório que guarda todo o estado do motor em ficheiros (não há base de dados):
`DELONIX_ROOT` quando definido, senão `~/.local/share/delonix` (ou `$XDG_DATA_HOME/delonix`) para um
utilizador e `/var/lib/delonix` para root. Os sockets de rede vivem num directório de runtime à parte
(`DELONIX_NET_RUNTIME_DIR`). Define sempre os dois quando testas. Os registos debaixo dela são lidos,
escritos e trancados pelo adapter `delonix-state`. Ver:
`bins/delonix-runtime-bin/src/cmd/util.rs::state_root`,
`crates/adapters/delonix-state/src/store.rs::Store::default_root`,
[Estado em disco](architecture.md#state-on-disk),
[Isolar o estado do motor](build-and-test.md#isolating-the-engines-state).

**subuid / subgid** — Um intervalo de ids de utilizador e de grupo delegados ao teu utilizador em
`/etc/subuid` e `/etc/subgid`. Com ele, um user namespace rootless mapeia muitos ids (escritos através
do `newuidmap` e do `newgidmap`); sem ele, só o teu próprio uid é mapeado e as imagens que usam outros
utilizadores partem-se. Os ficheiros que um container escreve como um id mapeado não pertencem ao teu
uid no host, e é por isso que algumas operações voltam a entrar num namespace mapeado. Ver:
`crates/adapters/delonix-sdn/src/pin_userns.rs`,
[Requisitos do kernel](environment.md#kernel-requirements).

**Supervisor** — O processo que o `container run -d` faz fork para ser o pai real do container:
espera pelo container, regista o seu verdadeiro estado de saída (e uma razão `OOMKilled`), e aplica a
política `--restart`. Como faz `fork`, tem de ser arrancado a partir de um processo com uma só thread;
os servidores re-executam primeiro um `delonix` novo. Ver:
`crates/adapters/delonix-linux/src/supervise.rs::run_supervised`,
`crates/adapters/delonix-linux/src/lib.rs::wait_and_record`.

**userns (user namespace)** — O namespace Linux que mapeia ids de utilizador, dando a um processo
privilégios de root só sobre os objectos que o seu namespace possui. É a base da operação rootless, e
em Ubuntu recentes pode ser bloqueado pelo AppArmor para binários fora dos caminhos esperados. Ver:
[Namespaces Linux](cloud-native-primer.md#41-linux-namespaces-and-rootless-operation),
[AppArmor](environment.md#ubuntu-2310-apparmor-blocks-user-namespaces-for-your-dev-binary),
`namespaces(7)` e `user_namespaces(7)`.

**Verdict map** — Um map do nftables de uma chave para um veredicto (`jump`, `accept`, …), usado para
que um pacote encontre a sua regra numa só consulta, em vez de percorrer uma regra por workload. O
Delonix usa o `fwmap` (endereço de um workload → a sua chain de firewall) e o `netpair` (um par de
bridges → uma isenção que abre uma rota entre duas redes). Ver:
`crates/adapters/delonix-sdn/src/infra.rs::FWMAP`, `NETPAIR_MAP`.

**VmBackend** — A porta que todo o backend de VM implementa (`boot`, `stop`, `destroy`, `is_running`,
`ip`, pausa e snapshots, …). O Cloud Hypervisor e o libvirt estão registados por omissão; um provider
remoto regista-se na raiz de composição. Registar não faz I/O, e a auto-detecção filtra pela
registration antes de construir seja o que for, por isso um backend remoto só se liga quando é
escolhido. Ver: `crates/adapters/delonix-vm/src/lib.rs::VmBackend`, `register_backend`,
`select_backend`,
[Traits como portas](rust-primer.md#33-traits-as-ports-vmbackend-and-the-backend-registry).

**Workload** — Duas coisas relacionadas. `kind: Workload` é um Kind de açúcar com `spec.type:
container|pod|vm|microvm` que baixa para o Kind correspondente na altura do load
([ADR-0001](../../adr/0001-workload-kind-schema.md)). `delonix workload` é o grupo de comandos de
day-2 (`ls`, `describe`, `stop`, `rm`) que lista e actua sobre containers e VMs em conjunto
([ADR-0002](../../adr/0002-compute-driver-trait.md)). Ver:
`crates/contexts/delonix-stack/src/kinds.rs::WORKLOAD_LOWERS_TO`,
`bins/delonix-runtime-bin/src/cmd/workload.rs`.

**Worktree** — Um `git worktree`: um segundo directório de trabalho ligado ao mesmo repositório, no
seu próprio branch. Cada tarefa aqui tem o seu, criado a partir de `origin/main` num directório
persistente fora do repositório (nunca `/tmp`), e é removido juntamente com o seu branch no fim. Ver:
[Um worktree por tarefa](contributing-workflow.md#one-worktree-per-task).
