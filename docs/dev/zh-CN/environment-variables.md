<!-- translated-from: environment-variables.md sha256:89a40a69d327ce2599cf5b5695e2cc96151828496b52fecabe7e4b62bdc71387 -->
# 环境变量（`DELONIX_*`）

**阅读前须知：** 《克隆、构建与测试》里的[隔离引擎的状态](build-and-test.md#isolating-the-engines-state)。

这一页列出了引擎代码里出现的每一个 `DELONIX_*` 名字，说明它在哪里被读取、它改变了什么，以及你要不要设置它。它是一份参考资料：只读跟你现在在做的事情相关的那一节，不用读整页。读完之后，你就能隔离一次运行、打开你需要的诊断，并且在设置某个会降低安全边界的变量之前先认出它来。

## 如何阅读这一页

变量表的每一行都有五列：

- **读取方** —— 读取（或写入）这个值的 crate 或二进制文件，以及 `path:symbol`。路径是相对仓库根目录的。
- **用途** —— 这个变量改变了什么。
- **取值／默认** —— 代码如何解析它。当代码在解析不了时会静默地回退到某个值，这一格会写明。
- **备注** —— 什么时候用它，以及任何警告。

大多数变量是被需要它们的进程读取的，**不会**自动传递下去。少数是引擎自己在它启动的进程上设置的；那些在[由引擎自己设置](#set-by-the-engine-itself-internal)里，你不应该自己去设置它们。

**这张表被 CI 双向检查。** `python3 scripts/dev_docs.py --check`（`scripts/dev_docs.py` 里的 `env_var_problems` 函数）会从 `crates/` 和 `bins/` 下的 Rust 字符串字面量（包括 `build.rs` 和 `env!()`）以及 `scripts/install.sh` 里提取每一个 `DELONIX_*` 名字，如果这里少了某个名字、或者这一页列出了代码里已经不存在的名字，就会失败。一行只有在以 `` | `DELONIX_NAME` `` 开头时才算数。这个提取是纯文本的，所以它也会捡到一些**不是**环境变量的名字（一个 Rust 常量、测试用的 fixture、写进某个 VM 镜像里的一个 key）；那些列在它们自己的小节里，好让这个检查保持精确。

只被范围之外的脚本使用的变量——比如 `scripts/chaos.sh` 里的 `DELONIX_CHAOS_DIR` 和 `DELONIX_CHAOS_TRUENAS_*` 这一族——记录在每个脚本自己的头部，以及[克隆、构建与测试](build-and-test.md)里，不在这里。

### 优先级

在代码有优先级规则的地方，通常是**flag > 环境变量 > 默认值**；下表按代码实际解析的方式展示每一种情况，包括那些多一层或者没有 flag 的情况：

| 设置项 | 顺序（排前面的赢） | 在哪里 |
|---|---|---|
| CRI 的 socket、capability 上限和模式 | `--addr` / `--cap-ceiling` / `--cap-ceiling-mode` > `DELONIX_CRI_ADDR` / `DELONIX_CRI_CAP_CEILING` / `DELONIX_CRI_CAP_CEILING_MODE` > `unix:///run/delonix-cri.sock` / 无上限 / `reject` | `crates/interfaces/delonix-cri/src/bin/delonix-cri.rs:main` |
| 管理 API 的 socket | `--addr` > `DELONIX_API_ADDR` > `unix:///run/delonix-mgmt.sock` | `bins/delonix-mgmt-bin/src/main.rs:run` |
| Docker API 的 socket | `--addr` > `DELONIX_DOCKER_ADDR` > `unix:///run/delonix-docker.sock` | `bins/delonix-runtime-bin/src/cmd/dockerapi.rs:run` |
| 输出语言 | `--l18n` > `DELONIX_L18N` > 英文 | `bins/delonix-runtime-bin/src/cmd/po.rs:peek_lang` |
| VM 控制台的转义键 | `--escape` > `DELONIX_CONSOLE_ESCAPE` > `^]` | `bins/delonix-runtime-bin/src/cmd/vm.rs:resolve_escape` |
| VM 的 backend | `--backend`（或镜像自带的 `HYPERVISOR`） > `DELONIX_VM_BACKEND` > providers 文件中的 `defaultProvider`（ADR-0054） > `vm default-backend --set` > 自动检测（仅在没有 providers 文件时） | `crates/adapters/delonix-vm/src/lib.rs:standing_backend_choice` |
| 已发布端口绑定的地址 | `-p <ip>:<host>:<container>` 里的地址 > `DELONIX_PUBLISH_ADDR` > `127.0.0.1` | `crates/adapters/delonix-sdn/src/lib.rs:publish_bind_addr` |
| `image scan --update` 用的 CVE 数据源 | `--feed` > `DELONIX_ADVISORY_FEED` > 报错 | `bins/delonix-runtime-bin/src/cmd/scan.rs:cmd_scan_update` |
| 日志过滤器 | `DELONIX_LOG` > `RUST_LOG` > `info` | `crates/adapters/delonix-telemetry/src/telemetry.rs:init` |

`delonix serve cri` 和 `delonix serve api` 在 `exec` 启动它们要跑的那个服务端二进制文件时，**同时**以 flag 和对应的变量两种形式把自己的 flag 传下去（`bins/delonix-runtime-bin/src/cmd/serve.rs:run`），这样一个来自旧版本、只读变量的服务端也能拿到这个值。

### 隔离一次开发运行

在一台同时也跑着 Delonix 工作负载的机器上，运行任何除了 `--help` 之外的命令之前，先把两个状态位置都指向一个临时目录：

```bash
export DELONIX_ROOT=$HOME/scratch/dlx/root        # records, images, networks, IPAM, volumes, pidfiles
export DELONIX_NET_RUNTIME_DIR=/tmp/dlx-run        # the network holder's control and slirp sockets
mkdir -p "$DELONIX_ROOT" "$DELONIX_NET_RUNTIME_DIR"
./target/debug/delonix system info                # "state root" must show your scratch path
```

**为什么两个都要。** 网络基础设施把它的 pidfile 放在 `DELONIX_ROOT` 下面，但把它的 unix socket 放在一个单独的运行时目录里（`crates/adapters/delonix-sdn/src/infra.rs:runtime_dir`），因为 socket 路径限制在大约 108 字节，而 `DELONIX_ROOT` 可以任意深。只设置两者之一，可能让两个状态根共用同一组 socket。完整的来龙去脉，以及怎么把隔离出来的基础设施拆掉，见[隔离引擎的状态](build-and-test.md#isolating-the-engines-state)。

## 日常配置

| 变量 | 读取方 | 用途 | 取值／默认 | 备注 |
|---|---|---|---|---|
| `DELONIX_PERF_CONF` | `scripts/install.sh`（生成的 `/usr/local/sbin/delonix-performance`） | helper 读取的配置文件路径（`CPU=`/`THP=` flag）。 | 一个文件路径。未设置：`/etc/delonix/performance.conf`。 | 只用于拿一个假的 sysfs 测试这个 helper；`install.sh` 会写真正的那份。 |
| `DELONIX_PERF_CPU` | `scripts/install.sh`（生成的 `/usr/local/sbin/delonix-performance`） | helper 读写的 CPU sysfs 树的根（governor、EPP）。 | 一个目录路径。未设置：`/sys/devices/system/cpu`。 | 指向一棵假的树，就能在不碰宿主机的情况下测试 `apply`/`revert`。 |
| `DELONIX_PERF_STATE` | `scripts/install.sh`（生成的 `/usr/local/sbin/delonix-performance`） | helper 保存开机时数值的目录，`revert` 时会用它来恢复。 | 一个目录路径。未设置：`/var/lib/delonix-performance`。 | 跟其他几个一样，只是测试用的替代值。 |
| `DELONIX_PERF_THP` | `scripts/install.sh`（生成的 `/usr/local/sbin/delonix-performance`） | helper 读写的透明大页（transparent hugepage）`enabled` 文件。 | 一个文件路径。未设置：`/sys/kernel/mm/transparent_hugepage/enabled`。 | 跟其他几个一样，只是测试用的替代值。 |
| `DELONIX_ROOT` | 每一个 store 和二进制文件：`bins/delonix-runtime-bin/src/cmd/util.rs:state_root`、`crates/adapters/delonix-oci/src/image.rs:ImageStore::default_root`、`crates/adapters/delonix-state/src/store.rs:Store::default_root`、`crates/adapters/delonix-sdn/src/infra.rs:base_root`、`crates/interfaces/delonix-cri/src/bin/delonix-cri.rs:main`、`bins/delonix-mgmt-bin/src/main.rs:run`、`crates/interfaces/delonix-mcp/src/lib.rs` | 引擎的 state root：容器记录、镜像、网络、IPAM、卷、VM、密钥。 | 一个目录路径。未设置：非 root 时是 `$XDG_DATA_HOME/delonix`（或 `~/.local/share/delonix`），root 身份下是 `/var/lib/delonix`。**`delonix-cri` 和 `delonix-mgmt` 无论如何都默认用 `/var/lib/delonix`**，这就是为什么 `delonix serve …` 会显式地把 CLI 的 root 传过去（`cmd/serve.rs:exec_server`）。 | 测试时要设置的那个变量。引擎也会在它启动的每一个子进程上**设置**它（re-exec、网络 holder、服务端、CRI 生命周期调用），好让路径在不同 user namespace 之间保持一致。 |
| `DELONIX_NET_RUNTIME_DIR` | `crates/adapters/delonix-sdn/src/infra.rs:runtime_dir`（`RUNTIME_DIR_ENV`） | 网络基础设施的 unix socket（`control.sock`、`slirp.sock`）所在的目录。 | 一个目录路径；保持简短（socket 路径超过约 108 字节会因 `SUN_LEN` 失败）。未设置：`/tmp/delonix-net-<uid>` 加上一个从非默认 `DELONIX_ROOT` 派生出来的后缀。 | 隔离时和 `DELONIX_ROOT` 一起设置。引擎也会把它传给 holder 和 `--net <custom>` 的 re-exec（`infra::runtime_dir_env`），因为在 holder 的 user namespace 里 uid 是 0，默认值会不一样。 |
| `DELONIX_L18N` | `bins/delonix-runtime-bin/src/cmd/po.rs:peek_lang` | CLI 的输出和 `--help` 用的语言。 | `en`（默认）或 `pt`。 | `--l18n` 优先。错误的*类别*（退出码）不依赖语言，但消息依赖——不要在脚本里对消息文本做 grep。 |
| `DELONIX_LOG` | `crates/adapters/delonix-telemetry/src/telemetry.rs:init` | `delonix`、`delonix-cri`、`delonix-mgmt` 和 `delonix-mcp` 的日志过滤器。 | 一个 `tracing` 过滤表达式（`debug`、`warn`、`delonix_sdn=debug`）。回退到 `RUST_LOG`，再回退到 `info`。 | 日志写到 stderr；stdout 留给命令的输出。 |
| `DELONIX_LOG_FORMAT` | `crates/adapters/delonix-telemetry/src/telemetry.rs:init` | 日志行的格式。 | `json` 表示 JSON 行；其他任何值（或未设置）都是纯文本。 | 当一个服务端跑在 systemd 下、它的 journal 会被送到别处时有用。 |
| `DELONIX_VERBOSE` | `bins/delonix-runtime-bin/src/cmd/output.rs`（`Progress`） | 把每一步的输出都流式打印出来，而不是把它们折叠进一行进度信息。 | 设置且不为 `0` → 详细模式。 | 跟某个命令上的 `--verbose` 效果一样（如果它有的话）。 |
| `DELONIX_HOSTS_FILE` | `bins/delonix-runtime-bin/src/cmd/hosts_file.rs:hosts_path` | `HTTPRoute` 上的 `hosts: [host]` 和 `delonix hosts sync` 写入其托管代码块的那个 hosts 文件。 | 一个文件路径；默认 `/etc/hosts`。 | 在一次隔离运行里把它指向一个临时文件，这样真正的 `/etc/hosts` 永远不会被碰到（写它需要 root）。这个代码块是怎么工作的：[名字是怎么到达 `/etc/hosts` 的](service-names-and-hosts.md)。 |
| `DELONIX_CONSOLE_ESCAPE` | `bins/delonix-runtime-bin/src/cmd/vm.rs:resolve_escape` | 让 `delonix vm console` 脱离的按键。 | 一个控制键，写成 `^X` 或 `X`。默认 `^]`。一个无效的值是错误，而不是回退。 | 给那些打不出 `^]` 的键盘布局用的（比如葡萄牙语键盘）。`-e/--escape` 优先。 |
| `DELONIX_CRI_ADDR` | `crates/interfaces/delonix-cri/src/bin/delonix-cri.rs:main`；由 `bins/delonix-runtime-bin/src/cmd/serve.rs:run` 转发 | CRI 服务端监听的 socket。 | `unix://<path>`。默认 `unix:///run/delonix-cri.sock`。 | `--addr` 优先。kubelet 的 `--container-runtime-endpoint` 必须与之一致。 |
| `DELONIX_CRI_CAP_CEILING` | `crates/interfaces/delonix-cri/src/bin/delonix-cri.rs:main`（`cap_ceiling::CEILING_ENV`，由 `crates/interfaces/delonix-cri/src/cap_ceiling.rs:CapCeiling::parse` 解析）；由 `cmd/serve.rs:run` 转发 | 节点级别的上限，限制任何通过 CRI 创建的容器的 capabilities，`privileged: true` 也不例外。 | 空／未设置或 `all` → 没有上限（行为不变）。`none` → 没有 capability。`default` → 引擎的默认集合。`default,NET_ADMIN,…` → 默认集合加上点名的那些。一份名字列表（`CAP_` 前缀可选，不区分大小写；用逗号、空格或 `;` 分隔）→ 恰好是这些。列表里任何位置出现 `all` 都会生效。一个未知的名字，或者一个只由分隔符组成的值，**会让服务端启动不了**。 | `--cap-ceiling` 优先。只限制 capability：一个特权 pod 依然会拿到不受限的 seccomp 和一个可写的 `/sys`。生效中的上限在 `crictl info`（`capabilityCeiling`）里可见。 |
| `DELONIX_CRI_CAP_CEILING_MODE` | `crates/interfaces/delonix-cri/src/cap_ceiling.rs:CeilingMode::parse`（`MODE_ENV`）；由 `cmd/serve.rs:run` 转发 | 当一个 pod 显式要求的东西超过上限时会发生什么。 | `reject`（默认；也可以是 `enforce` 或空）→ `CreateContainer` 失败，并点名被拒绝的 capability。`clamp`（或 `trim`）→ 削减到上限，并给出警告。一个未知的词**会让服务端启动不了**。 | `--cap-ceiling-mode` 优先。在两种模式下，引擎隐含的默认集合都会被无声地削减到上限。 |
| `DELONIX_CRI_FROM_SOURCE` | `bins/delonix-runtime-bin/src/cmd/vmimage.rs:locate_cri_bin`（`CRI_FROM_SOURCE_ENV`，由 `cri_from_source_requested` 解析） | 当 `cluster apply` / `cluster kubeadm` 需要一个二进制文件去安装到各个节点上时，选择从 cwd 周围的源码 checkout 编译 `delonix-cri`。 | 只有 `1` 或 `true`（去掉首尾空白后）才会打开它；未设置、空、`0` 或其他任何值都是关闭的。 | 默认关闭，好让装在集群上的 runtime 永远不依赖命令是从哪个目录跑的。没有它的话，顺序是 `--cri-bin`、`delonix` 旁边的那个 `delonix-cri`，然后是正在运行版本的 release 资产，并对照它的 `SHA256SUMS` 校验。来源、路径和 sha256 总会被打印出来。 |
| `DELONIX_API_ADDR` | `bins/delonix-mgmt-bin/src/main.rs:run`；由 `cmd/serve.rs:run` 转发 | 本地管理 API（`delonix serve api`）的 socket。 | `unix://<path>`。默认 `unix:///run/delonix-mgmt.sock`。 | `--addr` 优先。这个 API 仅限本地（调用方的 uid）。 |
| `DELONIX_DOCKER_ADDR` | `bins/delonix-runtime-bin/src/cmd/dockerapi.rs:run` | Docker Engine API 切片（`delonix serve docker-api`）的 socket。 | `unix://<path>`（`unix://` 前缀可省略）。默认 `unix:///run/delonix-docker.sock`。 | `--addr` 优先。 |
| `DELONIX_VM_BACKEND` | `crates/adapters/delonix-vm/src/lib.rs:standing_backend_choice` | 当某条命令没有点名 backend 时，整个会话范围内用的 VM backend。 | 一个 backend 名字（`libvirt`、`cloud-hypervisor`，或者一个已注册的远端 backend，比如 `proxmox`）。空白会被忽略。 | 优先级低于 `--backend` 和镜像自带的 `HYPERVISOR`，高于 `delonix vm default-backend --set` 设的机器级默认值。跟显式选择一样，它会覆盖能力启发式判断，并且如果这个 backend 跑不了这个 VM，可能会在启动很晚的时候才失败。 |
| `DELONIX_PROVIDERS_CONFIG` | `cmd/providers_config.rs:locate_with` | 节点 providers 文件的路径（ADR-0054），优先于 `$XDG_CONFIG_HOME/delonix/providers.yaml` 和 `/etc/delonix/providers.yaml`。 | 一个路径。 | 必须指向已存在的文件——绝不退回到其他文件。找到的第一个文件就是配置；文件从不合并。无法读取的文件会让不带 `--backend` 的 VM 请求失败，而不是猜测一个 provider。 |
| `DELONIX_NO_CGROUP_WARN` | `crates/adapters/delonix-linux/src/lib.rs`（关于缺少 cgroup 委派的警告，以及 `warn_if_unprotected_memory`）、`bins/delonix-runtime-bin/src/cmd/kindmode.rs` | 让「缺少 cgroup 委派」和「容器在任何地方都没有内存上限」这两类警告不再出现。 | 设置（任意值）→ 静默。 | 引擎自己会**设置**它（`cmd/util.rs:silence_cgroup_warning`、`cmd/kindmode.rs`），这样 re-exec 出来的子进程就不会重复父进程已经打印过的警告。自己手动设置它会掩盖一个真实的情况：限制没有被强制执行。 |
| `DELONIX_POLICY_LINT` | `bins/delonix-runtime-bin/src/cmd/policy.rs:show_lints` | 让每条命令一次的 runtime-policy 警告（`warning: runtime policy [...]`）静默。 | `0` → 静默；其他任何值或未设置 → 显示。 | 给那些已经读过警告、并且另有决定的人用的。 |
| `DELONIX_NO_AUTO_RECOVER` | `bins/delonix-runtime-bin/src/cmd/netns.rs:reconcile_after_respawn` | 在网络 holder 被重建之后，只报告那些被搁浅的容器和重启它们的命令，而不自动重启它们。 | 设置（任意值）→ 只报告。 | 给那些想自己选时机重启数据库的宿主机用的。 |
| `DELONIX_NO_AUTO_DELEGATE` | `bins/delonix-runtime-bin/src/cmd/kindmode.rs`（`cluster create` 的 cgroup 预检） | 当 `cpu` 控制器没有被委派时，禁用 `cluster create` 在 `systemd-run --user --scope -p Delegate=yes` 下自动 re-exec 的行为。 | 设置（任意值）→ 打印错误，而不是重新执行。 | 给偏好直接看到原始错误的脚本和 CI 用的。 |

## 网络与安全的逃生舱

**这一节里的每一个变量都会降低某条边界。** 每一个生效时都会记录一条 `SECURITY WARNING`（或者一条警告）。它们的存在是为了调试，以及明确的、经过深思熟虑的选择退出（opt-out）；它们没有一个属于生产环境的配置。用其中任何一个之前，先读一下链接的那节 `AGENTS.md`。

| 变量 | 读取方 | 用途 | 取值／默认 | 备注 |
|---|---|---|---|---|
| `DELONIX_FORWARD_POLICY` | `crates/adapters/delonix-sdn/src/infra.rs`（ingress 规则集的构造器） | 把 holder netns 的 `forward` 链从默认拒绝（`policy drop`）改回默认允许。 | `accept` → 默认允许；其他任何值 → 默认拒绝。 | **降低了网络之间的隔离性。** 会记为一条安全警告。见 `AGENTS.md`，*«Bloco 0 do plano 33 (v0.37.1)»*。 |
| `DELONIX_ALLOW_LINK_LOCAL` | `crates/adapters/delonix-sdn/src/infra.rs`（`fwguard` 链） | 移除对容器流量里 `169.254.0.0/16`（云端 metadata）和 `127.0.0.0/8`（宿主机回环）的无条件丢弃。 | `1` → 允许；其他任何值 → 丢弃。 | **在云端宿主机上暴露实例 metadata 里的凭据。** 安全警告。`AGENTS.md`，*«Bloco 0 do plano 33 (v0.37.1)»*（RF-NET-02）。 |
| `DELONIX_ALLOW_HOLDER_INGRESS` | `crates/adapters/delonix-sdn/src/infra.rs`（`dlxinput` 链） | 让容器能访问 holder 自己暴露的服务（L7 代理、内部 DNS），超出允许清单（allowlist）的范围。 | `1` → 允许；其他任何值 → 新连接会被丢弃。 | **通过这个代理，一个容器可以越过某个后端自身的 ingress 策略，到达任意 namespace 里任何已注册的后端。** 安全警告。理由在 `docs/discovery/46_GAPS_ENCONTRADOS.md` §4.2。 |
| `DELONIX_ENABLE_IPV6` | `crates/adapters/delonix-sdn/src/infra.rs:ipv6_sdn_enabled` | 重新给 SDN 上的容器分配 IPv6 地址。 | `1` → 启用；其他任何值 → 容器里的 IPv6 被禁用，转发被拒绝。 | **没有任何防火墙规则、namespace 隔离或 `Dependency` 会应用到 IPv6 上**——每一条策略都只针对 IPv4。安全警告。`AGENTS.md`，*«Bloco 0 do plano 33 (v0.37.1)»*。 |
| `DELONIX_ALLOW_UNENFORCED_LIMITS` | `bins/delonix-runtime-bin/src/cmd/container.rs:preflight_resource_limits` | 即使这个会话没有 cgroup 委派，也让 `-m`/`--cpus`/`--cpu-weight` 跑起来，而不是拒绝（退出码 69）。 | 设置（任意值）→ 带着警告不受限地跑。 | 内核根本看不到这些限制。见[cgroup 委派](environment.md#cgroup-delegation-some-limits-are-refused-others-are-not-enforced)。 |
| `DELONIX_INSECURE_BESTEFFORT` | `crates/adapters/delonix-linux/src/lib.rs:insecure_besteffort` | 跳过容器 init 和 `exec` 里、`execve` 之前会跑的那个 fail-closed 的封闭性验证（seccomp、capability、`no_new_privs`）。 | 设置（任意值）→ 跳过检查。 | **一个容器可能会以「本应生效却静默没生效」的封闭状态启动。** 引擎在容器自己的环境被应用之前就读取它，所以容器没法为自己设置它。 |
| `DELONIX_PUBLISH_ADDR` | `crates/adapters/delonix-sdn/src/lib.rs:publish_bind_addr`；由 `bins/delonix-runtime-bin/src/cmd/vm.rs`（`vm reach`）建议 | 当 `-p` 没有点名地址时，已发布端口绑定的宿主机地址。 | 一个 IPv4 地址；不是 IPv4 的值会被忽略。默认 `127.0.0.1`。 | `0.0.0.0` 会把已发布的端口暴露在每一个网卡上。`vm reach` 会建议 libvirt 的网关地址，好让 VM 能连到某个容器而不必把它暴露给局域网。`AGENTS.md`，*«Revisão do flow `-p` ↔ `ingress`/`egress` (2026-07-27)»*。 |
| `DELONIX_SUBNET_BASE` | `crates/adapters/delonix-sdn/src/lib.rs:default_base` | 强制指定默认网络（`10.<base>.0.0/16`）的第二个八位组。 | 一个 0–255 的整数（不做进一步的范围检查）。未设置：`<root>/net/default-base` 里持久化的值，否则是宿主机上检测到的一个空闲八位组。 | 只用于检测没有发现的冲突。在已经有容器存在之后再改它，会改变默认网络的地址。 |
| `DELONIX_CNI` | `crates/adapters/delonix-sdn/src/cni.rs:enabled_conf`，被 `crates/interfaces/delonix-cri/src/runtime_svc/lifecycle.rs`（`RunPodSandbox`）和 `runtime_svc.rs`（`UpdateRuntimeConfig`）消费 | **仅限 rootless 的 CRI：** 通过 CNI 插件链（`/etc/cni/net.d`，插件来自 `CNI_PATH`）给 pod sandbox 联网，跑在网络 holder 里面，而不是用原生 SDN。 | `1` → 开启，并且只在存在一份配置时才生效；其他任何值 → 原生 SDN。 | 当 CRI 以 root 身份运行时没有任何效果：root 身份下总是用节点的 CNI 配置。 |
| `DELONIX_TRACE_UNPUBLISH` | `crates/adapters/delonix-sdn/src/infra.rs:trace_unpublish` | 记录每一次端口取消发布的函数、端口、pid、父 pid、可执行文件和一份 backtrace。 | `1` 或 `stderr` → 写到 stderr；其他任何值 → 追加到那个文件里。 | 一个诊断工具，未设置时零开销。为 `AGENTS.md` 里 *«RESOLVIDO — as portas publicadas morriam sozinhas»* 那次调查保留。 |

## 构建与镜像

| 变量 | 读取方 | 用途 | 取值／默认 | 备注 |
|---|---|---|---|---|
| `DELONIX_INSECURE_REGISTRIES` | `crates/adapters/delonix-oci/src/registry.rs` | 用明文 HTTP 而不是 HTTPS 联系的那些 registry。 | 用逗号分隔的 `host` 或 `host:port` 条目（不区分大小写）。未设置 → 除了已经用 HTTP 的回环 registry 之外，其他地方都用 HTTPS。 | **那些主机的流量和凭据都不经过 TLS 传输。** 是按每个主机明确选择加入（opt-in）的，绝不是一个范围。当到某个 registry 的 HTTPS 连接失败时，错误信息会建议用这个变量并填上那个主机。 |
| `DELONIX_SCAN_ON_PULL` | `bins/delonix-runtime-bin/src/cmd/scan.rs:admission_scan_on_pull` | 在每次 pull 镜像之后应用的 CVE 准入策略。 | 未设置／空 → 关闭。`warn` → 扫描并报告。`low`/`medium`/`high`/`critical` → 发现至少这个严重程度的漏洞时移除镜像并拒绝。一个未知的值会拒绝这次 pull。 | 一个 fail-closed 的门禁：打错字不会把它关掉。一个没有 SBOM 的镜像会带着警告被放行。 |
| `DELONIX_ADVISORIES` | `bins/delonix-runtime-bin/src/cmd/scan.rs:load_advisories` | `image scan` 用的一份公告数据库文件的路径。 | 一个文件路径。 | 只在 `<root>/advisories.json` 没有已同步的数据库时才会用到；已同步的那份优先。两者都没有的话，会用内嵌的占位数据，并且扫描结果会说明这一点。 |
| `DELONIX_ADVISORY_FEED` | `bins/delonix-runtime-bin/src/cmd/scan.rs:cmd_scan_update` | `delonix image scan --update` 的数据源。 | 一个 URL 或文件（OSV 格式或原生格式）。 | `--feed` 优先。两者都没有的话，`--update` 会报错，同时点名两者。 |

## Cloud Hypervisor 与 VM 资源调优

下面这些 libvirt 上限会被写进生成的 domain XML（`crates/adapters/delonix-vm/src/lib.rs:libvirt_domain_xml`）；在已经存在的 domain 重新启动之前，它们不会改变那个 domain。

| 变量 | 读取方 | 用途 | 取值／默认 | 备注 |
|---|---|---|---|---|
| `DELONIX_HYPERVISOR_FW` | `crates/adapters/delonix-vm/src/lib.rs:default_ch_firmware` | 当没有给出 `--firmware` 时，Cloud Hypervisor 启动用的固件。 | 一个文件路径，只在它存在时才用。否则用 `DEFAULT_CH_FIRMWARES` 里第一个存在的（EDK2 的 `CLOUDHV.fd` 排在 `hypervisor-fw` 前面）。 | 为什么 EDK2 的构建排在前面，见[构建 microVM](microvm-setup.md)。 |
| `DELONIX_VM_RESERVE_MIB` | `crates/adapters/delonix-vm/src/lib.rs:vm_admission_check` | 为宿主机保留、不给 VM 用的内存量：如果一个 VM 的内存加上这份保留超过了 `MemAvailable`，就会被拒绝。 | 单位 MiB；默认 `2048`；一个解析不了的值会回退到 2048。 | 调低它有让宿主机 OOM-kill 东西的风险。 |
| `DELONIX_VM_MEM_HARD_LIMIT` | `crates/adapters/delonix-vm/src/lib.rs:mem_hard_limit_kib` | libvirt domain 是否会在整个 QEMU 进程上得到一个 `<memtune><hard_limit>`。 | `off` → 没有硬性上限；其他任何值 → 有。 | |
| `DELONIX_VM_MEM_OVERHEAD_PCT` | `crates/adapters/delonix-vm/src/lib.rs:mem_hard_limit_kib` | 硬性上限允许超出 guest 内存的余量。 | 百分比，接受范围 5–200；默认 `25`；至少 1 GiB 的余量。超出范围 → 25。 | |
| `DELONIX_VM_CPU_QUOTA_CORES` | `crates/adapters/delonix-vm/src/lib.rs:cpu_quota_micros` | 一个 libvirt domain 的 CPU 上限（`<cputune><quota>`），以核心数为单位。 | 未设置 → vCPU 数 + 1（多出来那一核给 QEMU 的 emulator 和 IO 线程用）。一个正数 → 那么多核心。`off` → 没有上限。 | |
| `DELONIX_VM_IO_MAX_BPS` | `crates/adapters/delonix-vm/src/lib.rs:vm_iotune_xml` | 根磁盘吞吐量上限（`<iotune><total_bytes_sec>`）。 | 字节/秒，正整数。未设置或 0 → 没有上限（opt-in）。 | |
| `DELONIX_VM_IO_MAX_IOPS` | `crates/adapters/delonix-vm/src/lib.rs:vm_iotune_xml` | 根磁盘 IOPS 上限（`<iotune><total_iops_sec>`）。 | 正整数。未设置或 0 → 没有上限（opt-in）。 | |

### 容器资源预算

这些会调整引擎应用在容器上的默认值以及总量上限（`crates/adapters/delonix-linux/src/lib.rs`）。

| 变量 | 读取方 | 用途 | 取值／默认 | 备注 |
|---|---|---|---|---|
| `DELONIX_RESERVE_PCT` | `crates/adapters/delonix-linux/src/lib.rs:host_reserve_pct` | 引擎的 cgroup slice 总共可以用掉的宿主机份额（内存和 CPU）。 | 百分比，接受范围 10–95；默认 `85`。超出范围 → 85。 | 也是下面「每个工作负载默认值」的基数。 |
| `DELONIX_DEFAULT_PCT` | `crates/adapters/delonix-linux/src/lib.rs:default_workload_pct` | 一个工作负载在没有声明任何限制时，可以拿走引擎预算的份额。 | 百分比，接受范围 1–100；默认 `25`。超出范围 → 25。 | 内存默认值至少是 64 MiB；CPU 默认值被限制在 0.25 到 1.0 核之间。 |
| `DELONIX_SWAP_MAX` | `crates/adapters/delonix-linux/src/lib.rs:swap_max_value` | 某个容器 cgroup 的 `memory.swap.max`。 | 一个 cgroup 值；默认 `0`（没有 swap）。`max` 恢复为不限制 swap。 | Swap 会把一个硬性的内存限制变成一个软性的。 |
| `DELONIX_IO_MAX_BPS` | `crates/adapters/delonix-linux/src/lib.rs:host_io_max_bps` | 引擎 slice 的磁盘读写总量上限（`io.max`）。 | 字节/秒；默认 `500000000`（500 MB/s）。`0` 会禁用它。一个解析不了的值 → 用默认值。 | 一个防止某个容器把磁盘打满的安全上限，不是精细的 QoS。只在 `io` 控制器可用时才生效。 |

## Provider

### Proxmox VE

由 `bins/delonix-runtime-bin/src/cmd/vmbackends.rs:register_proxmox` 在 CLI 启动时读取一次。在某条 VM 命令选择 `proxmox` 这个 backend 之前，什么都不会被联系。空白值算作未设置。配置错了会打印一条警告，让这个 backend 保持未注册状态；不会阻止其他命令。背景：`docs/adr/0008-proxmox-vm-backend.md`。

| 变量 | 读取方 | 用途 | 取值／默认 | 备注 |
|---|---|---|---|---|
| `DELONIX_PROXMOX_URL` | `cmd/vmbackends.rs:register_proxmox_with`；在 `crates/adapters/delonix-vm/src/lib.rs`（`KNOWN_UNREGISTERED`）的错误信息里被点名 | 这个节点的 API 端点。设置它就是启用这个 backend 的开关。 | `https://<host>:8006`。 | 需要 `DELONIX_PROXMOX_NODE` 和一份凭据。 |
| `DELONIX_PROXMOX_NODE` | `cmd/vmbackends.rs:register_proxmox_with` | 要用的节点名字（`GET /nodes` 报告的那个）。 | 例如 `pve`。没有默认值：这个 backend 永远不会替你挑一个节点。 | |
| `DELONIX_PROXMOX_SECRET` | `cmd/vmbackends.rs:proxmox_auth` | 一个持有凭据的 `kind: Secret` 的名字。 | 带有 `tokenId`+`tokenSecret`（优先）或 `username`+`password` 的 Secret。 | **最先**被检查。比下面那些变量优先，因为那些最终会留在 shell 历史和 `ps` 里。 |
| `DELONIX_PROXMOX_TOKEN_ID` | `cmd/vmbackends.rs:proxmox_auth` | API token id。 | `user@realm!tokenname`。 | 和 `DELONIX_PROXMOX_TOKEN` 一起用；在 secret 之后被检查。 |
| `DELONIX_PROXMOX_TOKEN` | `cmd/vmbackends.rs:proxmox_auth` | API token 的密钥。 | | |
| `DELONIX_PROXMOX_TOKEN_FILE` | `cmd/vmbackends.rs:credential_value` | 存放 API token 密钥的文件路径；优先于 `DELONIX_PROXMOX_TOKEN`，因为后者会被每个子进程继承。 | 一个路径。 | 除非只有属主可读（`chmod 600`），否则拒绝。 |
| `DELONIX_PROXMOX_USER` | `cmd/vmbackends.rs:proxmox_auth` | 用密码认证的账号。 | `root@pam`，…… | 和 `DELONIX_PROXMOX_PASSWORD` 一起用；最后被检查。 |
| `DELONIX_PROXMOX_PASSWORD` | `cmd/vmbackends.rs:proxmox_auth` | 该账号的密码。 | | |
| `DELONIX_PROXMOX_PASSWORD_FILE` | `cmd/vmbackends.rs:credential_value` | 存放该密码的文件路径；优先于 `DELONIX_PROXMOX_PASSWORD`。 | 一个路径。 | 除非只有属主可读（`chmod 600`），否则拒绝。providers 文件中的 `passwordFile` 会映射到它。 |
| `DELONIX_PROXMOX_INSECURE_TLS` | `cmd/vmbackends.rs:register_proxmox_with` | 跳过对这个节点的 TLS 证书校验。 | `1`、`true` 或 `yes` → 跳过；默认要校验。 | **冒充这个节点应答的另一台机器会拿到凭据。** 只能主动选择加入，绝不会在 TLS 出错后作为回退被应用。 |
| `DELONIX_PROXMOX_BRIDGE` | `cmd/vmbackends.rs:register_proxmox_with` | 这个节点上 VM 网卡的默认网桥。 | 一个网桥的名字；这个 backend 的默认值是 `vmbr0`。 | 每个 VM 自己的 `bridge:` 优先。 |
| `DELONIX_PROXMOX_VLAN` | `cmd/vmbackends.rs:parse_vlan` | 这个节点上 VM 网卡的默认 VLAN 标签。 | 1–4094。超出范围是一个**错误**，绝不会被悄悄丢弃。 | |
| `DELONIX_PROXMOX_CA_FILE` | `cmd/vmbackends.rs:register_proxmox_with` | 除了系统根证书之外，额外为这个节点信任的一份 CA 证书（PEM）。 | PEM 文件的路径；读不了是一个**错误**。 | 校验一个证书是由内部 CA 签发的节点的方式，用它代替 `DELONIX_PROXMOX_INSECURE_TLS`。 |
| `DELONIX_PROXMOX_TRACE_ROUTES` | `cmd/vmbackends.rs:register_proxmox_with`（读取一次，作为 `ClientOptions::trace_routes` 交给客户端；`crates/providers/delonix-proxmox/src/lib.rs` 里的常量 `TRACE_ROUTES_ENV` 点名了它）以及 `crates/providers/delonix-proxmox/tests/live.rs:backend`（实机测试组，它的一次运行被提交为 `docs/proxmox/trace-9.2.2.routes`） | 把每一次请求的 `METHOD /path` 追加到这个文件里——覆盖率矩阵（ADR-0049）的分子。 | 一个文件的路径；空 = 关闭。 | 把它喂给 `scripts/proxmox_api_inventory.py --trace`，就能把路由标记为 `supported+tested`。 |

### OPNsense

针对一台真实 OPNsense 设备的 `GatewayProvider`（`kind: NetworkGateway`，ADR-0051），注册方式和上面的 Proxmox backend 一样，由 `cmd::gatewayproviders` 读取。

| 变量 | 读取方 | 用途 | 取值／默认 | 备注 |
|---|---|---|---|---|
| `DELONIX_OPNSENSE_URL` | `cmd/gatewayproviders.rs:register_opnsense_with` | 这台设备的 API 端点。设置它就是启用这个 provider 的开关。 | `https://<host>`。 | 需要一份凭据。 |
| `DELONIX_OPNSENSE_CREDENTIAL` | `cmd/gatewayproviders.rs:opnsense_auth` | 一个持有凭据的 `kind: Secret` 的名字。 | 带有 `key`+`secret` 字段的 Secret——一对生成出来的 API key/secret，绝不能是某个图形界面账号的用户名/密码（ADR-0051 阶段 0：那些会被这个 API 拒绝）。 | **最先**被检查。 |
| `DELONIX_OPNSENSE_KEY` | `cmd/gatewayproviders.rs:opnsense_auth` | API key。 | | 和 `DELONIX_OPNSENSE_SECRET` 一起用；在那个 `kind: Secret` 之后被检查。 |
| `DELONIX_OPNSENSE_SECRET` | `cmd/gatewayproviders.rs:opnsense_auth`（通过 `credential_value`） | API secret。 | 优先用 `DELONIX_OPNSENSE_SECRET_FILE`（一个 `chmod 600` 的路径）。 | |
| `DELONIX_OPNSENSE_INSECURE_TLS` | `cmd/gatewayproviders.rs:register_opnsense_with` | 跳过对这台设备的 TLS 证书校验。 | `1`、`true` 或 `yes` → 跳过；默认要校验。 | 一台开箱即用的 OPNsense 提供的是自签名证书（实机测量过，ADR-0051 阶段 0）。**冒充这台设备应答的另一台机器会拿到凭据。** 只能主动选择加入。 |
| `DELONIX_OPNSENSE_CA_FILE` | `cmd/gatewayproviders.rs:register_opnsense_with` | 除了系统根证书之外，额外为这台设备信任的一份 CA 证书（PEM）。 | PEM 文件的路径；读不了是一个**错误**。 | 用它代替 `DELONIX_OPNSENSE_INSECURE_TLS`。 |

### TrueNAS

TrueNAS 的配置器（`kind: Volume` 加上 `spec.provision.truenas`）从清单（manifest）和一个 `kind: Secret` 里取它的目标，而不是从环境变量。唯一的 `DELONIX_TRUENAS_*` 名字是测试用的设置——见[仅测试使用](#test-only)。

## 可观测性

| 变量 | 读取方 | 用途 | 取值／默认 | 备注 |
|---|---|---|---|---|
| `DELONIX_OTLP_ENDPOINT` | `crates/adapters/delonix-telemetry/src/telemetry.rs:build_otlp_layer` | 通过 OTLP/HTTP（protobuf）导出 tracing span。 | 一个基础 URL，比如 `http://localhost:4318`；如果没有会自动补上 `/v1/traces`。未设置或空白 → 没有导出器。 | 构建导出器失败会给出警告，然后只用日志继续跑下去。服务名是 `OTEL_SERVICE_NAME` 或者可执行文件的名字。 |
| `DELONIX_METRICS_ADDR` | `crates/interfaces/delonix-cri/src/lib.rs`（CRI 服务端启动） | 在 `delonix-cri` 里启用一个 Prometheus 的 `/metrics` HTTP 监听器。 | `host:port`，比如 `127.0.0.1:9100`。未设置 → 没有监听器。 | 是一个 TCP 监听器：除非这些指标应该能被网络访问，否则把它绑定到回环地址上。 |
| `DELONIX_WALK_THREADS` | `crates/adapters/delonix-volume/src/lib.rs:walk_threads`（`system df`、卷用量和 rootless 配额背后的磁盘遍历） | 设置并行目录遍历用多少个 worker。 | 一个正整数；`1` → 顺序遍历。未设置 → CPU 数量，有上限。 | 并行遍历和顺序遍历给出**相同**的总量（用不同 worker 看到的硬链接测试过）；这个变量存在是为了在特定宿主机上证明这一点，也是当某个文件系统在并发 `readdir` 下表现异常时的逃生舱。 |

## 由引擎自己设置／内部使用

**不要自己设置这些。** 引擎会在它启动的进程上写它们；手动设置它们会让一条正常的命令表现得像一次内部传递。

| 变量 | 读取方 | 用途 | 取值／默认 | 备注 |
|---|---|---|---|---|
| `DELONIX_BIN` | `crates/contexts/delonix-node/src/dispatch.rs:cli_bin`（被 `delonix-cri`、`delonix-mgmt`、`delonix-mcp` 使用） | 某个服务端为了生命周期操作而回调的那个 `delonix` 可执行文件。 | 一个路径。未设置：服务端可执行文件旁边的那个 `delonix`，然后是 `PATH` 上的 `delonix`。 | 由 `delonix serve …` / `delonix mcp`（`cmd/serve.rs:exec_server`）设置为正在运行的 CLI。只有当你直接启动一个服务端二进制文件、并且想让它调用某个特定的 CLI 时，手动设置它才是合理的。`scripts/cli-tree.sh` 和 `scripts/docs_cli_gate.py` 也会读一个同名的变量，来选择它们要检查的二进制文件（见[克隆、构建与测试](build-and-test.md)）。 |
| `DELONIX_DISPATCH_VERSION` | `crates/contexts/delonix-node/src/dispatch.rs:check_version`（在 `delonix-cri`、`delonix-mgmt`、`delonix-mcp` 里） | 服务端必须匹配的版本；来自另一个 release 的服务端会拒绝启动。 | 由 `cmd/serve.rs:exec_server` 设置为 CLI 的版本。 | 直接启动的服务端（比如由一个 systemd unit 启动的）没有这个期望，也不会被检查。 |
| `DELONIX_REEXEC_ID` | `bins/delonix-runtime-bin/src/cmd/container.rs`（`cmd_run`、`reexec_env`） | 标记 `container run/start --net <custom>` 或 `--pod` 的第二次执行——它会在 holder 的 namespace 里被重新执行——并带上容器 id。 | 由 `cmd/container.rs:reexec_env` 设置。 | 它的存在会跳过第一次执行已经做过的检查（端口归属）。 |
| `DELONIX_REEXEC_IP` | `bins/delonix-runtime-bin/src/cmd/container.rs` | 第一次执行时接入的 SDN 地址，供第二次执行记录下来。 | 由 `cmd/container.rs:reexec_env` 设置。 | |
| `DELONIX_PIN_SYNC` | `crates/adapters/delonix-sdn/src/pin_userns.rs`（`SYNC_ENV`） | 在写 user namespace 映射期间，调用方和网络 pin 之间握手管道的文件描述符。 | `<read-fd>,<write-fd>`。 | 只有当 pin 必须创建新的 namespace 时才会被设置；一个接手（adopt）的 pin 永远拿不到它。 |
| `DELONIX_DELEGATE_ATTEMPTED` | `bins/delonix-runtime-bin/src/cmd/kindmode.rs:reexec_under_delegated_scope` | 守卫 `cluster create` 在一个委派 scope 下唯一一次自动 re-exec，好让一台依然缺少 `cpu` 的宿主机显示真正的错误，而不是陷入循环。 | 在重新执行的进程上设为 `1`。 | 要禁用这次 re-exec，用 `DELONIX_NO_AUTO_DELEGATE`。 |
| `DELONIX_INTERNAL` | 由 `crates/adapters/delonix-sdn/src/infra.rs`（`start_control` 和其他 holder 的 spawn）、`crates/adapters/delonix-vm/src/lib.rs:launch_vmm`、`crates/interfaces/delonix-cri/src/runtime_svc/lifecycle.rs`、`spdy.rs`、`streaming.rs` 设置在子进程上 | 标记一次机器对机器的调用。 | 设为 `1`。 | **在当前代码里没有读取者**：`runtime_svc/lifecycle.rs:delonix` 里的注释说它会「绕过分组命令的屏障（bypasses the grouped-commands barrier）」，但已经没有任何东西再读这个变量了。它依然被写入，也出现在网络进程的 environ 里（见 `infra.rs:env_names_this_root` 的测试）。 |

## 构建期

编译期就固定下来的值：它们在二进制文件被构建的那一刻就定了，程序运行时设置一个变量也改变不了它们。

| 变量 | 读取方 | 用途 | 取值／默认 | 备注 |
|---|---|---|---|---|
| `DELONIX_GIT_HASH` | 由 `bins/delonix-runtime-bin/build.rs` 写入；在 `bins/delonix-runtime-bin/src/main.rs` 里用 `env!` 读取 | `delonix --version` 显示的短 commit hash。 | `git rev-parse --short=9 HEAD`，没有 git 的话就是 `unknown`。 | |
| `DELONIX_GIT_SINCE` | 由 `bins/delonix-runtime-bin/build.rs` 写入；在 `src/main.rs` 里读取 | 距离最新 tag 的距离，显示为 `(+N commits since vX.Y.Z)`。 | `<count>\|<tag>`，在一个 tag 上或者没有 git 时为空。 | 这就是你区分两个版本号相同的构建的方法。 |
| `DELONIX_BUILD_DATE` | 由 `bins/delonix-runtime-bin/build.rs` 写入；在 `src/main.rs` 和 `src/cmd/man.rs` 里读取 | `--version` 里以及生成的 man page 里的构建日期。 | `date -u +%Y-%m-%d`，或者 `unknown`。 | man page 用它而不是时钟，好让同一个构建的两次生成器运行产出相同的输出。 |
| `DELONIX_GIT_COMMIT` | 在 `bins/delonix-runtime-bin/src/cmd/dockerapi.rs` 里用 `option_env!` 读取（`/version` 的响应，`GitCommit`） | Docker API 切片报告的 commit。 | 如果构建环境里有设置就用那个值；否则是 `unknown`。 | **仓库里没有任何东西会设置它**（`build.rs` 没有，各个 workflow 也没有），所以发布出去的二进制文件报告的都是 `unknown`。 |
| `DELONIX_BPF_OBJECT` | 由 `crates/adapters/delonix-sdn/build.rs` 写入；在 `crates/adapters/delonix-sdn/src/bpf.rs` 里用 `env!` 读取 | 内嵌在二进制文件里、编译好的 eBPF 流量统计对象的路径。 | 只有在构建时 `clang` 和 libbpf 头文件都存在的情况下才会被设置（连同 `cfg(bpf_object)`）。 | 可选：没有它，runtime 会退化成用 nftables 计数器。 |
| `DELONIX_ASSET` | `scripts/install.sh`（二进制那一节） | 保存为这个 CPU 选中的 release 资产文件名（`delonix` 或它的 `-v3` 变体）的 shell 变量。 | 由脚本在下载那一步赋值。 | 不会从你的环境里读取；在跑安装器之前设置它没有任何效果。 |

## 仅测试使用

这些只被测试读取。没有它们，实机测试会**跳过**，并打印 `SKIP: … is not set`（一个没有目标却静默通过的实机测试，什么都证明不了）。

| 变量 | 读取方 | 用途 | 取值／默认 | 备注 |
|---|---|---|---|---|
| `DELONIX_PROXMOX_TEST_URL` | `crates/providers/delonix-proxmox/tests/live.rs:target` | 用来跑实机 backend 测试的 Proxmox 节点。 | `https://<host>:8006`。 | 启用这些测试。它们会创建并销毁一个 VM；只对你自己拥有的节点运行。 |
| `DELONIX_PROXMOX_TEST_NODE` | `crates/providers/delonix-proxmox/tests/live.rs:target` | 节点名字。 | 默认 `pve`。 | |
| `DELONIX_PROXMOX_TEST_USER` | `crates/providers/delonix-proxmox/tests/live.rs:target` | 用密码认证的账号。 | 比如 `root@pam`。必填。 | 这些测试里 TLS 校验是关闭的。 |
| `DELONIX_PROXMOX_TEST_PASS` | `crates/providers/delonix-proxmox/tests/live.rs:target` | 它的密码。 | 必填。 | |
| `DELONIX_PROXMOX_TEST_STORAGE` | `crates/providers/delonix-proxmox/tests/live.rs` | 测试用 VM 磁盘所在的存储。 | 默认 `local-lvm`。 | |
| `DELONIX_PROXMOX_TEST_BACKUP_STORAGE` | `crates/providers/delonix-proxmox/tests/live.rs` | 备份归档落地的存储——节点可能不接受在磁盘存储上放备份内容（一个 thin-LVM 池就不行）。 | 默认：和 `DELONIX_PROXMOX_TEST_STORAGE` 一样。 | |
| `DELONIX_PROXMOX_TEST_MOVE_STORAGE` | `crates/providers/delonix-proxmox/tests/live.rs` | `move_disk` 会把测试 VM 的启动磁盘移到的**第二个**存储——节点会拒绝移到格式相同的同一个存储，所以这个必须是真正不同的一个池。 | 默认 `local`（必须在它上面启用了 `content=images`）。 | |
| `DELONIX_PROXMOX_TEST_MOVE_NODE` | `crates/providers/delonix-proxmox/tests/live.rs` | `vm move` 实机用例把 VM 移到的同一集群中的节点（ADR-0053）。需要一个两节点的实验集群——绝不能是生产环境。 | 未设置：两个移动用例会跳过。 | |
| `DELONIX_PROXMOX_TEST_SHARED_STORAGE` | `crates/providers/delonix-proxmox/tests/live.rs` | 集群所有节点共享的存储（NFS、Ceph RBD），移动用例把 VM 的磁盘放在这里——位于本地存储上的磁盘会被拒绝，而不是被复制。 | 未设置：两个移动用例会跳过。 | |
| `DELONIX_PROXMOX_TEST_CALLBACK_ADDR` | `crates/providers/delonix-proxmox/tests/live.rs:sdn_controllers_fabric_dhcp_and_ip_reservations_round_trip_through_the_node` | 节点能够访问到这台机器的地址：测试会启动一个桩 HTTP 服务器，把 `http://<addr>:<port>/…` 作为一个 IPAM 控制器和一个 DNS 控制器的 URL 交给节点，因为节点会通过调用它们来验证两者。 | 一个节点能路由到的 IP（一个 libvirt-NAT 实验节点用 `192.168.122.1`）。 | 没有它，那个测试里控制器那一半会被跳过；其余部分照常运行。 |
| `DELONIX_PROXMOX_TEST_AGENT_VMID` | `crates/providers/delonix-proxmox/tests/live.rs:o_ip_vem_do_agente_de_um_convidado_a_serio` | 一个已经存在、跑着 QEMU guest agent 的 VM，测试会读取它的 IP。 | 一个 VM id。 | 未设置时跳过，即使 URL 已经设置了也一样。 |
| `DELONIX_TRUENAS_TEST_URL` | `crates/providers/delonix-truenas/tests/live.rs:target` | 用来跑实机配置器测试的 TrueNAS 设备。 | `https://<host>`。 | 启用这些测试。它们会创建并销毁 `<pool>/dlxlive-<pid>`。 |
| `DELONIX_TRUENAS_TEST_POOL` | `crates/providers/delonix-truenas/tests/live.rs:target` | 测试数据集用的池。 | 默认 `tank`。 | |
| `DELONIX_TRUENAS_TEST_KEY` | `crates/providers/delonix-truenas/tests/live.rs:target` | API key。 | | 设置了的话，会代替用户名和密码。 |
| `DELONIX_TRUENAS_TEST_USER` | `crates/providers/delonix-truenas/tests/live.rs:target` | 用密码认证的账号。 | 没有 key 时必填。 | 这些测试里 TLS 校验是关闭的。 |
| `DELONIX_TRUENAS_TEST_PASS` | `crates/providers/delonix-truenas/tests/live.rs:target` | 它的密码。 | 没有 key 时必填。 | |
| `DELONIX_OPNSENSE_TEST_URL` | `crates/providers/delonix-opnsense/tests/live.rs:target` | 用来跑实机客户端测试的 OPNsense 设备。 | `https://<host>`。 | 启用这些测试。它们会创建并移除一个别名和一条规则，然后确认这台设备被恢复干净。 |
| `DELONIX_OPNSENSE_TEST_KEY` | `crates/providers/delonix-opnsense/tests/live.rs:target` | API key。 | 必填。 | |
| `DELONIX_OPNSENSE_TEST_SECRET` | `crates/providers/delonix-opnsense/tests/live.rs:target` | API secret。 | 必填。 | 这些测试里 TLS 校验是关闭的。 |
| `DELONIX_UPDATE_FIXTURES` | `crates/adapters/delonix-linux/tests/advisor_fixtures.rs:goldens_match_the_rules_as_they_are_today` | 重写 `crates/adapters/delonix-linux/tests/fixtures/advisor/` 里 advisor 的 golden fixture，而不是拿它们做比较。 | 设置（任意值）→ 重写。 | 要和规则的改动放在**同一个** commit 里重新生成。 |

跑实机测试（把占位符换掉；永远不要提交真实凭据）：

```bash
DELONIX_PROXMOX_TEST_URL=https://<node>:8006 \
DELONIX_PROXMOX_TEST_NODE=pve \
DELONIX_PROXMOX_TEST_USER=root@pam \
DELONIX_PROXMOX_TEST_PASS='<password>' \
  cargo test -p delonix-proxmox --test live -- --nocapture

DELONIX_TRUENAS_TEST_URL=https://<appliance> \
DELONIX_TRUENAS_TEST_USER=<user> \
DELONIX_TRUENAS_TEST_PASS='<password>' \
DELONIX_TRUENAS_TEST_POOL=tank \
  cargo test -p delonix-truenas --test live -- --nocapture

DELONIX_UPDATE_FIXTURES=1 cargo test -p delonix-linux --test advisor_fixtures
```

## 看起来像变量、其实不是的名字

有几个 `DELONIX_*` 名字出现在代码里，但不会从任何进程环境里被读取。它们被 `scripts/dev_docs.py` 里的 `NOT_ENV` 排除在上面的检查之外，各自带着理由，你永远不需要设置它们：

- `DELONIX_CRI_SOCKET` —— `bins/delonix-runtime-bin/src/cmd/cluster.rs` 里的一个 Rust 常量，传给 `kubeadm … --cri-socket=`。CRI 服务端监听的那个 socket 是 `DELONIX_CRI_ADDR`。
- `DELONIX_ROOTX`、`DELONIX_ROOT_BACKUP` —— `crates/adapters/delonix-sdn/src/infra.rs` 里的测试 fixture，用来证明读取一个进程 environ 是按完整名字匹配的，而不是前缀匹配。
- `DELONIX_IMAGE`、`DELONIX_DISTRO`、`DELONIX_RELEASE`、`DELONIX_BUILT_BY`、`DELONIX_BASE_IMAGE`、`DELONIX_BASE_SHA256`、`DELONIX_K8S_VERSION`、`DELONIX_OFFLINE`、`DELONIX_NODE_EXPORTER`、`DELONIX_EXTRA_PACKAGES` —— `image vm build` 写在一个构建好的 VM 镜像**里面**（`bins/delonix-runtime-bin/src/cmd/vmimage.rs`）的溯源文件 `/etc/delonix-image-release` 的 key。在 guest 里用 `cat /etc/delonix-image-release` 读取它们；没有任何进程会读取它们。

---

**下一步：** [术语表](glossary.md)——你会在这个仓库里遇到的那些词，以及它们在 Delonix 里的含义和解释它们的地方。
