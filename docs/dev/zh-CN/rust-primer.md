<!-- translated-from: rust-primer.md sha256:1816db1c294bfdfdb88795ccb366f48135a04bbd7d7d37815302e6d006a262c8 -->
# 本代码库的 Rust 入门

**阅读之前：** [云原生入门](cloud-native-primer.md)（示例用到其中的词汇），以及基础 Rust（[The Rust Programming Language](https://doc.rust-lang.org/book/) 第 1–10 章）。

这不是一篇 Rust 教程。它是*阅读这个仓库*所需要的那部分 Rust，每个概念都钉在一个你可以打开的文件上。如果某个概念对你来说是新的，"延伸阅读"链接会指向官方资料；回到这里看引擎是怎么用它的。读完之后，你就能打开工作区里的任何一个 crate，一路追踪它的错误类型、trait、`unsafe` 块和测试，而不会被语言本身绊住。

路径都是相对于仓库根目录的。符号的命名是为了方便你 `grep`——行号被有意省略了，因为它们随每个 PR 都会变动。

固定下来的工具链写在 `rust-toolchain.toml` 里（一个固定的 `channel`，加上 `rustfmt` 和 `clippy`）。`rustup` 会自动遵守它，所以你构建时用的编译器和 CI 一样。

---

## 3.1 Cargo 工作区（The Cargo workspace）

这个仓库是一个 Cargo **workspace**（工作区）：根目录有一个 `Cargo.toml`，里面是 `[workspace] members = [...]`，每个 crate 各有一个自己的 `Cargo.toml`。这里有三条约定值得注意：

1. **每一个路径和每一个版本号都只写一次，写在根目录里。** 根目录有一张 `[workspace.dependencies]` 表，列出了引擎自己的 crate（用 `path`）以及每一个第三方 crate（用 `version`）。一个成员 crate 从不写版本号；它写的是：

   ```toml
   # crates/contexts/delonix-node/Cargo.toml
   [dependencies]
   delonix-model = { workspace = true }
   serde = { workspace = true }
   serde_json = { workspace = true }
   libc = { workspace = true }
   ```

   一个成员可以加上 `features = [...]`，仅此而已。`default-features = false` 留在根目录里，因为一个成员没办法关掉工作区已经打开的默认特性。`scripts/arch_fitness.py`（函数 `inline_versions`）会在某个成员写了自己的版本号时让构建失败。

2. **目录就是层（layer）。** Crate 分别位于 `crates/foundation/`、`crates/contexts/`、`crates/adapters/`、`crates/providers/`、`crates/interfaces/`，二进制程序位于 `bins/`（ADR-0040）。`scripts/arch_fitness.py` 会拒绝一个目录和它声明的层不匹配的 crate，也会拒绝一条指向不被允许方向的依赖。层的规则会在课程后面讲到，见
   [架构 — 层与允许的依赖方向](architecture.md#layers-and-the-allowed-direction)。

3. **Lint 是继承下来的。** 根目录声明了 `[workspace.lints.clippy]`，其中
   `undocumented_unsafe_blocks = "deny"`，每个成员用 `[lints] workspace = true` 来选择加入。因此每一个 `unsafe` 块都带有一句 `// SAFETY:` 注释（见 §3.4）。

根目录还设置了 `[workspace.package]`（共享的 `version`、`edition`、`license`），成员用 `version.workspace = true` 来引用它。这个版本号不是装饰：规则见
[版本对齐](contributing-workflow.md#version-alignment)，版本门禁见
[CI 运行的门禁](build-and-test.md#the-gates-ci-runs)。

**延伸阅读：** Cargo Book ——
[Workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html)，
[`[workspace.dependencies]`](https://doc.rust-lang.org/cargo/reference/workspaces.html#the-dependencies-table)，
[`[lints]`](https://doc.rust-lang.org/cargo/reference/manifest.html#the-lints-section)，
[`rust-toolchain.toml`](https://rust-lang.github.io/rustup/overrides.html#the-toolchain-file)。

---

## 3.2 错误处理：一个 `Error`，退出码由它的类型派生而来

几乎引擎里每一个可能失败的函数都返回 `delonix_model::Result<T>`，这是对定义在
`crates/foundation/delonix-model/src/error.rs` 里的那个共享枚举的一个别名。这个枚举位于纯粹的基础 crate `delonix-model` 里，引擎里的任何其他 crate 都可以依赖它。一些适配器定义了自己的错误类型，再把它转换成这一个（[编码约定](coding-conventions.md) §5.2）。它是用
[`thiserror`](https://docs.rs/thiserror) 构建的：`#[derive(Error)]` 会根据
`#[error("...")]` 属性生成 `Display`，`#[from]` 会生成 `From` 实现，这样 `?` 就能自动转换一个更底层的错误：

```rust
// crates/foundation/delonix-model/src/error.rs
#[error("I/O error: {0}")]
Io(#[from] std::io::Error),
```

这些变体说的是**调用者接下来该做什么**，而不是哪个子系统出了故障：`NotFound`、`NotRunning`、`Conflict`、`Unavailable`（缺少某个工具或内核特性）、`Timeout`、`Invalid`、`Registry`、给一次失败的系统调用用的 `Runtime { context, message }`，等等。每个变体上的 doc comment 都解释了它为什么存在；在新增一个变体之前先读一读它们。

这些变体在**一个地方**变成**进程退出码**：`crates/foundation/delonix-model/src/exitcode.rs`，函数 `for_error`。CLI 重新导出了这个模块（`bins/delonix-runtime-bin/src/cmd/mod.rs` 里的
`pub use delonix_model::exitcode;`），`bins/delonix-runtime-bin/src/main.rs` 调用
`std::process::exit(cmd::exitcode::for_error(&e))`。

```rust
// crates/foundation/delonix-model/src/exitcode.rs
pub fn for_error(e: &Error) -> i32 {
    match e {
        Error::NotFound(_) | Error::VmNotFound(_) => NOT_FOUND,
        Error::NotRunning(_) => NOT_RUNNING,
        // ...
```

有两点值得留意：

- 这个 `match` 是**穷尽的，没有 `_ =>` 分支**。给 `Error` 加一个新变体，会让构建在
  `for_error`（以及 `error.rs` 里的 `Error::code`）那里停下来，直到有人决定它该属于哪一类。这是故意把编译器当作一份检查清单来用。
- 消息是给操作者看的翻译文本，所以脚本必须根据退出码（或者来自 `Error::code` 的那个稳定机器码）来分支，绝不能根据消息文本。

**延伸阅读：** The Rust Book ——
[用 `Result` 处理可恢复的错误](https://doc.rust-lang.org/book/ch09-02-recoverable-errors-with-result.html)，
[`?` 运算符](https://doc.rust-lang.org/book/ch09-02-recoverable-errors-with-result.html#a-shortcut-for-propagating-errors-the--operator)；
Rust by Example ——[定义一个错误类型](https://doc.rust-lang.org/rust-by-example/error/multiple_error_types/define_error_type.html)；
[docs.rs 上的 `thiserror`](https://docs.rs/thiserror)。

---

## 3.3 作为端口的 trait：`VmBackend` 与后端注册表

引擎通过 **trait**（"端口"）与各个 provider 对话，一个 provider 就是某个端口的一份实现。最清楚的例子是 `crates/adapters/delonix-vm/src/lib.rs` 里的 `VmBackend`：

```rust
pub trait VmBackend {
    fn id(&self) -> &'static str;
    fn available(&self) -> bool;
    fn boot(&self, vmdir: &Path, cfg: &VmConfig, overlay: &str,
            on: &dyn Fn(CreateStage)) -> Result<Boot>;
    fn is_running(&self, vm: &Vm) -> bool;
    fn ip(&self, vm: &Vm) -> Option<String>;
    // ... more methods, many with default bodies
}
```

实现有：同一个文件里的 `CloudHypervisorBackend` 和 `LibvirtBackend`，以及
`crates/providers/delonix-proxmox/src/lib.rs` 里的 `ProxmoxBackend`。trait 里带有**默认实现体**的方法（比如 `auto_selectable`）让一个新后端可以继承一份合理的行为，只覆盖不一样的那部分。

后端是在运行时挑选出来的，所以它们被当作 **trait 对象**来处理，也就是
`Box<dyn VmBackend>`。它们是通过一个工厂（factory）注册表创建出来的：

```rust
// crates/adapters/delonix-vm/src/lib.rs
pub type BackendFactory = Box<dyn Fn() -> Result<Box<dyn VmBackend>> + Send + Sync>;

static BACKENDS: std::sync::OnceLock<std::sync::RwLock<Vec<BackendRegistration>>> =
    std::sync::OnceLock::new();
```

- 这个工厂是一个**闭包**（`dyn Fn`），因为一个远程后端需要携带一些捕获到的配置（端点、节点、凭据），而一个普通的 `fn()` 函数指针没法携带这些。
- 之所以要求 `Send + Sync`，是因为这张表是一个进程范围的 `static`；这个约束限制的是闭包，而不是 `VmBackend` 这个 trait 本身。
- `OnceLock` 惰性地初始化这张表（`builtin_backends()` 会播种两个本地后端），
  `RwLock` 让 `register_backend` 可以在启动时再加一个第三方后端。二进制程序在
  `bins/delonix-runtime-bin/src/cmd/vmbackends.rs`（`register_configured`）里就是这么做的。

这个设计背后的决定记在 [ADR-0008](../adr/0008-proxmox-vm-backend.md) 里。同样的"trait + 各种实现 + 一个地方来挑选"这种模式在别处也会出现（比如 `VmNetwork` 这个端口，保存在同一个文件靠前位置的一个 `OnceLock<Box<dyn VmNetwork>>` 里）。

**延伸阅读：** The Rust Book ——
[Trait](https://doc.rust-lang.org/book/ch10-02-traits.html)，
[Trait 对象](https://doc.rust-lang.org/book/ch18-02-trait-objects.html)，
[闭包](https://doc.rust-lang.org/book/ch13-01-closures.html)，
[`Send` 与 `Sync`](https://doc.rust-lang.org/book/ch16-04-extensible-concurrency-sync-and-send.html)；
标准库文档 ——[`OnceLock`](https://doc.rust-lang.org/std/sync/struct.OnceLock.html)。

---

## 3.4 `unsafe`、FFI 与 Linux 系统调用

一个容器引擎大部分时候做的都是系统调用。这个仓库通过三个 crate 触达内核：

| Crate | 用途 | 本仓库里的例子 |
|---|---|---|
| [`nix`](https://docs.rs/nix) | 相对安全的封装：`clone`、`setns`、`unshare`、`pivot_root`、`fork`、`mount`、信号 | `crates/adapters/delonix-linux/src/lib.rs` 里的 `use nix::sched::{clone, setns, unshare, CloneFlags};` |
| [`libc`](https://docs.rs/libc) | `nix` 没有封装的原始调用，或者结构体的确切布局很重要的地方 | `crates/contexts/delonix-node/src/peer_cred.rs`（`peer_uid`）里的 `libc::getsockopt(.., SO_PEERCRED, ..)`；`crates/adapters/delonix-state/src/store.rs` 里的 `libc::flock` |
| [`rustix`](https://docs.rs/rustix) | 新的挂载 API（`fsopen`/`fsconfig`/`fsmount`/`move_mount`） | `crates/adapters/delonix-linux/src/lib.rs` 里的 `fsopen_overlay` |

**每一个 `unsafe` 块都在旁边说明了它为什么是安全的**（workspace 的 lint 强制要求这句注释存在；审阅者来把关它说的是不是真的）：

```rust
// crates/contexts/delonix-node/src/peer_cred.rs
// SAFETY: getsockopt on SO_PEERCRED with a correctly-sized ucred buffer.
let r = unsafe { libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, ...) };
```

### 容器诞生的地方

`crates/adapters/delonix-linux/src/lib.rs` 里的 `fn spawn` 以这样结尾：

```rust
// SAFETY: single-threaded; the child mounts the container and does `exec`.
let cloned = unsafe { clone(cb, &mut stack, flags, Some(Signal::SIGCHLD as i32)) };
```

子进程运行 `container_init`，它会调用 `setup_rootfs`（先绑定挂载，再
`pivot_root`）、`drop_capabilities`、安装 seccomp，最后 `execvp`。`setns` 就是
`exec` 用来进入一个已存在容器的命名空间的方式。用户命名空间的 uid/gid 映射是从父进程写入的（`write_userns_maps`，在可用时会用到 `newuidmap`/`newgidmap` 这两个辅助程序）。

### 为什么在多线程进程里 `fork`/`clone` 是危险的

`fork` 或 `clone` 之后，子进程里只存在调用它的那一个线程，但子进程继承的却是**那一刻的原样**——包括那些已经不存在的线程当时持有的锁。如果另一个线程恰好在那一刻持有分配器的锁，子进程的第一次内存分配就会永远阻塞。`fork` 靠
`pthread_atfork` 处理程序缓解了部分这个问题；原始的 `clone` 不会运行它们。

正因为这个原因，这个仓库有两条具体的规则，两条都在注释里解释过，值得你去读一读：

- **`serve docker-api` 绝不在进程内调用 `spawn`。**
  `bins/delonix-runtime-bin/src/cmd/dockerapi.rs` 里、重新执行（re-exec）那个辅助函数上方的 doc comment 解释道，这个服务器是一个多线程的 tokio 运行时，所以它会重新执行这个二进制程序本身（`__apirun <spec.json>`），以此得到一个全新的、单线程的进程，让 `clone` 的前提条件重新成立。
- **在原始 `fork` 之后，只能做异步信号安全的事情。**
  `crates/adapters/delonix-linux/src/lib.rs` 里的 `reexec_mapped` 和
  `reexec_mapped_hold` 会在 fork *之前* 就把每一次内存分配都预先算好。
  `reexec_mapped_hold` 上的注释还记录了为什么它们用原始的 `fork`，而不是
  `std::process::Command` + `pre_exec`：`Command::spawn` 会等子进程到达
  `exec`，而一个阻塞着等父进程的 `pre_exec` 钩子会造成死锁。

### 新的挂载 API，以及它为什么会出现在这里

`mount_overlay_if_marked` 通过 `rustix::mount::fsopen`，再对每一层调用一次
`fsconfig_set_string(&fs, "lowerdir+", lower)`，来挂载一个容器的 overlay 根。经典的 `mount(2)` 把所有选项都打包进一个页大小的 `data` 字符串里，对于层数很多的镜像，内核会把它默默地截断。这个测量和这个决定都记在
[ADR-0037](../adr/0037-overlay-mount-new-api.md) 里。这是这个仓库一个习惯的好例子：一个不那么直观的系统调用上方的 `///` 注释，会解释促使它这么写的那次故障。

**延伸阅读：** The Rust Book ——[Unsafe Rust](https://doc.rust-lang.org/book/ch20-01-unsafe-rust.html)；
[Rustonomicon](https://doc.rust-lang.org/nomicon/)（尤其是
[FFI](https://doc.rust-lang.org/nomicon/ffi.html)）；man 手册页
[`clone(2)`](https://man7.org/linux/man-pages/man2/clone.2.html)，
[`setns(2)`](https://man7.org/linux/man-pages/man2/setns.2.html)，
[`pivot_root(2)`](https://man7.org/linux/man-pages/man2/pivot_root.2.html)，
[`fork(2)`](https://man7.org/linux/man-pages/man2/fork.2.html)，
[`signal-safety(7)`](https://man7.org/linux/man-pages/man7/signal-safety.7.html)，
[`fsopen(2)`](https://man7.org/linux/man-pages/man2/fsopen.2.html)。

---

## 3.5 序列化：serde、YAML 清单、自动生成的 schema

磁盘上的状态是 JSON；清单是 YAML。两者都用 [`serde`](https://serde.rs/) 的 derive 宏。

**向后兼容的记录。** 一份由更老的二进制程序写下的记录，必须仍然能被加载。规则是：一个新字段加上 `#[serde(default)]`，当"缺失"必须表示一种和该类型默认值不一样的意思时，就把它做成 `Option`：

```rust
// bins/delonix-runtime-bin/src/cmd/vmimage.rs  (VmImage)
#[serde(default)]
pub cloud_init: Option<bool>,
```

这里，`None`（也就是每一份在这个字段存在之前写下的记录）被读作"是"；一个普通的 `bool` 会默认为 `false`，从而悄悄改变旧镜像的行为。你会在
`crates/contexts/delonix-compute/src/record.rs` 里看到同样的推理：
`Mount::propagation` 是一个 `Option`（`#[serde(default, skip_serializing_if = "Option::is_none")]`），而 `Mount::optional` 是一个带 `#[serde(default)]` 的普通
`bool`，因为 `false` 正是每一份更老的记录本来的意思。

**清单。** `bins/delonix-runtime-bin/src/cmd/manifest.rs` 用
`serde_yaml::Deserializer::from_str(text)` 解析多文档的 YAML，得到
`ManifestDoc`，它的 `spec` 会一直保持一个原始的 `serde_yaml::Value`，直到拥有该 Kind 的代码把它反序列化成自己类型化的 spec 为止。

**自动生成的 schema。** Spec 类型会 derive `schemars::JsonSchema`（比如
`crates/contexts/delonix-compute/src/pod.rs` 里的 `PodSpec`），
`bins/delonix-runtime-bin/src/cmd/schema.rs` 会从它们生成出已发布的 JSON Schema
（[ADR-0007](../adr/0007-generated-manifest-schema.md)）。留意那里的一句注释：schemars 遵守 `#[serde(rename)]`，但不遵守 `#[serde(alias)]`。

**延伸阅读：** [serde.rs](https://serde.rs/) ——
[字段属性](https://serde.rs/field-attrs.html)（`default`、`skip_serializing_if`、`alias`）；
[`serde_yaml`](https://docs.rs/serde_yaml)；[`schemars`](https://docs.rs/schemars)。

---

## 3.6 CLI：clap 的 derive 与翻译后的输出

`delonix` 这个二进制程序就是 `bins/delonix-runtime-bin`。它的顶层是
`src/main.rs` 里的一个 [`clap`](https://docs.rs/clap) derive：
`#[derive(Parser)] struct Cli` 里有一个全局的 `--l18n` 标志和
`#[command(subcommand)] cmd: Cmd`，其中 `enum Cmd` 是一个
`#[derive(Subcommand)]`。每个命令组在 `src/cmd/` 下都有自己的模块和枚举——比如
`src/cmd/container.rs` 里的 `pub enum ContainerCmd`。

变体和字段上的 doc comment（`///`）会变成 `--help` 的文字。你会在命令枚举上看到
`#[allow(clippy::large_enum_variant)]`，旁边有一句注释解释原因：它们每次调用只解析一次，所以把变体装箱（box）不会带来任何好处。

**源码里的字符串是英文的；葡萄牙语来自一份目录（catalogue）。**
`src/cmd/po.rs` 用 `include_str!` 内嵌了 `data/pt.po`，并暴露出：

- `po::t("already exists, nothing to do")` —— 一个固定字符串；
- `po::tf("port {port} is taken by '{owner}'", &[("port", &hp), ("owner", &ow)])`
  —— 一个带**具名**占位符的模板，因为一次翻译可能会调整它们的顺序；
- `po::translate_help`，它会在 `po::peek_lang` 于*解析之前*就先决定好语言之后，重写 clap 的帮助文字。

一个新的、面向用户的字符串，做法是在代码里写英文，再加一条到 `data/pt.po` 里。语言策略（LANG-01）及其门禁在[贡献工作流](contributing-workflow.md)里有讲。

**延伸阅读：** [clap derive 教程](https://docs.rs/clap/latest/clap/_derive/_tutorial/index.html)；
标准库 ——[`include_str!`](https://doc.rust-lang.org/std/macro.include_str.html)。

---

## 3.7 异步与 gRPC——以及为什么引擎的大部分是同步的

核心引擎是**同步**的：CLI 启动，用阻塞式的系统调用和文件 I/O 完成自己的工作，然后退出。没有守护进程，也没有常驻的异步运行时。异步代码只出现在服务某个协议的那些*接口*里：

- **CRI 服务器**——`crates/interfaces/delonix-cri`。gRPC 的桩代码是在构建时生成的：`build.rs` 调用
  `tonic_build::configure()...compile_protos(&["proto/api.proto"], &["proto"])`。这需要安装**`protoc`**（CI 会安装 `protobuf-compiler`）；缺少
  `protoc` 是第一次构建时最常见的失败原因。`src/lib.rs` 里的
  `serve_blocking` 会创建自己的
  `tokio::runtime::Builder::new_multi_thread()` 运行时，处理函数通过
  `src/runtime_svc.rs` 里的 `blocking` 辅助函数，把阻塞式的引擎工作挪出异步工作线程——否则一次 `clone`，或者对 `delonix` 的一次 shell-out，都会把 tokio 的工作线程拖住。
- **L7 反向代理**——`bins/delonix-runtime-bin/src/cmd/ingress_proxy.rs` 在它自己的 tokio 运行时上直接使用 [`hyper`](https://docs.rs/hyper)（`hyper::service::service_fn`）。
- **MCP 服务器**——`crates/interfaces/delonix-mcp` 通过 stdio
  （`rmcp::transport::io::stdio`）使用 [`rmcp`](https://docs.rs/rmcp)。
- **遥测（telemetry）**——`crates/adapters/delonix-telemetry/src/telemetry.rs`
  用一个带*阻塞式* HTTP 客户端的专用线程导出 OpenTelemetry span，这正是为了让同步的 CLI 不需要一个运行时。

`proto/delonix/node/v1` 下的节点契约是一个独立的 protobuf API，有它自己的门禁（`scripts/contract_gate.py`、`buf`）；见[构建与测试](build-and-test.md)。

**延伸阅读：** [Rust 中的异步编程](https://rust-lang.github.io/async-book/)；
[Tokio 教程](https://tokio.rs/tokio/tutorial)；
[`tonic`](https://docs.rs/tonic) 与 [`tonic-build`](https://docs.rs/tonic-build)；
[Protocol Buffers 安装说明](https://protobuf.dev/installation/)。

---

## 3.8 并发与共享状态

没有数据库。状态是状态根目录下的一堆 JSON 文件，而**多个进程**（两次 CLI 调用、CRI 服务器、一个 supervisor）可能会同时碰同一条记录。每一次读—改—写遵循的都是同一个模式：在一把互斥的 `flock` 之下，用一个闭包调用 `update`：

```rust
// crates/adapters/delonix-state/src/store.rs  (Store::update)
let id = self.load(id_or_name)?.id;
let _lock = FileLock::acquire(&self.lock_path(&id))?;
// Re-read UNDER the lock ...
let mut c = self.load(&id)?;
if !f(&mut c) { return Ok(c); }
self.save(&c)?;
```

同一个文件里的 `JsonStore<T>::update` 是给其他记录类型用的通用版本。两者在拿不到锁的时候都会**拒绝**运行（`Error::Lock`），而不是不加锁就继续下去。（`secret.rs` 里的 `SecretStore::update` 是个例外：它的锁是尽力而为的。）由此而来的几条规则：

- 永远不要对一份可能被另一个进程写入的记录，手动去做 `load` → 修改 →
  `save`；你会丢掉别人的更新。用 `update`。
- 在锁内重新读一遍。你之前加载到的那个值可能已经过时了。
- `FileLock` 在 `Drop` 里释放锁——RAII 模式。

在单个进程内部，共享状态用的是标准类型：一个
`Arc<std::sync::RwLock<Arc<Vec<Route>>>>` 持有代理的可热替换路由表
（`cmd/ingress_proxy.rs` 里的 `SharedRoutes`），dashboard 通过一个
`Arc<Mutex<...>>`（`cmd/dash.rs`）共享它那份较慢的采样数据。

你还会在 `crates/foundation/delonix-model/src/typestate.rs` 里遇到一个相关的想法：**typestate** 模式，生命周期状态被表示成类型，非法的转换根本编译不过去（它的 doc test 里就包含 `compile_fail` 的例子）。

**延伸阅读：** The Rust Book ——
[共享状态并发](https://doc.rust-lang.org/book/ch16-03-shared-state.html)，
[`Drop`](https://doc.rust-lang.org/book/ch15-03-drop.html)；
[`flock(2)`](https://man7.org/linux/man-pages/man2/flock.2.html)；
标准库 ——[`Arc`](https://doc.rust-lang.org/std/sync/struct.Arc.html)，
[`RwLock`](https://doc.rust-lang.org/std/sync/struct.RwLock.html)。

---

## 3.9 测试

- **单元测试写在代码旁边**，放在文件末尾的 `#[cfg(test)] mod tests { ... }` 里。许多纯函数存在的理由，正是为了让一个决策能像数据一样被测试（比如
  `crates/contexts/delonix-stack/src/reconcile.rs` 里的 `reconcile::plan`）。
- **集成测试**放在 `crates/<layer>/<crate>/tests/*.rs` 里，只使用该 crate 的公开 API。例子：`crates/interfaces/delonix-cri/tests/grpc_status.rs` 在一个 unix 套接字上启动 CRI 服务器，再用*自动生成的 gRPC 客户端*和它对话（这也是为什么 `build.rs` 要设 `build_client(true)` 的原因）。`crates/providers/*/tests/live.rs` 下的测试需要一个真实的 provider，是可选启用（opt-in）的。
- **属性测试（property test）**用的是 [`proptest`](https://docs.rs/proptest)：见
  `crates/adapters/delonix-sdn/tests/ip_invariants.rs`（`proptest! { ... }`）。
- **Doc test** 也会被运行——包括 `typestate.rs` 里那些 `compile_fail` 代码块。
- **基准测试（benchmark）**用的是 [`criterion`](https://docs.rs/criterion)
  （`crates/adapters/delonix-oci/benches/`）。
- **命名。** 测试的名字描述的是被证明的那个行为。许多既有的测试用的是葡萄牙语名字；新的标识符用英文（LANG-01，由 `scripts/lang_ratchet.py` 作为一个棘轮强制执行）。
- **测试不能碰主机的真实状态。** 拿一个临时目录传进去（各个 store 都接受一个根路径），而不是调用那些会解析出真实状态根目录的代码。任何需要真实命名空间、cgroup 或者网络 holder 的东西，都属于[构建与测试](build-and-test.md)里讲的现场（live）/ E2E 验证。

**延伸阅读：** The Rust Book ——[编写测试](https://doc.rust-lang.org/book/ch11-01-writing-tests.html)，
[测试的组织结构](https://doc.rust-lang.org/book/ch11-03-test-organization.html)；
rustdoc ——[文档测试](https://doc.rust-lang.org/rustdoc/write-documentation/documentation-tests.html)。

---

## 3.10 CI 强制执行的工具

| 工具 | CI 命令（来自 `.github/workflows/ci.yml`） | 它会抓住什么 |
|---|---|---|
| rustfmt | `cargo fmt --all --check` | 格式漂移 |
| clippy | `cargo clippy --workspace --all-targets --locked -- -D warnings` | 任何警告，包括 `undocumented_unsafe_blocks` |
| tests | `cargo test --workspace --locked --no-fail-fast` | 回归 |
| cargo-deny | 带 `deny.toml` 的 `EmbarkStudios/cargo-deny-action` | RUSTSEC 安全公告、许可证、crate 来源 |

这个仓库还有一些 Python 门禁（`scripts/arch_fitness.py`、
`scripts/lang_ratchet.py` 等等）。完整列表以及如何在本地逐一运行它们，见
[构建与测试](build-and-test.md)。

**延伸阅读：** [Clippy](https://doc.rust-lang.org/clippy/)，
[rustfmt](https://rust-lang.github.io/rustfmt/)，
[cargo-deny](https://embarkstudios.github.io/cargo-deny/)。

---

**下一步：** [准备你的环境](environment.md)——一台能构建这棵树、能跑现场（live）路径的主机，以及那些看起来像引擎 bug 的主机陷阱。
