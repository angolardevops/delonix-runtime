<!-- translated-from: engineering-process.md sha256:b765b1a8f9865fdcb1254f484027564885de6238b1bcaa4a3a234c48e63f1e21 -->
# 工程流程（权威）

本页是工作如何在本仓库中流转的**唯一真实来源（single source of truth）**：一次变更从哪个分支开始、
如何隔离构建、如何验证、如何落地，以及如何清理它留下的痕迹。AGENTS.md、[CONTRIBUTING.md](../../../CONTRIBUTING.md)、
[贡献流程](contributing-workflow.md)以及各会话技能（session skill）在分支模型和生命周期方面都以本页为准——
它们不会重述这些内容。当它们与本页不一致时，以本页为准；去修正另一处，而不是分叉出一条规则。

> **这是 Delonix 的决定，而非对其他项目的断言。** 下面的分支模型是依据所有者的决定（2026-10-23）而采用
> GitFlow 形态。Docker/Moby 和 Podman 是 PR 优先、基于主干（trunk-based）的项目；我们借鉴它们的*贡献*和
> *评审*纪律（合并前测试、对面向内核的变更做实际（live）验证、DCO 风格的聚焦提交、CODEOWNERS 评审）——
> 而非它们的分支拓扑。不要断言它们使用本流程。

## 两条长期存在的分支

- **`main`** 是已发布线。它只会因一次发布或一次热修复（hotfix）而向前移动。它的 `HEAD`
  就是从源码安装的用户所得到的内容。
- **`develop`** 是集成线。日常工作从它切出，并回落到它上面。一次发布会把 `develop` 提升进 `main`。

两者在使用前都会重新 fetch；绝不要从陈旧的本地引用切分支，也绝不要为了方便而从另一个任务的分支切出。

## 分支策略（强制）

按**问题的性质**分类，而不是按某人建议的前缀分类。前缀跟随分类而定；它不驱动分类。

| 工作类型                    | 分支前缀               | 起始于（最新远端）          | 正常目的地                    |
| --------------------------- | ---------------------- | -------------------------- | ----------------------------- |
| 功能（Feature）             | `feat/`                | `origin/develop`           | `develop`                     |
| 杂务 / 维护                 | `chore/`               | `origin/develop`           | `develop`                     |
| 重构                        | `refactor/`            | `origin/develop`           | `develop`                     |
| 性能                        | `perf/`                | `origin/develop`           | `develop`                     |
| 文档                        | `docs/`                | `origin/develop`           | `develop`                     |
| 测试 / CI / 构建            | `test/` `ci/` `build/` | `origin/develop`           | `develop`                     |
| **缺陷修复 / 热修复**       | `fix/` `bug/` `hotfix/`| **`origin/main`**          | **`develop`，并且当它影响已发布线时还有 `main`** |

名称要简短且具描述性，若存在任务标识符则带上它：
`feat/DX-1234-http-route-timeouts`、`fix/compose-secret-lazy-read`。

### fix/bug/hotfix 规则，及其唯一真正的陷阱

修复从 `main` 切出，因为修复在默认情况下是对已发布内容的纠正。同一个纠正还必须到达 `develop`，
以免下一次发布使它回归（regress）。

本策略所禁止的那个陷阱：**只存在于 `develop` 上的缺陷**。不要从 `main` 切出它（它在那里无法复现），
也不要把 `develop` 合并进 `main` 来让它复现。一个仅存在于 develop 的缺陷，应在一条从 `develop`
切出的分支上纠正，并且只落到 `develop` 上——在分类时明确记录这一条件。如果本策略曾阻碍一个
连贯的修复，请把具体冲突上报给所有者以作决定；不要悄悄地弯曲规则。

### 把一个修复带到两个目的地

当一个修复以已发布线为目标时，它通过发布门禁落到 `main`，同一变更也到达 `develop`。
**绝不要把 `develop` 合并进 `main`** 来把一个孤立的修复横移过去。按下面的线性历史约定选择
merge、back-merge 或 `cherry-pick`，记录提交的等价关系，并把变更应用一次——而非两次。
获得集成进 `develop` 的授权**并不**授权切出一次发布、一个标签或发布制品；那些要遵循发布流程。

## 每个任务一个 worktree

多个会话会同时写入这个克隆（实测：某次会话期间 `main` 的 `HEAD` 移动了四次，八个活跃的 worktree，
两个在相隔几分钟内重新生成同一个文件）。因此每个任务都有自己的 worktree 和分支，并且不在 `main`
或 `develop` 的检出中直接编辑任何东西。

