<!-- translated-from: coding-conventions.md sha256:5c91adc6b2f71d8a63325e98fe7cc34464469e6339179fb1178bf42d204ee147 -->
# 编码规范

**阅读前须知：** [Rust 入门](rust-primer.md)、[架构](architecture.md) 和 [crate 一览](crates.md) —— 下面的规则会按名称引用层（layer）、端口（port）和 crate。

本页告诉你如何写出能在本仓库通过评审的代码，这样你就不必去猜规则或自己编一套。读完之后，你可以在提交给评审者之前，先用文末的评审清单检查一遍自己的 diff。下面每条规则都带有一个标签和一个来源：

- **强制（门禁）**：如果你违反了它，CI 任务会失败。门禁都有名字，你可以在本地跑它（见[构建与测试](build-and-test.md#the-gates-ci-runs)）。
- **已决定（ADR/AGENTS.md）**：`docs/adr/` 中某个状态为**已接受**的 Architecture Decision Record，或 `AGENTS.md` 的某一节，已经把这件事定下来了。目前还没有门禁去检查它，所以由评审来把关。
- **提议中（ADR，尚未决定）**：唯一的书面依据是一份状态仍为**提议中**的 ADR（见 [`docs/adr/README.md`](../../adr/README.md) 的状态一栏）。这是代码正在走的方向，评审也会照此执行，但它仍可能改变；当那份 ADR 被接受或被否决时，标签会跟着变。
- **惯例（观察所得）**：代码一直这么做，但没有人把它写下来。举的例子都是真实的 `path:symbol` 引用，照抄即可。

如果这里没有回答某个问题，而你改动周围的代码也没有回答，那诚实的答案就是**「未决定——跟着周围的代码走」**。已知的悬而未决的问题清单在[未决问题](#undecided)。不要把个人偏好在某个 PR 里变成一条规则。

相关页面：层以及它们背后的原因见[架构](architecture.md)，每个 crate 里有什么见 [crate 一览](crates.md)，Rust 的惯用写法见 [Rust 入门](rust-primer.md)，工作流程（worktree、版本号、ADR、PR）见[贡献流程](contributing-workflow.md)。

---

## 1. 强制风格的工具

| 工具 | 检查什么 | 标签与来源 |
|---|---|---|
| **rustfmt** | 格式化。**没有配置文件**：根目录没有 `rustfmt.toml` 或 `.rustfmt.toml`，所以用的是默认设置。 | **强制（门禁）**：CI 任务 `fmt` 跑 `cargo fmt --all --check`（`.github/workflows/ci.yml`）。`CONTRIBUTING.md` § Style："`cargo fmt` defaults, no custom config"。 |
| **clippy** | 每一条 clippy lint、**以及 rustc 自身发出的每一条警告**都会让构建失败，测试也包含在内（`--all-targets`）。没有 `clippy.toml`，所以用的是 lint 的默认设置。 | **强制（门禁）**：CI 任务 `clippy` 跑 `cargo clippy --workspace --all-targets --locked -- -D warnings`。 |
| **`[workspace.lints]`** | 根目录的 `Cargo.toml` 只声明了一条 workspace lint：`[workspace.lints.clippy] undocumented_unsafe_blocks = "deny"`。每个成员 crate 的清单文件都有 `[lints] workspace = true`。 | **强制（门禁）**：clippy。它是 ADR-0040 P0 阶段引入的（"`[workspace.lints]`（`undocumented_unsafe_blocks = deny`）"），这份 ADR 目前仍是**提议中**；但门禁不受此影响，照样生效。 |
| **cargo-deny** | RUSTSEC 安全公告和已撤回（yanked）的 crate，只允许宽松许可证（不允许 GPL/AGPL），crates.io 是唯一的注册表，不允许 git 来源。`deny.toml` 的 `[bans]` 一节（重复版本和通配符，设为警告）在 CI 里**不**被评估。 | **强制（门禁）**：CI 任务 `deny` 用 `deny.toml` 跑 `check advisories licenses sources`。**惯例（观察所得）**：`deny.toml` 里每一条被忽略的公告都带有说明理由的注释——没有门禁检查这条注释是否存在。 |
| **`scripts/lang_ratchet.py`** | 标识符、注释和面向用户的字符串里的葡萄牙语（LANG-01，见[§2](#2-language-of-the-code)）。 | **强制（门禁）**：CI 任务 `lang`，基线在 `scripts/lang_baseline.json`。 |
| **`scripts/arch_fitness.py`** | 依赖方向、crate 所在目录是否等于其所属层、版本号是否只在根目录声明、消费者名称，以及[架构](architecture.md#layers-and-the-allowed-direction)里列出的那几个债务棘轮（见[§4](#4-structure-where-code-goes)和[§5](#5-internal-api-and-boundaries)）。 | **强制（门禁）**：CI 任务 `arch`，基线在 `scripts/arch_baseline.json`。 |
| **`scripts/contract_gate.py`** | `proto/delonix/node/v1` 下的节点契约（见[§5.4](#54-the-node-contract)）。 | **强制（门禁）**：CI 任务 `contract`。 |

对**棘轮**（`lang_ratchet.py` 和 `arch_fitness.py` 里的那些数字）要记住一个机制。一个棘轮数字**上升**时会失败，数字**下降**但基线没有在同一个提交里一起降下来时**也**会失败（`arch_fitness.py` 会打印 "debt was paid; lower the baseline in the same commit (--update)"）。如果你还了债，就跑一次 `--update`，把新的基线和修复放进同一个提交。用 `<=` 判断的话，债务就会永远显示为绿色（两个脚本的模块级 docstring 里都写了这一点）。

由此带来一个后果：**强制（门禁）**。因为 `-D warnings` 也覆盖了 rustc 自身的 lint，rustc 里关于命名的 lint（`non_snake_case`、`non_camel_case_types`、`non_upper_case_globals`）同样被强制执行。函数和变量用 `snake_case`、类型用 `UpperCamelCase`、常量用 `SCREAMING_SNAKE_CASE`，在这里不是一种风格上的选择。

树里存在 `#[allow(clippy::…)]`，例如 `crates/contexts/delonix-compute/src/run.rs` 和 `crates/contexts/delonix-compute/src/pod.rs` 里的 `#[allow(clippy::too_many_arguments)]`。**未决定**：没有规则说明什么时候可以接受一个 `allow`。如果你加了一个，就在旁边放一句 `// why` 注释，就像对待任何其他例外一样（见[§10](#10-comments-and-documentation)）。

---

## 2. 代码的语言

- **标识符、注释和消息都用英语写。** **强制（门禁）**：`scripts/lang_ratchet.py`。**已决定**：AGENTS.md § "Língua do código: inglês (LANG-01)"。这个棘轮会扫描 `target/`、`third_party/` 等目录之外的每一个 `.rs`、`.py`、`.ts`、`.go` 和 `.yml`/`.yaml` 文件，统计三样东西：
  - **标识符**：任何由 `fn`、`struct`、`enum`、`trait`、`const`、`static`、`type`、`mod`、`union` 或 `let` 声明的名字，只要其 `snake_case`/`camelCase` 的某个片段包含 `scripts/lang_pt_lexicon.txt` 里的一个词；
  - **注释**：包含词典中某个词的 `//`、`///` 和 `//!` 行；
  - **面向用户的文本**：`format!`、`println!`、`eprintln!`、`panic!`、`anyhow!`、`bail!`、`expect` 或 `unimplemented!` 里长度不小于 8 个字符的字面量。
- **葡萄牙语只通过目录文件到达操作者。** **已决定**：AGENTS.md § "i18n (fonte EN + catálogo pt.po embutido)" 以及 `CONTRIBUTING.md` § "If you add or change a CLI command"。源码里写英语文本。葡萄牙语放在 `bins/delonix-runtime-bin/data/pt.po` 里，`bins/delonix-runtime-bin/src/cmd/po.rs` 用 `include_str!` 把它嵌入进去。
  - 固定字符串用 `po::t("…")`。
  - 插值文本用 `po::tf("… {name} …", &[("name", value)])`。要用**具名**占位符：因为一次翻译可能会调整它们的顺序，而 `format!` 又需要编译期字面量，所以模板会先被翻译，值再在之后被代入（`po.rs`，`tf` 的 doc comment）。

    ```rust
    // bins/delonix-runtime-bin/src/cmd/config.rs — refuse_unknown_key
    Err(Error::Invalid(super::po::tf(
        "'{key}' is not a config key — known: {known}",
        &[("key", key), ("known", &KNOWN_KEYS.join(", "))],
    )))
    ```
  - `--help` 的文字在源码里也是英语，由 `po::translate_help` 在运行时翻译。**强制（门禁）**：`bins/delonix-runtime-bin/src/main.rs` 里的 `help_i18n_tests`。`todo_o_help_de_comando_tem_traducao_pt` 对命令帮助是严格检查，`o_help_dos_argumentos_so_pode_encolher` 是针对参数帮助中 `ARG_HELP_PENDING` 的棘轮。新增一个命令或标志需要一条 `pt.po` 条目。
  - 缺失的条目会回退到英语，所以界面永远不会空白（`po::t`）。直接在代码里写死一条葡萄牙语字符串是一个 bug，不是一条捷径。
  - 不要复用一个其葡萄牙语译文依赖主语语法性别的 `msgid`。"created" 对于一个网络可以是 *criada*，对于一个卷则是 *criado*，所以要用不同的 key。**已决定**：AGENTS.md § "v0.32.2 — 380+ strings PT hardcoded"。
- **词典的陷阱。** **已决定**：AGENTS.md § LANG-01。
  - **不要给任何东西起名叫 `num`。** 它被故意留在词典里，因为它能抓到真正的葡萄牙语（"num apply falhado"）。所以一个叫 `num` 的标识符会被算作葡萄牙语债务，让门禁失败。请用 `count` 或 `number`。
  - 往词典里加一个词会让计数**上升**，让门禁失败。要在同一个提交里把基线降下来。同形异义词（`data`、`base`、`no`、`nas`……）默认排除在外，除非有测量证明它们真的能抓到葡萄牙语；`num` 是有记录的例外（AGENTS.md § LANG-01）。

---

## 3. 命名

### 3.1 crate 和目录

- **目录就是层。** 一个 crate 要么放在 `crates/foundation/`、`crates/contexts/`、`crates/adapters/`、`crates/providers/` 或 `crates/interfaces/` 之一，要么是 `bins/` 下的一个二进制。目录必须和这个 crate 在 `LAYERS` 表里的条目一致。**强制（门禁）**：`scripts/arch_fitness.py` 的 `misplaced()` / `LAYER_DIR`。一个新 crate 要在同一个提交里同时进入 `LAYERS`**和**对应的目录。
- **新建或重构 crate 时的目标命名惯例。** **提议中（ADR，尚未决定）**：ADR-0040 D2.1。

  | 角色 | 名称 |
  |---|---|
  | 共享的纯粹基础层 | `delonix-model` |
  | 限界上下文（bounded context） | `delonix-<context>`，以其对外发布的 API 组命名（D2.2）：`delonix-compute`、`delonix-stack` |
  | 技术适配器 | `delonix-<technology>`：`delonix-linux`、`delonix-sdn`、`delonix-oci` |
  | 可插拔 provider | `delonix-provider-<technology>` |
  | 接口库 | `delonix-<protocol>`：`delonix-cri`、`delonix-mcp` |

  一个 crate 只有在它是一个限界上下文、隔离了某个笨重或需要特权的依赖、或者是一个单独安装的二进制时才应该存在。**不允许 `-core`、`-common`、`-utils` 或 `-types` 后缀**（ADR-0040 D2.1 记录了一个 `-core` crate 是如何变成"the sink of everything"的；那个 crate，`delonix-runtime-core`，已在 #406 中被移除）。有些 crate 仍保留着旧名字：`delonix-proxmox`、`delonix-truenas`、`delonix-security-runtime`。ADR-0040 会在重构对应部分的那个阶段里把它们逐一改名，"never twice"（永远不改第二次）。**不要在某个 crate 的对应阶段之外去改它的名字。**
- **每个 crate 的路径只写一次**，写在根目录 `Cargo.toml` 的 `[workspace.dependencies]` 里。各成员之间用 `{ workspace = true }` 互相依赖。**已决定**：AGENTS.md § "A direcção das dependências é um portão (ADR-0040, fase P0)"，以及 `[workspace.dependencies]` 顶部的注释。

### 3.2 模块和文件

- **CLI 命令组**：每个组一个模块，放在 `bins/delonix-runtime-bin/src/cmd/<group>.rs` 里，包含一个 `pub enum <Group>Cmd`（clap 子命令）、一个分发函数 `pub fn run(action: <Group>Cmd) -> Result<()>`，以及每个子命令对应一个 `cmd_<verb>` 函数。**惯例（观察所得）**：`cmd/volume.rs:VolumeCmd` + `run` + `cmd_create`/`cmd_ls`/`cmd_describe`；`cmd/secret.rs:SecretCmd` + `run`；`cmd/container.rs:cmd_run`/`cmd_start`/`cmd_stop`。AGENTS.md § "CLI (`delonix`)" 说明了"每个组一个模块"这部分。
- **compute 端口的适配器**放在以该端口所关心的事情命名的文件里，而不是以技术命名：`crates/adapters/delonix-linux/src/workload.rs`、`.../run_host.rs`、`crates/adapters/delonix-sdn/src/run_network.rs`、`.../vm_network.rs`、`crates/adapters/delonix-oci/src/run_images.rs`。**惯例（观察所得）**。

### 3.3 类型、trait、函数、常量

- **端口以能力命名，而不是以技术命名。** **提议中（ADR，尚未决定）**：ADR-0040 D3 列出了这些端口：`WorkloadRuntime`、`SandboxProvider`、`VmProvider`、`NetworkProvider`、`StorageProvider`、`ImageRegistry`、`ImageStore`……今天已经存在的那些在 `crates/contexts/delonix-compute/src/ports.rs`（`ImageStore`、`StorageProvider`、`DeviceResolver`、`RunHost`、`VmNetwork`、`NetworkProvider`）和 `.../launch.rs`（`WorkloadRuntime`）里。较老的 `VmBackend`（在 `crates/adapters/delonix-vm/src/lib.rs`）会在 P4 阶段改名为 `VmProvider`。
- **一个端口的宿主机实现叫 `Host<Thing>`。** **惯例（观察所得）**：`delonix-linux/src/workload.rs:HostWorkload`（实现 `WorkloadRuntime`）、`delonix-sdn/src/run_network.rs:HostNetwork`（实现 `NetworkProvider`）、`delonix-oci/src/run_images.rs:HostImages`、`delonix-volume/src/lib.rs:HostVolumes`、`delonix-linux/src/cdi.rs:HostDevices`。
- **纯粹的决策函数**是动词或疑问句的形式：`resolve_*`、`parse_*`、`valid_*`、`is_*`、`*_plan`。**惯例（观察所得）**：`cmd/vm.rs:resolve_vm_defaults`、`delonix-oci/src/registry.rs:parse_content_range`、`cmd/stack.rs:is_pending`、`delonix-net-rules/src/lib.rs:bridge_name`。
- **常量**用 `SCREAMING_SNAKE_CASE`（**强制（门禁）**，clippy 的 `-D warnings` 下的 rustc lint）。**Kind 的名字也一律是常量，从不用重复的字符串字面量**（**已决定**：AGENTS.md § "Os Kinds ganham grupos e nomes definitivos"；这些常量在 `crates/contexts/delonix-stack/src/kinds.rs` 里：`pub const VM: &str = "VirtualMachine";`）。

### 3.4 测试名

- **棘轮会统计测试名。** `lang_ratchet.py` 匹配每一个 `fn` 声明，并不会跳过 `#[cfg(test)]`，所以一个葡萄牙语的测试名会让 `identifiers` 上升，让门禁失败。**强制（门禁）**。
- 许多既有测试用的是葡萄牙语的句子式命名，例如 `delonix-model/src/exitcode.rs:nao_existe_e_rebentou_deixam_de_ser_o_mesmo_numero`。那是被计入统计的债务，不是要照抄的写法。新测试要用**陈述所要证明的行为的英语句子**，就像同一个文件里较新的那些测试：`a_missing_capability_is_not_a_wrong_argument` 和 `the_text_class_and_the_number_cannot_diverge`。**已决定**：LANG-01（AGENTS.md）；"句子式"这个形式是**惯例（观察所得）**。
- 如果你翻译了一个既有测试的名字，计数会下降，所以要在同一个提交里跑一次 `python3 scripts/lang_ratchet.py --update`。

### 3.5 CLI 命令与标志

- **命令要分组，形如 `delonix <group> <verb>`。** 不要有扁平的顶层快捷方式。**已决定**：AGENTS.md § "Reorganização da raiz da CLI (v0.30.0)"；`docs/cli-stability.md`（那些顶层快捷方式已在 v1.0.0 中被移除）。
- **有对应动词存在时，就跟随 Docker/Podman/kubectl 的用词。** **已决定**：AGENTS.md § "Reestruturação da CLI (semântica Docker/Podman/kubectl)" 系列 sprint；`docs/cli-stability.md` § "Estável"。
  - 列举用的动词是 `ls`（`network ls`、`volume ls`、`image ls`……）。`image list` 在 v2.0.0 中被改回了 `ls`（`docs/cli-stability.md`）。
  - `create` 只负责创建，遇到已存在的名字会以退出码 5 拒绝，除非带上 `--force`。Upsert 是另一个独立的动词（`secret set`）。`apply` 是幂等的"确保存在"。**已决定**：AGENTS.md § "Sprint 1: `secret create` vs `secret set`"。
  - `describe` 面向人（kubectl 风格），`inspect` 是给脚本用的 JSON。**已决定**：AGENTS.md § "Output: `ls` estilo docker, `describe` estilo kubectl"。
  - 在 Docker 已有对应概念的地方，标志的顺序和名字要照搬 Docker：`network connect <NETWORK> <CONTAINER>`、`-p [hostIp:]hostPort:containerPort`、`volume create --driver … --opt k=v`。**已决定**：AGENTS.md § Sprint 5 和 6。
- **不兼容的改动要干净利落地切断，不留别名。** 旧的写法必须以 `unrecognized subcommand` 失败，绝不能悄悄地做别的事情。在你切断之前，先在**整个** workspace 里 grep 一遍内部调用者。**已决定**：`docs/cli-stability.md` § "Como uma quebra é feita"。该文件里列为稳定的那些组，只能在大版本发布时才允许破坏兼容。
- **一个从多条路径都能到达的命令，必须在所有路径上都接好**（例如 `vm pull` / `image vm pull` / `image --vm pull`）。**已决定**：`CONTRIBUTING.md`；见[贡献流程](contributing-workflow.md#adding-or-changing-a-cli-command)。
- **叶子命令的变动要在同一个提交里更新 CLI 基线**（`scripts/cli-tree.sh --update`）。**强制（门禁）**：见[贡献流程](contributing-workflow.md#adding-or-changing-a-cli-command)。

### 3.6 Kind、API 组和清单字段

- **Kind 是 `UpperCamelCase` 形式的名词**，属于已发布分组中的一个：`core`、`compute`、`networking`、`gateway`、`storage`、`artifact`、`infrastructure`（`<group>.delonix.io/v1alpha1`）。**已决定**：AGENTS.md § "Identidade e fronteira do motor" 和 § "Os Kinds ganham grupos"（引入这些分组的 ADR-0020 目前仍是**提议中**）。每个 Kind 在 `crates/contexts/delonix-stack/src/kinds.rs` 里都是**一行**（`KindFacts`：`kind`、`plural`、`short`、`api_version`、`domain`、`form`、`in_stack`、`converges`……）。`delonix api-resources` 会打印那张表。新增一个 Kind 还要动到那些没有从 `kinds.rs` 派生的表（`hot_fields`、`NAMESPACE_SOURCES`、`TYPED_KINDS`、生成的 schema）。在每一个都改完之前测试都会失败。**强制（门禁）**：AGENTS.md § "`kind: Service`" 列出了每张表分别由哪个测试抓住。
- **一个改了名字的 Kind 会把旧名字保留为一个不区分大小写、默认不打印警告的别名。** 一次**合并**则会给出警告，因为它的含义变了。**已决定**：AGENTS.md § "Os Kinds ganham grupos e nomes definitivos"（"Alias silencioso, não depreciação"）；实现在 `cmd/manifest.rs:KIND_ALIASES`。ADR-0020 目前仍是**提议中**。
- **清单字段用 `camelCase`。** 如果某个字段以前是 `snake_case` 的拼法，那个拼法会作为 `serde` 的 `alias` 继续被接受。**惯例（观察所得）**，按字段逐个处理，而不是用 `rename_all`：

  ```rust
  // bins/delonix-runtime-bin/src/cmd/vm.rs — VmSpec
  /// Canonical `cpuAffinity`; `cpu_affinity` stays accepted (back-compat).
  #[serde(rename = "cpuAffinity", alias = "cpu_affinity")]
  cpu_affinity: Option<String>,
  ```

  其他例子：`delonix-compute/src/pod.rs:PodSpec.restart_policy`（`rename = "restartPolicy"`）、`cmd/service.rs:ServiceSelector.match_labels`（`rename = "matchLabels"`）。已发布的 schema（`docs/schema/v1/delonix.json`）是从这些结构体**生成**的，并有测试保证它和这些结构体一致（ADR-0007）。清单的 schema 被声明为**稳定**（`docs/cli-stability.md` § "O schema dos manifestos"）。
- **内部记录**（state root 下的那些 JSON）保留 Rust 的 `snake_case` 字段名。见 `crates/contexts/delonix-compute/src/record.rs`（`net_mode`、`namespace`）。**惯例（观察所得）**。

### 3.7 环境变量

- **前缀 `DELONIX_`**，全部大写：`DELONIX_ROOT`、`DELONIX_NET_RUNTIME_DIR`、`DELONIX_L18N`、`DELONIX_LOG_FORMAT`、`DELONIX_CRI_CAP_CEILING`。**惯例（观察所得）**，贯穿 `crates/` 和 `bins/`。
- **遥测（telemetry）要读标准的 `OTEL_*` 变量**，而不是新建一个 `DELONIX_*` 别名。**提议中（ADR，尚未决定）**：ADR-0040 D6。这是目标，不是今天的代码现状：`crates/adapters/delonix-telemetry/src/telemetry.rs` 为 OTLP 导出器读取 `DELONIX_OTLP_ENDPOINT`，而在标准变量集里只读 `OTEL_SERVICE_NAME`。不要新增一个 `DELONIX_*` 遥测变量，也不要在迁移它的那个阶段之外移除 `DELONIX_OTLP_ENDPOINT`。
- **一个会削弱某个安全默认值的逃生舱口，要做得响亮且明确。** 除非设为 `1`，否则它是关闭的，而且一旦开启就会发出警告：`DELONIX_ENABLE_IPV6=1`、`DELONIX_ALLOW_LINK_LOCAL=1`。**已决定**：AGENTS.md § "Bloco 0 do plano 33 (v0.37.1)"。
- 标志优先于环境变量，环境变量优先于默认值（`serve cri --cap-ceiling` 相对于 `DELONIX_CRI_CAP_CEILING`）。**已决定**：AGENTS.md § "Tecto de capabilities no CRI"。

### 3.8 退出码与 `DX_*` 代码

- **退出码在一个地方从错误类型推导而来**：详尽穷举的 `crates/foundation/delonix-model/src/exitcode.rs:for_error`。各个类别是 1（通用）、2（用法错误）、3 `NOT_RUNNING`、4 `NOT_FOUND`、5 `CONFLICT`、69 `UNAVAILABLE`、74 `IO`、77 `NO_PERMISSION`、124 `TIMEOUT`。每个错误还有一个稳定的文本身份，`Error::code()`，会返回一个 `DX_*` 字符串。**强制（门禁）**：那个 match 没有 `_ =>` 分支，所以一个新的变体会让构建停下来，直到有人把它归类。测试 `the_text_class_and_the_number_cannot_diverge` 让这两者保持同步。**已决定**：AGENTS.md § "Códigos de saída com classe (v0.49.0)"；`docs/cli-stability.md` § "Códigos de saída"。
- **你不是自己挑一个数字，而是返回正确的那个变体。** "不存在"是 `Error::NotFound`，"已经存在"是 `Error::Conflict`，"这台宿主机缺少某个工具"是 `Error::Unavailable`。一个新数字需要一个真实的生产者。**已决定**：`exitcode.rs` 的模块文档（"every extra number is a promise"）。

---

## 4. 结构：代码该放在哪里

### 4.1 层与依赖方向

允许的依赖方向写在一个地方，`scripts/arch_fitness.py` 里的 `ALLOWED`。**强制（门禁）**：

| 层 | 可以依赖 |
|---|---|
| foundation | foundation |
| context | foundation, context |
| adapter / provider | foundation, context |
| interface | foundation, context, adapter, provider |
| bin | 全部 |

开发依赖和构建依赖不计入统计。一个声明的例外必须指名会移除它的那个 ADR-0040 阶段，一个已经不再适用的例外同样会让检查失败（`EXCEPTIONS`）。生成的层级表和当前的例外列表在[架构](architecture.md#layers-and-the-allowed-direction)里。

更多结构性规则，每一条都由 `scripts/arch_fitness.py` **强制（门禁）**：

- **基础层和各上下文都不带重量级依赖。** `tokio`、`axum`、`hyper`、`tonic`、`reqwest`、`clap`、`ratatui`、`serde_yaml`、`rmcp`、OpenTelemetry 和 `prometheus-client` 在那里都会被拒绝（`HEAVY`）。
- **一个二进制只组合恰好一个接口。** 在 `rule_failures()` 里强制执行（由 ADR-0040 D2.4 引入，目前仍是**提议中**；AGENTS.md § "A direcção das dependências é um portão" 里也写了这一点）。
- **依赖版本号只存在于根目录的 `[workspace.dependencies]` 里。** 一个成员只写 `{ workspace = true, features = [...] }`，别的都不写。`default-features = false` 只留在根目录，因为一个成员无法关掉根目录已经打开的东西（`inline_versions()`）。
- **库不打印任何东西。** `library_prints` 棘轮统计 `bins/` 之外的 `println!`/`eprintln!`/`print!`。改用 `tracing`（例如 `crates/adapters/delonix-sdn/src/lib.rs` 里的 `tracing::warn!`），让接口层来呈现输出。**已决定**：AGENTS.md § "A direcção das dependências é um portão (ADR-0040, fase P0)"（"Uma biblioteca não escreve para o terminal; emite `tracing`"）。
- **库不会重新执行引擎自己的二进制文件。** `self_exec_sites` 棘轮统计 `bins/` 之外的 `current_exe()`、`cli_bin()` 和 `delonix_bin()`。要调用一个函数或一个用例来代替。`Command::new("ip")`、`nft`、`qemu-img` 和 `ssh` **不**被计入统计，因为运行这些工具正是一个适配器存在的意义（`SELF_EXEC` 上方的注释）。
- **不要写进进程环境。** `env_writes` 棘轮统计所有地方（测试也包括在内）的 `env::set_var`/`remove_var`。测试运行在并行线程上，一次写入会和每一个读取者发生竞争。改为把值作为参数传进去（`ENV_WRITES` 上方的注释）。

### 4.2 "我的改动是 X → 它应该放进 Y"

这张表用的是 crate 今天实际的样子。在往某个 crate 里加东西之前，先去[crate 一览](crates.md)查一下它的内容。凡是来源一栏引用 ADR-0040 或 ADR-0026 的地方，标签都是**提议中（ADR，尚未决定）**：这两份 ADR 目前都仍是提议中，尽管它们描述的那些 crate 已经存在。

| 你的改动 | crate（层） | 来源 |
|---|---|---|
| 一条关于 CIDR、桥接名或 IPAM 算术、且双方都必须算出完全相同结果的纯规则 | `delonix-net-rules`（foundation） | 06；AGENTS.md § Arquitetura |
| 一个新的错误类别、退出码或 `DX_*` 代码；生成的名字 | `delonix-model`（foundation） | ADR-0040 D1 |
| 一种被持久化的工作负载记录类型（`Container`、`Vm`、`Mount`……） | `delonix-compute`（context）：`record.rs` | ADR-0040 D2.2；#406 |
| 一条没有机制、纯粹是数据的记录（`Status`、`ContainerFw`/`FwRule`、`typestate`） | `delonix-model`（foundation）：`records.rs`、`typestate.rs` | ADR-0040 P3（#405） |
| 一个向宿主机或某个进程提出的问题（`now_unix`、pid 存活判断、user namespace、id 生成）、事件日志、服务器分发规则、`SO_PEERCRED` | `delonix-node`（context） | ADR-0040 D2.2；#406 |
| 一条 secret 模型的纯规则（`Secret`、合法的名字和 key、env-file 解析） | `delonix-model`（foundation）：`secret.rs` | ADR-0040 P3（迁移 store 的那个 PR） |
| 一个 store、文件锁、`write_atomic*`/`write_private_temp`、加密的 secret store 或凭证保险库 | `delonix-state`（adapter） | ADR-0040 D2.3 |
| Kind 的事实表、planner/diff、condition、revision | `delonix-stack`（context） | AGENTS.md § Arquitetura |
| 运行规格（run specification）、它的纯校验逻辑、run 用例需要的某个端口 | `delonix-compute`（context）：`run_opts.rs`、`preflight.rs`、`ports.rs` | ADR-0040 D2.2 |
| 安全策略、准入（admission）、评分、脱敏 | `delonix-security-runtime`（context） | ADR-0026 |
| namespace、cgroup、挂载、capabilities、seccomp、设备 | `delonix-linux`（adapter） | ADR-0040 D2.3 |
| netns holder、nftables、slirp、DNS、DHCP、overlay、WireGuard、CNI | `delonix-sdn`（adapter） | ADR-0040 D2.3 |
| registry 客户端、CAS、层（layer）、overlay、镜像构建 | `delonix-oci`（adapter） | ADR-0040 D2.3 |
| SBOM / CVE | `delonix-scanner`（adapter） | ADR-0040 D2.3 |
| Tracing、OpenTelemetry、Prometheus registry 的搭建 | `delonix-telemetry`（adapter） | ADR-0040 D2.3 |
| 一个本地 VM 后端（Cloud Hypervisor、libvirt） | `delonix-vm`（adapter） | ADR-0008 |
| 一个远程或可插拔的 provider（hypervisor 的 API、NAS 的 API） | `crates/providers/` 下的一个 provider crate。**先写一份 ADR** | ADR-0008、ADR-0009；[贡献流程](contributing-workflow.md#when-to-write-an-adr) |
| 一个 CRI RPC | `delonix-cri`（interface） | AGENTS.md |
| 本地管理 API、`/metrics` | `delonix-mgmt`（interface） | ADR-0010 |
| 一个 MCP tool | `delonix-mcp`（interface） | ADR-0025 |
| 一个 CLI 命令、它的呈现方式和翻译 | `bins/delonix-runtime-bin/src/cmd/<group>.rs` + `data/pt.po` | AGENTS.md § CLI |
| 哪个适配器承载哪个端口（组合） | 二进制的组合根（composition root）。那里不放业务逻辑 | ADR-0040 D1 "Binaries" |

### 4.3 纯粹的核心，I/O 在边缘

- **决策是作用于你已经读到手的数据的纯函数。** 它们不接受 store、不运行任何命令、不需要任何特权，所以测试可以用普通的值去调用它们。**提议中（ADR，尚未决定）**：ADR-0040 D1（一个上下文的 `domain/` "no I/O, no `tokio`, `libc`, `nix`, `std::fs`"）。**惯例（观察所得）**：`delonix-stack/src/reconcile.rs`（"decides it WITHOUT touching the machine"）、`cmd/vm.rs:resolve_vm_defaults`、`delonix-sdn/src/infra.rs:vmtap_line`、`delonix-oci/src/registry.rs:parse_content_range`。

  ```rust
  // bins/delonix-runtime-bin/src/cmd/stack.rs — is_pending
  fn is_pending(present: &str, kind: &str, status: &str) -> bool {
      match present {
          // Declarative: nothing to observe, so nothing to wait for.
          "-" => false,
          "yes" => !ready_status(kind, status),
          // "no" (absent) and "?" (unknown/unreadable) both keep waiting.
          _ => true,
      }
  }
  ```

- **如果一个纯函数需要外部的某样东西，就把它当作参数接收进来。** 举例来说，`resolve_image_ref` 接收镜像 store 作为参数，而不是自己去打开那个真实的 store，这样测试就可以传一个临时目录进去。**已决定**：AGENTS.md § "O manifesto de VM resolvia a imagem de outra maneira que a CLI"。
- **一条规则，一个主人。** 当两处调用点需要同一种推导时，抽出一个函数，让两处都调用它。第二份拷贝会渐渐走偏。例子：`delonix_net_rules::bridge_name`，由 `delonix-sdn` 重新导出（桥接名以前有两套公式，会打印出一个根本不存在的设备名）、`infra::dhcp_lease_ip`、`effective_entrypoints`。**已决定**：AGENTS.md § "`delonix network`"、§ "Isolamento de namespace"、§ "Reverse-proxy L7"。

---

## 5. 内部 API 与边界

### 5.1 端口与适配器

- **一个新后端要实现一个端口。它绝不能在别的什么地方写成一个 `if provider == …`。** **已决定**：AGENTS.md § "Identidade e fronteira do motor"（"Um provider novo entra como implementação de uma porta, nunca como um `if provider == …`"）。ADR-0040 D3 规则 3（"No string matching on provider names outside the composition root"）重申了这一点，目前仍是**提议中**。D3 计划为此写一个 fitness test，但**它在 `arch_fitness.py` 里还不存在**，所以现在靠评审来执行这条规则。
- **和某个具体后端相关的知识要放在那个后端里。** 例如，`VmBackend::ip_is_predicted()` 会回答一台 VM 的 IP 是不是靠预测得到的，而不是让调用点去检查 `backend.contains("cloud-hypervisor")`。**已决定**：ADR-0008，被引用在 `crates/adapters/delonix-vm/src/lib.rs` 的 doc comment 里。
- **一个适配器不依赖另一个适配器。** 它需要的、属于另一个关注点的东西，要以 hook 或端口的形式进来，由组合根来接线。**强制（门禁）**：`ALLOWED`（adapter → foundation、context）。**惯例（观察所得）**：`delonix-linux/src/workload.rs:HostWorkload` 的 doc comment 就是这样解释它的 `addresses`/`attach_slirp` 这两个 hook 的。

  ```rust
  // crates/contexts/delonix-compute/src/ports.rs — NetworkProvider (excerpt)
  pub trait NetworkProvider {
      /// Refuses a network that does not exist.
      fn check_network(&self, name: &str) -> Result<()>;
      /// Undoes an attach; best effort, used on the way out of a failure.
      fn detach(&self, id: &str, ip: &str);
      /// Publishes one `-p` specification on a container's address.
      fn publish(&self, ip: &str, spec: &str) -> Result<()>;
  }
  ```

- **一个 trait 落地时需要一个真实的消费者。** 不要写那种等着第一个调用者的脚手架代码。每个方法都必须有一个调用者。**已决定**：AGENTS.md § "`delonix workload`"（ADR-0002）。一个没有调用者的公开函数同样是一个已知的隐患：其中好几个后来都被发现藏着一个潜伏的 bug（`mount_live`、`set_net_rate`、`update_limits`、`publish_port_allow`），有些干脆被删掉，而不是被接上线（AGENTS.md § "Endurecimento do ingress/egress"）。

### 5.2 各 crate 自己的错误类型（ADR-0040 P3）

- **一个适配器或 provider 定义自己的 `Error`，并把它转换成共享的错误类别**（`delonix_model::Error`），后者带有 `DX_*` 代码。**提议中（ADR，尚未决定）**：ADR-0040 P3。
  **强制（门禁）**：`arch_fitness.py` 里的 `shared_error_imports` 棘轮（`SHARED_ERROR`，只限定在 `crates/adapters/` 和 `crates/providers/`）会统计那些把共享类型当成本 crate 自身结果类型的 `use delonix_model::{…Error/Result…}` 导入。共享类型仍然可以在一个 `From` 实现内部被点名使用。参照实现是 `crates/adapters/delonix-scanner/src/error.rs`：

  ```rust
  impl From<Error> for Dx {
      fn from(e: Error) -> Self {
          match e {
              e @ (Error::EmptySbom | Error::OsvShape | /* … */ Error::NoModule) => Dx::Invalid(e.to_string()),
              Error::ModuleScan(io) => Dx::Runtime { context: "module scan", message: io.to_string() },
              Error::Engine(e) => e,
          }
      }
  }
  ```

  这个文件里有三处体现了这个模式：转换逻辑**决定**了类别；`code()` 会去问这个转换逻辑，而不是自己再维护第二张表；还有一个测试（`the_code_is_the_code_of_the_class_it_converts_into`）让两者保持同步。转换后的消息也和 CLI 以前打印的那个消息保持逐字节一致（`the_converted_message_is_the_one_printed_before`）。
- **去问错误本身它属于哪个类别；不要对共享错误的某个变体做匹配。** 在 foundation 之外，要写 `e.is_not_found()` 或 `e.class()`，或者在需要负载内容时对 `e.root()` 做匹配——绝不要写 `Err(Error::NotFound(_))` 或 `matches!(…, Error::NotFound(_))`。一个 crate 自己的错误会带着它自己的代码，旅行在共享类别内部，所以一次针对变体的匹配会在编译器不吭一声的情况下悄悄失效。**已决定**：ADR-0043 D4（已接受）。
  **强制（门禁）**：`arch_fitness.py` 里的 `raw_error_variant_matches` 棘轮（`RAW_VARIANT_MATCH`，跳过 `crates/foundation/delonix-model/`），其基线为 0——任何新增的此类原始匹配都会让 CI 失败。相关方法在 `crates/foundation/delonix-model/src/codes.rs` 里。

### 5.3 可见性

- **默认私有。** 在 CLI crate 内部，当另一个 `cmd` 模块需要调用某样东西时，用 `pub(crate)`。**惯例（观察所得）**：`cmd/container.rs:cmd_run`、`cmd_start` 和 `cmd_stop` 都是 `pub(crate)`，好让 `pod`、`compose` 和 `stack` 可以委托给它们；`cmd/firewall.rs:update_locked`；`cmd/manifest.rs:KIND_ALIASES`。
- **一个库 crate 里的 `pub` 是对其他 crate 的承诺。** 移除一个公开项，对使用这个库的人来说是一个破坏性变更，即使在这个 workspace 里它的调用者数量是零也一样。rustc 的 `dead_code` lint 看不见没有被使用的 `pub` 项，所以当你删除一个时，要自己手动数一数有多少个公开项因此变成了孤儿。**已决定**：AGENTS.md § "`delonix_sdn::Net` foi APAGADO — e é breaking para quem usa a biblioteca"。

### 5.4 节点契约

`proto/delonix/node/v1` 是 gRPC 和 HTTP/JSON 这两种编码方式共同的真相来源。`docs/api/openapi.yaml` 是从它生成出来的。**永远不要手动编辑那份 OpenAPI 文件。** **强制（门禁）**：`scripts/contract_gate.py` 会跑 `buf format`、`buf lint`、针对最近一个包含 `proto/` 的 tag 的 `buf breaking`，检查每个 RPC 的 HTTP 映射，并检查那份 OpenAPI 文件是否和生成出来的那份一致。**已决定**：AGENTS.md § "O contrato de nó é um portão"（引用了 ADR-0040 P1；ADR-0040 D4 目前仍是**提议中**）。

- **每个 RPC 只有一个请求消息**，命名为 `<Rpc>Request`。**强制（门禁）**：`buf lint` 的 `RPC_REQUEST_STANDARD_NAME`（见 `buf.yaml`）。一个共享的请求消息会让某个本该只属于一个方法的字段出现在五个方法里。
- **请求里的身份是显式的**：`name` 和 `namespace` 字段，绝不用一个带着引擎会忽略的字段的通用元数据消息。**已决定**：AGENTS.md。**惯例（观察所得）**：`compute.proto:GetContainerRequest { string name = 1; string namespace = 2; }`。
- **镜像通过 query 寻址，而不是放在路径里**，因为在像 `alpine:3.20` 这样的路径里，`:` 会被当成一个自定义动词来解析。**已决定**：AGENTS.md。**惯例（观察所得）**：`infra.proto` 的 `GetImage` → `get: "/v1/images:get"`，配合 `GetImageRequest { string reference = 1; }`。
- **响应**返回资源本身，或者对于长时间运行的变更操作返回一个 `Operation`。`buf lint` 里关于响应命名的规则被有意关掉了（`buf.yaml` 里的注释）。
- **每个 RPC 都要有 HTTP 映射，双向流式的除外**（`Exec`、`Console`），它们必须没有映射。**强制（门禁）**：`contract_gate.py` 的第 4 项检查。

### 5.5 引擎不认识任何消费者

引擎不知道是谁在用它。任何平台、control plane、控制台或 agent 的名字，以及任何关于租户、账户、套餐或计费的概念，都不能出现在 `crates/`、`bins/`、`proto/`、根目录的 `Cargo.toml` 或 `Makefile` 里，**注释也包括在内**。

- **消费者的名字。** **强制（门禁）**：`scripts/arch_fitness.py` 的 `consumer_mentions()` 会在这些路径里匹配 `CONSUMER_NAMES` 这个正则表达式，那是一份固定的名单。不在这份名单里的名字不会被抓到。
- **租户、账户、套餐和计费这些概念。** **已决定**：AGENTS.md § "Identidade e fronteira do motor"。没有门禁去匹配它们；由评审来检查。

如果某个消费者需要什么东西，就用引擎自己的词汇，把它写成一种通用的能力（Kind 和资源），并且只有在它对任意客户端都说得通时才加进来。引擎会校验自己的契约，绝不指望调用方去替它拒绝它自己都不支持的东西。

---

## 6. 错误处理与消息

- **不允许静默失败。** 如果一个选项被接受了却又被悄悄忽略，那比根本没有这个功能还糟，因为用户会以为它生效了。要用一个清楚的错误拒绝它，并且点名那个标志。**已决定**：AGENTS.md § "Falhas silenciosas corrigidas (fail-closed)"；v0.37.0 的那次审计（§ "Auditoria sistemática dos 208 subcomandos"）把这一类问题叫做"relato desonesto"（不诚实的汇报）。那一节列出的模式有：
  - **在确认某个对象确实归你销毁之前，不要动手销毁任何东西，账本要最后才删。** 如果记录先被移除、随后数据的移除又失败了，数据就成了孤儿，之后的一次 `create` 会把它交给别人。
  - **一次读不出来的测量结果是*未知*，绝不是零。** 一个失败的 `read_dir` 不等于一个空目录。这正是 `Usage { bytes, unreadable }` 存在的原因。
  - **留意那些会把失败变成成功的写法**：对一个重要结果（熵、一次 socket 读取）用 `let _ =`；对一个 `f64` 用 `as u64`（它会饱和截断）；用 `capture()` 的 `Result` 而不是它的输出去判断结果。AGENTS.md § "A classe «X não é Y»" 把这些都归了类。
- **未知或无法测量的东西，不能靠猜。** 当引擎没办法读到一个值时，它要么报告自己不知道，要么直接拒绝。它不会去挑那个更可能对的答案。例子：`system prune --auto` 在读不出磁盘占用时会拒绝执行，IPAM 的回收器在某个 store 读不出来时会失败关闭（一份空列表会被读成"什么都没活着"）。**已决定**：AGENTS.md § CLI（`system prune`）、§ "O IPAM vaza"。
- **消息的形状：先说事实，再说该怎么做**，有确切命令时就给出那个确切命令。**已决定**：AGENTS.md § "`-p 80:80` respondia com o JSON cru do slirp"（"facto primeiro, depois os comandos prontos a copiar"）。**惯例（观察所得）**：`delonix-model/src/error.rs:Error::VmNotFound` → `"no such VM: {0} (see \`delonix vm ls\`)"`；`cmd/config.rs:refuse_unknown_key` 会点名有效的 key 有哪些。要点名**缺失的工具及其所属的包**，而不是原样传出一个 `ENOENT`，后者读起来像是缺了一个文件（AGENTS.md § "A bateria mede o `--help` de tudo"，achado 1）。
- **一次不完整的测量不是一次成功。** `--wait` 必须真的去观察它所声称的那件事，`✓ … is up` 只能在检查过之后才打印出来。**已决定**：AGENTS.md § "O `--wait` de uma VM CH"。
- **绝不要靠解析错误消息来决定该做什么。** 消息是会被翻译的（`--l18n=pt`/`DELONIX_L18N`），所以一个 `grep 'no such'` 在一台机器上能分类，到另一台机器上会悄悄地不再起作用。要用退出码类别或 `DX_*` 代码。**已决定**：`delonix-model/src/exitcode.rs` 的模块文档。
- **返回和该类别匹配的那个变体**（`NotFound`、`Conflict`、`NotRunning`、`Unavailable`、`Timeout`），不要什么都用 `Invalid`。**已决定**：AGENTS.md § "Códigos de saída com classe"：`util::find` 对"没找到"返回 `Invalid`，让最常用的那种资源变得没法分类。

---

## 7. `unsafe`、系统调用与进程

- **每一个 `unsafe` 块的正上方都要有一句 `// SAFETY:` 注释。** **强制（门禁）**：`[workspace.lints.clippy]` 里的 `undocumented_unsafe_blocks = "deny"`。

  ```rust
  // crates/adapters/delonix-linux/src/lib.rs — apply_filter_logged
  // SAFETY: `fprog` points to a valid BPF program; NO_NEW_PRIVS is already set.
  let rc = unsafe {
      libc::syscall(libc::SYS_seccomp, SET_MODE_FILTER, FLAG_LOG, &fprog as *const _)
  };
  ```

  这句注释必须说明使这次调用得以成立的那个不变量。只有紧跟在一次相同的、已经给出理由的调用之后，才可以写"same"（就像 `delonix-linux/src/lib.rs` 里紧跟在第一个 `_exit(126)` 之后那样）。
- **在多线程进程里（tokio 的那些服务器、Docker API 的 shim）不允许原始的 `clone()`/`fork()`。** `clone` 不会执行 `pthread_atfork` 的处理函数，所以子进程可能会在 malloc 的锁上死锁。要改为重新执行一份类型化的 spec，通过一个 `0600`/`O_EXCL` 的文件传递，而不是通过 argv。**已决定**：AGENTS.md § "Auditoria de segurança #3"，第 5 项。背景：[面向本代码库的 Rust 入门](rust-primer.md#why-forkclone-in-a-multi-threaded-process-is-dangerous)。
- **一个 `pre_exec` hook 不能等待父进程在 `spawn` 返回之后才做的事情。** `Command::spawn` 只有在 `exec` 之后才会返回，所以两个进程会永远互相等待对方。握手场景要用一次原始的 `fork`。**已决定**：AGENTS.md § "A classe «X não é Y»"（`reexec_mapped_hold` 那一条）。
- **临时文件：用 `delonix_state::write_private_temp`。** 它以一个唯一的名字、`O_EXCL` 和 `0600` 模式打开，所以永远不会跟着一个被人预先埋好的符号链接走。不要在 `/tmp` 下用固定名字或从 pid 派生的名字。**已决定**：AGENTS.md § "Auditoria de segurança #3"，passagem 2；`crates/adapters/delonix-state/src/store.rs` 里的 doc comment。**惯例（观察所得）**：`delonix-sdn/src/bpf.rs`、`delonix-linux/src/run_host.rs`。
- **必须私密或原子写入的文件：用 `write_atomic_mode(path, bytes, Some(0o600))`。** 它在创建时就设好模式，并通过一次原子的 rename 来发布。绝不要先写文件再 `chmod`，因为在这个间隙里别的用户可以打开它。**已决定**：`delonix-state/src/store.rs:write_atomic_mode` 的 doc comment；AGENTS.md（kubeconfig 的 TOCTOU）。
- **在向一个从文件里读出来的 pid 发信号之前，先确认它还是同一个进程。** 用 `delonix_node::safe_to_signal(pid, starttime)`，它会比较进程的启动时间，避免一个被回收再利用的 pid 被误杀。**已决定**：AGENTS.md § "A classe «X não é Y»"（pid 相关的那几条）。
- **一个进程的 argv 并不能证明它是我们自己的。** 同一个用户下别的 state root、以及别的工具，都可能以相同的 argv 运行。要检查一个只有我们自己会选择的 token：一个从我们的 root 派生出来的路径，或者一个我们在 spawn 时钉死的环境变量。**已决定**：AGENTS.md § "A classe «X não é Y»"（"o argv de um processo" 那一条）。
- **在外部工具**（`ssh`、`scp`、`virsh`、`mount`、`qemu-img`）**的 argv 里，来自输入的位置参数前面要加 `--`**，并且要用字符白名单去校验任何最终会进入远程 shell 的值。`shell_quote` 不会对内容做任何消毒处理。**已决定**：AGENTS.md § "Auditoria de segurança (skill `delonix-runtime-sec`)" 和 § "#2"。
- **从用户或清单输入的名字拼出来的路径必须被限制住。** 在引擎边界用一个 `valid_*` 名字检查（`delonix_vm::valid_vm_name`），再加上一个会拒绝 `..`/绝对路径分量和符号链接的安全拼接函数（`safe_join`、`safe_bind_target`）。**已决定**：AGENTS.md 中的同几节。

---

## 8. 状态与并发

- **读—改—写要走 `update`，绝不能是 `load` → 改 → `save`。** `Store::update` 和 `JsonStore::update`（`crates/adapters/delonix-state/src/store.rs`）会拿一把 `flock`，**在锁内重新读一遍**，套用你的闭包，再原子地写回去。一个返回 `false` 的闭包会中止这次写入。CLI、CRI 服务器和后台刷新任务都会并发地碰同一批记录，没有这把锁，一次写入会悄无声息地丢失。**已决定**：这两个函数的 doc comment；AGENTS.md § "Revisão ampla de código/arquitectura (2026-07-27)"，第 5 号 bug 和关于 `JsonStore` 的那一条。**惯例（观察所得）**：一个自身可能失败的变更是这样包装的：

  ```rust
  // bins/delonix-runtime-bin/src/cmd/firewall.rs — update_locked
  let c = store.update(id_or_name, |c| match f(c) {
      Ok(commit) => commit,
      Err(e) => {
          err = Some(e);
          false
      }
  })?;
  ```

- **每一步一旦被数据平面确认，就立刻持久化。** 如果一次多步骤的变更在中途失败，记录必须仍然和内核实际拥有的状态保持一致。**已决定**：AGENTS.md § "Reconfiguração a quente"（"Persistência"）。
- **持久化记录上的新字段要带 `#[serde(default)]`**（或 `default = "fn"`），这样旧版本写下的记录才能继续加载。默认值必须描述旧记录实际上是什么，而不是一个猜测。**已决定**：AGENTS.md（例如 `Vm.namespace`、`VmImage.cloud_init`）。**惯例（观察所得）**：`delonix-compute/src/record.rs`，`Vm.namespace` 的 doc comment（"the default is a statement of fact and not a guess"）。
- **重建一个资源所需要的一切都必须被持久化，而不能只在创建时用一下就丢掉。** 当你改动某个 `start`/`restart` 路径时，要逐个字段比较创建时用到的东西和记录里保存的东西。**已决定**：AGENTS.md § "BUG GRAVE corrigido… `-v` nunca era persistido"（在那里被列为同一类问题里的第三个 bug）。
- **锁文件永远不会被删除。** 删除一个锁文件会打开一个窗口，让两个进程锁住不同的 inode。**已决定**：`store.rs:lock_path`（`delonix-state`）的 doc comment。
- **`SecretStore::update` 是唯一一个锁是尽力而为（best-effort）的 `update`。** 它的 `FileLock::acquire` 会返回 `Option`，如果锁文件打不开就不加锁继续往下走，这和 `Store`、`JsonStore` 不一样。**未决定**：它是否也该像其他两个一样直接拒绝；跟着周围的代码走，如果你动了它，就在 PR 里说明这一点。

---

## 9. 测试

- **放在哪里。** 单元测试放在文件底部的 `#[cfg(test)] mod tests` 里。集成测试放在 `crates/<layer>/<crate>/tests/`，只使用公开 API。针对真实 provider 的测试是可选启用的。**惯例（观察所得）**；细节见 [03 § 3.9](rust-primer.md#39-tests)。
- **先纯粹再说。** 把决策放进一个纯函数，把它当作数据来测试。任何需要真实 namespace、cgroup 或网络 holder 的东西，都靠实机验证或 `scripts/e2e.sh` 来验证。**已决定**：`CONTRIBUTING.md`（"Write a unit test for any new pure function"）；AGENTS.md § "IaC nativo"（`reconcile.rs` 是纯粹的，所以能被当作数据来测试）。
- **命名**：陈述所验证行为的英语句子（见[§3.4](#34-test-names)）。
- **测试绝不能碰宿主机的真实状态。** 给 store 一个临时根目录。不要调用那些会解析出真实 state root 的代码。不要用 `set_var`（`env_writes` 棘轮，见[§4.1](#41-the-layers-and-the-direction)）。**已决定**：AGENTS.md § "IaC nativo"，`ShareVolume` 合并那条方法论笔记（"Nota de método: um teste que chamasse `apply_share` … escreveria no estado REAL da máquina"）。**惯例（观察所得）**：`delonix-state/src/store.rs` 里的测试用了一个 `tmp_dir(tag)` 辅助函数。对手动和 E2E 测试而言，要**同时**隔离 `DELONIX_ROOT` 和 `DELONIX_NET_RUNTIME_DIR`。只隔离一个比一个都不隔离还糟（AGENTS.md § "Meia-isolação é pior que nenhuma"；[克隆、构建与测试](build-and-test.md#isolating-the-engines-state)）。
- **一个回归测试必须在修复被回退之后失败。** 把修复回退掉，看着测试失败，再把修复恢复回来。一个在两种情况下都能通过的测试什么都证明不了，AGENTS.md 里记录了好几个这样的例子（一个 `1` 没办法区分开的退出码检查；一个在被回退之后依然是绿色的混沌场景）。**已决定**：AGENTS.md，全篇多处出现"verificado pela regra do repo"，例如 § "IaC nativo"（`stack_converge`）和 § "A bateria mede o `--help`"。
- **要测试生产环境实际会走的那条路径。** 如果生产环境传的是相对路径，测试也要用相对路径。一个测试是可能把 bug 也一起写进去的。**已决定**：AGENTS.md § "Auditoria sistemática dos 208 subcomandos"（`default_project_name`）。
- **并发类的 bug 需要一次真实的竞争。** 用线程，再在临界窗口内加一个明确的 sleep。**惯例（观察所得）**：`delonix-state/src/store.rs:jsonstore_update_concorrente_nao_perde_escritas`。
- **优先验证属性，而不是采样的时序。** 当一次竞争只能靠采样来发现时，就制造负载并重复多次。**已决定**：AGENTS.md § "Um `exec` logo a seguir ao `run -d` corria no HOST"。
- **生成的区域不携带易变的数字**（行数、测试数、提交数）。**已决定**：`scripts/dev_docs.py` 的 docstring（"Deliberately NOT generated: line counts, test counts, commit counts"）。一次**手写的测量**要连同测量当天的日期一起引用，绝不是一个不断累加的总数。**已决定**：AGENTS.md § "A bateria mede o `--help` de tudo e EXECUTA um quarto"（"Cita-se a fracção medida e a data, nunca o total"）。数字是否可以出现在正文或代码注释里，这件事本身**未决定**——跟着周围的文字走。

---

## 10. 注释与文档

- **注释要解释*为什么*，而不是*做了什么*。** 为一个隐藏的约束、一个针对具体 bug 的变通办法、或者一个代码本身没有把它说明白的不变量写一句注释。复述代码本身的注释会在评审中被删掉。**已决定**：`CONTRIBUTING.md` § Style。
- **当一个决定是靠测量得出的，就说清楚测的是什么。** "Measured: …" 比"should"更有说服力。`exitcode.rs` 和 `reconcile.rs` 里的模块文档就是这方面的范本。**惯例（观察所得）**。
- **公开项和模块头部的 doc comment（`///`、`//!`）**要说明这个项承诺了什么、为什么存在。**惯例（观察所得）**：`delonix-compute/src/ports.rs` 里的每一个端口、`delonix-state/src/store.rs:write_private_temp`、`exitcode.rs`。**未决定**：没有启用 `missing_docs` 这个 lint。
- **注释用英语写，也不点名任何消费者。** 语言棘轮和消费者门禁都会扫描注释。**强制（门禁）**。
- **不要为根本不可能发生的情况添加抽象、配置开关或错误处理。** **已决定**：`CONTRIBUTING.md` § Style。
- **当你移动一条边界时，要在同一次改动里更新它对应的记录**：
  - 对于一个新的 provider、端口、守护进程、引擎 crate 里的外部依赖、特权边界，或者对契约或层级的改动，要**先**写一份 ADR，再动代码。已被接受的 ADR 永远不会被重写；只有新的 ADR 能取代它们（**已决定**：`docs/adr/README.md`；[贡献流程](contributing-workflow.md#when-to-write-an-adr)）；
  - 更新描述那块区域的那节 `AGENTS.md`。一节过时的内容会误导下一个人，而且**`AGENTS.md` 里的冲突要靠保留双方内容来解决**（**已决定**：AGENTS.md § "Método: um worktree por sessão"）；
  - 更新生成出来的那些文件：`docs/schema/v1/delonix.json`、`docs/api/openapi.yaml`、CLI 基线、`docs/gen.py`，以及通过 `python3 scripts/dev_docs.py` 生成的 `docs/dev/` 各区域（**强制（门禁）**；见[发布文档](publishing-docs.md)）。

---

## 未决问题

仓库里的任何东西都没有把这些问题定下来。跟着周围的代码走，并在你的 PR 里提一下这个选择：

- **`#[allow(clippy::…)]` 什么时候可以接受。** 它确实被用到了（`too_many_arguments`），但没有一份写下来的政策。
- **今天 compute 端口的 `Result` 类型。** `delonix-compute/src/ports.rs` 里的端口返回的是 `delonix_model::Result`，所以实现它们的那些适配器（`HostWorkload`、`HostNetwork`……）要导入这个共享的结果类型，而这些导入会被计入 `shared_error_imports`。P3 阶段会决定每个 crate 各自的错误方案。目前没有任何文档说明一个端口的签名该怎么改，而在一个新文件里加上这样一条导入就会让棘轮失败。如果你的改动确实需要这样做，在 PR 里提出来，不要绕开那条正则表达式。
- **过渡期里 crate 的命名。** ADR-0040 D2（目前仍是提议中）定下了目标命名，但如果一个全新的 crate 在它所属的上下文还不存在之前就落地了（例如在 `delonix-provider-*` 系列改名之前出现了第二个 provider），目前没有写下来的规则。先在 issue 里问一下。
- **是否要求写 doc comment。** 没有启用 `missing_docs` 这个 lint，只有观察到的习惯。
- **针对 provider 名字匹配的 fitness test**（ADR-0040 D3 规则 3）已经提出，但尚未实现。

---

## 评审清单

在开 PR 之前，过一遍这份清单：

1. `cargo fmt --all --check` 和 `cargo clippy --workspace --all-targets --locked -- -D warnings`
   都是干净的。→ [§1](#1-the-tooling-that-enforces-style)
2. `python3 scripts/lang_ratchet.py` 和 `python3 scripts/arch_fitness.py` 都能通过，任何你降低过的
   基线都在这个提交里。→ [§1](#1-the-tooling-that-enforces-style)
3. 新增的标识符、注释、消息**以及测试名**都是英语。面向用户的文本走 `po::t`/`po::tf`，并带上
   `pt.po` 条目。不要用 `num`。→ [§2](#2-language-of-the-code)、[§3.4](#34-test-names)
4. 代码放在正确的 crate 和层里，版本号只在根目录声明，库里没有 `println!`，也不会重新执行
   自己的二进制。→ [§4](#4-structure-where-code-goes)
5. 决策是带单元测试的纯函数，I/O 留在边缘。→
   [§4.3](#43-pure-core-io-at-the-edges)
6. 新后端要实现一个端口，不做 provider 名字匹配。适配器不依赖适配器。→
   [§5.1](#51-ports-and-adapters)
7. 适配器/provider 的失败要用一个能转换成 `delonix_model::Error` 的 crate 级 `Error`。→
   [§5.2](#52-errors-per-crate-adr-0040-p3)
8. 如果你动了 `proto/`：`scripts/contract_gate.py` 要通过，做到每个 RPC 一个请求消息、身份显式、
   OpenAPI 已重新生成。→ [§5.4](#54-the-node-contract)
9. `crates/`、`bins/` 或 `proto/` 里任何地方都没有消费者的名字，注释也包括在内。→
   [§5.5](#55-the-engine-knows-no-consumer)
10. 没有什么是先被接受、后又被悄悄忽略的。错误先陈述事实，再说该怎么做，并使用正确类别的
    变体。→ [§6](#6-error-handling-and-messages)、[§3.8](#38-exit-codes-and-dx_-codes)
11. 每一个 `unsafe` 块都有一句 `// SAFETY:` 注释。多线程进程里没有原始的 fork，临时文件用
    `write_private_temp`，secret 用 `write_atomic_mode`。→
    [§7](#7-unsafe-syscalls-and-processes)
12. 记录的变更都走 `update`，新的记录字段都有 `#[serde(default)]`。→
    [§8](#8-state-and-concurrency)
13. 测试用了隔离的根目录，回归测试在修复被回退之后会失败，一个测量出来的数字带着它的日期。→
    [§9](#9-tests)
14. CLI 的改动：所有入口都接好了线，动词和 Docker 对齐，不兼容的切断没有别名，CLI 基线已经
    更新。→ [§3.5](#35-cli-commands-and-flags)
15. Kind 和清单字段：在 `kinds.rs` 里有一行，字段用 `camelCase`、旧拼法作为别名保留，schema
    已重新生成。→ [§3.6](#36-kinds-api-groups-and-manifest-fields)
16. 如果某条边界发生了移动，ADR、`AGENTS.md` 和生成的文档都已更新。→
    [§10](#10-comments-and-documentation)

---

**下一篇：** [新增一个 Kind](adding-a-kind.md) —— 通过一个真实的例子，走一遍新增一个声明式 Kind 所需要的表、schema 和协调器接线。
