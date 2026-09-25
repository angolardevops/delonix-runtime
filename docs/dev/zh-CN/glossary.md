<!-- translated-from: glossary.md sha256:d7eafa64fdd49da297f0db08240898eafeb97292834d5c229bc98cd4d97d7053 -->
# 术语表

**阅读前须知：** 没有——这是一份参考资料。看别的页面时把它放在旁边随时查。

这里收录了新人在这个仓库里会遇到的那些词，以及它们**在 Delonix 里**的含义——有时候比云原生领域里通常的含义要窄一些。每个词条都指向它被解释或实现的地方。路径是相对仓库根目录的；`file.rs::symbol` 指的是那个文件里的一个符号。

词条按字母顺序排列。至于更一般的背景知识，内核原语（进程、namespace、cgroup、文件描述符、信号）在[Linux 基础](linux-foundations.md)里讲，OCI、CRI、CNI 和 KVM 在这个引擎里是怎么用的，则在[云原生入门](cloud-native-primer.md)里讲。

---

**Adapter（适配器）** —— `crates/adapters/` 下的一个 crate，针对某个本地机制实现一个端口（port）：内核（`delonix-linux`）、网络数据面（`delonix-sdn`）、OCI 存储（`delonix-oci`）、本地的 VM hypervisor（`delonix-vm`）。一个适配器可以依赖 foundation 和 context 层的 crate，绝不能依赖 interface 层的 crate。见：[层（Layer）](architecture.md#layers-and-the-allowed-direction)、`scripts/arch_fitness.py::LAYERS`。

**ADR（Architecture Decision Record，架构决策记录）** —— 每一个结构性决定对应一个 Markdown 文件，放在 `docs/adr/` 下，命名为 `NNNN-title.md`，用英文写，在写代码之前先写。一份被接受的 ADR 永远不会被改写；一份新的 ADR 会取代它。见：[`docs/adr/README.md`](../../adr/README.md)，[什么时候要写 ADR](contributing-workflow.md#when-to-write-an-adr)。

**Apply / plan / prune（应用／规划／修剪）** —— 声明式收敛（convergence）的三个动词。`delonix plan` 展示一次 apply 会改变什么，但什么都不改变（有变化时 `--detailed-exitcode` 会退出码 2）；`delonix apply` 会让清单（manifest）收敛；`--prune` 还会移除那些 stack 拥有、但清单里已经不再声明的东西，默认永远不会跑。见：`crates/contexts/delonix-stack/src/reconcile.rs::plan`，[声明式协调](cloud-native-primer.md#48-declarative-reconciliation)。

**CAS（content-addressed storage，内容寻址存储）** —— 镜像 blob 的存储：每一个 blob 都以它的 digest 为地址，存放在 state root 下的 `blobs/sha256/<hex>` 里，所以相同的内容只存一份。完整性是在内容**进入**这个 store 时检查的，而不是在读取的时候：一次 pull 会拿 manifest、config 和每一层去对照期望的 digest 做比较（`crates/adapters/delonix-oci/src/registry.rs` 里的 `verify_manifest_digest` 和各处 digest 比较）。`Cas::read` 是一次普通的文件读取，不会重新算 hash；`Cas::verify` 会按需重新算 hash。见：`crates/adapters/delonix-oci/src/cas.rs::Cas`，[磁盘上的状态](architecture.md#state-on-disk)。

**CDI（Container Device Interface，容器设备接口）** —— 一份 CNCF 规范，描述怎么把一个设备（通常是 GPU）暴露给容器。Delonix 只**消费**已经由厂商工具生成好的 spec，把它们转换成和 `-v`/`--device` 一样的挂载和设备节点；它自己从来不去发现驱动。见：`crates/adapters/delonix-linux/src/cdi.rs`。

**cgroup 委派（delegation）** —— cgroup v2 里让一个无特权用户能管理一棵 cgroup 子树的机制。没有它，rootless 下的资源限制（`-m`、`--cpus`）就没法强制执行；引擎会检测到这一点，拒绝一个它没法应用的限制，而不是悄悄地接受它。通过 SSH 打开的一个 shell 往往是没有被委派的；`systemd-run --user --scope -p Delegate=yes` 会给你一个已经委派的。`delonix system info` 会用 `cgroup2 delegated` 显示这个答案。见：`crates/adapters/delonix-linux/src/lib.rs::cgroup_limits_apply`，[cgroups v2 与委派](cloud-native-primer.md#42-cgroups-v2-and-delegation)，[cgroup 委派](environment.md#cgroup-delegation-some-limits-are-refused-others-are-not-enforced)。

**CNI（Container Network Interface，容器网络接口）** —— Kubernetes 用来给一个 pod 配置网络的插件标准。Delonix 可以针对一个命名的网络 namespace 跑一个节点的 CNI 插件链，这就是一个 CRI pod sandbox 拿到网络的方式。见：`crates/adapters/delonix-sdn/src/cni.rs::attach_named_netns`，[容器网络](cloud-native-primer.md#45-container-networking)。

**契约（节点契约，Contract）** —— 单个节点的 API，用 Protocol Buffers 定义在 `proto/delonix/node/v1/` 下（包名 `delonix.node.v1`），并从它生成一份 OpenAPI 文档 `docs/api/openapi.yaml`。`scripts/contract_gate.py` 会守护格式、lint、跟上一个 tag 的兼容性，以及生成出来的 OpenAPI。这是一份已发布的契约；目前还没有服务端实现它。见：[重构进行到哪一步了](architecture.md#where-the-restructuring-stands)，[ADR-0040](../../adr/0040-engine-restructuring-layers-ports-node-contract.md)。

**控制进程（Control process）** —— rootless 网络基础设施里可重启的那一半：引擎的二进制文件以内部参数 `netns control`（不是一个面向用户的命令）启动，跑在 pin 持有的那些 namespace 里，监听一个 `0600` 权限的 unix 控制 socket（只接受引擎自己的 uid），一次处理一个请求地完成 attach、发布端口、防火墙改动、DNS 和 DHCP。杀掉它不会打扰正在运行的工作负载；下一条命令会重启它。见：`crates/adapters/delonix-sdn/src/infra.rs::start_control`、`control_loop`；另见 **Holder / pin**。

**CRI（Container Runtime Interface，容器运行时接口）** —— kubelet 用来运行 pod 的 gRPC API。`delonix-cri` 这个 crate（二进制文件 `delonix-cri`）在引擎之上实现了它，这样一个 Kubernetes 节点就能用 Delonix 代替另一个 runtime。见：`crates/interfaces/delonix-cri/`，[Kubernetes](cloud-native-primer.md#46-kubernetes-cri-kubelet-kubeadm-and-kind)。

**无守护进程（Daemonless）** —— 引擎工作不需要任何驻留进程：每条 CLI 命令做完自己的事就退出。必须持久化的东西要么归 systemd 所有（unit、timer），要么归一个有明确归属者的按工作负载（per-workload）进程所有（一个容器的 supervisor、网络的 pin）。一个新的驻留进程需要一份 ADR。见：[`AGENTS.md`](../../../AGENTS.md) 里的 *«Identidade e fronteira do motor»*（引擎的身份与边界），[无守护进程](cloud-native-primer.md#410-daemonless-in-one-paragraph)。

**委派的 cgroup（Delegated cgroup）** —— 见 **cgroup 委派**。

**`DX_*` 退出类别** —— 每一个引擎错误都有一个稳定的代码字符串（`DX_NOT_FOUND`、`DX_INVALID_ARGUMENT`……），并且映射到一个从错误的**类型**决定出来的进程退出码，绝不是从它（经过翻译的）消息文本决定的：比如 4 = 没有这个资源，5 = 冲突。脚本和 reconciler 是按这个数字分支的，不是按文本。见：`crates/foundation/delonix-model/src/error.rs::Error::code`，`crates/foundation/delonix-model/src/exitcode.rs::for_error`，[错误](rust-primer.md#32-errors-one-error-and-exit-codes-derived-from-its-type)。

**适应度函数（Fitness function）** —— 一个自动化检查，确认架构依然是当初决定的那个形状。这里指的是 `scripts/arch_fitness.py`（CI 作业 `arch`）：层的方向、目录即是层、依赖版本只放在根目录、代码里不出现消费者的名字，以及那些债务棘轮（ratchet）。见：[引擎的身份与边界](architecture.md#engine-identity-and-boundaries)，[架构规则](contributing-workflow.md#architecture-rules-the-gates-enforce)。

**Holder / pin（持有进程／钉子进程）** —— rootless 网络基础设施里长期存在的那一半。引擎的二进制文件以内部参数 `netns pin` 启动时，会创建一个 user、network 和 mount namespace，然后就只是睡眠，把它们钉住（hold）；它的 pidfile 保留着历史上的名字 `holder.pid`，树里每一个针对基础设施的 `nsenter -t <pid>` 都以它为目标。在拆分成 pin 和 control process 之前，一个「holder」同时做这两件事，这也是为什么代码里这两个词都会出现。见：`crates/adapters/delonix-sdn/src/infra.rs::start_pin`、`pin_main`，`crates/adapters/delonix-sdn/src/pin_userns.rs`；另见 **控制进程**。

**IPAM（IP address management，IP 地址管理）** —— 在一个网络的前缀（prefix）内部分配工作负载地址。Delonix 在 state root 下的 `ipam/` 里给每个前缀保留一个 lease 文件，还有一个回收器（reaper），只有在一个 lease 跨越一个宽限期被连续两次看到处于孤儿状态之后才会回收它。`delonix network ipam ls` 会列出这些 lease。见：`crates/adapters/delonix-sdn/src/ipam.rs::allocate`、`reap_orphan_leases`；纯粹的地址算术在 `crates/foundation/delonix-net-rules/src/lib.rs` 里。

**Kind（种类）** —— 清单（manifest）里一个声明式资源的类型（`kind: Network`、`kind: Pod`、`kind: VirtualMachine`……），按 `apiVersion` 分组（`core`、`compute`、`networking`、`gateway`、`storage`、`artifact`、`infrastructure`）。每一个 Kind 的事实——它所属的组、是不是有 namespace、会不会收敛、它的形态——都活在一张表里。`delonix api-resources` 会把它打印出来；`delonix explain <Kind>` 会记录它的字段。见：`crates/contexts/delonix-stack/src/kinds.rs::FACTS`、`KindFacts`。

**LANG-01** —— 关于代码的语言规则：标识符、注释和面向用户的消息都用英文写；葡萄牙语只通过翻译目录（`bins/delonix-runtime-bin/data/pt.po`，用 `--l18n pt` 选中）到达用户。`scripts/lang_ratchet.py` 会把代码里仍然存在的葡萄牙语当作一个棘轮（ratchet）来计数。见：[语言](contributing-workflow.md#language-english-in-the-code-lang-01)。

**层（Layer，ADR-0040）** —— 每个 crate 所属的架构层环之一：foundation、contexts、adapters、providers、interfaces、binaries。依赖指向内层，而且一个 crate 的目录（`crates/<layer>/`）必须和它声明的层一致。不要跟一个**镜像层（image layer）**混淆（见 **Overlay / lowerdir**）。见：[层](architecture.md#layers-and-the-allowed-direction)，[ADR-0040](../../adr/0040-engine-restructuring-layers-ports-node-contract.md)。

**降级（Lowering，糖衣 Kind）** —— 在清单被加载的时候，把一个便利性的 Kind 改写成真正干活的那个 Kind，让引擎的其余部分永远看不到那个便利性 Kind。`Workload` 会降级成 `Container`/`Pod`/`VirtualMachine`，`Dependency` 会降级成 `NetworkPolicy`；一个 `VirtualMachine` 的 `spec.expose` 会降级成一个名叫 `<vm>-expose` 的 `HTTPRoute`（`cmd/vm_expose.rs`）。`delonix api-resources` 的 `FORM` 那一列说明了每个 Kind 会变成什么：`primary`（主要）、`sugar → X`（降级为糖衣）、`compat → X`（一个外来 schema 被保留下来，但会编译到 X 上）、`sunset → X`（依然作为自己被应用，但已宣告了继任者）、`aggregate`（聚合，展开成它包含的那些文档，比如 `Stack`）。见：`crates/contexts/delonix-stack/src/kinds.rs::Form`，`bins/delonix-runtime-bin/src/cmd/manifest.rs::load`。

**MCP（Model Context Protocol，模型上下文协议）** —— 一个 AI 客户端调用工具所通过的协议。`delonix mcp serve`（crate `delonix-mcp`）是一个**本地的、没有租户概念**的控制面（control surface），走 stdio：一个由客户端为一次会话启动的前台进程，被信任为运行它的那个本地 uid——不是一个远程的管理 API。见：`crates/interfaces/delonix-mcp/src/lib.rs`，[ADR-0025](../../adr/0025-mcp-local-ai-control-surface.md)。

**microVM（微虚拟机）** —— 一台设备模型极简、启动很快的轻量级虚拟机，用在一个工作负载需要自己内核的地方。在 Delonix 里，microVM 的 hypervisor 是 Cloud Hypervisor；`libvirt`（QEMU/KVM）是另一个本地 backend。一个 `type: microvm` 的 `Workload` 会强制使用 Cloud Hypervisor 这个 backend。见：[构建 microVM](microvm-setup.md)，[ADR-0006](../../adr/0006-workload-type-microvm.md)。

**NoCloud 种子（NoCloud seed）** —— 一个携带 cloud-init 的 `user-data`、`meta-data` 和 `network-config` 的小型 ISO，挂给一台 VM，让它第一次启动时应用 hostname、SSH key 和网络配置。除非镜像是一个不跑 cloud-init 的 appliance，否则 Delonix 会给每台 VM 生成一份。见：`crates/adapters/delonix-vm/src/cloudinit.rs::generate_seed_iso`，[虚拟化](cloud-native-primer.md#47-virtualization-kvm-virtio-cloud-hypervisor-libvirt-cloud-init)。

**OCI（Open Container Initiative，开放容器倡议）** —— 关于容器镜像（image spec）、从 registry 分发它们（distribution spec）以及运行它们（runtime spec）的一组标准。Delonix 自己拉取、构建、存储和推送 OCI 镜像。见：`crates/adapters/delonix-oci/`，[OCI 镜像](cloud-native-primer.md#44-oci-images-content-addressed-storage-and-overlayfs)。

**Overlay / lowerdir（覆盖层／下层目录）** —— overlayfs 会把只读的镜像层（那些 `lowerdir`）叠在每个容器自己的可写 `upper` 目录之下。Delonix 把每一个镜像层解包一次，放在 `layers/` 下面，那个镜像的每一个容器都共享它们；容器目录里放着 `upper/`、`work/`、`merged/`，以及一个列出各层的 `overlay-lowers` 文件。挂载是在容器自己的 user namespace 里、用新的 mount API 完成的，每一层调用一次 `lowerdir+`，这样有很多层的镜像就不会撞上经典 `mount(2)` 选项长度的上限。见：`crates/adapters/delonix-oci/src/overlay.rs::prepare_overlay`，`crates/adapters/delonix-linux/src/lib.rs::mount_overlay_if_marked`，[ADR-0037](../../adr/0037-overlay-mount-new-api.md)。

**端口（Port，六边形架构里的 port）** —— 一个用例（use case）需要的、由某个 adapter 或 provider 实现的 trait，这样领域代码就永远不用点名一个具体的机制。例子：`VmBackend`，以及 compute 层的端口 `ImageStore`、`StorageProvider`、`NetworkProvider`、`WorkloadRuntime`。见：`crates/contexts/delonix-compute/src/ports.rs`、`launch.rs`，[作为端口（port）的 trait](rust-primer.md#33-traits-as-ports-vmbackend-and-the-backend-registry)。

**Provider** —— `crates/providers/` 下的一个 crate，针对**一个远端管理 API**（目前是 Proxmox VE 和 TrueNAS）实现一个端口，自己带着 HTTP 客户端。一个新的 provider 是作为某个端口的实现进入的，注册在组合根（composition root）——绝不会是代码里的 `if provider == …`——而且需要一份 ADR。见：[Provider](crates.md#providers)，[ADR-0008](../../adr/0008-proxmox-vm-backend.md)，[ADR-0009](../../adr/0009-truenas-storage-provisioner.md)。

**棘轮（Ratchet）** —— 一个针对债务计数器的门禁（gate），当数字**上升**时会失败，而且当它**下降**、却没有在同一个 commit 里把提交过的基线（baseline）一起调低时，也会失败——这样进展就会被记录下来，永远不会丢失。`scripts/lang_ratchet.py`（代码里的葡萄牙语）和 `scripts/arch_fitness.py` 里的债务棘轮都是这么工作的；两者都有 `--list` 和 `--update`。见：[架构规则](contributing-workflow.md#architecture-rules-the-gates-enforce)。

**协调（3 方，Reconcile）** —— `plan`/`apply` 在没有状态文件的情况下决定要改什么的方式。三方分别是清单（desired，期望状态）、在节点上观察到的东西（actual，实际状态），以及最后一次应用的 spec，它存放在资源自己身上的 `delonix.io/last-applied` 注解（annotation）里。第三方把「你把这个字段从文件里删掉了」（要回退它）和「有人手动设置了这个」（不要动它）这两种情况区分开来。见：`crates/contexts/delonix-stack/src/reconcile.rs`，[没有状态文件的声明式协调器](system-design-interview.md#55-the-declarative-reconciler-without-a-state-file)。

**Rootless（无根）** —— 以一个无特权用户的身份运行，只在引擎自己创建的 user namespace 内部拥有特权。这是 Delonix 里的默认路径（「rootless-first」）；root 是一个明确的可选项（opt-in）。见：[Linux namespace 与 rootless 运行](cloud-native-primer.md#41-linux-namespaces-and-rootless-operation)。

**slirp4netns** —— 一个用户态网络栈，不需要特权就能把一个 rootless 的网络 namespace 连到宿主机的网络上，并把宿主机的端口转发进去。Delonix 为整个 rootless 网络基础设施只跑一个 `slirp4netns`，NAT 和端口发布是由基础设施 namespace 内部的 nftables 完成的。见：`crates/adapters/delonix-sdn/src/infra.rs`，[网络](system-design-interview.md#52-networking-pin-control-slirp-nftables)。

**Stack** —— 一份清单拥有的那组资源。归属关系是每个资源上的一个 label，`delonix.io/stack`，而不是一份单独的记录：`apply --prune` 和 `stack destroy` 只会碰带着这个 label 的资源，一个手动创建的资源永远不会被它们移除。`Stack` 同时也是一个聚合型的 Kind，把资源分组在一份文档里。见：`crates/contexts/delonix-stack/src/reconcile.rs::STACK_LABEL`。

**State root（状态根）** —— 以文件形式保存全部引擎状态的目录（没有数据库）：设置了就用 `DELONIX_ROOT`，否则对一个普通用户是 `~/.local/share/delonix`（或 `$XDG_DATA_HOME/delonix`），对 root 是 `/var/lib/delonix`。网络的 socket 活在一个单独的运行时目录里（`DELONIX_NET_RUNTIME_DIR`）。测试时两个都要设置。它下面的记录由 `delonix-state` 这个 adapter 负责读、写和加锁。见：`bins/delonix-runtime-bin/src/cmd/util.rs::state_root`，`crates/adapters/delonix-state/src/store.rs::Store::default_root`，[磁盘上的状态](architecture.md#state-on-disk)，[隔离引擎的状态](build-and-test.md#isolating-the-engines-state)。

**subuid / subgid** —— 在 `/etc/subuid` 和 `/etc/subgid` 里委派给你这个用户的一段用户和组 id。有了它，一个 rootless 的 user namespace 就能映射很多 id（通过 `newuidmap` 和 `newgidmap` 写入）；没有它，就只有你自己的 uid 会被映射，用到其他用户的镜像会出问题。容器以一个被映射的 id 写入的文件，在宿主机上并不归你的 uid 所有，这也是为什么有些操作要重新进入一个被映射的 namespace。见：`crates/adapters/delonix-sdn/src/pin_userns.rs`，[内核要求](environment.md#kernel-requirements)。

**Supervisor（监督进程）** —— `container run -d` fork 出来、作为容器真正父进程的那个进程：它会等待容器，记录它真实的退出状态（以及一个 `OOMKilled` 的原因），并应用 `--restart` 策略。因为它要 `fork`，所以必须从一个单线程的进程里启动；服务端会先重新执行一次干净的 `delonix`。见：`crates/adapters/delonix-linux/src/supervise.rs::run_supervised`，`crates/adapters/delonix-linux/src/lib.rs::wait_and_record`。

**userns（user namespace，用户命名空间）** —— 映射用户 id 的 Linux namespace，让一个进程只对它自己 namespace 拥有的对象有 root 权限。它是 rootless 运行的基石，在较新的 Ubuntu 上，位于预期路径之外的二进制文件可能会被 AppArmor 挡住，不能用它。见：[Linux namespace](cloud-native-primer.md#41-linux-namespaces-and-rootless-operation)，[AppArmor](environment.md#ubuntu-2310-apparmor-blocks-user-namespaces-for-your-dev-binary)，`namespaces(7)` 和 `user_namespaces(7)`。

**Verdict map（判决映射表）** —— 一个从 key 映射到判决（`jump`、`accept`……）的 nftables map，让一个包只需要一次查找就能找到自己的规则，而不用每个工作负载一条规则地遍历过去。Delonix 用 `fwmap`（一个工作负载的地址 → 它的防火墙链）和 `netpair`（一对网桥 → 一条打开两个网络之间路由的豁免）。见：`crates/adapters/delonix-sdn/src/infra.rs::FWMAP`、`NETPAIR_MAP`。

**VmBackend** —— 每一个 VM backend 都要实现的端口（`boot`、`stop`、`destroy`、`is_running`、`ip`、暂停和快照……）。Cloud Hypervisor 和 libvirt 是默认注册好的；一个远端 provider 会在组合根注册。注册本身不做任何 I/O，而且自动检测是在构建任何东西**之前**先按注册信息过滤，所以一个远端 backend 只有在被选中时才会去连接。见：`crates/adapters/delonix-vm/src/lib.rs::VmBackend`、`register_backend`、`select_backend`，[作为端口（port）的 trait](rust-primer.md#33-traits-as-ports-vmbackend-and-the-backend-registry)。

**Workload（工作负载）** —— 两个相关的东西。`kind: Workload` 是一个糖衣 Kind，`spec.type: container|pod|vm|microvm`，在加载时会降级成对应的 Kind（[ADR-0001](../../adr/0001-workload-kind-schema.md)）。`delonix workload` 是把容器和 VM 放在一起列出、对它们操作的 day-2 命令组（`ls`、`describe`、`stop`、`rm`，[ADR-0002](../../adr/0002-compute-driver-trait.md)）。见：`crates/contexts/delonix-stack/src/kinds.rs::WORKLOAD_LOWERS_TO`，`bins/delonix-runtime-bin/src/cmd/workload.rs`。

**Worktree（工作树）** —— 一个 `git worktree`：附加在同一个仓库上的第二个工作目录，在它自己的分支上。这里的每一个任务都会有自己的一个，从 `origin/main` 创建在仓库之外一个持久化的目录里（永远不是 `/tmp`），结束时连同它的分支一起被移除。见：[每个任务一个 worktree](contributing-workflow.md#one-worktree-per-task)。

---

**下一步：** [概览与按角色的阅读路径](README.md#reading-paths-by-role)——这就是这门课的终点；回到按角色划分的阅读路径，挑一个接下来要深入的方向。
