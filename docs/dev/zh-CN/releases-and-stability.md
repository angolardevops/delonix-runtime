<!-- translated-from: releases-and-stability.md sha256:7d575f084b949f5b59b58dec29fb1ba7eb63904aa8514433ec4a2809e13a25e4 -->
# 发布与稳定性

**阅读前须知：** [贡献流程](contributing-workflow.md#version-alignment)（版本门禁）和[发布文档](publishing-docs.md)（一次发布会重新生成什么）。

本页讲的是一次发布的另一半：`Cargo.toml` 里的 `version` 承诺了什么、推送一个 `v*` 标签会
发生什么，以及引擎保证在没有主版本号变更的情况下不会破坏什么。[发布文档](publishing-docs.md)
已经讲过一次发布中文档那一侧的事情（什么会被重新生成、CI 检查什么）；本页讲的是版本号本身，
以及它所支撑的 CLI/清单（manifest）契约。

## 版本门禁

`scripts/version_gate.py`（CI 任务 `version`，用 `fetch-depth: 0` 是因为它需要每一个标签）
只允许根目录 `Cargo.toml` 里工作区（workspace）的 `version` 恰好是以下两者之一：

1. **等于这次提交所包含的最新标签。** 这是发布之间的日常工作状态。两次版本号相同的构建，
   靠 `delonix --version` 来区分——它会打印出与那个标签之间的距离：
   `commit: <hash> (+N commits since vX.Y.Z)`。
2. **大于所包含的最新标签，且只在发布提交中如此**——此时 `docs/releases/v<version>.md`
   必须存在，因为发布工作流程会把它作为 GitHub Release 的正文发布出去。

除此之外的一切都会失败，每一种失败都对应一个真实的失败场景：

| 门禁看到什么 | 为什么失败 |
|---|---|
| 存在一个更新的标签，而这次提交并不包含它 | 这条分支是在那次发布之前开始的；原样合并会撤销那次发布已经发布出去的内容。先合并 `origin/main`。 |
| `Cargo.toml` 低于所包含的最新标签 | 会把一个更旧的版本号发布到一个更新的版本号之上。 |
| `Cargo.toml` 高于所包含的最新标签，却没有对应的 `docs/releases/v<version>.md` | 版本号提升了却没有匹配的标签——这会把 `delonix-cri` 的下载（它是根据正在运行的二进制文件自身的版本号来解析的，见 `vmimage.rs`）指向一个永远不会存在的发布。 |

版本号不是装饰：除了 `delonix-cri` 的下载之外，它还是 Docker API 的 `ServerVersion`、记录在
每一份资源备份里的 `delonix_version`，也是发布工作流程（见下文）在发布任何东西之前用来核对
构建出的二进制文件的依据。

不要在一个功能 PR 里提升版本号，也不要用 `-dev` 后缀——门禁的第一条规则已经覆盖了日常工作，
而 `-dev` 后缀会把 CRI 的下载指向一个不存在的发布。

## 发布一次版本做了什么

推送标签**什么也不会发布**。自 2026-09-23 起，发布工作流程只能通过 `workflow_dispatch`
触发：标签和发布是两个各自独立的决定，所以一个意外到达远端的标签（`git push --tags`、
`push.followTags=true`）再也不能把一个二进制文件推到全世界面前。一个没有对应发布的标签，
在这里是正常的中间状态。

```bash
git tag -a v4.4.0 -m "v4.4.0" && git push origin v4.4.0   # cuts the version
gh workflow run release.yml -f tag=v4.4.0                 # publishes it
```

一个 `guard` 任务会先运行，只花几秒钟：它会拒绝一个形状不是 `vX.Y.Z` 的 `tag`、一个在远端
不存在的 `tag`，以及一个已经有发布的 `tag`——这个输入决定了要构建哪次提交、要给发布起什么
名字，所以一个未经检查的 `tag: main` 本会发布出一个叫"main"的发布。只有到这一步之后，才会
在同一个任务里：

1. 构建 `delonix`、`delonix-cri`、`delonix-mcp` 和 `delonix-mgmt` 两次——一次是通用
   x86-64，一次带上 `-C target-cpu=x86-64-v3`（AVX2/BMI2/FMA）——特意选在 `ubuntu-22.04`
   上构建，好让 glibc 基线版本（2.35）与 RHEL 9 和 Debian 12 保持兼容，而不只是与最新的
   Ubuntu 兼容。`scripts/install.sh` 会在宿主机 CPU 支持时自动选用 `-v3` 版本的构建。一个
   独立的 `build-arm64` 任务会在一台 aarch64 运行器上原生构建同样这四个二进制文件（每个
   组件一个，没有 `-v3` 变体），以 `<name>-aarch64-linux` 的形式发布，并放进同一份
   `SHA256SUMS` 里。`install.sh` 会在一台 aarch64 宿主机上安装它们（#447）。这个任务只在
   发布时运行，所以它第一次执行就是 v4.2.0 那次发布本身；CI 会在每个 PR 上通过 `test
   (arm64)` 任务原生运行测试套件。
2. **针对这次确切的发布构建重新生成用户站点，如果 `docs/` 有差异就失败。** 这道门禁之所以
   存在，是因为它曾经不存在：v0.48.0 里就曾经发生过一个站点缺口在上线时随之发布，隐藏了一个
   新命令好几个小时，而与此同时另一个并行的 CI 任务已经因此报红——只是这两条工作流程谁也没
   看谁。在这里、在发布之前跑一遍 `docs/gen.py`，堵上了这个缺口。
3. 构建一份带校验和的 `SHA256SUMS`、一份 SPDX 2.3 的软件物料清单（SBOM，`scripts/sbom.py`，
   从 `Cargo.lock` 生成，它自身也被哈希进 `SHA256SUMS`），以及——当配置了
   `MINISIGN_SECRET_KEY` 密钥时——一份对 `SHA256SUMS` 的 minisign 签名。`SHA256SUMS`
   本身只能证明传输完整性（它来自与二进制文件相同的 URL）；签名证明的是这次发布确实来自
   本项目，因为 `install.sh` 内置了公钥，并且在没有 `--insecure-skip-signature` 的情况下
   会拒绝安装一个未签名的发布。当密钥**确实**已配置却签名失败时，这是一个硬性错误——因为
   那会一次性破坏所有安装脚本的签名校验。
4. 在发布任何东西之前，先验证它自己构建出的二进制文件报告的版本号与标签一致
   (`delonix --version | grep <tag>`)——这和 `version_gate.py` 更早运行过的检查是同一类,
   只不过这次核对的是真正即将发布出去的那个构件。
5. 附加 SLSA 构建来源证明（`actions/attest-build-provenance`，GitHub 自己的动作，故意与
   minisign 分开：来源证明面向的是那些并不预先信任本项目的人，证明某个东西*是在哪里、从哪次
   提交构建出来的*；minisign 面向的是那些已经信任其内置公钥的人，证明这次发布*确实属于本
   项目*）。
6. 发布 GitHub Release：当 `docs/releases/<tag>.md` 存在时，用它作为发布说明；否则用
   `--generate-notes`。
7. 检出 `main`，重新生成 `docs/RELEASES.md`（`scripts/gen-releases.sh`）以及手册生成的
   事实部分（`scripts/dev_docs.py`），并单独重新生成手册的**站点**
   （`scripts/dev_docs_site.py`）——把有变化的部分提交上去，带 `[skip ci]`，这样文档就
   永远不会比一次发布落后超过这一次提交。生成器在这一步失败只是一个响亮的警告，而不是一次
   失败的发布：发布本身并不依赖它。

手册的**叙述**半边——这些页面上的文字——不会由 CI 重新生成。在一次发布发布出去之后，会有一位
维护者主导的复核，阅读自上一个标签以来发生了什么变化（提交、发布说明），并只更新受影响的
那些页面，具体做法正如[发布文档 § 发布时会发生什么](publishing-docs.md#what-happens-at-release-time)
所描述的那样。

## 什么是稳定的，以及这个承诺写在哪里

`docs/cli-stability.md` 才是真正的契约，就在同一个仓库里，被 `delonix explain` 和生成出来
的页面共同读取——本节只是把你引导到那里去，因为在这里再复述一遍它的内容,只会多出一份会与
代码脱节的副本。这条契约自 v0.42.3 起生效，到了 v1.0.0，它读起来就是本项目真正的语义化
版本承诺,而不再只是一条局限于 `0.x` 阶段内部的备注。

**稳定——不会在没有主版本号变更的情况下被破坏：**

- 容器/镜像生命周期的动词（`container run`、`ps`、`stop`、`exec` 等）以及镜像相关的动词，
  沿用 Docker/Podman 自己的命名和参数顺序，以及 `docs/cli-stability.md` 为
  `run`/`exec` 列出的那些具体的短/长标志位。
- 退出码（`0` 成功、`4` 未找到、`5` 冲突、`69` 缺少宿主机能力、`124` 超时……）以及每次
  失败所携带的 `DX-CDNN` 字典编号（ADR-0043）——这个编号标识的是*哪一种*失败，其含义永远
  不会改变，也不会被复用；`delonix explain DX-4501` 可以查到某一个具体编号。
- 每一个列表命令上的 `-o json`：字段只能被新增，绝不会被移除或改变类型（ADR-0005）。
- 拥有类型化 spec 的那些 Kind 的**清单 schema**（`Container`、`Pod`、`Volume`、`Network`,
  以及 `delonix manifest schema` 列出的其他那些）：一个字段绝不会被移除、改变类型或挪作
  他用；新字段永远是可选的，并带有一个能保持旧行为的默认值；一个改了名字的字段会把旧的
  拼法保留为别名；`apiVersion: delonix.io/v1` 在按领域划分的分组
  （`compute.delonix.io/v1alpha1` 等）成为规范写法之后依然能被加载。这是实践中分量最重的
  那条承诺——它保护的是人们提交进 git、在 PR 里评审的那些内容，而不只是敲在命令提示符里的
  那些字符。

**不稳定——可以在任何版本里改变：** `serve cri`/`serve api`/`serve docker-api`（本地管理
API 尤其没有已发布的契约，明确不是拿来做自动化的对象——见[各个 crate § `delonix-mgmt`](crates.md#delonix-mgmt)
以及 ADR-0040/0041）；`cluster`/`vm`/`pod`/`workload`/`net` 这些命令式（imperative）
接口（它们的清单 **schema**，如果存在的话，上面已经覆盖到了——不稳定的只是围绕它的那些
动词和标志位）；`compose`；`backup`；`mcp`；`system`/`dashboard`/`completion`/`init`/
`man`/`config`/`explain`；`$DELONIX_ROOT` 下的磁盘状态格式；以及 `stack history`/
`stack rollback`（ADR-0019——没有任何东西会读取这份历史来决定什么存在，所以丢掉它并不会
改变协调器（reconciler）做的任何事）。

## 一次破坏性变更是如何发生的，当它不得不发生时

已经不止一次应用过的先例（v0.30.0 的 CLI 重组、v2.0.0 把 `image list` 改回
`image ls`）：**干净利落地切一刀，不留兼容别名。** 旧的拼法会以 `unrecognized subcommand`
失败，响亮地失败，从切换那一刻起的每一个版本都是如此——绝不留一个悄悄改变行为、日后才让人
察觉的静默别名。`docs/cli-stability.md § Como uma quebra é feita` 记录了做这件事时得到的
一次真实教训：一次重命名可能会漏掉一个*内部*调用者（CRI 服务器自己在 v0.30.0 重组之后的
好几个月里，一直在调用一个已经被移除的 `delonix netns attach`，破坏了 rootless 下的 pod
创建）——对整个工作区（workspace）grep 旧的拼法，而不只是文档和测试，是切这一刀的一部分。

对本页或 `docs/cli-stability.md` 标记为稳定的东西做破坏性变更，首先需要一份 ADR
（[贡献流程 § 什么时候要写 ADR](contributing-workflow.md#when-to-write-an-adr)），因为
按定义它移动了一条结构性边界——这与新增一个后端或新增一条特权边界所适用的推理是一样的。

---

**下一篇：** [发布文档](publishing-docs.md)——站点和本手册是如何被生成、经过门禁检查并发布的，以及你的 PR 必须重新生成什么。
