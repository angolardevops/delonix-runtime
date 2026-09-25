<!-- translated-from: microvm-setup.md sha256:5e00934168ce8abaeb7e284855fff4df09522a9379b0b302800bcea4178b5543 -->
# 构建 microVM

**阅读之前：**[准备你的环境](environment.md)、[克隆、构建与测试](build-and-test.md)、[云原生入门中的虚拟化一节](cloud-native-primer.md#47-virtualization-kvm-virtio-cloud-hypervisor-libvirt-cloud-init)，以及[Delonixfile 与 VMfile](delonixfile-and-vmfile.md#part-2-vmfile)的第 2 部分。

本页带你从一台裸机 Linux 宿主机开始，一路用 Delonix 构建、启动并测试虚拟机，并指出改动某处时代码位于哪里。读完之后，你能够为 Cloud Hypervisor 和 libvirt 准备好宿主机、预测某台虚拟机会落到哪个后端，并通过 CLI 测试一次虚拟机相关的改动。它假设你已经读过[准备你的环境](environment.md)，并且能够构建这棵代码树（[克隆、构建与测试](build-and-test.md)）。

> **本页做过哪些核实。** 下面每一条命令和参数都对照过用这棵树构建出的二进制程序的
> `delonix <group> --help`；只读/配置类命令（`vm ls`、`vm reach`、`vm default-backend`、
> `image vm ls`、`manifest validate`、`stack apply --dry-run`，以及后端选择相关的错误）在把
> `DELONIX_ROOT` 和 `DELONIX_NET_RUNTIME_DIR` 指向一个临时目录之后被**实际运行**过。**启动一台
> 虚拟机、构建一个镜像、从镜像仓库拉取，这些在本次复核中都没有被执行过**——它们的行为来自
> 代码本身、ADR，以及下文引用的脚本化测试。

实验时始终使用你自己构建出的那个二进制程序（`./target/debug/delonix`），而不是 `PATH` 里的那个，并且始终**同时**隔离两个状态根目录——参见[先做隔离](#isolation-first)。

---

## 1. 宿主机的前置条件

### KVM

两个本地后端都需要硬件虚拟化：

```bash
ls -l /dev/kvm            # must exist; missing = VT-x/AMD-V off in firmware, or no nested virt
id -nG | tr ' ' '\n' | grep -x kvm   # your user must be in the kvm group
```

`scripts/install.sh`（默认情况下，也就是不带 `--no-vm`）会把你加进 `kvm` 和 `libvirt` 用户组，并在 `/dev/kvm` 缺失时给出警告。用户组的变更需要一次新的登录会话才能生效。

### Cloud Hypervisor 及其固件

当 `cloud-hypervisor` 出现在 `PATH` 里时，这个后端就可用（`crates/adapters/delonix-vm/src/lib.rs` 里的 `CloudHypervisorBackend::available`）。发行版没有打包它的情况下，安装脚本会把上游的**静态**二进制程序下载到 `/usr/local/bin/cloud-hypervisor`，并在 `scripts/install.sh` 里把它固定到某个版本**以及**某个 SHA-256。

要在不带 `--kernel` 的情况下启动一个云镜像，CH 需要 UEFI 固件。`default_ch_firmware` 会先用 `$DELONIX_HYPERVISOR_FW`（如果设置了的话），否则用 `DEFAULT_CH_FIRMWARES` 里第一个存在的文件：

```text
/usr/local/share/delonix/CLOUDHV.fd       ← EDK2 build from cloud-hypervisor/edk2 (preferred)
/usr/share/delonix/CLOUDHV.fd
/usr/local/share/delonix/hypervisor-fw    ← rust-hypervisor-firmware (fallback)
/usr/share/delonix/hypervisor-fw
```

**顺序很重要。** 已经测量并记录在这个常量的文档注释里：用
`rust-hypervisor-fw` 时，这个项目构建出的镜像没有一个能在 CH 里启动；换成 EDK2 的
`CLOUDHV.fd` 就可以。`hypervisor-fw` 仍然作为只有它可用的宿主机的后备项。安装脚本会把两者
都下载下来（各自按 tag 和 SHA-256 固定版本）。单元测试
`o_edk2_vem_antes_do_hypervisor_fw_na_procura_de_firmware` 守护着这个顺序。

### libvirt / QEMU

当 `virsh` 和 `qemu-system-x86_64` 同时出现在 `PATH` 里时，libvirt 这个后端就可用
（`LibvirtBackend::available`）。安装脚本会安装 QEMU 和一个 libvirt 守护进程包，并启用
`libvirtd`（在支持的地方以 socket 方式激活）。

用的是哪一种 libvirt 连接，比看上去更重要（`libvirt_uri_for`）：

| 情形 | 连接 | 后果 |
|---|---|---|
| `--net-mode nat` 或 `bridge` | `qemu:///system` | IP 可达；需要 `libvirt` 用户组（或 root） |
| 没有 `--net-mode`，system 连接可用 | `qemu:///system`，**自动选择 `nat`** | 由 libvirt 网络（`virbr0`）的 DHCP 分配 IP |
| 没有 `--net-mode`，system 连接**不**可用，rootless | `qemu:///session`，用户态模式 | **没有可见或可达的 IP**；`vm create` 会给出警告 |

在 `qemu:///system` 上，QEMU 以 libvirt 服务用户的身份运行，读不到一个权限为 0700 的家目录下的磁盘。对一个 rootless 调用者来说，domain 的 XML 会带上一个静态的 DAC `seclabel`，用 `relabel='no'` 把 QEMU 固定到你自己的 uid/gid 上，这样你自己的 overlay 就能启动，而不会被 chown 走。

### 虚拟机代码会 shell 出去调用的工具

| 工具 | 软件包（Debian / Fedora） | 用途 |
|---|---|---|
| `qemu-img` | `qemu-utils` / `qemu-img` | 每台虚拟机的 overlay、`vm convert`、CH 上的快照、镜像构建 |
| `cloud-localds` | `cloud-image-utils` / `cloud-utils` | NoCloud seed ISO——除非给出 `--seed`，否则**每次** `vm create` 一个 cloud-init 镜像时都会生成（`crates/adapters/delonix-vm/src/cloudinit.rs`） |
| `virsh` | `libvirt-clients` / `libvirt-client` | libvirt 后端 |
| `virt-customize`、`virt-sparsify`、`virt-copy-out` | `libguestfs-tools` / `guestfs-tools` | 仅用于 `vm build` / `image vm build` |

`vmimage::tool_package` 会把一个缺失的二进制程序映射到它所属的软件包，因此缺少某个工具时报告的是它的名字，而不是一句干巴巴的 `No such file or directory`。

### 仅用于构建镜像：`--with-image-build`

`image vm build` 会运行 `virt-customize`，它用 supermin 构建一个小型 appliance。有三类宿主机问题会让它出错，而出错的样子并不像是宿主机问题；`scripts/install.sh --with-image-build` 会处理这三类问题，构建失败时 `tool_failure_hint`（`cmd/vmimage.rs`）会指出具体是哪一类：

1. **宿主机上没有 DHCP 客户端。** supermin 会把宿主机上的软件包*复制*进 appliance；没有
   `isc-dhcp-client`，appliance 就没有网络，构建会死在 `Temporary failure resolving …` 上。安装脚本会装上它。
2. **`/boot/vmlinuz-*` 权限是 0600**（Debian/Ubuntu）。supermin 会复制宿主机的内核，因而以
   `Permission denied` 失败。安装脚本会执行 `chmod 0644 /boot/vmlinuz-*`——这**降低了一道宿主机
   安全边界**（任何本地用户都能读取内核镜像），所以它是可选加入的，而且会打印怎么还原
   （`sudo chmod 0600 /boot/vmlinuz-*`）。失败提示里还会说明怎么让它在内核更新后依然生效。
3. **passt**，只在 `image vm build --network` 时才会遇到。libguestfs 通过 passt 给 appliance
   接上网络。它的 AppArmor 配置文件（Debian/Ubuntu）禁止 libguestfs 用的那个运行时目录，而
   Ubuntu 24.04 打包的 passt 能启动，却始终不发放租约——`dhclient` 会等大约 300 秒，构建接着
   在**没有**网络的情况下继续，随后在某个软件包镜像那里失败。补救办法：

   ```bash
   mkdir -p /tmp/delonix-run && chmod 700 /tmp/delonix-run
   XDG_RUNTIME_DIR=/tmp/delonix-run ./target/debug/delonix image vm build --network …
   ```

   如果这样还不行，就让一个较新的 passt**排在 `PATH` 最前面**（安装脚本会把一个这样的版本
   构建进 `/usr/local/bin`）。不要用一个会失败的桩程序去"禁用" passt：libguestfs 会去用那个
   桩程序，然后死在它上面。

黄金配方的 `--offline` 模式完全避开了第 3 个陷阱：它在宿主机上获取并校验软件包，然后带着
`--no-network` 运行客户机。

### 先做隔离

这台机器上可能还跑着别的工作负载。在执行 `--help` 以外的任何 `vm` 命令之前：

```bash
export DELONIX_ROOT=$HOME/dlx-dev/root
export DELONIX_NET_RUNTIME_DIR=/tmp/dlx-dev-run     # keep it SHORT (AF_UNIX sun_path is 108 bytes)
export TMPDIR=$HOME/dlx-dev/tmp                     # VMfile builds put whole disks here
mkdir -p "$DELONIX_ROOT" "$DELONIX_NET_RUNTIME_DIR" "$TMPDIR"
```

两个根目录，始终都要隔离：只隔离 `DELONIX_ROOT` 会让网络套接字仍然和真实状态共享，之前
就曾因此重启过真实的工作负载（见[克隆、构建与测试](build-and-test.md)）。当
`<root>/vms/<name>.sock` 放不进 `sun_path` 时（`ch_socket_paths_fit`），一台 Cloud Hypervisor
虚拟机还会在动手之前就直接拒绝，所以要避免层级很深的 `DELONIX_ROOT` 路径。

---

## 2. 后端及其选取方式

### 端口与注册表

`VmBackend`（`crates/adapters/delonix-vm/src/lib.rs`）是每一个 hypervisor 都要实现的端口：
`id`、`available`、`boot`、`is_running`、`ip`、`stop`，再加上一批有默认实现的方法
（`destroy`、`pause`、`unpause`、`resume`、`snapshot`/`restore`/`snapshots`/`delete_snapshot`、
`preserve_snapshots`、`ip_is_predicted`、`manages_own_storage`、`auto_selectable`、
`disk_health`）。一个无法兑现的默认实现会带着一条消息 **fail closed**（失败即拒绝），
而绝不会悄悄地什么都不做。

各个后端存放在一个注册表（`BACKENDS`）里，而不是一个 `match` 里：

| 后端 | Crate | 别名 | 能否自动选取 |
|---|---|---|---|
| `cloud-hypervisor` | `delonix-vm`（内置） | `ch`、`cloudhypervisor` | 能 |
| `libvirt` | `delonix-vm`（内置） | `kvm`、`qemu` | 能 |
| `proxmox` | `delonix-proxmox`（由 CLI 负责注册） | — | **不能**——只能按名字选取 |

`register_backend` 会拒绝一个已经属于别的后端的 id/别名，也会拒绝给任何非内置的后端标上
`auto_selectable: true`：自动检测会向每一个候选者询问 `available()`，而一个远程后端只能通过
网络才能回答这个问题。注册这一步不做任何 I/O；工厂函数只在该后端第一次被选中时才运行。
对一台已存在的虚拟机，`backend_for` 会解析出记录下来的那个后端，一个未知的名字是一个
**错误**（它以前会回退到 CH）。

### 新建虚拟机时的选取优先级

来自 `delonix_vm::create_with` 和 `resolve_vm_defaults`（`cmd/vm.rs`），第一个命中的生效：

1. `--backend`（或者清单里的 `backend:`）。
2. 镜像自己的 `HYPERVISOR`（由一次 VMfile 构建记录下来），前提是 `--disk` 指向的是一个本地
   镜像。
3. `DELONIX_VM_BACKEND`（整个会话范围）。
4. `delonix vm default-backend --set <backend>`（整台机器范围，存放在
   `<DELONIX_ROOT>/vm-default-backend`）。
5. 能力启发式规则：有 `volumes` ⇒ `libvirt`（只有 libvirt 支持 virtio-9p）；一个没有
   `--kernel` 的云镜像 ⇒ **如果 libvirt 可用**就选 `libvirt`；否则走自动检测——选中第一个
   已安装、且标记为可自动选取的已注册后端（先 CH，再 libvirt）。

所以在同时装了两种 hypervisor 的宿主机上，一次普通的 `vm create` 某个云镜像会落到
**libvirt** 上；要得到一台接在 SDN 上的 microVM，就传 `--backend cloud-hypervisor`
（或者设置一个默认值）。

```text
$ delonix vm default-backend
none (auto-detection: cloud-hypervisor if installed, else libvirt)
$ delonix vm default-backend --set ch
default backend set to cloud-hypervisor
$ delonix vm default-backend --set bogus
error invalid argument: unknown VM backend: 'bogus' (use 'cloud-hypervisor', 'libvirt')
$ delonix vm default-backend --clear
default backend cleared (falls back to auto-detection)
```

### Proxmox VE（远程）

当通过环境变量做了配置时，`bins/delonix-runtime-bin/src/cmd/vmbackends.rs::register_configured`
会在启动时注册 Proxmox 后端（配置有误只是一条警告，绝不会让无关的命令因此失败）：

| 变量 | 含义 |
|---|---|
| `DELONIX_PROXMOX_URL` | API 的基础 URL，例如 `https://pve.example:8006`。不设置 = 不注册这个后端。 |
| `DELONIX_PROXMOX_NODE` | 必填。这个后端所对应的那唯一一个节点（就是 `GET /nodes` 里给它起的名字）。 |
| `DELONIX_PROXMOX_SECRET` | 首选的凭据方式：一个带有 `tokenId`+`tokenSecret`（或 `username`+`password`）的 `kind: Secret` 的名字。 |
| `DELONIX_PROXMOX_TOKEN_ID` + `DELONIX_PROXMOX_TOKEN` | 来自环境变量的 API token。 |
| `DELONIX_PROXMOX_USER` + `DELONIX_PROXMOX_PASSWORD` | 密码登录（ticket 方式，在 401 时重新认证）。 |
| `DELONIX_PROXMOX_INSECURE_TLS` | `1`/`true`/`yes` 表示跳过证书校验。是可选加入项，绝不是回退方案。 |
| `DELONIX_PROXMOX_BRIDGE` | 该节点上的默认 bridge（单台虚拟机自己的 `bridge` 优先）。 |
| `DELONIX_PROXMOX_VLAN` | 默认的 VLAN 标签，取值 1–4094；超出范围是一个错误，而不是悄悄地给网卡不打标签。 |

没有配置的情况下，`--backend proxmox` 会回答说这个后端"在这次构建里不可用"，并说明该
设置什么（*已运行*）。这个后端拥有自己的存储（`manages_own_storage`），所以不会生成本地
overlay 或 NoCloud seed；`--hostname`/`--ssh-key` 会被送到节点自己的 cloud-init 里，而
`--user-data` 会被拒绝。设计与限制：[ADR-0008](../adr/0008-proxmox-vm-backend.md)。一个
OpenStack 后端**仅仅是提议阶段**（[ADR-0039](../adr/0039-openstack-vm-backend.md)）；还没有
任何代码。

---

## 3. 镜像

虚拟机镜像存放在 `VmImageStore`（`cmd/vmimage.rs`）里，位于
`<DELONIX_ROOT>/vm-images/` 之下：每个镜像一个 qcow2 加一条 JSON 元数据记录
（`VmImage`）。`delonix image vm ls` 会把它们列出来，带上 `TYPE`（cloud-init / appliance）
和 `DEFAULTS`（记录下来的 vCPU/内存）。

### 官方镜像

`cmd/vmimage.rs` 里的 `OFFICIAL_REPOS`：

| 键 | 仓库 | 内容 |
|---|---|---|
| `k8s` | `ghcr.io/angolardevops/delonix-vm-k8s` | Kubernetes 节点（kubeadm/kubelet/kubectl + `delonix-cri`） |
| `base` | `ghcr.io/angolardevops/delonix-vm-base` | 带 `delonix` 引擎的基础操作系统，不含 Kubernetes（例如 `ubuntu-24.04`） |
| `appliances` | `ghcr.io/angolardevops/delonix-vm-appliances` | 厂商 appliance，不含 cloud-init |

```bash
delonix vm ls-remote                 # tags of the Kubernetes golden repo
delonix vm ls-remote --no-k8s        # tags of the base repo
delonix vm pull                      # the official Kubernetes golden
delonix vm pull --no-k8s             # the official base image
delonix vm pull <oci-ref> --name <local-name>
```

镜像是单 blob 的 OCI artifact；拉取时会校验 manifest 和 blob 的摘要，并从 manifest 的
annotation 里还原出元数据。同样这几个动词也以 `delonix image vm pull/ls-remote/push` 的
形式存在。**注意：**不带 `--disk` 又没有本地镜像的 `vm create` 会下载官方的黄金镜像，因此
需要联网。

### 构建黄金配方

`delonix vm build -t <tag>`（和 `delonix image vm build` 是同一个命令；两者共用同一套参数）
在上下文里既没有 `vm.yaml` 也没有 `VMfile` 时，会运行内置配方。下面的例子用的是
`image vm` 这种写法：

```bash
# Kubernetes node, packages fetched and verified on the HOST, guest offline
delonix image vm build --offline --k8s-version 1.34 -t delonix-vm-k8s:1.34
# no Kubernetes: just the engine, rootless-ready
delonix image vm build --no-k8s --distro debian --debian-release bookworm -t delonix-vm-base:debian-bookworm
```

相关参数（默认值请查 `image vm build --help`）：`--distro ubuntu|debian|rocky|fedora`、
`--ubuntu-release`、`--debian-release`、`--rocky-release`、`--fedora-release`（release **和**
build 两部分，例如 `42-1.1`）、`--k8s-version`、`--offline`、`--no-k8s`、`--extra-package`、
`--extra-run`、`--cri-bin`、`--delonix-bin`、`--root-password`（不给它，任何账户都不会有
密码）、`--node-exporter[=<addr>]`、`--no-compress`。你自己的配方是一个 `VMfile` 或一个
`vm.yaml`——见[Delonixfile 与 VMfile](delonixfile-and-vmfile.md)。仓库里的 `images/` 目录为
每种发行版（`ubuntu`、`debian`、`rocky`、`fedora`，均离线构建）和每个 appliance 各带了一份
`vm.yaml`；`scripts/verify-images.sh` 会在一个隔离的 `DELONIX_ROOT` 里构建它们，并把 qcow2
读回来做核实（`--self-test` 证明它的检查确实能失败）。软件包安装、黄金配方的各个
profile、启动构建出来的镜像，以及 appliance 构建器，都**还没有被验证过**（v4.2.0 发布
说明）。镜像构建只支持 amd64（[ADR-0018](../adr/0018-vm-images-stay-amd64.md)）。

### 转换与导入

```bash
delonix vm convert <image-or-path> --to raw|qcow2|vmdk|vdi|vhdx|vhd [-o out] [--compress]
delonix image vm import disk.qcow2 -t opnsense:26.1 --appliance --default-vcpus 2 --default-memory 3G
```

`vm convert` 会拍平磁盘（没有 backing chain）；`--compress` 只在目标是 `qcow2` 或 `vmdk` 时
才被接受。`import --appliance` 会记录下 `cloud_init: false`：这样一来 `vm create` 就**不会**
再挂载 seed，并且会按名字拒绝 `--hostname`/`--ssh-key`/`--user-data`，因为客户机根本不会
去读它们。Appliance 的构建脚本放在 `scripts/appliances/` 里。

---

## 4. 创建与运行

### `vm create`

```bash
delonix vm create dev --disk delonix-vm-base:ubuntu-24.04 \
  --backend cloud-hypervisor --vcpus 2 --memory 2G \
  --ssh-key @$HOME/.ssh/id_ed25519.pub --hostname dev --wait
```

发生了什么（`cmd/vm.rs` → `delonix_vm::create_with`）：

1. 在解析任何镜像之前，先强制执行**节点策略**（`policy::enforce`）。
2. **磁盘解析**（`resolve_image_ref`）：`--url-img` 优先（下载、缓存，并在提供了
   `<url>.sha256` 时对照校验）；否则先把 `--disk` 当作本地镜像名去查，再当作某个已存镜像
   qcow2 的路径去查；再不行就把它当作一个普通路径来用；不带 `--disk` 时，会拉取那唯一的
   本地黄金镜像，或者官方镜像。
3. 来自镜像元数据的**默认值**只会去填补 `--vcpus`/`--memory`/`--backend` 里没有设置的部分。
4. **Seed**：除非给出 `--seed`、这个镜像是一个 appliance、或者后端是远程的，否则会生成一个
   NoCloud ISO（按 MAC 配置网络、主机名、密钥）。`--user-data` 会替换掉生成出来的
   user-data。
5. 选定**后端**（见第 2 节）；宿主机内存不够时，一次准入检查会拒绝创建；在 libvirt 上，
   除 `default` 以外的 `--namespace` 会被拒绝（这台虚拟机活在 `virbr0` 上，在 Delonix SDN
   之外）。
6. **Overlay**：`<root>/vms/<name>.qcow2`，一个叠在 base 之上的瘦身 qcow2
   （`prepare_local_overlay`）；`--disk-size <GiB>` 可以把它撑大，但不能比 base 还小。
7. **启动**：调用该后端的 `boot`。`create` 是幂等的：一台已存在且正在运行的虚拟机会被原样
   返回。

已发布的镜像不会给任何账户设置密码（见上面的 `--root-password`），所以想通过 SSH 登录
的话就传 `--ssh-key`。

**`--wait` 与预测出来的 IP。** 在 libvirt 上，IP 来自一个真实的 DHCP 租约，所以拿到了 IP
就是客户机已经启动的证据。在 Cloud Hypervisor 上，IP 是在客户机运行之前就**从 MAC 计算出来
的**（`ip_is_predicted`），所以 `--wait` 还会在网络 holder 内部用 ARP 去探测这个地址
（`delonix_sdn::infra::sdn_reachable`），直到 `--boot-timeout`（默认 120 秒）为止。三种结果：
已启动；"从这里无法核实"（没法发出探测）；"正在运行但从未应答……是从 MAC 算出来的，
不是观测到的"。想知道原因，用 `vm console`。

### Day-2 动词

| 命令 | 说明 |
|---|---|
| `delonix vm ls [--namespace <ns>] [--ports] [-o json]` | 列出各台虚拟机 |
| `delonix describe vm <name>` / `delonix delete vm <name>` | 没有 `vm describe`/`vm rm` 这两个命令；通用动词取代了它们 |
| `delonix vm console <name> [-e ^X]` | 串口控制台；默认用 `Ctrl-]` 脱离（`$DELONIX_CONSOLE_ESCAPE`）。libvirt：`virsh console` 作为子进程；CH：控制台套接字 `<root>/vms/<name>.console` |
| `delonix vm ssh <name\|ip> [-l user] [-i key] [-- cmd]` | IP 取自记录；cloud-init 镜像默认用户是 `delonix`，appliance 上是 `root` |
| `delonix vm vnc <name>` | 仅对用 `--vnc` 创建的 libvirt 虚拟机有效（CH 没有显示设备） |
| `delonix vm stop <name>` | 保留磁盘、记录和快照。libvirt：domain 会被 undefine（快照元数据会先被保留下来） |
| `delonix vm start <name>` / `restart <name>` | 从记录重建启动配置并复用 overlay；`start` 一台正在运行的虚拟机是空操作，`restart` 总会重新启动 |
| `delonix vm pause` / `unpause <name>` | 挂起各 vCPU，内存留在 RAM 里；CH 和 libvirt 都支持 |
| `delonix vm migrate <name> --host <h> --network <n>` | 通过 SSH 对另一台宿主机做 stop-copy-start；有真实停机时间（[ADR-0031](../adr/0031-live-vm-migration-no-go.md) 说明了为什么实时迁移不在范围内） |
| `delonix vm prune` | 回收没有任何虚拟机记录认领的状态 |

### 快照

`delonix vm snapshot create|ls|rm|restore <vm> [<snapshot>]`：

| 后端 | 运行中的虚拟机 | 已停止的虚拟机 |
|---|---|---|
| libvirt | `create` 是一次系统检查点（内存 + 磁盘） | 只有磁盘；这四个动词只在命令执行期间临时 define 这个 domain |
| cloud-hypervisor | 只有 `ls`（`qemu-img info -U`）；`create`/`restore`/`rm` 都**被拒绝**——运行中的 VMM 锁着磁盘，而且 CH 没有在线磁盘快照的 API | 全部四个，通过 `qemu-img snapshot` |
| proxmox | `create`（带虚拟机状态）、`ls`、`restore` | 一样；`rm` 未实现（fail closed） |

libvirt 的快照能挺过 `vm stop`/`vm start`：`undefine --snapshots-metadata` 只会移除 libvirt
自己的记账信息，所以 `preserve_snapshots` 会在停止之前，把每个快照的 XML 转存到
`<root>/vms/<vm>/snapshots/`，而 `boot` 会重新 define 它们（改写 domain uuid，因为每次
define 它都会变）。

### 声明式

`kind: VirtualMachine`（`compute.delonix.io/v1alpha1`）映照着 `vm create`；一份带完整注释
的例子是 `examples/vm.yaml`。带 `type: microvm` 的 `kind: Workload` 会降解为一个
`VirtualMachine`，后端被**强制**为 `cloud-hypervisor`；要求换成别的后端是一个错误
（[ADR-0006](../adr/0006-workload-type-microvm.md)）。下面两个都在一个临时根目录里
*运行过*：

```yaml
apiVersion: compute.delonix.io/v1alpha1
kind: VirtualMachine
metadata: { name: dev }
spec:
  disk: delonix-vm-base:ubuntu-24.04
  resources: { vcpus: 2, memory: 2G }
  cloudInit:
    hostname: dev
    sshKeys: ["@~/.ssh/id_ed25519.pub"]
---
apiVersion: compute.delonix.io/v1alpha1
kind: Workload
metadata: { name: fast }
spec:
  type: microvm
  microvm: { disk: delonix-vm-base:ubuntu-24.04, vcpus: 1, memory: 1G }
```

```text
$ delonix manifest validate -f vm.yaml
stack validate: OK — 2 document(s), all references resolved
$ delonix stack apply -f vm.yaml --dry-run | grep backend
  backend: null
  backend: cloud-hypervisor
```

用 `delonix vm apply -f vm.yaml` 或 `delonix stack apply -f vm.yaml` 来应用（这里没有执行过）。

---

## 5. 虚拟机的网络

| | Cloud Hypervisor | libvirt |
|---|---|---|
| 网卡活在哪里 | 挂在某个 Delonix 网络上的一个 tap（`--network`，默认 `ingress`），**位于网络 holder 内部**——和容器用的是同一套 SDN | `virbr0`（nat）或宿主机上的一个 bridge，位于宿主机自己的网络命名空间里 |
| IP | 由 holder 的 DHCP 分配，从 MAC 确定性地算出 | libvirt DHCP；`--ip` 可以预留一个（仅限 nat） |
| 命名空间隔离 | 支持（`--namespace`） | 拒绝 |
| 虚拟机 ↔ 容器 之间按 IP 互通 | 直连 | 容器 → 虚拟机 通过宿主机可行；虚拟机 → 容器 需要一个已发布的端口，或者 `vm bridge` |

**`vm reach`**（只读，不需要权限）会列出 libvirt 的各个网关，并针对某个正在运行的容器
所发布的每一个端口，说明一台虚拟机能不能连到它。一个发布在默认 `127.0.0.1` 上的端口，
对虚拟机来说是不可见的；这条命令会打印出确切的重新发布方式，例如
`DELONIX_PUBLISH_ADDR=<gateway> delonix net ingress publish <c> <port>`——那样该网络上的
虚拟机就能连到，但外部局域网依然连不到。

**`vm bridge <network> [--vm-subnet <cidr>] [--apply]`** 是**实验性的，需要 root**：从
宿主机拉一条 veth 进 holder，再加上路由，让 libvirt 虚拟机能以 IP 方式直连某一个容器网络。
不带 `--apply` 时只会打印计划。`vm unbridge <network>` 负责拆掉它（同样，不带 `--apply`
就是 dry-run）。这是虚拟机代码里唯一一处刻意对 rootless 做出的例外（`cmd/vmbridge.rs`）。

---

## 6. 排查问题

| 症状 | 原因 | 解决办法 |
|---|---|---|
| `/dev/kvm does not exist` / 虚拟机启动失败 | 虚拟化被禁用，或没有嵌套虚拟化 | 开启 VT-x/AMD-V；如果是在一台虚拟机里，开启嵌套虚拟化 |
| `no VM backend available` | `PATH` 里既没有 `cloud-hypervisor`，也没有 `virsh`+`qemu-system-x86_64` | 装一个；`scripts/install.sh` 会做这件事 |
| CH 虚拟机显示"running"但从不应答；overlay 一直很小 | 固件无法启动这个镜像（例如只装了 `hypervisor-fw`） | 把 EDK2 的 `CLOUDHV.fd` 装到 `/usr/local/share/delonix/`，或者设置 `DELONIX_HYPERVISOR_FW`，或者用 `--backend libvirt` |
| `vm ls` 在一台 libvirt 虚拟机上不显示 IP | 回退到了 `qemu:///session` 用户态模式 | 加入 `libvirt` 用户组并重新登录，或者用 `--net-mode nat` |
| `warning: cannot reach qemu:///system for NAT networking` | 不在 `libvirt` 用户组里 | `sudo usermod -aG libvirt $USER`，然后开一个新会话 |
| libvirt 上的 `vm console`："Active console session exists" | 上一次会话没有正常退出 | 现在的代码会给 `virsh console` 传 `--force`；更新你的二进制程序 |
| CH 虚拟机在启动前就因为提到套接字路径而被拒绝 | `<root>/vms/<name>.sock` 超过了 108 字节 | 换一个更短的 `DELONIX_ROOT` 或虚拟机名字 |
| `namespace '…' is not enforceable on the 'libvirt' backend` | libvirt 虚拟机在 SDN 之外 | 用 `--backend cloud-hypervisor`，或者去掉 `--namespace` |
| `vm snapshot create` 在一台运行中的 CH 虚拟机上被拒绝 | CH 没有在线磁盘快照能力 | 先 `vm stop`，或者改用 libvirt |
| `--hostname`/`--ssh-key` 被拒绝 | 这个镜像是一个 appliance（`cloud_init: false`） | 通过 appliance 自己的控制台/界面去配置 |
| `cloud-localds not found` | 缺少 `cloud-image-utils` | 安装它（每一次 cloud-init 的 `vm create` 都需要） |
| `image vm build`：`cp: cannot open '/boot/vmlinuz-…'` | 宿主机内核权限是 0600 | `install.sh --with-image-build`，或者 `sudo chmod 0644 /boot/vmlinuz-*`（会降低一道边界） |
| `image vm build`：`Temporary failure resolving …` | appliance 没有网络：缺少宿主机的 DHCP 客户端，或者是 passt 的问题 | 安装 `isc-dhcp-client`；用 `--network` 时用 `XDG_RUNTIME_DIR=/tmp/delonix-run`，并让一个较新的 passt 排在 `PATH` 最前面；或者改用 `--offline` 构建 |
| 某一步暂停约 300 秒，然后软件包安装失败 | passt 从未发放租约；`dhclient` 超时了 | 同上 |
| `--offline belong to the built-in golden recipe …` | 对着 VMfile 用了黄金配方专属的参数 | 去掉它；VMfile 自己描述整个构建 |
| `` `--network` is for VMfile builds `` | 没有 VMfile 却用了 `--network` | 想用黄金配方就改用 `--offline` |
| VMfile 构建把 `/tmp` 填满了 | 每个阶段都是 `$TMPDIR` 下一整块拍平的磁盘 | 把 `TMPDIR` 指到一个空间足够大的文件系统；失败之后记得清理残留的 `delonix-vmfile-*` 目录 |
| `VM backend 'proxmox' is not available in this build` | 这个后端没有被配置 | 设置 `DELONIX_PROXMOX_*` 系列变量（见第 2 节） |

---

## 7. 给贡献者

### 代码在哪里

| 领域 | 路径 |
|---|---|
| 端口、注册表、CH 和 libvirt 后端、`create_with`、快照、固件查找 | `crates/adapters/delonix-vm/src/lib.rs` |
| NoCloud seed 的生成 | `crates/adapters/delonix-vm/src/cloudinit.rs` |
| Proxmox 后端 | `crates/providers/delonix-proxmox/` |
| `vm` CLI、`kind: VirtualMachine`、`vm reach` | `bins/delonix-runtime-bin/src/cmd/vm.rs` |
| 镜像存储、黄金配方、pull/push/import/convert、失败提示 | `bins/delonix-runtime-bin/src/cmd/vmimage.rs` |
| VMfile | `bins/delonix-runtime-bin/src/cmd/vmfile.rs` |
| 从环境变量注册后端 | `bins/delonix-runtime-bin/src/cmd/vmbackends.rs` |
| `vm bridge` | `bins/delonix-runtime-bin/src/cmd/vmbridge.rs` |
| `kind: Workload` 的降解 | `bins/delonix-runtime-bin/src/cmd/workload.rs` |
| Appliance 构建 | `scripts/appliances/` |

### 新增一个后端

先读 [ADR-0008](../adr/0008-proxmox-vm-backend.md)；它就是模板。简单说：

- 如果要对接一个远程 API，就把 `VmBackend` 实现在**它自己的 crate** 里（这样引擎 crate 就
  不会沾上 HTTP 客户端），放在它所属层的目录下，并登记进 `scripts/arch_fitness.py`
  （见[架构](architecture.md)）。
- 由知道其配置的那个进程通过 `register_backend` 注册它，`auto_selectable` 设为 `false`，
  除非它是一个内置于 `delonix-vm`、本地且零配置的后端。
- 有意地去覆盖 `manages_own_storage`、`destroy`、`resume` 和 `ip_is_predicted`：对一个远程
  后端来说，`stop` 和 `destroy` **不是**同一个操作，而对一台已停止的虚拟机执行 `boot` 绝
  不能创建出第二台。
- 不支持的动词就让它们停留在 fail-closed 的默认实现上；在创建任何东西之前，先按名字拒绝
  不支持的 `VmConfig` 字段。
- 不要发布一个从没被人看着它启动过虚拟机的后端。新的后端和 hypervisor 边界都要走一份 ADR
  （[docs/adr/](../adr/)）。

### 测试

- **纯单元测试**——argv 和 XML 构造器都是纯函数，所以不需要 hypervisor 就能测试：例如
  `libvirt_snapshot_argv_uses_flags_not_positional`、`snapshot_xml_with_uuid`、
  `libvirt_domain_xml`、前面提到的固件顺序测试、注册表和 `auto_detect` 相关的测试，以及
  `cmd/vmfile.rs` 里的解析器/脚手架测试。运行 `cargo test -p delonix-vm` 和
  `cargo test -p delonix-runtime-bin vmfile`（关于 `protoc` 和目标目录，见
  [克隆、构建与测试](build-and-test.md)）。
- **`scripts/e2e.sh`**——`vm` 相关的几个部分不需要 hypervisor 就能跑（列出、拒绝场景），
  在条件具备时还会在 libvirt 上（需要 `virsh`、`qemu-img` 和一个可用的 `qemu:///system`）
  和 Cloud Hypervisor 上跑一遍跨 stop/start 的快照。它默认会隔离两个状态根目录；如果只
  隔离了 `DELONIX_ROOT`，CH 相关部分会拒绝运行。没有相应 hypervisor 的部分会被报告为
  跳过，而不是通过。
- **Proxmox 实机测试**——`crates/providers/delonix-proxmox/tests/live.rs` 会对着一个真实
  节点创建并销毁一台虚拟机，除非设置了 `DELONIX_PROXMOX_TEST_URL`（以及 `_NODE`、`_USER`、
  `_PASS`），否则会被跳过：`cargo test -p delonix-proxmox --test live -- --nocapture`。
  用一个可以随意丢弃的节点。
- **Provider 的生命周期**——改动 `delonix-vm` 或某个 provider 时，**只通过 `delonix`
  CLI** 去证明整条生命周期（create、stop、start、snapshot、destroy）走得通，对 hypervisor
  只做只读观察（`virsh -r`、API 的 `GET`），绝不在步骤之间手动去修补它。
- **去看，别去猜。** 当客户机起不来时，串口控制台（`vm console`）或者一张 libvirt 截图，
  几秒钟就能回答那些靠猜测要花上几个小时才能找到的问题——并且要用一个用户真的会敲的命令去
  验证，而不是那些为了方便调试而加上的参数（`--vnc` 就曾经掩盖过一次只在没有显示设备时
  才会发生的启动失败）。

---

**下一篇：** [排查问题](troubleshooting.md)——一份以症状为起点的索引，收录了构建和测试过程中可能撞上的各种门禁和宿主机陷阱。
