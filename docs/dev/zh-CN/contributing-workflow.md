<!-- translated-from: contributing-workflow.md sha256:7a27cf21c3440859aaa70b55740a2566cfff13b0f365c965900c98be188a10db -->
# 贡献流程

**阅读之前：**[克隆、构建与测试](build-and-test.md#the-gates-ci-runs)（门禁部分）和
[编码规范](coding-conventions.md)（评审者会检查的东西）。

本页是手册里「我们怎么干活」的部分：改动该往哪里放、门禁强制的是哪些规则、为什么要有
这些规则、什么时候一个改动需要先有一份书面决定，以及怎么把它发出去。门禁本身，以及怎么跑
它们，在 [克隆、构建与测试](build-and-test.md) 里。读完本页，你就能把一个改动从一个 issue
带到一个被合并的 pull request，中途既不弄坏任何门禁，也不破坏另一个会话的工作。

对任何不那么琐碎的事 —— 一个新命令、一个新的清单 Kind、对命名空间或 cgroup 设置的改动、
一个新后端 —— 先开一个 issue，把方案谈妥。这能省掉一次重写。

## 从最新的 tag 出发，别靠记忆

写代码之前：

```bash
git fetch --tags origin
git describe --tags --abbrev=0 origin/main          # the newest release
git log --oneline "$(git describe --tags --abbrev=0 origin/main)"..origin/main | wc -l   # how far main is past it
git log --oneline -- <path you will touch>          # what was already decided, fixed or removed there
```

读一读你要动的那块区域的历史，不是走形式：这个代码库有很大一部分，就是尝试过、测量过、
改动过的东西的记录。已经决定过或者已经移除的东西，不会因为不知情就被重做一遍。完整的记录
在 [`AGENTS.md`](../../AGENTS.md)（按区域组织）和 [`docs/adr/`](../adr/README.md) 里。

## 一个任务一个 worktree

常常有好几个人和工具同时在同一个 clone 上工作。在一个共享的 checkout 里编辑，已经在这里
真真切切地造成过损失：改动被吸收进别人的提交、`HEAD` 在一个任务进行到一半时切换了分支，
还有在一棵脏的树上跑出一个绿色的 `cargo test`，却什么都没证明 `HEAD` 是能编译的。所以每个
任务都会有自己的 git worktree 和分支，从 `origin/main` 创建出来：

```bash
git fetch origin
git worktree add -b <topic>/<task> <workspace>/.worktrees/delonix-runtime/<task> origin/main
cd <workspace>/.worktrees/delonix-runtime/<task>
```

- **永远不要把 worktree 放进 `/tmp`。** 很多系统会在启动时清空 `/tmp`；任务进行到一半
  重启，会带走还没提交的工作，还可能在共享的 `.git` 里留下写了一半的对象。用仓库外面一个
  持久化的目录（这里的惯例是在各个仓库旁边放一个 `.worktrees/` 目录），这样任何
  `grep -r`、`docker build` 的上下文，或者门禁脚本都不会捡到它。
- **尽早提交、尽早推送**，在每一步通过检查之后就做。一个持久化的 worktree 能扛过一次
  重启；一个已经推送的分支能扛过其余的一切。
- **按名字暂存文件**：`git add <file> <file>`，绝不用 `git add -A`、`-u` 或 `.`。提交
  之前先查一下 `git branch --show-current`，并确认 `git status --short` 里的每一项都是你
  自己的改动。
- **绝不要在别人可能正在用的树上执行 `git checkout -- <path>`**：它会不做 stash 就还原
  到 `HEAD`，把对方还没提交的工作毁掉。
- 推送因为分叉被拒绝时，**用 rebase，不要用 merge**：`git pull --rebase`。这里的历史
  是线性的。
- **任务做完之后，把两样东西都删掉**：worktree 和分支 —— 分支不会随着
  `worktree remove` 一起消失，过期分支就是这样堆起来的：

  ```bash
  git worktree remove <path>
  git branch -D <topic>/<task>
  git worktree list
  ```

## 版本对齐

根目录 `Cargo.toml` 里的 `version` 是承重的：它决定 `delonix-cri` 从哪个发行版下载，
会被报告成 Docker API 的 `ServerVersion`，会被记录进每一份备份，发布 workflow 也会拿它跟
构建出来的二进制程序比对。`scripts/version_gate.py`（CI 的 `version` job）只允许恰好两种
状态：

1. **和这个提交所包含的最新 tag 相等** —— 所有日常工作都是这样。不要在一个功能 PR 里
   升版本号，也不要用 `-dev` 后缀（那会把 `delonix-cri` 的下载指向一个不存在的发行版）。
2. **更大，且只能在发布提交里**，同时要有 `docs/releases/v<version>.md`。

它还会拒绝一个**不包含最新 tag** 的分支：这个分支是在那次发布之前就开始的，照原样合并会
撤销那次发布已经发出去的东西。要先 rebase 到 `origin/main` 上。

在两次发布之间，`delonix --version` 靠提交号和距离来区分不同的构建
（`commit: <hash> (+N commits since vX.Y.Z)`），因为两个版本号相同的构建，不一定是同一个
构建。

## 语言：代码用英文（LANG-01）

标识符、注释和面向用户的消息都用**英文**写。葡萄牙语只通过翻译目录到达操作者：

- `bins/delonix-runtime-bin/src/cmd/po.rs` —— 固定字符串用 `po::t("…")`，需要插值的用
  `po::tf("… {name} …", &[("name", value)])`（用命名占位符，因为一份翻译可能会把它们的
  顺序调换）。CLI 的 `--help` 文本由 `po::translate_help` 在运行时翻译。
- `bins/delonix-runtime-bin/data/pt.po` —— 葡萄牙语的条目，嵌在二进制程序里，用
  `--l18n pt` 或者 `DELONIX_L18N=pt` 来选用。

缺失的目录条目会退化成英文；一段直接写在代码里的葡萄牙语字符串是一个 bug。
`scripts/lang_ratchet.py`（CI 的 `lang` job）会统计标识符、注释和消息里还剩多少葡萄牙语，
拿去跟 `scripts/lang_baseline.json` 比对。它是一个**棘轮**，不是一个上限：数字上升（进来了
新的葡萄牙语）会失败，数字下降却没有同步调低基线，**同样**会失败。翻译了什么东西之后，运行
`python3 scripts/lang_ratchet.py --update`，并把新的基线和翻译**放在同一个提交**里。CLI
crate 里的测试还会检查命令的 help 是否有葡萄牙语条目 —— 加一个新命令或 flag 时记得加上。

## 门禁强制执行的架构规则

`scripts/arch_fitness.py`（CI 的 `arch` job）强制执行
[ADR-0040](../adr/0040-engine-restructuring-layers-ports-node-contract.md) 里定下来的结构。
每个 crate 所在的层，以及允许的依赖方向，列在[架构](architecture.md)里。这对一次改动意味着：

- **引擎不认识任何消费者。** 在 `crates/`、`bins/`、`proto/` 或者清单里，包括注释在内，
  不能出现任何使用这个引擎的产品、平台、control plane、控制台或 agent 的名字 —— 也不能
  出现租户、账户、方案或计费。一个来自消费者的需求，要以它本质上是什么通用能力来写，用引擎
  自己的词汇，而且只有在对任何客户都说得通时才能进来。需要提到外部名字的历史，放在
  `docs/` 里，绝不放进代码。
- **依赖指向内部。** 基础层只依赖基础层；上下文不依赖适配器；适配器和 provider 不依赖
  接口；一个二进制程序只组合**一个**接口。
- **目录就是层。** 一个 crate 位于 `crates/<layer>/` 下面，要和它在 `LAYERS` 表里的
  条目对上。一个新 crate，要在同一个提交里同时进入 `LAYERS` 和正确的目录。
- **依赖的版本号只活在根目录的** `[workspace.dependencies]` 里；成员 crate 只写
  `{ workspace = true, features = [...] }`，别的什么都不写。
- **例外要点名是哪个阶段会移除它。** 一个没有阶段的例外会失败，一个已经不再适用的例外
  也一样会失败。
- **债务棘轮** —— 比如 `self_exec_sites`（一个库重新执行引擎自己的二进制程序，而不是
  调用一个函数）、`library_prints`（在库 crate 里用 `println!`/`eprintln!` —— 库应该
  发出 `tracing`，打印是接口该干的事）、`env_writes`（`env::set_var`/`remove_var`），以及
  `shared_error_imports`（一个 adapter 或 provider 把共享的 `Error` 当成自己的类型用，
  而不是用一个能转换成它的、crate 自己的错误类型）。目前的清单是生成出来的：

<!-- dev-docs:begin ratchets -->
`scripts/arch_fitness.py` 维护 **5 个债务棘轮（ratchet）**（基线在 `scripts/arch_baseline.json`）：

- `self_exec_sites`
- `library_prints`
- `env_writes`
- `shared_error_imports`
- `raw_error_variant_matches`
<!-- dev-docs:end ratchets -->
语义和语言棘轮一样（`--list`、`--update`）。

除了门禁能看见的东西之外，还有三条原则决定评审的走向：**daemonless（无守护进程）**
（一个新的常驻进程需要一份 ADR，附上 systemd unit、定时器或 socket 激活为什么做不到的证据）、
**rootless-first（无特权优先）**（特权是一个明确的、被公开宣布的可选项，绝不是一个悄悄的
默认值），以及**没有静默失败**（一个被接受却被忽略的 flag，比一个根本不存在的 flag 更糟糕
—— 应该用一个清晰的错误去拒绝它）。

## 什么时候要写 ADR

当一个改动会挪动一道结构性边界时，**先**在 `docs/adr/` 里写一份 Architecture Decision
Record，再动代码，比如：

- 一个新后端或 provider（一个虚拟化程序、一个存储系统），或者一个新端口；
- 引擎 crate 里的一个新外部依赖，或者一个新的守护进程/常驻进程；
- 一道新的特权边界 —— 这还需要先做一次 GO/NO-GO 的 spike；
- 对节点契约、清单 schema 的稳定性，或者分层结构的改动。

在已有边界之内的日常功能，不需要 ADR。格式和现有清单都在
[`docs/adr/README.md`](../adr/README.md) 里：一个决定一个文件，`NNNN-title.md`，用英文写。
**已被接受的 ADR 永远不会被重写** —— 只会被一份新的 ADR 取代。

## 新增或修改一个 CLI 命令

- **把每一个入口都接上。** 有几个命令能从不止一条路径到达（比如 `delonix vm pull` 和
  `delonix image vm pull`）。把它们全部改掉，然后用你构建出来的二进制程序逐一检查 ——
  包括 shell 补全，因为你改的那份 `clap` 声明，未必就是用户实际那条路径解析用的那份。
  补全引擎可以直接探测；`_CLAP_COMPLETE_INDEX` 是正在被补全的那个词的位置：

  ```bash
  COMPLETE=bash _CLAP_COMPLETE_INDEX=3 ./target/debug/delonix -- delonix image vm ''
  ```
- **对着二进制程序验证**，不要对着源码：`./target/debug/delonix <group> <command> --help`，
  再做一次把状态根隔离开的真实运行（见
  [克隆、构建与测试](build-and-test.md#isolating-the-engines-state)）。
- 新增或删除一个叶子命令时，在同一个提交里**更新 CLI 基线**
  （`scripts/cli-tree.sh --update`）；help 文本变了的话，就用一个 release 构建重新生成
  站点（`python3 docs/gen.py`）。
- **给每一个新的纯函数写单元测试** —— 解析器、校验器、参数构造函数。这个代码库有一长串
  真实 bug 正是在那里被抓到的记录。
- **给错误分类。** 退出码带着一个类别（未找到、冲突……），这个类别是从错误类型在同一处
  统一决定出来的；要返回正确的 `Error` variant，而不是一个笼统的。

## 提交与 pull request

- 一个提交对应一个逻辑改动；提交信息里要说清楚**为什么**这么改 —— diff 本身已经说明了
  改了什么。
- 有对应 issue 的话，要引用它。
- 针对 `main` 开 PR，并填好[模板](../../.github/PULL_REQUEST_TEMPLATE.md)：你跑过什么；
  如果涉及运行时、命名空间、cgroup 或网络相关的代码，还要写你在一台真实宿主机上**实机**
  跑过什么，带上命令和它的输出。
- 「能编译」和「命令返回了 0」都不能算一个改动的收尾。要说清楚证明了什么，同样明确地
  说清楚没有验证过什么、为什么没验证。
- 每一个改动都由 [`.github/CODEOWNERS`](../../.github/CODEOWNERS) 里列出的代码所有者
  评审。

## 涉及安全的改动

当一个改动跨越了特权或命名空间边界时 —— user 命名空间映射、网络 holder 或它的控制
套接字、`setns`/`unshare`、capability 或 seccomp 处理，或者由用户/清单输入驱动的路径
处理 —— 要在 PR 里明确点出来。这些会得到额外的评审。

如果你发现的是一个**漏洞**而不是一个 bug —— 提权、命名空间逃逸、命令注入、路径穿越 ——
不要开一个公开的 issue 或 PR。按照 [`SECURITY.md`](../../SECURITY.md) 的流程走（GitHub
私密漏洞报告）。

---

**下一篇：**[发布与稳定性](releases-and-stability.md) —— 推送一个 tag 会发生什么，以及
CLI 和清单 schema 承诺不破坏的是什么。