```bash
git fetch origin --prune
# base is origin/develop for feat/chore/..., origin/main for fix/bug/hotfix
git worktree add -b <prefix>/<task> \
    <workspace>/.worktrees/delonix-runtime/<task> origin/<base>
cd <workspace>/.worktrees/delonix-runtime/<task>
export CARGO_TARGET_DIR=<repo>/target   # reuse the build cache safely
```

- **绝不在 `/tmp` 中** —— 它在开机时被清空，而本宿主机会重启；任务进行中的一次重启会带走未提交的工作。
  使用仓库旁边持久的 `.worktrees/`（由工作区 harness 做 gitignore）。**尽早并在每个有价值的步骤**
  提交并推送分支。
- **隔离的是状态，不只是代码。** worktree 隔离的是文件；它**不**隔离本宿主机上那个唯一共享的 Delonix
  引擎，也不隔离端口、卷或测试 fixture。不要在并行任务之间共享状态文件、端口、卷或测试资源。
  仅当工具保证安全时，构建缓存才可以共享；绝不要在不核对身份（`target/release/delonix --version` +
  提交距离）的情况下运行来自另一个 worktree 的二进制。
- **按名称暂存**（`git add <file>`，绝不用 `-A`/`-u`/`.`），把每一条 `git status --short`
  条目都认作你自己的，并且在共享树中**绝不使用 `git checkout -- <path>`** —— 它会在不经 stash
  的情况下销毁另一个会话未提交的工作。
- 推送被拒时要 **rebase，不要 merge**（`git pull --rebase`）：历史是线性的，没有合并提交。
- **在制品上限（WIP cap）：同时最多 3 个开放的 worktree/会话。** 开启第四个会强制关闭或搁置
  （park）一个，并附带书面交接。

## 一个 prompt 的生命周期

每个新请求都运行这个循环。小任务可能就是一个 sprint——不要制造仪式感；但也不要把"实现就绪"
与"完成"混为一谈。

1. 恢复指令、决策与检查点（会话契约、该领域的 `AGENTS.md`、`docs/adr/` 中的决策）。
2. 定义目标、范围与验收标准。
3. 对工作分类，并从上表选择基分支。
4. 识别所需的专家（见"委派"）。
5. 构建依赖图；对有依赖的工作前线排序，对独立的并行化。
6. 以与工作量相称的短 sprint 组织执行。
7. 创建 worktree。
8. 实现、评审、验证。
9. 把完成的工作集成进 `develop`。
10. 清理资源并更新检查点。

### 每个 sprint 的契约

一个 sprint 交付一个**具体、可用的变更** —— 没有被当作已完成来呈现的桩（stub）或虚构路径，
也绝不留下一个使 `develop` 在功能上损坏的状态。它要点名：功能结果；任务及其依赖；负责的专家；
验收标准；风险；测试；所收集的证据；以及集成条件。

## 委派给专家 agent

当环境提供专家 agent 时，按问题挑选它们（Rust/系统、OCI 生命周期、CRI/CNI/Kubernetes、
网络/隔离、虚拟化/provider、云镜像、CLI/DX、安全、功能测试、CI/发布）。对每一个，说明：目标、专长、
范围、分支与 worktree、它拥有的文件/契约、依赖、验收标准、验证命令，以及交付格式。独立的前线
并行运行；有依赖的前线从一个已集成的契约出发按序运行。**两个 agent 绝不同时编辑同一个 worktree。**
如果环境不提供 agent，就说明这一点并按序运行各项职责——不要模拟一次并未发生的委派。

主 agent 拥有架构、公共契约与集成。当一条前线依赖另一条时：稳定并验证第一条，把它集成进 `develop`，
从更新后的 `develop` 切出下一条前线，并重新验证所消费的契约。不要在关于某个 schema、trait 或 API
的不兼容假设上启动并行实现。控制构建/测试的并发度，以免耗尽 CPU、RAM 和 I/O。

## 集成前的质量

在一个任务被视为已验证之前：

- 确认需求和相关来源；评审 diff。
- 运行仓库要求的门禁—— `make ci`（或变更所触及的子集，并引用 `rc=0`）。强制零 clippy 警告，
  而非仅作建议。
- 运行与风险相称的测试；一个依赖内核、provider 或协议的变更要做**实际（live）**验证，
  并且 PR 要说明测试了什么（命令、预期 vs. 实际）。
