<!-- translated-from: publishing-docs.md sha256:e7464ba7b5b1c113bb55466d80c2d833cf32878863e3e6f711e35fbc59b0b281 -->
# 发布文档

**阅读之前：** [贡献工作流](contributing-workflow.md)、[发布与稳定性](releases-and-stability.md)（一个被推送的标签会做什么——本页的 *发布时会发生什么* 一节正是它对应的文档说明）以及项目结构页里的[生成 vs 手写对照表](project-structure.md#generated-vs-hand-written)。

本仓库里的文档要么是**从代码生成的**（然后由一个门禁检查），要么是**手写的**（然后经过评审）。本页解释哪个是哪个、每一种是怎么发布的，以及当你的改动影响到文档时你该做什么。读完之后，对于任何一种改动，你都会知道该跑哪个生成器、该把哪些文件和它一起提交。

## 发布在哪里

这个站点由 **GitHub Pages** 从 `main` 的 `/docs` 目录提供服务，地址是
<https://angolardevops.github.io/delonix-runtime/>。Pages 的来源设置本身存在于仓库设置里，不在代码树中；代码树里实际存在的是 `docs/.nojekyll`，它关掉了 Jekyll，让 `docs/` 下的每一个文件都原样按提交时的样子被提供。

值得知道的一个后果：因为 Jekyll 被关掉了，`docs/` 下的 Markdown 文件——包括 `docs/dev/` 里的这本手册——**不会**被 Pages 渲染成 HTML；它们是以纯文件形式提供的。手册的渲染视图，是 GitHub 自己在浏览仓库时对 Markdown 的渲染（github.com 上的 `docs/dev/README.md`），它也会渲染 Mermaid 图。手册各页面之间的链接都是相对链接，所以在两个地方都能用。

## 谁负责什么

| 表面 | 是什么 | 如何保持真实 |
|---|---|---|
| `README.rst` | 项目的门面页 | 手写，由 `docs_cli_gate.py` 检查 |
| `docs/*.html`、`docs/comandos/*.html` | 用户站点 | 由 `docs/gen.py` 从二进制文件的 `--help` 加上生成器里的编辑文字**生成**；由 `docs` CI 任务以及发布时检查 |
| `docs/releases/v<version>.md` | 发布说明，一个标签一份 | 在发布提交中手写；作为 GitHub Release 的正文发布 |
| `docs/RELEASES.md` | 按发布版本划分的功能附录 | 由 `scripts/gen-releases.sh` 从 `docs/releases/` **生成**；绝不手动编辑 |
| `ARCHITECTURE.md` | C4 架构图 | 手写，与代码保持同步；由 `docs/gen.py` 渲染进站点 |
| `docs/adr/` | 架构决策 | 一个决策一个文件；已接受的 ADR 会被后来者取代，绝不重写 |
| `docs/api/openapi.yaml` | 节点 API 的 REST 编码 | 由 `scripts/contract_gate.py --update` 从 `proto/delonix/node/v1` **生成** |
| `docs/dev/` | 本贡献者手册 | 生成的**事实**（`scripts/dev_docs.py`）加上手写的**叙述** |
| `CONTRIBUTING.md` | 贡献者的简短入口 | 手写；指向 `docs/dev/` |

## 用户站点：`docs/gen.py`

参考页面嵌入的是生成器运行时捕获的 `delonix` 二进制文件的**真实** `--help` 输出，所以站点里绝不会写着一个不存在的标志位。编辑性的内容（介绍、示例、说明）存在于 `docs/gen.py` 自身内部的字典里——要编辑就在那里编辑，不要在 HTML 里改。

```bash
cargo build --release -p delonix-runtime-bin
python3 -m pip install markdown     # the generator renders ARCHITECTURE.md
python3 docs/gen.py                 # uses the tree's release binary by default
git diff --stat -- docs/
```

生成器也接受二进制文件路径作为它的第一个参数。把重新生成的文件和引起它们变化的 CLI 改动一起提交。有两个检查会在你不这样做时失败：

- CI 里的 **`docs` 任务** 会重新生成站点，如果 `git diff -- docs/` 不为空就失败；同一个任务还会用 `stack apply --dry-run` 和 `stack validate` 验证每一个 `examples/*.yaml`，并用 `groff` 检查由 `delonix man --dir <dir> --index` 生成的 man 手册页；
- **发布工作流** 会在发布构建之上重新跑一遍生成流程，然后才发布，所以一个站点已经过时的标签不会发出去。

## 命令引用门禁：`scripts/docs_cli_gate.py`

当前文档中任何以代码上下文（`<code>`、`<pre>`、Markdown 代码围栏、reST 字面量块、YAML 注释）引用的 `delonix …` 命令，都必须能在这棵代码树构建出的二进制文件的命令树里解析出来（通过 `scripts/cli-tree.sh` 读取）。它运行在 CI 的 `cli-surface` 任务里：

```bash
cargo build --release -p delonix-runtime-bin
DELONIX_BIN="$PWD/target/release/delonix" python3 scripts/docs_cli_gate.py
python3 scripts/docs_cli_gate.py --list      # every citation and where it is
```

它之所以存在，是因为命令在连续几个大版本里被移除过，而当时有好几个现存页面还在教人 `unrecognized subcommand`。带日期的历史记录——`docs/releases/`、`docs/RELEASES.md`、`docs/discovery/` 以及带日期的审计与测量报告——是刻意被排除在外的：它们必须继续沿用它们所描述的那个版本的拼写。一处故意错误的引用，会被记录进 `scripts/docs_cli_gate.py` 并附上原因。

它扫描的文件列在脚本顶部的 `TARGETS` 里。当你新增一个文档目录时，要检查这份列表；不在里面的页面不会被检查。

## 手册的事实部分：`scripts/dev_docs.py`

`docs/dev/` 里的结构性事实——各个 crate 及其所在的层、依赖图、各个二进制文件、锁定的工具链版本、CI 任务——是从 `Cargo.toml`、`scripts/arch_fitness.py`、`rust-toolchain.toml` 和 `.github/workflows/ci.yml` 生成的。在页面中，它们位于标记之间：

```markdown
<!-- dev-docs:begin <key> -->
…generated, do not edit…
<!-- dev-docs:end <key> -->
```

```bash
python3 scripts/dev_docs.py            # rewrite every generated region
python3 scripts/dev_docs.py --check    # exit 1 when docs/dev is stale (CI job `arch`)
```

规则：

- **绝不在一个区域内部编辑。** 下一次重新生成会把它覆盖掉，`--check` 也会在 CI 里失败。如果某个事实错了，去修它的源头（`Cargo.toml`、`arch_fitness.py` 里的 `LAYERS` 表、`ci.yml`），或者修生成器本身。
- 标记**之外**的文字永远不会被触碰，所以叙述可以围绕着一张生成的表来写。
- 一个**新区域**需要一个 `render_*` 函数、`regions()` 里的一个键，以及某个页面上的一个标记——三者在同一个提交里一起加上。
- 不要写任何每次提交都会变的数字（行数、测试数、提交数）——生成器里不要写，叙述里也不要写。一个每次 PR 都是红的门禁会不再被人看，一个手写的数字会在不知不觉中变成假的。

如果你的 PR 新增或移动了一个 crate、改变了 crate 之间的依赖、新增了一个二进制文件、升级了工具链，或者改动了一个 CI 任务，就跑一遍 `python3 scripts/dev_docs.py`，并把结果一起提交。

## 发布时会发生什么

发布工作流（`.github/workflows/release.yml`）只在有人要求时才运行（`gh workflow run release.yml -f tag=v4.4.0`）——单单推送一个标签什么都不会发生。就文档而言，它会：

1. 针对发布构建重新生成用户站点，如果 `docs/` 有差异就**失败**；
2. 以 `docs/releases/<tag>.md` 作为正文发布 GitHub Release（如果该文件不存在，就用生成的说明）；
3. 检出 `main`，跑 `scripts/gen-releases.sh`（`docs/RELEASES.md` 附录）和 `scripts/dev_docs.py`（手册的事实部分），如果有变化，就把两者一起提交到 `main`，并带上 `[skip ci]`。

手册的**叙述**部分不会被 CI 重新生成。在一次发布被发布并验证之后，会有一个由维护者手动触发的评审步骤，读取上一个标签和新标签之间的改动以及发布说明，只更新那些改动所影响到的手册页面，并为此开一个拉取请求。当没有任何结构性或流程性的东西发生变化时，那次评审的结论就是"没什么要更新的"，并明确说出来。

## 这对你的拉取请求意味着什么

| 你的改动 | 还要做的事 |
|---|---|
| CLI 帮助文字、一个命令、一个标志位 | `python3 docs/gen.py`（发布构建）并提交 `docs/`；如果叶子命令变了，更新 `scripts/cli_baseline.tsv`；修好 `docs_cli_gate.py` 报告的任何引用问题 |
| 一个 crate、一个 crate 依赖、一个二进制文件、工具链，或者一个 CI 任务 | `python3 scripts/dev_docs.py` 并提交 `docs/dev/` |
| `proto/` 里的节点契约 | `python3 scripts/contract_gate.py --update` 并提交 `docs/api/openapi.yaml` |
| 一个结构性决策 | 在 `docs/adr/` 里写一份 ADR（见[贡献工作流](contributing-workflow.md#when-to-write-an-adr)） |
| 一个用户可见的功能 | 在 PR 里描述它，这样它就能进入下一份发布说明 |
| 贡献者如何构建、测试或工作 | 本手册相应的那一页 |

---

**下一篇：** [云原生标准，逐层讲解](cloud-native-standards.md)——参考部分：每一项云原生标准、它要求什么，以及引擎对它的符合情况和日期。
