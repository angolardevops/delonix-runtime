<!-- translated-from: cloud-native-primer.md sha256:a9064f13ecba90c0cc4992eccd73ec17bf783621b44ed9ce4319b3b07ee11793 -->
# 云原生入门

**阅读之前：**[Linux 基础](linux-foundations.md)（命名空间、cgroups v2、文件描述符）和
[IaaS 与云原生](iaas-and-cloud-native.md)（引擎负责什么）。

引擎是架在 Linux 内核特性和少数几份开放规范之上的一层薄而审慎的封装。本页会给你每个概念
刚好够用来读代码的量，并说明它**位于本仓库何处**。读完之后，任何一种机制——一个命名空间、
一个 cgroup 限制、一个镜像层、一条防火墙链、一次虚拟机启动、一次清单 apply——你都能说出
实现它的文件和符号。要深入了解，请跟随官方链接——它们都比这里的任何总结更好。

本手册里的每个概念只教一次。内核原语（命名空间、用户命名空间、cgroups v2）是在
[Linux 基础](linux-foundations.md)中亲手教的，所以 4.1 和 4.2 两节只是复述它们并把它们
对应到代码上。每份开放标准*要求*什么、引擎符合到什么程度，写在课程后面的
[云原生标准](cloud-native-standards.md)里。路径指的是各个 crate（`crates/<layer>/<crate>`）；
层在[架构](architecture.md)中有解释，目前你只需把一个路径读作代码所在的位置。

每一节都分三个部分：概念本身、**在 Delonix 中**（你可以 `grep` 到的文件和符号），以及
**延伸阅读**。路径都是相对于仓库根目录的。

