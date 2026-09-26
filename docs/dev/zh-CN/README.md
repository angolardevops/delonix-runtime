<!-- translated-from: README.md sha256:e9205b57d61260f2d44b7c78c60bc5ee9e79f9781fa0948ce1738e55a800ee77 -->
# Delonix Runtime — 贡献者手册

本手册面向想要**修改引擎**的人：你今天克隆了本仓库，想在不破坏你的宿主机或引擎的前提下发出第一个
pull request。读完本页后，你会知道本手册的编排顺序，以及针对你的角色应按什么顺序阅读哪些页面。如果你只想
*使用* Delonix，请改为从 [README](../../../README.rst) 和
[用户文档网站](https://angolardevops.github.io/delonix-runtime/) 开始。

<!-- dev-docs:begin crate-count -->
工作区共有 **25 个 crate**，产出 **5 个二进制**（`delonix`, `delonix-cri`, `delonix-mcp`, `delonix-mgmt`, `delonix-node-api`）。
<!-- dev-docs:end crate-count -->

## 引擎是什么——以及它不是什么

Delonix Runtime 是**单个节点**上的执行抽象：它运行**容器和微虚拟机（microVM）**，
并管理它们所需的网络和存储。它是声明式的（拥有自己的 Kind，按
`apiVersion` 分组——见 `delonix api-resources`），并且只通过端口（port）与各 provider
（Linux 内核、libvirt、Cloud Hypervisor、Proxmox VE、Kubernetes 的 CRI）通信，绝不会在代码中
散布 `if provider == …` 这样的分支。

几乎每一条你会收到的评审意见，都由以下三条原则塑造：

- **云原生（Cloud native）** —— plan / apply / drift（偏差），API 优先（CLI、节点 API、CRI
  和 MCP 服务器暴露相同的操作），通过开放标准实现可观测。
- **无守护进程（Daemonless）** —— 默认不驻留任何进程。必须持久化的东西属于 systemd，
  或者属于一个有明确归属者的按工作负载（per-workload）进程。新增一个守护进程需要一份 ADR。
- **无根优先（Rootless-first）** —— 正常路径在没有 root 权限下运行；特权是显式的可选项（opt-in）。

还有一条由 CI 门禁（gate）强制的边界：**引擎不认识任何消费者。** 它不知道是谁在调用它，
也没有租户、账户、方案（plan）或计费的概念。来自某个消费者的需求，要么以通用引擎能力的形式进入，
要么就不进入。权威文本是 [`AGENTS.md`](../../../AGENTS.md) 顶部的
*«Identidade e fronteira do motor»*（引擎的身份与边界）一节。

## 两个部分：生成的事实与叙述

本手册中的页面混合了两类内容：

- **事实（Facts）** —— 存在哪些 crate、它们所处的层、谁依赖谁、有哪些二进制文件、锁定的
  工具链、CI 作业。它们存在于 `<!-- dev-docs:begin <key> -->` 和
  `<!-- dev-docs:end <key> -->` 标记之间，由 `python3 scripts/dev_docs.py` 从
  `Cargo.toml`、`scripts/arch_fitness.py`、`rust-toolchain.toml` 和
  `.github/workflows/ci.yml` **生成**。绝不要手工编辑它们——CI 会运行
  `dev_docs.py --check` 并使其失败。如果某个事实有误，请修正源头或生成器。
- **叙述（Narrative）** —— 事情为何如此、各个流程如何运作、如何贡献。它是手写的，
  并在每次发布后接受复核。见[发布文档](publishing-docs.md)。

## 本手册的组织方式

各页面构成**一门课程**，从头到尾阅读。每一页开头都有一行 **阅读之前**，
列出它所假设你已读过的前置页面；结尾则有一行 **下一步**，指向承接它的页面。
站点侧栏中页面旁显示的数字，就是它在此顺序中的位置。页面*内部*的章节编号（例如
Rust 入门中的 §3.8，或标准页面中的 13.4）是为链接保持稳定的本地标签；它们不是页面位置。

课程分为八个部分：

| 部分 | 页面 | 你会从中获得什么 |
|---|---|---|
| **入门** | [从这里开始](start-here.md) · [IaaS 与云原生](iaas-and-cloud-native.md) | 一个可用的检出（checkout）、一条第一次贡献的路径，以及节点引擎在云中所处位置的心智模型 |
| **基础** | [Linux 基础](linux-foundations.md) · [云原生入门](cloud-native-primer.md) · [Rust 入门](rust-primer.md) | 亲手操作的内核原语、引擎如何使用其中每一个，以及本代码所使用的 Rust |
| **搭建与构建** | [准备你的环境](environment.md) · [克隆、构建与测试](build-and-test.md) | 一台能运行实际（live）路径的宿主机，以及作为本地命令的每一个 CI 门禁 |
| **架构** | [项目结构](project-structure.md) · [架构](architecture.md) · [各个 crate](crates.md) · [系统设计面试（System Design Interview）](system-design-interview.md) | 各部分位于何处、为何如此划分、每个 crate 拥有什么，以及设计背后的推理 |
| **镜像与微虚拟机** | [Delonixfile 与 VMfile](delonixfile-and-vmfile.md) · [构建微虚拟机](microvm-setup.md) | 两种构建语法，以及虚拟机从宿主机前置条件到启动的全过程 |
| **运维与调试** | [故障排查](troubleshooting.md) · [名称如何进入 `/etc/hosts`](service-names-and-hosts.md) | 一份以症状为索引的清单，说明门禁或实际运行会打印出什么，以及在操作者机器上发布服务名称和路由主机名的机制 |
| **贡献** | [编码约定](coding-conventions.md) · [新增一个 Kind](adding-a-kind.md) · [贡献流程](contributing-workflow.md) · [发布与稳定性](releases-and-stability.md) · [发布文档](publishing-docs.md) | 代码必须如何编写、如何新增一个声明式 Kind、如何提交一次变更、一次发布承诺不破坏什么，以及文档如何随之更新 |
| **参考** | [云原生标准](cloud-native-standards.md) · [环境变量](environment-variables.md) · [术语表](glossary.md) | 供你查阅的页面：按标准划分的合规性、每一个 `DELONIX_*` 名称、每一个术语 |

概念**只教一次**：内核原语在 [Linux 基础](linux-foundations.md)中讲解，引擎如何使用它则在
[云原生入门](cloud-native-primer.md)中讲解，它所遵循的标准及其合规状态则在
[云原生标准](cloud-native-standards.md)中讲解。当某一页提到在别处教过的内容时，
它会链接过去，而不是重复一遍。

## 按角色划分的阅读路径

没有人需要在第一次改动之前读完全部二十二页。挑选描述你的那一行，按给定顺序阅读其中的页面；
让[术语表](glossary.md)保持打开。

| 角色 | 按此顺序阅读——以及原因 |
|---|---|
| **第一个 PR，没时间** | 1. [从这里开始](start-here.md)——第 0 天检查，以及第一个 PR 的八个步骤。2. [准备你的环境](environment.md#known-host-traps)——只看*已知的宿主机陷阱*。3. [克隆、构建与测试](build-and-test.md#the-gates-ci-runs)——你必须通过的门禁。4. [各个 crate](crates.md)——只看你要改动的那个 crate 的部分。5. [贡献流程](contributing-workflow.md)——PR 如何被评判。 |
| **DevOps 工程师**（CI、打包、安装、发布） | 1. [从这里开始](start-here.md)——配置与规则。2. [准备你的环境](environment.md)——宿主机需要什么，以及看起来像引擎缺陷的陷阱。3. [克隆、构建与测试](build-and-test.md)——安装一个构建版本，把每个 CI 作业当作本地命令，E2E 与混沌测试。4. [项目结构](project-structure.md)——什么是生成的、CI 检查什么、`release.yml` 会刷新什么。5. [故障排查](troubleshooting.md)——通过消息识别门禁失败。6. [发布与稳定性](releases-and-stability.md)——版本门禁、推送一个标签会做什么、什么是稳定的。7. [发布文档](publishing-docs.md)——发布时会发生什么。8. [环境变量](environment-variables.md)——每一个开关，以及哪些会降低边界。 |
| **平台工程师**（在引擎接口之上构建） | 1. [IaaS 与云原生](iaas-and-cloud-native.md)——引擎处于哪一层、它把什么留给控制平面。2. [云原生入门](cloud-native-primer.md#48-declarative-reconciliation)——Kind 与三路协调器。3. [架构](architecture.md)——接口（CLI、CRI、管理 API、MCP、节点契约）与各层。4. [各个 crate](crates.md)——`delonix-stack`、`delonix-cri`、`delonix-mgmt`、`delonix-mcp`。5. [新增一个 Kind](adding-a-kind.md)——新增一个 Kind 所需的表与协调器接线。6. [系统设计面试](system-design-interview.md)——API 的选择及其取舍。7. [云原生标准](cloud-native-standards.md)——什么是合规、部分合规或缺失的，附带日期。 |
| **SRE**（运维节点、诊断故障） | 1. [Linux 基础](linux-foundations.md)——用一条命令回答"是哪个命名空间、哪个 cgroup、谁持有这个 fd"。2. [准备你的环境](environment.md#diagnosing-the-host)——诊断宿主机及其陷阱。3. [故障排查](troubleshooting.md)——针对门禁和运行时故障的按症状索引。4. [架构](architecture.md#level-2-containers-executables-and-processes)——运行时存在哪些进程、磁盘上的状态、已知限制。5. [系统设计面试](system-design-interview.md#7-failure-modes-and-the-limits-of-one-node)——故障模式与单节点的限制。6. [编码约定](coding-conventions.md#38-exit-codes-and-dx_-codes)——退出码意味着什么。7. [环境变量](environment-variables.md#observability)——日志、OTLP 与逃生舱口（escape hatch）。8. [云原生标准](cloud-native-standards.md#1311-opentelemetry)——OpenTelemetry 与 Prometheus。 |
| **云开发者**（Kind、清单（manifest）、镜像、Compose/Docker 兼容性） | 1. [IaaS 与云原生](iaas-and-cloud-native.md)——这些原则在代码中如何呈现。2. [云原生入门](cloud-native-primer.md)——OCI 镜像与声明式协调。3. [克隆、构建与测试](build-and-test.md)——隔离地构建与运行。4. [各个 crate](crates.md#delonix-stack)——`delonix-stack` 与 `delonix-oci`。5. [Delonixfile 与 VMfile](delonixfile-and-vmfile.md)——构建语法。6. [编码约定](coding-conventions.md#36-kinds-api-groups-and-manifest-fields)——Kind 与字段的规则。7. [新增一个 Kind](adding-a-kind.md)——端到端地把一个 Kind 接入协调器。8. [云原生标准](cloud-native-standards.md#138-the-workload-api-own-kinds-and-the-node-contract)——自有 Kind、Docker API 与 Compose 子集。 |
| **Linux 开发者**（命名空间、cgroup、网络、虚拟机） | 1. [Linux 基础](linux-foundations.md)——亲手操作各原语。2. [云原生入门](cloud-native-primer.md)——每个原语在代码中位于何处。3. [Rust 入门](rust-primer.md#34-unsafe-ffi-and-linux-syscalls)——`unsafe`、系统调用、多线程进程中的 `fork`/`clone`。4. [准备你的环境](environment.md)——AppArmor 与 cgroup 委派（delegation）的陷阱。5. [架构](architecture.md)——无根网络基础设施，以及作为序列的两条流程。6. [各个 crate](crates.md#delonix-linux)——`delonix-linux`、`delonix-sdn`、`delonix-vm`。7. [构建微虚拟机](microvm-setup.md)——KVM、Cloud Hypervisor、libvirt。8. [编码约定](coding-conventions.md#7-unsafe-syscalls-and-processes)——`unsafe` 与进程的规则。 |

两项更窄的任务有各自的捷径：**改变文档的生成方式**从
[发布文档](publishing-docs.md)开始；**配置或隔离一次运行**从
[隔离引擎的状态](build-and-test.md#isolating-the-engines-state)开始，然后是
[环境变量](environment-variables.md)。

## 各页面

| 页面 | 它回答什么 |
|---|---|
| [从这里开始](start-here.md) | 第 0 天的设置检查、你端到端的第一次贡献、一次改动会流向哪里、规则及其来源、卡住时该怎么办 |
| [IaaS 与云原生](iaas-and-cloud-native.md) | IaaS 由什么构成、本引擎处于哪一层、它把什么留给控制平面，以及云原生原则如何体现在它的文件中 |
| [Linux 基础](linux-foundations.md) | 进程、命名空间、cgroups v2、文件描述符与信号——亲手操作，附带检查每一项的命令 |
| [云原生入门](cloud-native-primer.md) | 引擎如何使用命名空间、cgroup、capability、OCI、网络、CRI、KVM 与协调——附文件与符号 |
| [面向本代码库的 Rust 入门](rust-primer.md) | 本代码库实际使用的 Rust |
| [准备你的环境](environment.md) | 内核和宿主机需要什么、锁定的工具链，以及看起来像引擎缺陷的宿主机陷阱 |
| [克隆、构建与测试](build-and-test.md) | 构建、安装、运行测试、把每个 CI 门禁当作本地命令、带隔离的 E2E 与混沌测试 |
| [项目结构](project-structure.md) | 每个顶层文件和目录是什么、谁来改动它，以及什么是生成的 |
| [架构](architecture.md) | 各层、crate 图、运行时的进程、控制路径与数据路径、磁盘上的状态 |
| [各个 crate](crates.md) | 每个 crate 一段：职责、主要类型、从哪里开始阅读 |
| [系统设计面试](system-design-interview.md) | 把本引擎设计成一份面试答案，再与实际构建的成果对比 |
| [Delonixfile 与 VMfile](delonixfile-and-vmfile.md) | 构建文件的语法，以及它们与 Dockerfile 的区别 |
| [构建微虚拟机](microvm-setup.md) | KVM、Cloud Hypervisor 与固件、libvirt、虚拟机镜像 |
| [故障排查](troubleshooting.md) | 一份症状索引：门禁失败信息、宿主机陷阱及其修复方法，集中于一处 |
| [名称如何进入 `/etc/hosts`](service-names-and-hosts.md) | 唯一的定界代码块、一个名称进入其中的两种方式（`hosts: [host]` 与 `delonix hosts sync`）、它拒绝什么，以及如何在没有 root 权限的情况下测试它 |
| [编码约定](coding-conventions.md) | 本仓库中的代码如何编写，以及评审者所使用的检查清单 |
| [新增一个 Kind](adding-a-kind.md) | 新增一个声明式 Kind 所需的表、schema 与协调器接线，以 `Service` 为例逐步讲解 |
| [贡献流程](contributing-workflow.md) | Worktree、版本、语言规则、架构规则、ADR、提交与 PR |
| [发布与稳定性](releases-and-stability.md) | 版本门禁、推送一个标签会做什么，以及 CLI 和清单 schema 承诺不破坏什么 |
| [发布文档](publishing-docs.md) | 站点和本手册如何被生成、经过门禁检查并发布 |
| [云原生标准](cloud-native-standards.md) | 每一项标准、它要求什么、Delonix 如何实现它，以及它的合规状态 |
| [环境变量](environment-variables.md) | 代码读取的每一个 `DELONIX_*` 变量：谁读取它、它改变什么、它的默认值，以及哪些会降低边界 |
| [术语表](glossary.md) | 你在这里遇到的引擎与云原生术语，附带它们在 Delonix 中的含义，以及在何处可以阅读更多内容 |

你会被指向的其他参考资料：[`ARCHITECTURE.md`](../../../ARCHITECTURE.md)（C4 图表）、
[`docs/adr/`](../../adr/README.md)（架构决策）、[`SECURITY.md`](../../../SECURITY.md)
（私密漏洞报告），以及 [`CONTRIBUTING.md`](../../../CONTRIBUTING.md)（简短的入口）。

---

**下一步：** [从这里开始](start-here.md)——花三十分钟检查你的配置，并端到端地走一遍第一次贡献。