- 更新变更所影响的文档和契约
  （`scripts/dev_docs.py --check`、`scripts/dev_docs_site.py --check`）。
- 确认没有密钥或意外制品；在适用处检查迁移与兼容性。
- 对相关的变更，由一个**不同于实现者的评审 agent** 去搜寻缺陷、回归、测试缺口和契约违背——
  认可作者的描述不算评审。

不要为了拿到绿色而更改断言、降低隔离或忽略失败。在关键测试缺失时，不要宣称集成已验证。

## 集成进 `develop`（强制）

已完成、已验证的工作要在**同一工作序列中**集成进 `develop`，当权限和门禁允许时。集成采用
**pull request、squash 合并、CI 套件为绿色**的方式——与已在使用的线性历史和 Conventional Commits
相匹配。集成前：

1. `git fetch origin --prune` 并检查 `develop` 是否移动过。
2. 在工作分支上解决冲突（`git pull --rebase origin develop`）。
3. 重新运行受影响的测试。
4. 确认要集成的确切候选。
5. 取得所需的评审。
6. 对 `develop` 开启/完成 PR；在合并前让 CI 跑完。

尊重 PR、检查和保护。绝不要用直接 push 或 force-push 绕过某个控制。如果某个保护阻碍集成，
把 PR 和证据准备好并解释确切的阻碍者——不要把工作标记为已集成。

CI 在推送到 `main` 和 `develop` 时、以及在以两者之一为目标的 pull request 上运行门禁套件（见
[`.github/workflows/ci.yml`](../../../.github/workflows/ci.yml)）。一个没有 PR 的已推送分支，
本身并不证明"CI 绿色"。

## 集成后的清理

在集成被**确认**——而非假定——之后，核实：本地和远端的目的地分支；变更确实被包含；所需的检查；
没有遗留的本地改动；没有未保留的提交；以及没有 agent/进程仍在使用该 worktree。然后：

1. `git worktree remove <path>`（绝不用 `--force` 来掩盖待处理的改动）。
2. `git branch -d <branch>`（绝不用 `-D`）。
3. 如果远端分支属于该任务且不再需要，则删除它。
4. 对陈旧引用执行 `git worktree prune`。
5. 更新任务记录 / 检查点。

squash 或 cherry-pick 意味着**提交身份发生变化**——用 PR、diff 和 `git cherry develop <branch>`
来证明包含关系，而不是仅凭血缘。绝不要移除别人的分支或 worktree。一个被阻塞的资源要保持被标识，
连同其原因和下一步——没有不带状态的被遗弃分支。

## 完成的定义，及途中的各个状态

一个任务只有在满足以下条件时才算 **CONCLUÍDA / DONE**：验收标准达成；评审完成；所需测试通过；
文档与契约已更新；集成进 `develop` 已确认；当额外集成进 `main` 处于被授权范围内时也已解决；
临时 worktree 和分支已安全移除；以及检查点已更新。

在会话契约中使用明确的中间状态：

`EM IMPLEMENTAÇÃO` → `EM REVISÃO` → `BLOQUEADO NA VALIDAÇÃO` →
`VALIDADO, A AGUARDAR INTEGRAÇÃO` → `INTEGRADO, LIMPEZA PENDENTE` → `CONCLUÍDO`。

## 强制执行此流程的 harness

`scripts/gitflow.py` 把本页变成可运行的控制。它在任何变更之前验证仓库和远端，支持 `--dry-run`，
是幂等的，保全数据，并传播退出码。

```bash
python3 scripts/gitflow.py classify "fix a crash in compose config"   # -> prefix + base
python3 scripts/gitflow.py start  <prefix>/<task>  [--dry-run]          # fetch, base, worktree, contract
python3 scripts/gitflow.py preflight                                    # remote, base freshness, stale worktree refs, WIP cap
python3 scripts/gitflow.py gates                                        # run the gates, collect evidence
python3 scripts/gitflow.py integrate  [--dry-run]                       # rebase, re-gate, push, open PR to develop
python3 scripts/gitflow.py done                                         # prove inclusion, checks, clean tree
python3 scripts/gitflow.py cleanup  [--dry-run]                         # remove worktree + branches safely
```

`make gitflow ARGS="<subcommand> ..."` 是通过 Makefile 做的同一件事。本地钩子（hook）辅助键盘前的那个人；
真正守住重要规则的，是 CI 检查以及（当所有者启用时）分支保护。指令文件不能替代一个可执行的控制。
