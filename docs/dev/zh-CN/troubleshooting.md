<!-- translated-from: troubleshooting.md sha256:47d1c5984d20cc95f303cbc9d9579447fa3fec9218d0e0c4f6e052daf1781f74 -->
# 排查问题

**阅读前须知：** [准备你的环境](environment.md)（宿主机的那些陷阱）和[克隆、构建与测试](build-and-test.md#the-gates-ci-runs)（各个门禁，以及如何隔离引擎的状态）。

这一页是按**症状**建立的索引：某个门禁或某次实机运行打印出来的那段原文，它实际上是什么意思，以及怎么修。它不会重新讲一遍[准备你的环境](environment.md)和[克隆、构建与测试](build-and-test.md)已经深入讲过的内容——而是直接指向对应的那一节，这样一个刚克隆下来的仓库才能从"我卡住了"直接跳到"我知道该看哪一节"，不必先从头到尾读完那两页。如果你遇到的症状不在这里，[从这里开始 § 当你卡住时](start-here.md#when-you-are-stuck)是下一个该看的地方——这一页只是一个捷径，专门针对*工具本身已经把哪里错了告诉你了*、只是你还没认出那段文字这种具体情况。

## 快速索引

| 你看到了 | 它是什么 | 修复方法在 |
|---|---|---|
| `FAIL <name>: N (baseline M) — new debt entered` | `scripts/arch_fitness.py` | [某个架构棘轮动了](#a-debt-ratchet-moved-arch_fitnesspy) |
| `FALHA <kind>: N > M — entrou português novo.` | `scripts/lang_ratchet.py` | [语言棘轮](#new-portuguese-entered-lang_ratchetpy) |
| `<name> (<layer>) → <name> (<layer>): forbidden direction` | `scripts/arch_fitness.py` | [某个依赖违反了层级方向](#a-dependency-goes-against-the-layer-direction) |
| `<file>:<n>: names a consumer (…) — the engine knows none` | `scripts/arch_fitness.py` | [某个消费者的名字混进了引擎](#a-consumer-name-leaked-into-the-engine) |
| `FAIL <tag> is published but this commit does not contain it` | `scripts/version_gate.py` | [你的分支比最新的 tag 还旧](#the-version-gate-refuses-your-branch) |
| `FAIL Cargo.toml says X, above Y, and docs/releases/vX.md does not exist` | `scripts/version_gate.py` | [升了版本号却没有对应的发布提交](#the-version-gate-refuses-your-branch) |
| `FAIL  buf format` / `buf lint` / `buf breaking against …` / `has no google.api.http mapping` / `openapi.yaml is not the generated one` | `scripts/contract_gate.py` | [节点契约门禁](#the-node-contract-gate) |
| `unshare()` 失败，`EPERM` | AppArmor + user namespace | [准备你的环境 § AppArmor](environment.md#ubuntu-2310-apparmor-blocks-user-namespaces-for-your-dev-binary) |
| `-m`/`--cpus`/`--cpu-weight` 被拒绝，退出码 `69` | cgroup 委派 | [准备你的环境 § cgroup 委派](environment.md#cgroup-delegation-some-limits-are-refused-others-are-not-enforced) |
| `path must be shorter than SUN_LEN` | `DELONIX_NET_RUNTIME_DIR` 太长 | [克隆、构建与测试 § 隔离引擎的状态](build-and-test.md#isolating-the-engines-state) |
| 某个门禁针对一份你根本没写过的 diff 失败了，或者一次构建结束得快得可疑 | 一个共享或过期的 `CARGO_TARGET_DIR` | [共享或过期的构建缓存](#a-shared-or-stale-build-cache) |
| 二进制返回的是旧版本，或者一个根本不存在的命令 | `PATH` 上有一个过期的 `delonix` | [准备你的环境 § 过期的 PATH](environment.md#a-stale-delonix-on-your-path) |

## 消息里自带原因的 CI 门禁

每个门禁都会说明该修什么——下面这张表只是为了让你在仔细读之前先认出消息的*样子*。从[克隆、构建与测试 § CI 会跑的那些门禁](build-and-test.md#the-gates-ci-runs)里找到对应的本地命令，就能在不等 CI 的情况下重现下面任何一种情况。

### 新的葡萄牙语混进来了（`lang_ratchet.py`）

```
FALHA identifiers: 1051 > 1050 — entrou português novo.
       `python3 scripts/lang_ratchet.py --list --only identifiers` mostra onde.
```

（这个门禁自己的消息是用葡萄牙语写的——LANG-01 管的是*代码*，不是这个脚本；见[贡献流程 § 语言](contributing-workflow.md#language-english-in-the-code-lang-01)。）
葡萄牙语标识符、注释或面向用户消息的计数上升了。`--list --only <kind>` 会点出新增的那些行。如果你*翻译*了什么东西，结果计数反而**下降**了，却没有把基线一起降下来，消息会是镜像版本（"traduziste, mas não baixaste a linha de base"）——运行 `python3 scripts/lang_ratchet.py --update`，并把 `scripts/lang_baseline.json` 和这次翻译放进同一个提交。

### 某个架构棘轮动了（`arch_fitness.py`）

```
FAIL  self_exec_sites: 6 (baseline 5) — new debt entered
```

五个被追踪的数字之一（`self_exec_sites`、`library_prints`、`env_writes`、`shared_error_imports`、`raw_error_variant_matches`）上升了。`python3 scripts/arch_fitness.py --list` 会打印出每一个被计入统计的地方，并在旁边附上对这个计数含义的简短说明——每一个具体是什么，见[贡献流程 § 门禁强制执行的架构规则](contributing-workflow.md#architecture-rules-the-gates-enforce)。和语言棘轮一样，当计数**下降**、却没有在同一个提交里让基线一起动的时候，也会出现同样形状的消息（"debt was paid; lower the baseline … `--update`"）。

### 某个依赖违反了层级方向

```
FAIL  delonix-oci (adapters) → delonix-cri (interfaces): forbidden direction
```

某个 crate 从一个它不被允许依赖的层导入了另一个 crate——谁可以依赖谁，见[架构 § 层与允许的方向](architecture.md#layers-and-the-allowed-direction)里的那张表。要么是这个依赖本身就是错的（多数情况都是这样——一个适配器根本没理由依赖一个接口），要么这个改动确实需要在 `scripts/arch_fitness.py` 内的 `EXCEPTIONS` 里声明一个分阶段的例外，而门禁本身会拒绝接受一个没有附带移除阶段和理由的例外。

### 某个消费者的名字混进了引擎

```
FAIL  crates/adapters/delonix-oci/src/registry.rs:42: names a consumer ('SomeControlPlaneName') — the engine knows none
```

引擎的那条基本边界——*「引擎不认识任何消费者」*，[`AGENTS.md`](../../../AGENTS.md) 顶部的那一节——是靠 grep 强制执行的，不只是靠评审。任何使用这个引擎的平台、control plane、控制台或 agent 的名字，只要出现在 `crates/`、`bins/` 或 `proto/` 下的代码里**或注释里**，都会触发这条检查。把这个需求泛化成它实际所代表的那种能力，用引擎自己的词汇表达出来；需要用到那个外部名字的历史背景，属于 `docs/`，永远不属于代码。

### 版本门禁拒绝了你的分支

```
FAIL  v1.4.2 is published but this commit does not contain it (newest contained: v1.4.0) —
      merge origin/main first; merging a branch that predates a release undoes what it shipped
```

你的分支是在某个后来已经发布的版本之前开始的。`git fetch --tags origin && git merge origin/main`（这个仓库用的是合并，不会把功能分支 rebase 到某个发布上——为什么你自己的提交历史依然要求是线性的，见[贡献流程 § 一个任务一个 worktree](contributing-workflow.md#one-worktree-per-task)）。

```
FAIL  Cargo.toml says 1.5.0, above 1.4.2, and docs/releases/v1.5.0.md does not exist —
      a bump belongs only to the release commit
```

你在 `Cargo.toml` 里把 `version` 往上调了。不要这样做——这种事只能在发布提交里做，并且要和发布说明文件一起提交。把这次调整撤回去；见[发布与稳定性 § 版本门禁](releases-and-stability.md#the-version-gate)。

### 节点契约门禁

`scripts/contract_gate.py` 包裹了五项相互独立的检查，每一项都会打印自己的 `FAIL` 行——在本地跑一遍是最快看出五项里到底是哪一项出了问题的办法：

```
FAIL  buf format — run `buf format -w proto`
FAIL  buf lint
FAIL  buf breaking against v1.4.0
<buf's own stdout/stderr follows, naming the field or RPC that changed incompatibly>
FAIL  node.proto: SomeRpc has no google.api.http mapping
FAIL  docs/api/openapi.yaml is not the generated one — run `python3 scripts/contract_gate.py --update` and commit it
```

它需要 `PATH` 上有 `protoc`、`buf`（钉在 v1.73.0）和 `protoc-gen-openapi`（钉在 v0.7.1），还需要拉取到 git 的 tag——缺工具会失败在更常见的 "command not found" 上，但缺 tag 会让 `buf breaking` 那项检查打印出 `ok`，同时明确说明目前还没有可比较的基线，而不是悄悄跳过这项检查。对 `proto/delonix/node/v1` 的一次真正的破坏性改动，需要先有一份 ADR，就像对任何一个稳定节点契约的改动一样——见[贡献流程 § 什么时候要写 ADR](contributing-workflow.md#when-to-write-an-adr)。如果实际改变的只是生成器的输出（一个新字段、一个新 RPC），`python3 scripts/contract_gate.py --update` 会重新生成 `docs/api/openapi.yaml`；把它和 `.proto` 的改动放进同一个提交里一起提交。

## 共享或过期的构建缓存

[克隆、构建与测试 § 构建](build-and-test.md#build)里已经点明了这个取舍：让几个 worktree 共用同一个 `CARGO_TARGET_DIR` 能省磁盘，但两个同时对它进行构建的进程会互相等待，还**可能互相覆盖对方的产物**。这个症状很特殊，也很容易被误读成一次真正的失败：某个门禁（尤其是 `test` 或 `clippy`）针对一段看起来和你的改动毫无关系的代码失败了，或者一次构建结束得快得可疑，随后产出的那个二进制却表现得像一个更老的版本——包括某个本地的 pre-commit 或 pre-push 钩子，跨会话复用同一个共享的 target 目录，链接的时候用的是当时恰好躺在那里的、来自另一次并发构建的目标文件。

这不是门禁本身的 bug：它确确实实是在构建错误的东西。在你开始调试这个"失败"本身之前，先排除是不是缓存过期：

```bash
cargo clean -p delonix-runtime-bin   # or the crate the failure points at
cargo build -p delonix-runtime-bin   # rebuild clean, then re-run the gate that failed
```

如果你经常同时从好几个 worktree 里工作，给每一个都配上自己的 `CARGO_TARGET_DIR`（取消共享的那个环境变量，或者导出一个 worktree 本地的路径），就能把这一整类症状彻底消除，代价是每个 worktree 第一次构建会慢一些——这正是[克隆、构建与测试](build-and-test.md#build)里已经说明过的那个取舍。

---

**下一篇：** [名字是如何抵达 `/etc/hosts` 的](service-names-and-hosts.md) —— 发布服务名和路由 host 的那个模块，以及它为什么会拒绝。然后是[编码规范](coding-conventions.md) —— 本仓库里的代码必须怎么写，每条规则都标注了它背后的门禁或决定。
