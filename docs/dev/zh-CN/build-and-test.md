<!-- translated-from: build-and-test.md sha256:a509235c85941dead7c564eb0fe69325be76ad29f67752755461f3637af80128 -->
# 克隆、构建与测试

**阅读之前：** [准备你的环境](environment.md)：固定的工具链、`protoc`，以及一台能通过其检查的主机。

本页假设你手上已经是[准备你的环境](environment.md)里那台主机：固定的 Rust 工具链和 `protoc`。读完之后，你就能构建并安装你的这棵树、在本地跑通每一个 CI 门禁，并且在不触碰真实引擎状态的情况下运行 E2E 测试和 chaos 测试装置（harness）。

这里的一切都从你 checkout 出来的根目录运行——理想情况下是一个 **git worktree**：一个拥有自己分支的独立工作目录，每个任务一个，从 `origin/main` 创建出来（命令见[从这里开始，第 2 步](start-here.md#2-open-a-worktree-from-originmain)；规则见[贡献工作流](contributing-workflow.md#one-worktree-per-task)）。

## 克隆

```bash
git clone https://github.com/angolardevops/delonix-runtime.git
cd delonix-runtime
git fetch --tags origin      # several gates compare against the release tags
```

## 构建

```bash
cargo build --workspace                    # every crate and every binary
cargo build -p delonix-runtime-bin         # just the `delonix` CLI
cargo build --release -p delonix-runtime-bin   # what the docs generator and the CLI gates use
```

这个 workspace 会产出下面这些二进制程序，来自这些包：

| 二进制程序 | 包 | 产物 |
|---|---|---|
| `delonix`（CLI） | `delonix-runtime-bin` | `target/debug/delonix` 或 `target/release/delonix` |
| `delonix-cri` | `delonix-cri` | `target/<profile>/delonix-cri` |
| `delonix-mcp` | `delonix-mcp-bin` | `target/<profile>/delonix-mcp` |
| `delonix-mgmt` | `delonix-mgmt-bin` | `target/<profile>/delonix-mgmt` |

发布工作流构建的正是这四个包。如果你设置了 `CARGO_TARGET_DIR`，二进制程序会落到那里，而不是 `target/` 下。

两条实用的提示：

- **永远测试你自己构建出来的那个二进制程序**（`./target/debug/delonix`），绝不要用你
  `PATH` 上的那个 `delonix`——那是一个已安装的发布版，往往落后好几个版本。
- 如果你同时在多个 worktree 里工作，把它们都指向同一个共享的 `CARGO_TARGET_DIR` 能省磁盘空间，但两个同时在上面跑的构建会互相等待，还可能让彼此的产物失效。每个 worktree 单独一个 target 目录，第一次会慢一些，之后则是可预测的。

## 在本地安装你的构建

大多数改动根本不需要一个已安装的构建：直接在你的 worktree 里跑
`./target/debug/delonix` 就够了。只有当你需要一条稳定路径时才安装——比如一个 systemd unit、另一个 shell 里的脚本、一个和 `delonix-cri` 对话的 kubelet。安装前后，都要确认**你正在跑的是哪个构建**：

```bash
./target/debug/delonix --version    # commit: <hash> (+N commits since vX.Y.Z) · built: <date>
command -v delonix                  # which `delonix` your shell would run instead
```

`commit:` 这一行来自 `bins/delonix-runtime-bin/build.rs`（`DELONIX_GIT_HASH`、
`DELONIX_GIT_SINCE`）。在两次发布之间，每一次构建携带的都是同一个版本号，所以 commit 是把你的构建和已发布版本区分开的唯一办法。

### `delonix` 如何找到它的服务器二进制程序

`delonix serve cri`、`delonix serve api` 和 `delonix mcp` 本身并不包含这些服务器：它们会 `exec` `delonix-cri`、`delonix-mgmt` 和 `delonix-mcp`
（`bins/delonix-runtime-bin/src/cmd/serve.rs` 里的 `exec_server`）。查找顺序是：

1. 与正在运行的这个 `delonix` **同目录下**的同名文件；
2. 否则就是 `PATH` 上的那个同名文件。

`delonix` 会通过 `DELONIX_DISPATCH_VERSION` 把自己的版本号传给服务器，一个来自不同发布版本的服务器会拒绝启动。它还会通过 `DELONIX_BIN` 传入自己的路径，这样服务器就能回调同一个 CLI。一个被直接启动的服务器（比如被某个 unit 启动）会依次通过
`DELONIX_BIN`、自己同目录下的一个 `delonix`、再到 `PATH`
（`crates/contexts/delonix-node/src/dispatch.rs` 里的 `cli_bin`）来找到 CLI。
**把同一次构建产出的这四个二进制程序放在一起**；混用你的构建和一个发布版会被拒绝，或者会跑一些你本不打算测试的代码。

`delonix cluster kubeadm` 和 `delonix image vm build` 用它们自己的顺序去找
`delonix-cri`（`bins/delonix-runtime-bin/src/cmd/vmimage.rs` 里的
`resolve_cri_bin`）：`--cri-bin`，然后是 `delonix` 同目录下，然后——如果当前目录在一个源码 checkout 里面——是一次 `cargo build --release -p delonix-cri`，只有到最后才会去下载已发布的产物。

在安装它们之前，先把这四个都构建出来：

```bash
cargo build --release -p delonix-runtime-bin -p delonix-cri -p delonix-mgmt-bin -p delonix-mcp-bin
```

### 方案 A——直接从 worktree 运行（最安全）

什么都不会被拷贝，所以你 checkout 之外的任何东西都不会意外用上它：

```bash
alias delonix-dev="$PWD/target/release/delonix"
delonix-dev --version
```

服务器之所以能被找到，是因为它们就和它一起放在 `target/release/` 里。在
Ubuntu 23.10+ 上，这条路径需要一份自己的 AppArmor profile（见
[AppArmor](environment.md#ubuntu-2310-apparmor-blocks-user-namespaces-for-your-dev-binary)）。

### 方案 B——为你自己的用户安装到 `~/.local/bin`

```bash
install -d ~/.local/bin
install -m 0755 target/release/delonix target/release/delonix-cri \
  target/release/delonix-mgmt target/release/delonix-mcp ~/.local/bin/
hash -r                              # forget the path your shell cached
command -v delonix && delonix --version
```

如果 `/usr/local/bin` 里也装了一个发布版，`PATH` 上排在前面的那个目录说了算。

**AppArmor。** `scripts/install.sh` 只在设置了
`kernel.apparmor_restrict_unprivileged_userns=1` 的主机上写一份 profile，
`/etc/apparmor.d/delonix`，绑定到 `<安装目录>/delonix`。你拷贝到新路径下的二进制程序不在这份 profile 的覆盖范围内。不要在一台同时也在用已安装发布版的机器上，重新跑一遍安装脚本来"搬动"那份 profile：那个 profile 文件会被重写，已发布的二进制程序会因此失去它。改成用一个不同的名字添加第二份 profile——形状和安装脚本写的一样，所以不会替换掉任何东西：

```bash
printf 'abi <abi/4.0>,\ninclude <tunables/global>\nprofile delonix-dev %s flags=(unconfined) {\n  userns,\n}\n' \
  "$HOME/.local/bin/delonix" | sudo tee /etc/apparmor.d/delonix-dev >/dev/null
sudo apparmor_parser -r /etc/apparmor.d/delonix-dev
```

*此处未经验证：* 这条命令照搬了 `install.sh` 里的那段 AppArmor 逻辑（只是换了一个 profile 名字和文件）；写这一页的时候，并没有在一台该限制处于启用状态的主机上真的加载过它。

安装脚本还会加上 shell 补全、man 手册页和编辑器语法文件，但只在它的二进制程序那一阶段才会做。对于你自己的构建，如果你想要这些东西，就从二进制程序本身生成它们：

```bash
mkdir -p ~/.local/share/bash-completion/completions
delonix completion shell bash > ~/.local/share/bash-completion/completions/delonix
delonix man --dir ~/.local/share/man
```

### 方案 C——系统级安装到 `/usr/local/bin`

```bash
sudo install -m 0755 target/release/delonix target/release/delonix-cri \
  target/release/delonix-mgmt target/release/delonix-mcp /usr/local/bin/
```

**只在一台没有任何 Delonix 工作负载在使用的机器上这样做。** 已安装的二进制程序不只是一个命令：

- 由 `delonix system boot enable` 写下的开机 unit 会启动
  `ExecStart=<exe> container start <name>`，其中 `<exe>` 是运行了 `enable` 的那个二进制程序的路径（`bins/delonix-runtime-bin/src/cmd/boot.rs`，unit 前缀
  `delonix-boot-`）；替换掉那个文件，会改变下一次重启之后起来的是什么；
- `dist/delonix-cri.service` 运行的是 `/usr/local/bin/delonix-cri`，所以在一个 Kubernetes 节点上，kubelet 会在这个 unit 下一次重启时用上你的构建；
- 之前已经启动的长生命周期进程（网络 pin 和 control 进程、容器 supervisor）会继续运行它们启动时用的那份代码，所以有一段时间会是两个构建并存地跑。

先检查一下：

```bash
delonix container ls -a; delonix vm ls
ls ~/.config/systemd/user/delonix-boot-* /etc/systemd/system/delonix-* 2>/dev/null
pgrep -a delonix
```

### 准备主机

`scripts/install.sh` 做的是两件互相独立的事。只有第一件是关于二进制程序本身的：

| 部分 | 它做什么 | 跳过或启用它的标志 |
|---|---|---|
| 二进制程序 | 下载一个发布版，验证 minisign 签名和 sha256，安装 `delonix`（加上 `delonix-mcp`、`delonix-mgmt`，以及带 `--with-cri` 时的 `delonix-cri`），然后是补全、man 手册页、编辑器语法和编辑器扩展 | 用 `--no-binary` 跳过；`--user` 选择 `~/.local/bin` |
| 主机软件包 | `slirp4netns`、`uidmap`、`nftables`、`iproute2`、`conntrack` | 总是执行 |
| Rootless 身份 | 为你的用户设置 `/etc/subuid` 和 `/etc/subgid` 的区间 | 总是执行 |
| AppArmor | 当用户命名空间限制启用时，为 `<目录>/delonix` 写 profile | 总是执行（限制开启时） |
| 旧版 Debian | 当 `kernel.unprivileged_userns_clone` 为 `0` 时把它设为 `1` | 总是执行（需要时） |
| 虚拟机依赖 | libvirt、qemu、cloud-init 工具链；Cloud Hypervisor 及其固件从上游下载 | 用 `--no-vm` 跳过 |
| 默认 VM provider | 带 `defaultProvider: libvirt` 的 `providers.yaml`（ADR-0054）：`/etc/delonix/`，或配合 `--user` 写到 `~/.config/delonix/`；仅在不存在时写入，从不改写 | `--vm-provider cloud-hypervisor` 改变默认值；用 `--no-vm` 跳过 |
| 内核调优 | `/etc/modules-load.d/delonix.conf`、`/etc/sysctl.d/99-delonix.conf` | 用 `--no-tune` 跳过 |
| cgroup 委派 | `user@.service` 的 drop-in，只在尚未被委派时才写 | 用 `--no-delegate` 跳过 |
| 加速器 | NVIDIA CDI 和 `render` 组，只在存在 GPU 时才配置 | 用 `--no-gpu` 跳过 |
| 可选启用项（opt-in） | 低于 1024 的端口（`--low-ports`）、虚拟机镜像构建（`--with-image-build`）、规模调优（`--production`） | 默认关闭 |

要在**不下载任何 Delonix 发布版**的情况下为你自己的构建准备一台主机，就从你的 checkout 里带上 `--no-binary` 运行安装脚本：

```bash
bash scripts/install.sh --no-binary            # add --no-vm if you do not need VM dependencies
bash scripts/install.sh --help                 # the full flag list, from the script header
```

加上 `--no-binary` 时，AppArmor profile 会为 `command -v delonix` 找到的那个
`delonix` 所在的目录而写（找不到时就是 `/usr/local/bin`）——在一台有已安装发布版的机器上，上面那条提醒同样适用。脚本在做主机相关的步骤时会用 `sudo`。

然后问问这个二进制程序，主机是否已经就绪（只读）：

```bash
delonix system doctor     # every prerequisite, and how to fix each; --strict exits non-zero on a failure
delonix system info       # state root, rootless, cgroup delegation, network infra
```

每项检查的含义，见[诊断主机](environment.md#diagnosing-the-host)。

### 使用一个隔离的状态根目录

一个已安装的构建默认会用你**真实**的状态根目录：和发布版一样的容器、网络和卷。先导出 `DELONIX_ROOT` 和 `DELONIX_NET_RUNTIME_DIR`（见
[隔离引擎的状态](#isolating-the-engines-state)），你的构建还会读取的其他每一个变量，见[环境变量](environment-variables.md)。

### 卸载与回滚

`install.sh` 里没有卸载标志。把你拷贝过去的东西删掉：

```bash
rm -f ~/.local/bin/delonix ~/.local/bin/delonix-cri ~/.local/bin/delonix-mgmt ~/.local/bin/delonix-mcp
hash -r
sudo apparmor_parser -R /etc/apparmor.d/delonix-dev && sudo rm /etc/apparmor.d/delonix-dev   # if you added it
```

要回到一个已发布的二进制程序，再跑一遍安装脚本；它会把安装目录里的二进制程序替换成你指定的那个发布版：

```bash
curl -fsSL https://github.com/angolardevops/delonix-runtime/releases/latest/download/install.sh | bash -s -- --user
curl -fsSL https://github.com/angolardevops/delonix-runtime/releases/latest/download/install.sh | bash -s -- --version vX.Y.Z
```

你手动生成的补全文件和 man 手册页，这两步都不会删除。最后用 `delonix --version` 确认一下你回到的是哪个 commit。

## 运行测试

```bash
cargo test --workspace                       # the whole suite
cargo test -p delonix-sdn                    # one crate
cargo test -p delonix-stack -- reconcile       # tests whose path contains "reconcile"
cargo test -p delonix-stack -- --exact kinds::tests::nenhum_kind_aparece_duas_vezes
```

需要特权或者一台真实主机的测试，会自己跳过而不是失败，所以这套测试在笔记本和 CI 上都有意义。少数现场（live）测试标了 `#[ignore]`，并在它们的 doc comment 里写出了运行它们的命令（比如在 `crates/adapters/delonix-vm/src/lib.rs` 里）；只在你自己拥有的机器上运行这些测试：

```bash
cargo test -p <crate> -- --ignored <test-name>
```

一次绿色的 `cargo test` 证明的是纯逻辑没问题。它**不能**证明一次对命名空间、cgroup、网络 holder 或虚拟机启动的改动是有效的——那需要一次现场运行（见
[端到端测试](#end-to-end-battery-scriptse2esh)和
[chaos 测试装置](#chaos-harness-scriptschaossh)）。

## CI 运行的门禁

<!-- dev-docs:begin ci-gates -->
| CI 作业 | 检查内容 |
|---|---|
| `fmt` | rustfmt |
| `lang` | lang ratchet |
| `arch` | arch fitness |
| `contract` | contract gate |
| `version` | version gate |
| `cli-surface` | cli surface |
| `clippy` | clippy -D warnings |
| `test` | test |
| `test-arm64` | test (arm64) |
| `deny` | cargo-deny |
| `fuzz` | fuzz (60s smoke, per target) |
| `script-tests` | script tests (Python gates) |
| `perf-probe` | perf probe (environment and bench) |
| `perf` | perf gate (regression against the baseline) |
| `release-verify` | release verify |
| `docs` | generated docs and valid examples |
<!-- dev-docs:end ci-gates -->

`.github/workflows/ci.yml` 里的每一个任务都能在本地复现。推送之前，先跑跟你改动相关的那些；请求评审之前，把它们全部跑一遍。

你可以在还没搞懂背后规则之前就先跑一个门禁；它的失败信息会说明要改什么。这些规则会在课程后面讲到：层、依赖方向和债务棘轮在
[架构](architecture.md#layers-and-the-allowed-direction)里；节点契约在
[架构](architecture.md#one-set-of-operations-several-interfaces)里；LANG-01 和版本对齐在[贡献工作流](contributing-workflow.md#language-english-in-the-code-lang-01)里。

| Job | 本地命令 | 何时失败 |
|---|---|---|
| `fmt` | `cargo fmt --all --check` | 代码没有按 rustfmt 格式化（默认配置） |
| `lang` | `python3 scripts/lang_ratchet.py` | 葡萄牙语的标识符、注释或消息**增加了**——或者减少了却没有在同一个 commit 里下调 `scripts/lang_baseline.json`（`--list` 显示它们，`--update` 下调基线） |
| `arch` | `python3 scripts/arch_fitness.py` | 某条依赖违反了层的方向、某个 crate 放错了目录、某个成员 crate 固定了依赖版本号、代码里出现了某个消费者的名字，或者某个债务棘轮变动了（`--list`、`--update`） |
| `arch` | `python3 scripts/dev_docs.py --check` | `docs/dev/` 里某个生成出来的事实过时了——运行 `python3 scripts/dev_docs.py` 并提交 |
| `contract` | `python3 scripts/contract_gate.py` | `proto/delonix/node/v1` 里的节点契约不满足 `buf format` 的干净度、没通过 `buf lint`、破坏了与上一个 tag 的兼容性、缺少某个 HTTP 映射，或者 `docs/api/openapi.yaml` 不是生成出来的那一份（`--update` 会重写它）。需要 `PATH` 上有 `protoc`、`buf` v1.73.0 和 `protoc-gen-openapi` v0.7.1，以及各个 tag |
| `version` | `python3 scripts/version_gate.py` | workspace 的版本号不是这个 commit 所包含的最新 tag（见[贡献工作流](contributing-workflow.md#version-alignment)），或者这个分支不包含最新的 tag。需要各个 tag |
| `cli-surface` | `cargo build --release -p delonix-runtime-bin && scripts/cli-tree.sh --gate` | 新增、移除或重新归类了一个 CLI 叶子命令，却没有在同一个 commit 里更新 `scripts/cli_baseline.tsv`（`scripts/cli-tree.sh --update`） |
| `cli-surface` | `python3 scripts/docs_cli_gate.py` | 当前文档里引用的某条 `delonix …` 命令，在这个二进制程序的命令树里并不存在 |
| `clippy` | `cargo clippy --workspace --all-targets --locked -- -D warnings` | 任何警告 |
| `test` | `cargo build --workspace --locked && cargo test --workspace --locked --no-fail-fast` | 任何一个测试失败 |
| `deny` | `cargo deny check advisories licenses sources` | 一条 RUSTSEC 安全公告、一个不被允许的许可证或来源（`deny.toml`） |
| `docs` | `cargo build --release -p delonix-runtime-bin && python3 docs/gen.py && git diff --exit-code -- docs/` | 已提交的站点内容，和这个二进制程序生成器产出的内容不一致 |
| `docs` | `./target/release/delonix stack apply -f examples/<file>.yaml --dry-run` 与 `./target/release/delonix stack validate -f examples/<file>.yaml` | 某个已发布的示例用了一种已废弃的写法，或者含有未解析的引用 |

`cli-tree.sh` 和 `docs_cli_gate.py` 是从二进制程序真实的 `--help` 里读出命令树的；设置
`DELONIX_BIN=/path/to/delonix` 来选择用哪个二进制程序。`docs/gen.py` 默认用
`target/release/delonix`，还需要 Python 的 `markdown` 模块。`docs` 这个任务还会生成 man 手册页（`delonix man --dir <dir> --index`），并用 `groff -mandoc -ww -z` 检查它们。

另外几个独立的工作流，不是每次改动都要求跑：`chaos.yml` 在一个干净的 runner 上运行 chaos 测试装置（当 runner 屏蔽用户命名空间时会报告 `skipped`），
`release.yml` 发布一个 tag，`vm-image.yml` / `vm-appliances.yml` 构建虚拟机镜像。

## 隔离引擎的状态

除了 `--help` 之外的任何东西都会触碰引擎的状态。默认情况下，那就是**你真实的状态**：你的容器、网络、卷，以及网络 holder。在为了测试而运行引擎之前——不管是手动运行、通过 `e2e.sh`，还是通过任何脚本——把**两个**状态根目录都指向一个临时目录：

```bash
export DELONIX_ROOT=$HOME/scratch/dlx/root          # containers, images, networks, IPAM, volumes
export DELONIX_NET_RUNTIME_DIR=/tmp/dlx-run          # the holder's control and slirp sockets
mkdir -p "$DELONIX_ROOT" "$DELONIX_NET_RUNTIME_DIR"
```

**两个都要设置，而且每次都要。半吊子的隔离比完全不隔离还糟糕。** 网络套接字和 pid 文件是分开解析的：pid 文件放在*状态根目录*下，holder 的 control 套接字和 slirp 套接字放在一个*运行时目录*里（默认是 `/tmp/delonix-net-<uid>`）。当两个状态根目录最终落到同一个运行时目录上时，各自都会读到自己（不存在）的 pid 文件，断定没有网络基础设施，然后在对方的套接字之上启动或拆掉基础设施。在一台跑着现场工作负载的开发主机上，这最后导致真实的根目录重建了它的网络基础设施，并重启了真实的容器。

现在，引擎会从一个非默认的 `DELONIX_ROOT` 为运行时目录派生出一个后缀
（`crates/adapters/delonix-sdn/src/infra.rs` 里的 `runtime_dir`/`root_suffix`），这在常见情况下堵上了这个碰撞的漏洞。但还是要两个都导出：这让隔离变得明确，让套接字路径保持简短、在你的掌控之内，而且这也是 `scripts/e2e.sh` 和
`scripts/chaos.sh` 的做法（e2e 会替你补上你没导出的那个变量）。

让 `DELONIX_NET_RUNTIME_DIR` 保持简短：一个长度超过约 108 字节的 unix 套接字路径，会以 `path must be shorter than SUN_LEN` 失败。`e2e.sh` 会拒绝一个长度超过 80 字节的运行时目录。

结束之后，在导出同样这两个变量的情况下，拆掉这套隔离出来的网络基础设施：

```bash
./target/debug/delonix net netns down
```

## 虚拟机镜像配方（`scripts/verify-images.sh`）

`images/` 里的配方（recipe）用两种方式来检查。CLI crate 里的一个单元测试
（`vmspec::every_shipped_recipe_is_valid_and_complete`）会在某个配方解析失败、或者指向了一个不存在的文件或 builder 时失败。`scripts/verify-images.sh` 走得更远：它在一个隔离的 `DELONIX_ROOT` 里离线构建这四个云镜像发行版，再把构建出来的 qcow2 读回来，对照配方声明的内容做核对；`--self-test` 证明这些检查在一个没人构建过的镜像上确实能失败。它需要
`libguestfs-tools`（见[构建微虚拟机](microvm-setup.md)），不属于 CI 门禁的一部分。
`--packages`、`--profile`、`--boot` 和 `--appliance` 这几个阶段是存在的，但在 v4.2.0 发布时还没有被运行过。

## 端到端测试（`scripts/e2e.sh`）

`e2e.sh` 让 CLI 对着真实的内核跑：每一个叶子命令的 `--help`，加上对其中一大部分命令面的真实执行，最后打印一份 PASS/FAIL/SKIP/XFAIL 报告（详细的 JSONL 在
`$OUT/results.jsonl` 里，默认 `OUT=/tmp/delonix-e2e`）。

```bash
./scripts/e2e.sh                          # uses ./target/debug/delonix
./scripts/e2e.sh ./target/release/delonix
```

- 它**默认会自我隔离**：会把 `DELONIX_ROOT` 和 `DELONIX_NET_RUNTIME_DIR` 设成它自己的目录（除非你先把这两个都导出了），并在结束时拆掉它自己启动的基础设施。
  `E2E_SHARED_STATE=1` 会对着真实的机器状态跑——只用于诊断一台主机。
- 当某项检查失败时，退出码是非零的；当一项标记为已知缺陷（`XFAIL`）的检查意外地通过了，退出码同样是非零的。SKIP 不会让这次运行失败，但会被单独列在它自己的一个区块里：一项被跳过的检查什么都没证明。
- 它需要网络访问来拉取镜像；前置条件不满足的那些部分，会带着原因被跳过。
- 一次绿色的运行，意味着每一个叶子命令的 `--help` 都被验证过了，并且*一部分*叶子命令被真的执行了。要知道哪些被执行、哪些没有，读这个脚本的开头部分。

## Chaos 测试装置（`scripts/chaos.sh`）

这个 chaos 测试装置会故意把一个正在运行的引擎弄坏——杀掉 holder、把磁盘写满、并发的 attach、只做了一半的 apply——然后报告它退化的方式是否和它承诺的一致。

```bash
scripts/chaos.sh                        # every scenario, ./target/debug/delonix
scripts/chaos.sh holder_kill oom        # selected scenarios
scripts/chaos.sh --keep scale           # leave the sandbox up for a post-mortem
scripts/chaos.sh --clean                # tear the kept sandbox down
```

- 它总是把两个根目录都重定向到它自己的沙箱里（`DELONIX_CHAOS_DIR`，默认
  `/tmp/dlx-chaos`），绝不触碰真实引擎的容器、网络或记录。镜像相关的目录
  （`images`、`layers`、`blobs`）是**指向你真实 store 的符号链接**，为的是避免重复下载：实际上这个测试装置只会读它们，但如果某个场景写入了一个镜像，那就会写到真实的 store 里。
- 它**拒绝在一台繁忙的机器上运行**（负载超过一个阈值，这个阈值通过
  `scripts/bancada.sh` 与 `scripts/bench.sh` 共用）：在高负载下，场景失败的原因属于那台跑测试的机器，而不属于产品本身。`--max-load N` 改变这个阈值；
  `--force` 会照样运行，并把结论标记为不可发布。
- 只有在没有任何场景失败时，退出码才是 0。SKIP 会被单独列出来。
- 有些场景需要外部资源，没有这些资源就会跳过（比如 `truenas_destroy` 需要
  `DELONIX_CHAOS_TRUENAS_URL`/`_USER`/`_PASS`）。

对这些用完即弃的沙箱来说，`/tmp` 下的临时目录没有问题。你的 **worktree**
则不行——见[贡献工作流](contributing-workflow.md#one-worktree-per-task)。

---

**下一步：** [项目结构](project-structure.md)——这个仓库的地图：每个目录是什么、谁在改动它，以及什么是生成出来的。
