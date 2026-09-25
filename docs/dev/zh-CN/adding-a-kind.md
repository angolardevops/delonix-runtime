<!-- translated-from: adding-a-kind.md sha256:b5a2233cec9f575cc201a08988e44f84c483edc92591778fb2b44afc200baaec -->
# 新增一个 Kind

**阅读之前：**[编码规范](coding-conventions.md)、[架构](architecture.md) 和 [各个 crate](crates.md#delonix-stack) —— 本页假设你已经知道 `delonix-stack` 拥有什么、以及为什么规划（planning）是纯函数式的。

Kind 是这个引擎能理解的声明式资源 —— `Container`、`Volume`、`Service` 等等
（`delonix api-resources` 会把它们全部列出来）。新增一个 Kind 涉及的不只是一处 `match`
分支：描述这个 Kind 是什么的那张表、应用它的代码，以及让 `stack plan`/`apply` 能像对待
其他 Kind 一样对待它的协调器接线。本页依序走一遍这个过程，全程以一个真实的 Kind ——
**`Service`**（ADR-0032）—— 作为完整示例。它不是最新的 Kind（`IPPool`、`NetworkGateway`、
`NetworkZone` 和 `RuntimePolicy` 都是在它之后出现的），但它是唯一一次性用到全部路径的
Kind：primary、可收敛、可移除、按命名空间隔离，并拥有自己的注册表。更新的那些 Kind 也走过
同样的步骤；下面引用的每个文件都是从代码树里读到的，不是凭对旧结构的记忆写的。

## 一个 Kind 必须回答的这张唯一的表

`crates/contexts/delonix-stack/src/kinds.rs` 是「一个 Kind 是什么」的唯一来源。它自己的
模块文档解释了为什么要有它：在这张表出现之前，关于一个 Kind 的同一批事实分散在六份独立的
清单里（`KINDS`、`CONVERGING_KINDS`、`TEARDOWN_KINDS`、`kind_honors_namespace`、等待测试里
的 `DECLARATIVOS`，以及 `presence()` 的各个分支），而它们会走样 —— `Vm`、`FirewallPolicy`
和 `ShareVolume` 都曾经拿到了一个协调器适配器，却仍然留在 `CONVERGING_KINDS` *之外*，于是
收敛这一步悄无声息地跳过了它们，而它们各自的 `apply` 又通过错误的路径，让资源看起来是对的。

新增一个 Kind，从一行 `KindFacts` 开始：

```rust
pub const SERVICE: &str = "Service";
```

```rust
KindFacts {
    kind: SERVICE,
    plural: "services",
    short: &["svc"],
    api_version: "networking.delonix.io/v1alpha1",
    domain: Domain::NetConnectivity,
    form: Form::Primary,
    in_stack: true,
    stack_group: "services",
    converges: true,
    teardown: true,
    namespaced: Namespaced::Always,
    presence: Presence::Registry,
},
```

每个字段都是一项决定，不是走个形式：

| Field | 它回答的问题 | 答错的后果 |
|---|---|---|
| `kind` | 名称，以 `const` 的形式存在 —— 绝不用裸的字符串字面量。在这张表出现之前，一次牵动所有调用点的改名，实测涉及**十个文件里的 106 处**；用 `const` 能让打错字变成编译错误，而不是一个悄无声息、永远匹配不到的 `match` 分支（一个拼错的 `&'static str` 模式会退化成一个兜底的*绑定*，crate 自己在 `SECRET`/`NETWORK` 等常量上的文档注释点名提到了这一点）。 | 出现一个谁也认不出来的第二个 Kind，或者一次漏掉某处、却没有构建错误的改名。 |
| `plural` | `delonix get <plural>` 接受的那个词。它是独立的字段，不是 `kind` 加上 `"s"` —— `Dependency`→`dependencies`、`Ingress`→`ingresses` 都不遵循这条规则。 | `delonix get services`（正确）对上瞎猜出来的 `delonix get servicess`。 |
| `short` | 可接受的缩写。刻意保持稀疏 —— 有一个测试强制要求全局唯一，所以一个和别的 Kind 缩写冲突的 shortname 会让构建失败，而不是悄悄把它遮住。 | 两个 Kind 抢答同一个三字母缩写。 |
| `api_version` | 清单里写的那个 `apiVersion`，自 ADR-0020 起按域拆分（`compute.delonix.io/…`、`networking.delonix.io/…` 等）。之所以是一列而不是共享常量，正是为了让各个 Kind 能一个一个地迁移到拆分后的方案。 | 某个 Kind 卡在旧的 `delonix.io/v1` 字符串上，或者分到了错误的组。 |
| `domain` | 作用范围，显示为 `stack ls`/`plan --fields` 里的 `DOMAIN` 列。三个网络相关的域是刻意拆开的：`NetConnectivity` 回答「路径是否存在」，`NetPolicy` 回答「这条路径上的流量是否被允许」—— 把它们合并会掩盖一个事实：`NetworkRoute` 打开一条路径，而 `FirewallPolicy` 决定要不要放行走这条路径的流量。 | 一个域回答的问题，跟这个 Kind 实际作用的对象对不上。 |
| `form` | 这个 Kind 的文档会变成什么：`Primary`（有自己的 apply，会在 load 之后继续存在）、`Sugar(target)`（在 load 时被改写成另一个 Kind，自身消失）、`Aggregate`（展开成它内部包含的那些文档，像 `Stack` 一样）、`Compat(target)`（一种外来的 schema —— `Ingress` 就是 `networking.k8s.io/v1` —— 被编译到另一个 Kind 的机制上，和 `Sugar` 不同的是它在 load 之后会*继续存在*），或者 `Sunset(target)`（仍然是 primary，但已经宣布了继任者；用在改写会悄悄改变引擎实际*行为*的场合 —— `Container` 不能降级成一个单成员的 `Pod`，因为一个 Pod 总是会构建一个共享的 netns，那是另一种运行时形态）。 | 给一个必须保留自己 apply 的东西选了 `Sugar`，或者反过来。 |
| `in_stack` | `stack apply` 到底会不会处理它。**`in_stack: true` 的那些行必须留在表格开头连续的一段** —— `destroy` 是靠反转 stack 顺序来推导拆除顺序的，所以把一行放在某个非 stack 的 Kind 后面，就会在没有人动过任何「顺序」字段的情况下改变 apply 的顺序。一个测试（`os_kinds_do_stack_sao_um_prefixo_contiguo`）强制要求这一点。 | 一个 Kind 没按依赖顺序应用，或者像 `KubernetesCluster` 这样的远程过程型 Kind（对已经存在的主机执行 SSH，不是本地资源）被错误地接进了这个循环。 |
| `stack_group` | `kind: Stack` 的 `spec` 里容纳这个 Kind 文档的那个键（`services:`），如果这个 Kind 没法被分组，就是 `""`。这一列决定了展开方式：`expand_stack`、schema、未知字段告警，以及生成的文档全都读它，不存在第二份分组清单（见下文 [Stack 分组](#the-stack-group)）。它跟 `in_stack` 回答的不是同一个问题 —— `Workload` 和 `Dependency` 在 load 时就被降级，所以它们不是作为自身被应用的，但它们又都是有人会写在 Stack 里面的东西。 | `stack apply` 会处理、却放不进 Stack 里的一个 Kind —— 在分组清单还是手写的年代，`NetworkRoute`、`NetworkAccessRule`、`Service` 和 `App` 都遇到过这种事。 |
| `converges` | 一个*被改动*的字段是否真的会被应用，而不只是「确保存在」。`false` 是合理的取值 —— `Secret` 的状态是加密后的值，规划时不会解密来比较 —— 但它需要一个理由（见下文 `not_converged_reason`）；一个笼统的借口会让测试失败。 | 某个 Kind 在每次规划时都报 `!`，理由却读起来像「没人管它」，而真相其实是这个资源本身的性质。 |
| `teardown` | `destroy_one` 能不能移除它，从而让 `--prune` 和 `destroy` 能够作出这个承诺。一个测试（`so_um_kind_convergente_tem_teardown`）会拒绝 `teardown: true` 却 `converges: false` 的 Kind —— 那等于承诺去修剪一样连规划都无法表示为「已改动」的东西。 | `--prune` 承诺了移除，`destroy_one` 却在拆除顺序里靠前的那些 Kind 已经被清掉之后、运行到一半时拒绝执行。 |
| `namespaced` | `Never`、`Always`，或者 `PerDocument`。不是一个 `bool` —— `Volume` 真的有三种答案：普通 volume 没有命名空间，带 `share:` 块的才有；如果把它建模成 `true`，就会在每一个普通 volume 上都警告「namespace 没有效果」；建模成 `false`，又会在一个 share 上发出同样错误的警告 —— 而这个 share 的命名空间恰恰决定了它的数据存放在哪个目录里。 | 命名空间告警的内容，跟这个 Kind 自己的 `apply` 对该字段的实际处理相矛盾。 |
| `presence` | `stack ls`/`wait` 是怎么知道这个资源存不存在的：`Registry`（有个 store 能回答是或否）、`Derived`（从别的东西算出来 —— 一个 Pod 就是它那些带标签的成员）、`Declarative`（没有东西可以读回来；这个资源是应用在某个目标上的一条指令，`presence()` 回答 `-`，这*不是*「不存在」的意思），或者 `NotObservable`（永远不会走到 `presence()` —— 它在 load 之后不会继续存在，或者根本不是本地资源）。 | `NetworkRoute` 曾经在 `presence()` 里完全没有分支，落进了 `_ => ("?", "unsupported kind")` —— 被 `ls`/`describe` 打印出来，还被 `wait` 读成永远处于挂起状态。 |

`Service` 这一行用一句话概括就是：由 stack 应用，紧跟在它所选择的那些 compute Kind 之后；
在 Stack 里分组到 `services:` 下；真的存在一条路径（`NetConnectivity`）；是 primary；能在
不重建的情况下收敛；能被拆除；始终带命名空间；并且有一个真正的注册表在背后支撑。

## Stack 分组

一个 `in_stack: true` 的 Kind 必须有一个 `stack_group`，`kinds.rs` 里的四个测试把这一列
钉在原地（ADR-0045）：

- **`every_kind_applied_by_the_stack_has_a_group`** —— 凡是 stack 会应用的东西，都不能缺席
  于 `kind: Stack`。
- **`a_kind_without_a_group_says_why`** —— 一行 `stack_group: ""` 需要在
  `stack_group_absent_reason` 里有对应条目（`Stack` 自己、`KubernetesCluster`），而有分组的
  那一行不能出现在那里。
- **`a_group_key_is_unique_and_no_alias_shadows_it`** —— 两个 Kind 共用一个键，会把它们的
  子项合并到一起，给其中一个塞进错误的 spec。
- **`a_group_is_the_plural_of_its_kind`** —— 键是用 lowerCamelCase 写的复数形式
  （`NetworkRoute` 对应 `networkRoutes`），这样谁都不用去查。只有三个较早的键是例外
  （`ingress`、`vms`、`firewallPolicies`），因为改一个分组的名字会破坏所有已发布的 Stack。

然后这个分组还得在 `bins/delonix-runtime-bin/src/cmd/` 里被端到端地证明出来：

- `manifest.rs` 里的 **`stack_group_sample`** —— 这个分组里一个子项最简的 `spec`，以及它
  最终落地成的 Kind；`every_stack_group_loads` 会遍历每一个分组，遇到没有样例的就失败。
- **`examples/stack.yaml`** —— 必须提到这个分组；否则 `the_stack_example_names_every_group`
  会失败。这个文件同时也是用户站点 Kinds 页面所展示的内容。
- `schema.rs` 里的 **`every_stack_group_is_typed_against_its_kinds_own_spec`** —— 分组里的
  条目要按 Kind 自己的 spec 来做类型校验，所以这个 Kind 得先有 `TYPED_KINDS` 分支（见下一节）
  才能校验它的分组。

## Spec 类型与 schema

带类型化清单 spec 的 Kind（大多数都是）需要一个
`#[derive(Deserialize, Serialize, JsonSchema)]` 结构体 —— 本例里是
`bins/delonix-runtime-bin/src/cmd/service.rs` 里的 `ServiceSpec` —— 以及
`bins/delonix-runtime-bin/src/cmd/schema.rs` 里 `TYPED_KINDS` 的一个分支，再加上
`manifest_schema` 里指名这个结构体的对应分支。这个常量同时供给 `delonix manifest schema`
和 `delonix explain <Kind>.<field>`，两者都是从同一个结构体生成的（ADR-0007），所以发布出去
的 schema 不可能和代码实际接受的东西走样。把某个 Kind 排除在外是一个真实的、被允许的状态
—— `Storage` 和 `ShareVolume` 就刻意没有 schema，因为它们在 load 时会被改写成 `Volume`，
第二个结构体只会是 `Volume` 自身字段的手工复制品 —— 但这件事必须*说出来*：
`untyped_hint(kind)` 给出具体的指引，`todo_kind_conhecido_tem_schema_ou_dica`（在
`schema.rs` 里）会在表里认识的某个 Kind 既不在 `TYPED_KINDS` 里、也没有 hint 的时候让构建
失败。那条通用的提示信息（"no typed schema for X"）读起来像是清单的 bug；而这条 hint 说的是
这是这个 Kind 本身的性质。

还有两个地方会读这个 spec，都在 `bins/delonix-runtime-bin/src/cmd/manifest.rs` 里：

- **`filled_spec`** —— 一个调用这个 Kind 的 `spec_with_defaults(doc)` 的分支，也就是让
  `stack apply --dry-run` 和 `manifest render` 打印出「每个默认值都已填好」的那趟经过类型化
  结构体的往返。
- **`spec_fields_for`** —— 一个返回这个 Kind 的 `*_SPEC_FIELDS` 清单的分支，
  `warn_unknown_fields` 就是拿这份清单来检查一份文档的。schema 里 `additionalProperties:
  false` 所接受的那些键，取的正是同一份清单，所以一个字段名打错字，两边都能抓到。

**然后重新生成发布出去的 schema**，因为这是一个编辑器会去获取的文件，而不是靠人手工维护的
副本：

```bash
delonix manifest schema > docs/schema/v1/delonix.json
```

`o_schema_publicado_esta_em_dia_com_o_codigo`（在 `schema.rs` 里）会一直失败，直到这个
文件和二进制程序生成的内容完全一致为止。请用你自己代码树构建出来的二进制程序
（`target/release/delonix` 或者 `cargo run -p delonix-runtime-bin --`），而不是 `PATH`
上那个。

## 把 Kind 接入协调器

协调器的规划函数（`crates/contexts/delonix-stack/src/reconcile.rs::plan`）是纯函数 —— 它从
不打开 store，也不执行命令。凡是会碰到机器本身的东西，都在
`bins/delonix-runtime-bin/src/cmd/stack.rs` 里，并以 `cmd::kinds`/`cmd::reconcile` 的名字
从 `delonix-stack` 重新导出（`cmd/mod.rs` 里的 `pub use delonix_stack::kinds;`，所以当这张
表搬进自己的 crate 时，任何已经在调用 `cmd::kinds::…` 的地方都不用改）。`stack.rs` 里有四个
函数，为一个会收敛的新 Kind 各需要一个分支：

1. **`desired_of`** —— 每份文档对应一个 `reconcile::Desired`，其中 `fields` 用**清单**里的
   字段名作键（`matchLabels`、`port`），绝不用内部记录的字段名，因为这份 diff 是写 YAML 的
   那个人要看的。`Service::desired` 就是从 `ServiceSpec` 构建出这个的。
2. **`actual_of`** —— 机器上实际存在的这个 Kind 的每一个实例，好让 `--prune` 有东西可以拿来
   比较。`Service::actual` 读取 `delonix_sdn::infra::service_list()`，并且填上和 `desired`
   用的*同一批*字段名，不然 diff 就成了拿苹果比橙子。
3. **`converge_and_stamp`** —— 实时应用一个 `Action::Update`。`Service` 在这里复用了自己的
   `apply_one`（`converge_doc`），因为这个函数本来就会完整地覆写注册表条目 —— 和
   `FirewallPolicy`、`NetworkAccessRule` 用的是同一种形状，理由也一样：为每个字段单独开一条
   路径，等于用第二种方式写同一条记录，而两种方式迟早会互相打架。一个完全没有实时更新路径的
   Kind（`Pod` 没有 hot field）会明确返回「没有实时更新路径」的错误，而不是悄悄地落空。
4. **`stamp_all`** —— 在一次成功的 apply 之后，记录所有权（`delonix.io/stack` 标签）和已
   应用的字段映射（`delonix.io/last-applied` 注解），正是这一步把下一次规划从两方 diff 变成
   了三方 diff。`Service::stamp` 把这些直接写在 service 自己的注册表条目上 —— 和
   `NetworkAccessRule` 不同（它的规则挂在一个它并不拥有的*容器*上），`Service` 有一条完全
   属于自己的记录。

还有 `destroy_one`，当 `teardown: true` 时，它需要一个调用 `remove_for_replace` 的分支 ——
`Service::remove_for_replace` 只是去调用 `delonix_sdn::infra::service_remove`。

**这一切都不是给每个 Kind 单独发明的。** `run_layers`（同样在 `stack.rs` 里）是一份文档
第一次真正被*创建*出来的地方，按表里的顺序，每个 `in_stack` 的 Kind 占一层
（`layers.run(k::SERVICE, "🧭", || super::service::apply(docs))?`）—— 那个 `apply(docs)`
函数就是命令式 CLI 分组早就有的那一个，是复用而不是重复实现。

## 不用重建就能收敛的字段

如果 `converges: true`，就要在 `reconcile.rs` 的 `hot_fields` 里加一条，明确写出哪些清单
字段可以*实时*改动。这张表是执行器必须遵守的承诺 —— 列出一个收敛步骤实际上没法应用的
字段，会把一次干净的 `Replace` 变成一次运行到一半失败的 `Update`，模块文档把这种情况称为
「比提前声明要重建还要糟糕」。`Service` 列出的是 `["matchLabels", "port"]`，因为
`service::apply_one` 已经会完整覆写这条记录，不需要重启 —— `Service` 身上没有什么是「冷」
的。一个没被写进 `hot_fields` 的字段照样会出现在规划里；只不过它会强制走 `Action::Replace`
（没有 `--replace <Kind>/<name>` 就会被拒绝），而不是 `Action::Update`。

`cmd/stack.rs` 里有三个测试，让这张表对齐 `kinds.rs` 那张表，也让它们彼此对齐：

- **`as_tres_listas_de_kinds_convergentes_concordam`** —— 每个标了 `converges: true` 的
  Kind 都必须出现在 `--fields` 的输出里（`compared_fields_table`），反过来也一样；而且一个会
  收敛的 Kind 绝不能带着 `not_converged_reason` 那条通用借口。
- **`todo_kind_nao_convergente_tem_razao_especifica`** —— 反过来：每一个*不*收敛的 Kind
  都需要在 `not_converged_reason` 里有一句属于自己的具体说法，而不是共用的
  `NOT_CONVERGED_GENERIC`。
- **`todo_kind_convergente_tem_teardown_ou_razao`** —— 一个会收敛的 Kind，要么可以被移除
  （`kinds::has_teardown`），要么在 `no_teardown_reason` 里有对应条目；绝不会两者都有，也
  绝不会两者都没有。

## Presence，供 `ls`/`describe`/`wait` 使用

在 `stack.rs` 的 `presence(kind, doc, containers)` 里加一个分支，说明一个实例是否存在，
如果存在，它的状态字符串是什么。`Service` 的分支会检查
`delonix_sdn::infra::service_get(namespace, name)`，如果找到了，还会数一数它的 selector
当前匹配到多少个活着的容器（读的是 DNS 解析器自己读的*同一份*索引，所以规划结果绝不会和
一个客户端实际查询这个名字得到的结果对不上）。有两个测试专门守着这一列：

- **`todo_o_kind_declarativo_de_kinds_tem_braco_no_presence`** —— 每一个 `presence` 为
  `Declarative` 的 Kind，`presence()` 真的会回答 `-`（而不是 `NetworkRoute` 在拥有自己的
  注册表之前落入的那个兜底 `_ => ("?", "unsupported kind")`）。
- **`um_kind_declarativo_nao_fica_pendente_para_sempre`** —— 对每一个 declarative 的 Kind，
  `is_pending("-", kind, status)` 都是 `false`。`stack wait` 曾经只把 `present == "yes"`
  当作就绪的唯一标志，所以只要清单里有*任何*一个 declarative 的 Kind，就会把整个 `--timeout`
  都耗在等一个这种 Kind 永远产生不出来的标记上。

## 通用动词与 `drift`

`delonix get`/`describe`/`delete <plural>` 是按 Kind，通过
`bins/delonix-runtime-bin/src/cmd/verbs.rs` 里的三份清单 —— `GET_ROUTES`、
`DESCRIBE_ROUTES` 和 `DELETE_ROUTES` —— 加上每个动词各一个、调用该 Kind 自己的
`cmd_ls`、`cmd_describe` 和 `remove_for_replace` 的分支来路由的。一个回答不了这些动词的
Kind，就把障碍写进 `no_verb_reason`（`Stack` 是从文件读的，`Workload` 在 load 时就降级了，
……）。要清楚这个门禁做什么、不做什么：`a_kind_never_both_routes_and_claims_it_cannot`
**只有**在一个 Kind 既路由了、又声称自己不能路由时才会失败；一个哪份清单都不在的 Kind，
只会在测试运行时在 stderr 上产生一行 `not wired yet: …`，而 `delonix get <plural>` 会对
用户回答 `not wired yet`。要读那一行；构建不会替你拦下来。

`delonix drift` 会拿 `last-applied` 印记和机器上实际持有的状态做比较，大多数 Kind 都可以
从自己的 store 里枚举出来。如果你的 `actual()` 需要已解析的文档才能回答 —— 节点没有一份能
自己列出这个 Kind 的注册表，就像 `NetworkPolicy` 是以 nft 规则的形式挂在目标上一样 —— 就把
它加进 `bins/delonix-runtime-bin/src/cmd/drift.rs` 里的 `DOC_SCOPED`。
`doc_scoped_matches_the_stack_wiring` 会读 `stack.rs`（调用点**以及** `actual` 的函数
签名），一旦清单和接线对不上就会失败；一个接收了 `docs` 却把它们晾在一边的模块（`fn
actual(_docs: …)`）不该出现在这份清单里。

## 命名空间与补全

如果这个 Kind 是带命名空间的（`namespaced != Namespaced::Never`），它还需要在
`NAMESPACE_SOURCES`（`bins/delonix-runtime-bin/src/cmd/complete.rs`）里有一条 —— 要么是
读取该 Kind 自己 store 的 `NsSource::Store(fn)`，要么是命名空间实际落在另一个 Kind 的记录
上时的 `NsSource::Via("OtherKind — reason")`（比如一个 `Pod` 的命名空间是记在它成员
`Container` 身上的）。否则 `every_namespaced_kind_declares_a_source` 会失败 —— 漏掉这一点
的实际后果是：`--namespace` 的 shell 补全永远没法给出一个「唯一资源就是这个新 Kind」的
租户。

## 清单

对于一个行为和 `Service` 一样的 Kind（primary、可收敛、有 teardown、带命名空间）：

1. `kinds.rs` —— 一个 `pub const` 名称和一行 `KindFacts`，包括它的 `stack_group`（或者在
   `stack_group_absent_reason` 里的对应条目）。
2. 一个带 `JsonSchema` 的 spec 结构体；`TYPED_KINDS` 和 `manifest_schema`（`schema.rs`）
   里的分支 —— 或者在 `untyped_hint` 里说明为什么没有。
3. `manifest.rs` —— `filled_spec`（`spec_with_defaults`）里的分支、`spec_fields_for` 里的
   分支，以及 `stack_group_sample` 里的样例；分组要写进 `examples/stack.yaml`。
4. `stack.rs` —— `desired_of`、`actual_of`、`converge_and_stamp`、`stamp_all`、
   `destroy_one`、`presence` 里的分支，以及 `run_layers` 里调用该 Kind 自己 `apply(docs)`
   的那一层。
5. `reconcile.rs` —— `hot_fields` 里的一条，写明哪些字段能实时收敛。
6. 如果这个 Kind 不收敛，或者没法被拆除：在 `not_converged_reason` / `no_teardown_reason`
   （`stack.rs`）里写一句具体的理由。
7. `verbs.rs` —— 把这个 Kind 连同它的分支写进
   `GET_ROUTES`/`DESCRIBE_ROUTES`/`DELETE_ROUTES`，或者在 `no_verb_reason` 里写理由。
8. `drift.rs` —— 只有当 `actual()` 真的需要文档本身时，才加进 `DOC_SCOPED`。
9. 如果带命名空间：在 `NAMESPACE_SOURCES`（`complete.rs`）里加一条。
10. 代码里每一个新的面向用户的字符串都用英文写，并在 `bins/delonix-runtime-bin/data/pt.po`
    里翻译；`DX-CDNN` 字典（`crates/foundation/delonix-model/src/codes.rs`）里新增的错误码，
    也需要它的葡语文本（`every_dictionary_text_has_a_portuguese_translation`）。运行
    `python3 scripts/lang_ratchet.py`。
11. 用代码树的二进制程序执行 `delonix manifest schema > docs/schema/v1/delonix.json`，再
    执行 `python3 docs/gen.py <该二进制程序>`，让用户站点的 Kinds 页面跟着更新。
12. `cargo test -p delonix-runtime-bin -p delonix-stack` —— 上面点名的那些测试，才是抓住
    某一步被跳过的东西，而不是靠评审者肉眼看 diff。

一个 `Sugar`/`Aggregate` 的 Kind（在 load 时被改写或展开，像 `Workload` 或 `Stack` 那样）
可以跳过这里的大部分内容：`um_kind_que_baixa_para_outro_nao_pertence_ao_ciclo_do_stack`
要求这两种 form 必须是 `in_stack: false` 且 `converges: false`，真正的工作是在改写发生的
`manifest::load` 里完成的。

---

**下一篇：**[贡献流程](contributing-workflow.md) —— worktree、版本对齐、语言规则、什么
时候要写 ADR，以及怎么把改动发出去。
