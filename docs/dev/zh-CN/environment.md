<!-- translated-from: environment.md sha256:0505a35c5a1e921e51ecd51f7feb5352158dd1985176b27ff3e1a76b6adc1719 -->
# 准备你的环境

**阅读之前：**[从这里开始](start-here.md#day-0-in-30-minutes)（第 0 天）和 [Linux 基础](linux-foundations.md) —— 下面这些宿主机陷阱，都是用用户命名空间和 cgroup 委派的术语来解释的。

Delonix Runtime **只支持 Linux**：它用到的每一个原语 —— 命名空间、cgroups v2、
nftables、`pivot_root`、新的 mount API —— 都活在 Linux 内核里。你可以在任何装有下面这套
工具链的 Linux 机器上*编译*大部分工作区，也能跑它的纯逻辑测试；但要*运行*容器、走一遍那些
实机路径，你需要一台满足本页内核与软件包要求的宿主机。读完本页之后，你的宿主机应该能通过
`delonix system info`；出问题的时候，你也能分清是宿主机的前提条件没满足，还是引擎本身
的 bug。

很多在一台全新机器上看起来像是引擎 bug 的东西，其实是宿主机的前提条件没满足。开
issue 之前，先读一读 [已知的宿主机陷阱](#known-host-traps) 这一节。

## 工具链

<!-- dev-docs:begin toolchain -->
- **Rust 工具链：** `1.96.0`（固定在 `rust-toolchain.toml` 中；首次调用 `cargo` 时由 `rustup` 安装）
- **组件：** `rustfmt`, `clippy`
<!-- dev-docs:end toolchain -->

安装 [`rustup`](https://rustup.rs/)，让它去读取那个固定版本的 channel；不要用 `stable`
去覆盖它。CI 用的是完全同一份文件（每个 job 里都会跑 `rustup show`）。

### `protoc`（构建必需）

`crates/interfaces/delonix-cri/build.rs` 用 `tonic-build`/`prost` 编译 Kubernetes CRI
的 protobuf，这需要 `PATH` 上有 Protocol Buffers 编译器。`delonix` 这个二进制程序依赖
`delonix-cri`，所以**如果没有它，一次普通的 `cargo build --workspace` 就会失败**：

```bash
# Debian / Ubuntu
sudo apt install protobuf-compiler
# Fedora / RHEL family
sudo dnf install protobuf-compiler
```

或者从 <https://github.com/protocolbuffers/protobuf/releases> 下载一个发行版。CI 里
每一个要编译的 job 都会用 apt 安装 `protobuf-compiler`。

### 可选工具，只用于特定门禁

| Tool | 用在哪里 | 版本固定在哪里 |
|---|---|---|
| Python 3.11+ | 每一个 `scripts/*.py` 门禁（它们用到了 `tomllib`） | — |
| `buf` v1.73.0 和 `protoc-gen-openapi` v0.7.1（需要 Go 工具链来安装它们） | `scripts/contract_gate.py` | `.github/workflows/ci.yml` 里的 `contract` job |
| `cargo-deny` | 供应链检查 | `deny` job，配置在 `deny.toml` |
| Python 的 `markdown` 模块 | `docs/gen.py`（渲染 `ARCHITECTURE.md`） | `docs` job |
| `groff` | 检查生成出来的 man page | `docs` job |

各自怎么跑，见 [克隆、构建与测试](build-and-test.md)。

## 内核要求

这些是引擎所依赖的内核特性。安装脚本（`scripts/install.sh`）和
`delonix system doctor` 会替你检查其中大部分。

| 要求 | 为什么 | 怎么检查 |
|---|---|---|
| **cgroup v2**（统一层级） | 资源限制和统计都是写到 `/sys/fs/cgroup` 里的 | `stat -fc %T /sys/fs/cgroup` 会打印出 `cgroup2fs` |
| **无特权 user 命名空间** | rootless 模型的基础：引擎只在自己的 user 命名空间*里面*才是「root」 | `unshare -r -n true` 能成功执行 |
| **`/dev/net/tun`** | `slirp4netns`（rootless 网络）和 VM 的 tap 设备 | `test -e /dev/net/tun` |
| 带新 mount API 和 `lowerdir+` 的 **overlayfs**（Linux **6.5** 或更新） | 容器的根文件系统是用 `fsopen`/`fsconfig`/`fsmount` 构建出来的 overlay 挂载，每一层调用一次 `lowerdir+` —— 见 [ADR-0037](../adr/0037-overlay-mount-new-api.md) | `uname -r` |
| 已加载 **`br_netfilter`**，`net.bridge.bridge-nf-call-iptables=1` | 命名空间隔离是在 nftables 的 `forward` 链里强制执行的；没有这个模块，同一网桥上两个容器之间的流量就永远到不了它们，隔离会悄无声息地失效 | `delonix system doctor` |
| **KVM**（`/dev/kvm`） | 只有 microVM 才需要 —— 见 [构建 microVM](microvm-setup.md) | `test -w /dev/kvm` |

6.5 这条要求**没有预检**：更老的内核会在第一个容器 rootfs 被挂载的时候才失败，而不是
在启动时（ADR-0037 记录了这是一个刻意的选择）。

在较老的 Debian 内核上，无特权 user 命名空间是被
`kernel.unprivileged_userns_clone=0` 关掉的；安装脚本会把它设成 `1`。

## 宿主机软件包

唯一的真相来源是 [`scripts/install.sh`](../../scripts/install.sh)，它同时也是官方安装
脚本（作为发布资产发布出去）。它通过 `/etc/os-release` 检测包管理器，支持 **apt**（Debian、
Ubuntu 及其衍生版）、**dnf**（Fedora、RHEL、CentOS Stream、Rocky、AlmaLinux）、**zypper**
（openSUSE、SLES）和 **pacman**（Arch 及其衍生版）。安装脚本会为 **x86_64** 和 **aarch64**
安装预编译的二进制程序（`arm64` 会被规范化）：资产名是由 `uname -m` 拼成
`<name>-<arch>-linux`，任何其他架构都会以「no prebuilt binary for <arch> yet」中止，需要从
源码构建。在 aarch64 上有三处不同：`-v3` 变体是一个 x86-64 的微架构级别，在那里根本不存在；
固定版本的静态 Cloud Hypervisor、EDK2 的 `CLOUDHV.fd` 和 `hypervisor-fw` 都是 x86-64 的
构建产物，所以不会被下载（如果包管理器里有，仍然会安装一个发行版打包的 `cloud-hypervisor`），
VM 后端会用 libvirt；而 QEMU 的探测会去找 `qemu-system-aarch64`。aarch64 的发布资产从
v4.2.0 起才有（v4.1.0 的发布里一个都没有）。
**尚未验证：** 在一台真正的 aarch64 宿主机上完整安装，以及 arm64 各发行版下 QEMU 软件包的
名字 —— 已经核实过的，只是资产名的拼法和已发布的 v4.2.0 资产是否一致。

要运行容器，引擎需要：

| Command | 软件包（apt / dnf 的名字） | 为什么 |
|---|---|---|
| `slirp4netns` | `slirp4netns` | rootless 网络和端口发布 —— 没有它 `run -p` 会失败 |
| `newuidmap` / `newgidmap` | `uidmap` / `shadow-utils` | 把不止一个 uid 映射进 user 命名空间的 setuid 辅助程序；没有它们，带非 root 用户的镜像会在 `chown()` 上失败 |
| `nft` | `nftables` | SDN 防火墙、隔离和端口 DNAT |
| `ip` | `iproute2` / `iproute` | veth、网桥和 netns 相关的管道 |
| `conntrack`（可选） | `conntrack` / `conntrack-tools` | 在一个端口被取消发布时清理连接 |

对于 VM，需要 `qemu-img`、`cloud-localds`（`cloud-image-utils`）、`virsh`（libvirt）
和/或带固件的 Cloud Hypervisor —— 见 [构建 microVM](microvm-setup.md)。要构建 VM 镜像，
需要 `libguestfs-tools`（`install.sh --with-image-build`）。

你还需要在 `/etc/subuid` 和 `/etc/subgid` 里为自己的用户配一段**从属 uid/gid 范围**；
没有它，user 命名空间就只能映射一个 uid。

### 最快搭出一台能用的宿主机

你不用手工照抄一遍安装脚本做的事。如果只想安装宿主机的依赖和配置，同时保留你自己构建
的二进制程序：

```bash
bash scripts/install.sh --no-binary
```

先读一遍脚本开头的那份 flag 清单：有些 flag 会改变宿主机级别的安全设置
（`--low-ports` 会让任何本地程序都能绑定 80 以上的端口，`--with-image-build` 会让
`/boot/vmlinuz-*` 变成任何人都能读），`--no-tune` 会跳过内核模块和 sysctl，包括
`br_netfilter`。`--performance` / `--no-performance` 控制 CPU 性能模式、带 `irqbalance`
的透明大页，以及一个跑 `system prune --auto --threshold 75` 的用户级定时器；两个 flag 都不给
的话，安装脚本会就每一项分别询问，而在没有终端的情况下会一律回答否。CPU 那部分是一个 systemd
服务，会保存启动时的原始值，并在 `stop` 时把它们还原回去。开发时都不需要这些。

### 内存与磁盘

没有固定的最低要求。真正耗资源的是你实际跑的东西：容器镜像和层、VM 磁盘，以及 Rust 的
`target/` 目录本身（整个工作区的 debug 构建能占好几个 GB）。留意可用磁盘空间 —— 本地集群里
的 kubelet 一遇到磁盘压力就会开始驱逐 pod，那看起来又会像是引擎的问题。

## 诊断宿主机

构建出这个二进制程序（见 [克隆、构建与测试](build-and-test.md)），然后去问它。下面这些
命令，只要不带 `--delegate`，就都是只读的：

```bash
./target/debug/delonix system doctor          # is every prerequisite met? says how to fix each
./target/debug/delonix system info            # rootless?, cgroup delegation, network infra, counts
./target/debug/delonix system setup           # diagnose cgroup delegation
./target/debug/delonix system resources       # which controllers are delegated, which flags are ignored
```

`delonix system doctor --strict` 在某项检查失败时会以非零码退出，这在置备脚本里很有用。
如果你是在一台已经有正在使用的 Delonix 安装的机器上跑这些命令，先把状态隔离开（见
[隔离引擎的状态](build-and-test.md#isolating-the-engines-state)）。

## 已知的宿主机陷阱

### Ubuntu 23.10+：AppArmor 会挡住你的开发二进制程序创建 user 命名空间

较新的 Ubuntu 会设置 `kernel.apparmor_restrict_unprivileged_userns=1`。这样一来，一个
没有 AppArmor 配置文件的二进制程序就没法创建 user 命名空间，引擎会在 `unshare()` 上带着
`EPERM` 死掉 —— 读起来就像引擎的 bug。

`install.sh` 会安装一个配置文件（`/etc/apparmor.d/delonix`，带 `userns` 的
`flags=(unconfined)`），但那个配置文件**绑定在一个路径上**：`<安装目录>/delonix`（默认是
`/usr/local/bin/delonix`，加 `--user` 就是 `~/.local/bin/delonix`）。你刚构建出来、放在
`target/debug/delonix` 的二进制程序 —— 或者复制到 `/tmp` 的那个 —— **不在**它的覆盖范围内。

几个选项，按侵入程度从小到大排列：

1. 为你的开发路径（比如你 worktree 里的 `target/debug/delonix`）另外加一个配置文件，
   照搬安装脚本写的那份的形状，再用 `sudo apparmor_parser -r <file>` 加载它。这不会碰任何
   已经在跑的东西。
2. 把你的构建安装到那个有配置文件覆盖的路径
   （`sudo install -m 0755 target/debug/delonix /usr/local/bin/`）—— **只能在一台没有
   Delonix 工作负载在用的机器上这么做**。boot unit（`ExecStart=<exe> container start …`）
   和各个服务器重新执行的就是这个已安装的二进制程序，所以在一台有实机工作负载的宿主机上，
   一个 debug 构建会悄无声息地变成生产环境的引擎。
3. 设置 `kernel.apparmor_restrict_unprivileged_userns=0` —— 这会降低一道宿主机级别的
   边界；只能在你自己的机器上这么做。

### cgroup 委派：有些限制会被拒绝，有些不会被强制执行

资源限制只有在你用来运行引擎的那个 shell 本身位于一个**受委派**的 cgroup 里时，才能
到达内核。这是 cgroup v2 的规则，不是 Delonix 的限制 —— rootless 的 Podman 也有同样的
要求。没有委派的情况下，引擎会做两种不同的事，取决于用的是哪个 flag：

- `-m`/`--memory`、`-c`/`--cpus` 和 `--cpu-weight`：`container run` 会在创建任何东西之前就
  **拒绝**执行，报出一个点名修法的错误，退出码是 **69**（`Error::Unavailable`，
  `EX_UNAVAILABLE` 这一类 —— 见 `bins/delonix-runtime-bin/src/cmd/container.rs` 里的
  `preflight_resource_limits`）。`DELONIX_ALLOW_UNENFORCED_LIMITS=1` 会带着告警照样把容器
  跑起来，只是不加限制。
- `--cpuset`、`--io-weight` 以及
  `--device-read-bps`/`--device-write-bps`/`--device-read-iops`/`--device-write-iops` 这一
  族，**不会**被那个探测检查：它们会被接受，并尽力应用，所以没有 `cpuset`/`io`
  控制器时，它们就是不生效，也不会报任何错误。

没有 `--pids-limit` 这个 flag；pids 的上限是引擎那个 cgroup 组本身的属性，不是
`container run` 的属性。

最常见的情况是 **SSH 会话**：它的 scope 位于你被委派的那棵子树之外，会话本身也没法把
自己挪进去 —— 具体为什么，以及怎么用命令看出来，见
[Linux 基础 —— 委派给用户](linux-foundations.md#delegation-to-users)。针对单条命令的修法
不需要 root：

```bash
systemd-run --user --scope -p Delegate=yes -- ./target/debug/delonix container run -d -m 128M alpine sleep 60
```

对长期运行的工作负载，用一个带 `Delegate=yes` 的 systemd **user** unit。有些宿主机只把
`cpu memory pids` 委派给用户会话；`cpuset` 和 `io` 在 rootless 下可能永远都拿不到，
`delonix system resources` 会点名哪些 flag 将会被忽略。当 `cpu` 控制器本身都缺失时，
`delonix system setup --delegate` 会写一个系统级的 drop-in（需要 root，在下次登录时生效）。

### `PATH` 上一个过期的 `delonix`

如果这台机器上装了 Delonix，`PATH` 上的 `delonix` 就是那个已安装的发行版，不是你的
代码树。测试改动时，永远运行 `./target/debug/delonix`（或者 `target/release/delonix`）。
`--version` 会显示提交号和它离上一个 tag 的距离
（`commit: <hash> (+N commits since vX.Y.Z)`），因为在两次发布之间，两个构建会共用同一个
版本号。

### 你可能遇到的其他陷阱

- **1024 以下的端口**在 rootless 下会以 `slirp_add_hostfwd failed` 失败：端口是由
  `slirp4netns` 这个无特权进程绑定的。用一个高位端口，或者用
  `install.sh --low-ports` 主动打开这个口子。
- **托管的 CI runner**（GitHub 托管的那种）会挡住无特权 user 命名空间。混沌测试的
  workflow 会检测到这一点，报告 `skipped`，而不是 `success`；要走一遍那些实机路径，你需要
  一台真实的宿主机、一个自托管 runner，或者一台 VM。
- **VM 固件和构建镜像时的陷阱**（Cloud Hypervisor 固件的选择、libguestfs 构建里那个
  过时的 `passt`、`/boot/vmlinuz-*` 的权限）在 [构建 microVM](microvm-setup.md) 里讲过。

---

**下一篇：**[克隆、构建与测试](build-and-test.md) —— 构建、安装并测试你的代码树，以及
把每一个 CI 门禁都当作本地命令来跑。