要在更广阔的生态系统中定位自己，[CNCF Landscape](https://landscape.cncf.io/) 和
[CNCF Glossary](https://glossary.cncf.io/) 都是不错的地图。与 runc、crun、containerd 或
Podman 的比较，只会出现在有助于解释某个设计选择的地方。

---

## 4.1 Linux 命名空间与 rootless 运作

**回顾。** 一个容器就是一个在一整套全新命名空间（挂载、PID、网络、IPC、UTS、cgroup、
用户）里启动的进程。**用户命名空间**是让它变得无根的关键：在里面，进程对那个命名空间所
拥有的资源而言是 uid 0；在外面，它是一个普通用户。一个无特权用户只能映射自己的 uid；
要映射一个*范围*，需要 `newuidmap`/`newgidmap` 和 `/etc/subuid`。这两者都在
[Linux 基础——命名空间](linux-foundations.md#namespaces)和
[用户命名空间与 uid 映射](linux-foundations.md#user-namespaces-and-uid-mapping)中配着
可以亲手敲的命令讲过了。有些发行版还会通过 AppArmor 进一步限制无特权的用户命名空间——
实际后果见
[准备你的环境](environment.md#ubuntu-2310-apparmor-blocks-user-namespaces-for-your-dev-binary)。

**在 Delonix 中**

- `crates/adapters/delonix-linux/src/lib.rs` 中的 `fn spawn` 构建 `CloneFlags`
  （`CLONE_NEWNS`、`CLONE_NEWPID`、`CLONE_NEWNET`、`CLONE_NEWUSER`……）并调用
  `nix::sched::clone`。pod 成员之间的 IPC/UTS 共享由 `container_init` 中的 `setns` 处理。
- 同一文件中的 `write_userns_maps` 从父进程写入映射：无根时是单 uid 映射
  （`0 <euid> 1`），或者当 `have_subid_helpers()` 说 `newuidmap`/`newgidmap` 可用时，
  是一个通过它们建立的 subuid 范围。`USERNS_UID_BASE`/`USERNS_RANGE` 定义了以 root
  身份运行时使用的那个范围。
- `setup_rootfs` 挂载容器的根并调用 `pivot_root`；`container_init` 是在新命名空间
  内部、`execvp` 之前运行的代码。
- 对由映射后的 subuid 拥有的文件进行的无根操作，会在一个已映射的用户命名空间内部
  重新执行这个二进制文件：`reexec_mapped`、`reexec_mapped_hold`、`remove_tree_mapped`。

**延伸阅读：** [`namespaces(7)`](https://man7.org/linux/man-pages/man7/namespaces.7.html)、
[`user_namespaces(7)`](https://man7.org/linux/man-pages/man7/user_namespaces.7.html)、
[`newuidmap(1)`](https://man7.org/linux/man-pages/man1/newuidmap.1.html)、
[`subuid(5)`](https://man7.org/linux/man-pages/man5/subuid.5.html)、
[`pivot_root(2)`](https://man7.org/linux/man-pages/man2/pivot_root.2.html)。

---

## 4.2 cgroups v2 与委派

**回顾。** cgroup v2 是位于 `/sys/fs/cgroup` 的一棵树，它的文件（`memory.max`、
`cpu.max`、`pids.max`、`memory.events`）对一组进程进行限制和统计。一个无特权用户只能
写 systemd 已经**委派**给他的那棵子树，而一个 SSH 会话 scope 里的 shell 恰恰在这棵
子树之外——所以限制可能在那里悄悄地不生效。这棵树、"无内部进程"规则和委派，都在
[Linux 基础——cgroups v2](linux-foundations.md#cgroups-v2)中亲手教过；委派契约作为
一份标准，以及引擎对它的符合情况，在
[云原生标准——13.15](cloud-native-standards.md#1315-linux-cgroup-v2-and-systemd-delegation)
里。引擎只支持 v2。

**在 Delonix 中**

- Root 模式把容器放在 `delonix_compute::DELONIX_SLICE`（`/sys/fs/cgroup/delonix.slice`）
  下面。
- 无根模式会找到用户的 service cgroup，并在 `<user@uid.service>/dlx-containers` 下
  创建叶子——见 `crates/adapters/delonix-linux/src/lib.rs` 中的 `user_service_base`
  和 `try_delegated_base`。`cgroup_limits_apply` 会在不启动任何容器的情况下，回答
  "限制会不会在这台主机上生效？"这个问题。
- 关于中间那一层的设计决策是 [ADR-0015](../adr/0015-intermediate-cgroup-level.md)；
  CRI 如何遵循 kubelet 的 cgroup 层级结构是
  [ADR-0038](../adr/0038-cri-follows-kubelet-resource-model.md)，kubelet 给出的那个
  父级由 `crates/contexts/delonix-compute/src/record.rs` 中的 `KubeCgroupParent::parse`
  校验。

**延伸阅读：** [Linux kernel — Control Group v2](https://docs.kernel.org/admin-guide/cgroup-v2.html)、
[systemd — Control Group APIs and Delegation](https://systemd.io/CGROUP_DELEGATION/)、
[`cgroups(7)`](https://man7.org/linux/man-pages/man7/cgroups.7.html)。

---

## 4.3 能力（Capabilities）、seccomp、AppArmor、屏蔽路径

root 的权力被拆分成一个个**能力（capabilities）**（`CAP_NET_ADMIN`、`CAP_SYS_ADMIN`……）。
一个容器只保留一个很小的默认集合，其余全部丢弃。**seccomp** 安装一个 BPF 过滤器，用来
允许或拒绝系统调用，从而缩小内核攻击面。**AppArmor**（在其他发行版上是 SELinux）是通过
profile 来限制一个进程的 Linux 安全模块。最后，运行时会把敏感的 `/proc` 和 `/sys` 路径
**屏蔽**掉（在它们上面 bind 一个空的东西），并把另一些路径设为只读，因为那些文件会
泄露宿主机信息，或者允许对宿主机进行控制。

代码里记录了一个微妙之处：`clone3` 是通过一个指针来传递它的标志位的，而 seccomp
过滤器无法检查指针指向的内容，所以一个拦截 `clone(CLONE_NEWUSER)` 的过滤器，还必须让
`clone3` 以 `ENOSYS` 失败，才能把 libc 逼回可以被过滤的 `clone`。

**在 Delonix 中**

- 能力（Capabilities）：`crates/adapters/delonix-linux/src/capabilities.rs` 中的
  `KEPT_CAPS` 和 `resolve_cap_keep`；`lib.rs` 中的 `drop_capabilities`。
- seccomp：`crates/adapters/delonix-linux/src/lib.rs` 中的 `apply_seccomp`（用
  [`seccompiler`](https://docs.rs/seccompiler) crate 构建，包含 `clone3` → `ENOSYS`
  这个预过滤器）；自定义的 JSON profile 在 `seccomp_profile.rs` 中解析和编译
  （`parse`、`compile`）。
- AppArmor：`lib.rs` 中的 `apply_apparmor`。
- 屏蔽路径与只读路径：`lib.rs` 中的 `DEFAULT_MASKED_PATHS`、`DEFAULT_READONLY_PATHS`、
  `apply_masked_paths`、`apply_readonly_paths`、`mask_proc_paths`。
- 节点级别的安全决策（策略、准入、事件、评分）是一个独立的纯 crate：
  `crates/contexts/delonix-security-runtime/src/admission.rs` 中的 `evaluate`
  （[ADR-0026](../adr/0026-security-runtime-decision-crate.md)）。

**延伸阅读：** [`capabilities(7)`](https://man7.org/linux/man-pages/man7/capabilities.7.html)、
[kernel — Seccomp BPF](https://docs.kernel.org/userspace-api/seccomp_filter.html)、
[AppArmor documentation](https://gitlab.com/apparmor/apparmor/-/wikis/Documentation)、
[OCI runtime spec — Linux config](https://github.com/opencontainers/runtime-spec/blob/main/config-linux.md)
（其他运行时所消费的 `maskedPaths`/`readonlyPaths`/`seccomp` 字段）。

---

## 4.4 OCI 镜像、按内容寻址的存储与 overlayfs

**开放容器计划（Open Container Initiative）**发布了三份规范：

- **镜像规范（image spec）**——一个镜像是一份*清单*（manifest，JSON 格式），它指向
  一份*配置*（config）和一份有序的*层*（layer，tarball）列表，也可能还有一份*索引*
  （index），为每个平台各指向一份清单；
- **分发规范（distribution spec）**——镜像仓库对外提供的 HTTP API
  （`/v2/<name>/manifests/<ref>`、`/v2/<name>/blobs/<digest>`、token 认证）；
- **运行时规范（runtime spec）**——像 runc 或 crun 这样的运行时，是如何被告知去运行
  一个文件系统 bundle 的。

一切都是**按内容寻址**的：一个 blob 是用它字节内容的 SHA-256 摘要来命名的，所以客户端
通过对下载内容取哈希来验证它。按摘要拉取（`name@sha256:…`）只有在*清单*也针对那个摘要
被校验、而不只是每个 blob 针对清单被校验时，才算得上一个保证。

在运行时，各层是用 **overlayfs** 叠起来的：若干个只读的 `lowerdir`、一个变更会被
copy-up 上去的可写 `upperdir`，以及一个 `workdir`。许多容器可以共享同样的 lower 层。

**在 Delonix 中**

- 镜像仓库客户端（分发规范）：`crates/adapters/delonix-oci/src/registry.rs`——
  `pull_from_registry*` 系列函数、`ACCEPT_MANIFEST` 媒体类型，以及
  `verify_manifest_digest`。类型来自 [`oci-spec`](https://docs.rs/oci-spec) crate。
- 按内容寻址的 blob 存储：`crates/adapters/delonix-oci/src/cas.rs` 中的 `Cas`。
- 写入一份 OCI 镜像布局归档：`save.rs` 中的 `write_oci_archive`。
- Overlay 准备：`overlay.rs` 中的 `ImageStore::prepare_overlay` 写入一个
  `overlay-lowers` 标记（`LOWERS_FILE`）；挂载本身发生在容器自己的用户和挂载命名空间
  内部，在 `mount_overlay_if_marked`（`crates/adapters/delonix-linux/src/lib.rs`）中
  完成，用的是新的挂载 API——见 [ADR-0016](../adr/0016-filesystem-under-the-state-root.md)
  和 [ADR-0037](../adr/0037-overlay-mount-new-api.md)。
- 引擎自己运行容器，而不是把一个 OCI 运行时 bundle 交给 runc/crun。

**延伸阅读：** [OCI image spec](https://github.com/opencontainers/image-spec)、
[OCI distribution spec](https://github.com/opencontainers/distribution-spec)、
[OCI runtime spec](https://github.com/opencontainers/runtime-spec)、
[kernel — Overlay Filesystem](https://docs.kernel.org/filesystems/overlayfs.html)。

---

## 4.5 容器网络

Linux 网络的构建模块：

- 一个**网络命名空间**拥有自己的网卡、路由和防火墙；
- 一个 **veth pair** 是一根虚拟网线，两端分别在两个命名空间里；
- 一个**网桥（bridge）**是一台把许多 veth 端点连接起来的虚拟交换机；
- **nftables** 是内核的包过滤和 NAT 引擎；**DNAT** 改写目的地址（这就是一个已发布
  端口抵达容器的方式），**conntrack** 跟踪连接流，让一条被允许的连接的回复流量能够
  通过（`ct state established,related`）；
- **slirp4netns** 通过在用户态模拟一套 TCP/IP 协议栈，给一个无特权的网络命名空间
  提供出站连接能力，并把宿主机端口转发进去；
- **VXLAN** 在主机之间通过 UDP 承载二层帧，**WireGuard** 则给一条隧道加密。

**CNI**（Container Network Interface，容器网络接口）是一份规范，运行时按照它去执行
插件二进制程序（`bridge`、`host-local`、`portmap`……），用的是 `ADD`/`DEL` 命令和一份
来自 `/etc/cni/net.d` 的 JSON 配置。Kubernetes 的各个运行时用它来实现 pod 网络。

**在 Delonix 中**

- 无根网络无法在宿主机上创建网卡，所以引擎保留了一个长期存在的 **holder** 网络
  命名空间：一个极简的 *pin* 进程持有这些命名空间，一个可重启的 *control* 进程为
  一个 Unix 套接字提供服务。见 `crates/adapters/delonix-sdn/src/infra.rs` 中的
  `start_pin`、`start_control` 和 `ensure_up`。
- 挂接一个工作负载：`attach_container`（IPAM + 控制命令）和 `do_attach`（把 veth
  接入 holder 内部的网桥）。网桥名字来自零依赖的
  `crates/foundation/delonix-net-rules/src/lib.rs` 中的 `bridge_name`。
- 出站与端口转发：`crates/adapters/delonix-sdn/src/lib.rs` 中的 `slirp_attach` 和
  `slirp_add_hostfwd`（它们会 spawn `slirp4netns`）；holder 内部的发布在
  `publish_port`/`do_publish`（`infra.rs`）中。
- 防火墙：`table ip dlxing`，带有基础链 `fwguard`、`fwdeny`、`fwcont`，以及 `fwmap`
  裁决 map（`FWMAP`），在 `infra.rs` 中生成（`do_firewall`、`apply_firewall_all`，
  用于命名空间隔离集合的 `ns_set_join`）。
- 内部 DNS（标准名字 `<name>.<namespace>.svc.delonix.internal`，较旧的
  `<name>.<namespace>.delonix.internal` 依然能应答；`service_fqdn`、
  `parse_internal_name`）：`infra.rs` 中的 `dns_server_main`、`handle_dns`、
  `dns_resolve_for`、`dns_resolve_multi_for`。
- Overlay 网络：`set_vxlan`（`infra.rs`）和 `crates/adapters/delonix-sdn/src/wg.rs`
  中的 WireGuard 辅助函数，由 `bins/delonix-runtime-bin/src/cmd/network.rs` 中的
  `realize_overlay` 编排。
- CNI：`crates/adapters/delonix-sdn/src/cni.rs`——`add`、`del`、`readiness`、
  `attach_named_netns`。无根场景下是可选启用的（`enabled_conf` 会检查
  `DELONIX_CNI=1`）；root 的 CRI 路径使用节点自己的 CNI 链
  （`crates/interfaces/delonix-cri/src/runtime_svc.rs` 中的 `root_cni_readiness`）。
- 拓扑决策：[ADR-0013](../adr/0013-network-topology.md)、
  [ADR-0014](../adr/0014-runtime-dir-per-root.md)。

**延伸阅读：** [`network_namespaces(7)`](https://man7.org/linux/man-pages/man7/network_namespaces.7.html)、
[`veth(4)`](https://man7.org/linux/man-pages/man4/veth.4.html)、
[nftables wiki](https://wiki.nftables.org/)、
[slirp4netns](https://github.com/rootless-containers/slirp4netns)、
[kernel — VXLAN](https://docs.kernel.org/networking/vxlan.html)、
[WireGuard](https://www.wireguard.com/)、
[CNI](https://www.cni.dev/) 及其[规范](https://www.cni.dev/docs/spec/)。

---

## 4.6 Kubernetes：CRI、kubelet、kubeadm 与 kind

**kubelet** 是 Kubernetes 的节点代理。它自己不运行容器；它通过**容器运行时接口**
（Container Runtime Interface）——一个在本地 Unix 套接字上提供服务的 gRPC API
（`RuntimeService`、`ImageService`）——与一个容器运行时对话。kubelet 会先创建一个
*pod sandbox*（`RunPodSandbox`），然后在里面创建容器。它还有一个 **cgroup 驱动**设置
（`systemd` 或 `cgroupfs`），必须和运行时管理 cgroup 的方式相匹配，否则 pod 的 cgroup
和容器的 cgroup 就会产生分歧。

**kubeadm** 在已有的机器上引导出一个集群（`kubeadm init`、`kubeadm join`）。**kind**
则把 Kubernetes 的各个节点作为从 `kindest/node` 镜像构建出来的容器来运行。

**在 Delonix 中**

- `crates/interfaces/delonix-cri` 是一个 CRI `runtime.v1` 服务器。protobuf 是该
  crate 内部的 `proto/api.proto`，由 `build.rs` 用 `tonic-build` 编译。二进制文件是
  `src/bin/delonix-cri.rs`。
- 汇报给 kubelet 的 cgroup 驱动：`runtime_svc.rs` 中的 `engine_cgroup_driver`；它的
  文档注释记录了这个答案为什么是这样，以及要换成另一个答案的话必须改动什么。
- 一次经过真实 gRPC 的往返，测试在
  `crates/interfaces/delonix-cri/tests/grpc_status.rs` 里。
- 集群引导命令：`bins/delonix-runtime-bin/src/cmd/cluster.rs` 中通过 SSH 执行的
  kubeadm（配合 `kubeadm_config.rs`、`etcd.rs`、`lb.rs`），以及 `kindmode.rs` 中
  kind 风格的本地集群。

**延伸阅读：** [Kubernetes — Container Runtime Interface](https://kubernetes.io/docs/concepts/architecture/cri/)、
[cri-api repository](https://github.com/kubernetes/cri-api)、
[Configuring a cgroup driver](https://kubernetes.io/docs/tasks/administer-cluster/kubeadm/configure-cgroup-driver/)、
[kubeadm](https://kubernetes.io/docs/reference/setup-tools/kubeadm/)、
[kind](https://kind.sigs.k8s.io/)。

---

## 4.7 虚拟化：KVM、virtio、Cloud Hypervisor、libvirt、cloud-init

**KVM** 是内核自带的 hypervisor，以 `/dev/kvm` 的形式暴露出来；一个用户态的 **VMM**
（QEMU、Cloud Hypervisor）用它来运行客户机。**virtio** 是客户机用来实现高效 I/O 的
一族半虚拟化设备（磁盘、网络、9p 文件系统共享）。**Cloud Hypervisor** 是一个专注于
云工作负载的 Rust VMM，只要能访问 `/dev/kvm`，就可以在无特权下运行；它通过一份固件
（一个 EDK2 UEFI 构建，或者 `rust-hypervisor-firmware`）来引导客户机，也可以直接从
一个内核镜像引导。**libvirt** 通过 `virsh` 和 `libvirtd`，管理用 XML 描述的
QEMU/KVM domain。

云镜像是通用的；每个实例各自的配置（主机名、SSH 密钥、用户、网络）来自
**cloud-init**，它会读取一个数据源。**NoCloud** 数据源是一个标签为 `cidata` 的小型
ISO，里面存放着 `user-data`、`meta-data`，以及可选的 `network-config`。

**在 Delonix 中**

- 这个端口是 `crates/adapters/delonix-vm/src/lib.rs` 中的 `VmBackend`，由同一文件里
  的 `CloudHypervisorBackend` 和 `LibvirtBackend`，以及 `crates/providers/delonix-proxmox`
  中的 `ProxmoxBackend`（[ADR-0008](../adr/0008-proxmox-vm-backend.md)）实现。
- Cloud Hypervisor 固件的查找顺序：`DEFAULT_CH_FIRMWARES`（EDK2 的 `CLOUDHV.fd` 排在
  `hypervisor-fw` 之前）；VMM 的命令行在 `boot_ch` 中构建。
- libvirt 的 domain XML：`libvirt_domain_xml`。
- NoCloud seed 的生成：`bins/delonix-runtime-bin/src/cmd/vm.rs` 中的
  `generate_seed_iso`。
- 无根 SDN 上的虚拟机，会获得一个从它们的 MAC 地址推导出来的 DHCP 租约：
  `crates/adapters/delonix-sdn/src/infra.rs` 中的 `dhcp_lease_ip`。
- 实际的搭建方法与已知的宿主机陷阱：[构建微虚拟机](microvm-setup.md)。

**延伸阅读：** [kernel — KVM](https://docs.kernel.org/virt/kvm/index.html)、
[virtio specification (OASIS)](https://docs.oasis-open.org/virtio/virtio/)、
[Cloud Hypervisor](https://www.cloudhypervisor.org/) 及其
[文档](https://github.com/cloud-hypervisor/cloud-hypervisor/tree/main/docs)、
[libvirt](https://libvirt.org/docs.html)、
[cloud-init NoCloud](https://docs.cloud-init.io/en/latest/reference/datasources/nocloud.html)。

---

## 4.8 声明式协调

Kubernetes 让一种模型流行起来：用户以带类型的对象（`apiVersion`、`kind`、`metadata`、
`spec`）提交**期望状态**，控制器反复把它和**实际状态**相比较，并采取行动去收敛。
`kubectl apply` 加入了一次**三路 diff**：它把最后一次应用的配置保存在对象上，这样
它就能分清"你从文件里删掉了这个字段"（把它还原）和"有人手动设置了这个字段"（不去
动它）这两种情况。

这条原则，以及当你新增一个 Kind 时它对你的要求，都在
[IaaS 与云原生——声明式与收敛式](iaas-and-cloud-native.md#declarative-and-convergent)里。

**在 Delonix 中**

- 引擎在各个 API 分组中拥有自己的 Kind（用 `delonix api-resources` 列出它们）。
  关于每个 Kind 的事实（所属领域、是否收敛、拆除方式、是否按命名空间划分）都住在
  一张表里：`crates/contexts/delonix-stack/src/kinds.rs` 中的 `KindFacts`。
- 规划器是纯函数：`crates/contexts/delonix-stack/src/reconcile.rs` 中的
  `plan(desired, actual, stack)`。模块文档里有那张三路真值表，最后一次应用的规格
  保存在资源本身上，放在 `LAST_APPLIED` 注解（`delonix.io/last-applied`）下面——
  没有单独的状态文件。
- 归属关系是资源上的一个标签；修订历史在 `revision.rs`
  （[ADR-0019](../adr/0019-stack-revision-history.md)）里。
- 清单在 `bins/delonix-runtime-bin/src/cmd/manifest.rs` 中解析；`stack plan`/`apply`
  住在 `cmd/stack.rs` 里。
- 后台没有运行任何控制器循环：协调只在一条命令运行时才发生（无守护进程）。提议中的
  pull 协调器保留了这个特性——它是一个调用同一个 apply 的 systemd 定时器，而不是
  一个驻留进程（[ADR-0021](../adr/0021-gitops-pull-reconciler.md)，状态为*提议中*）。

**延伸阅读：** [Kubernetes — Objects](https://kubernetes.io/docs/concepts/overview/working-with-objects/)、
[Controllers](https://kubernetes.io/docs/concepts/architecture/controller/)、
[Declarative management with `kubectl apply`](https://kubernetes.io/docs/tasks/manage-kubernetes-objects/declarative-config/)。

---

## 4.9 可观测性与 MCP 接口

**OpenTelemetry** 是一份关于 trace、指标和日志的 CNCF 标准，通过 **OTLP** 导出给一个
collector。**Prometheus** 以一种文本暴露格式，从一个 HTTP `/metrics` 端点抓取指标。
**Model Context Protocol**（模型上下文协议）是一个开放协议，让 AI 客户端能够发现并
调用一个服务器暴露出来的工具，通常是通过 stdio。

**在 Delonix 中**

- 结构化日志和可选的 OTLP span：`crates/adapters/delonix-telemetry/src/telemetry.rs`
  中的 `init`（只有设置了 `DELONIX_OTLP_ENDPOINT` 时才会导出 span；导出器运行在
  自己的线程上，所以同步的 CLI 不需要任何异步运行时）。
- Prometheus 注册表（前缀 `delonix`）与文本编码：
  `crates/adapters/delonix-telemetry/src/metrics.rs`（`encode`）。本地管理 API 在
  `crates/interfaces/delonix-mgmt/src/lib.rs` 中提供 `/metrics` 服务。
- MCP：`crates/interfaces/delonix-mcp`（构建在 [`rmcp`](https://docs.rs/rmcp) 之上，
  stdio 传输），二进制文件在 `bins/delonix-mcp-bin`。范围与限制：
  [ADR-0025](../adr/0025-mcp-local-ai-control-surface.md)。

**延伸阅读：** [OpenTelemetry documentation](https://opentelemetry.io/docs/)、
[OTLP specification](https://opentelemetry.io/docs/specs/otlp/)、
[Prometheus — exposition formats](https://prometheus.io/docs/instrumenting/exposition_formats/)、
[Model Context Protocol](https://modelcontextprotocol.io/)。

---

## 4.10 无守护进程（Daemonless），一段话讲清楚

containerd 和 Docker Engine 都保留着一个持有容器状态的驻留守护进程；Podman 证明了
一个运行时完全可以换成一条会退出的命令，用按容器划分的辅助进程，以及 systemd，去
处理任何必须持久化的东西。Delonix 遵循的是第二种模型：CLI 完成工作后就退出，状态是
状态根下面受 `flock` 保护的文件（见
[Rust 入门 §3.8](rust-primer.md#38-concurrency-and-shared-state)），每个分离运行的
容器都有一个 supervisor 进程存在，网络 holder 只在有东西需要它时才存在，而开机
持久化靠的是 systemd unit（`bins/delonix-runtime-bin/src/cmd/boot.rs`）。这样做的
后果——好的和坏的——在[架构](architecture.md)和
[系统设计面试](system-design-interview.md)里都有讨论。这条原则本身，以及"新增一个
守护进程需要一份 ADR"这条规则，都在
[IaaS 与云原生——无守护进程](iaas-and-cloud-native.md#daemonless)里。

---

**下一步：** [面向本代码库的 Rust 入门](rust-primer.md)——这个代码库是用什么样的
Rust 写的——workspace、错误、作为端口的 trait、`unsafe`、serde、clap、并发与测试——
全部钉在真实的文件上。
