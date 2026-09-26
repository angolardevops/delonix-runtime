<!-- translated-from: delonixfile-and-vmfile.md sha256:40e8ce16ae06683b26d6f4cd83f6266c6b58c55570381c2f91aee3bb1d09acc5 -->
# Delonixfile 与 VMfile

**阅读之前：**[克隆、构建与测试](build-and-test.md)（一个二进制程序和一个隔离的状态根目录），以及云原生入门里的[OCI 镜像、按内容寻址的存储与 overlayfs](cloud-native-primer.md#44-oci-images-content-addressed-storage-and-overlayfs)一节。

Delonix 有两种构建文件，它们看起来相像是故意的：任何写过 Dockerfile 的人都能读懂这两种。
它们构建出来的东西不一样。

- 一个 **Delonixfile** 构建出一个 **OCI 容器镜像**（一层层的文件系统）。它是在 Dockerfile
  语法上再加了少数几条 Delonix 指令，由 `delonix build` 构建。
- 一个 **VMfile** 为一台虚拟机构建出一块**可启动的 qcow2 磁盘**。它借用了 Dockerfile 的
  *形状*，但机制是对整块磁盘用 `qemu-img` + `virt-customize`，由 `delonix image vm build`
  构建。

本页描述的是这个仓库里的解析器实际接受什么——而不是 Docker 接受什么。下面每一条规则都指
向强制执行它的代码。读完之后，你能够写出这两种文件、预测每个解析器会接受还是拒绝，并且
找到该去哪里改动某种语法。

> 标了 *已通过解析校验* 的例子，都是把 `DELONIX_ROOT`、`DELONIX_NET_RUNTIME_DIR` 和
> `TMPDIR` 指向一个临时目录之后，对着用这棵树构建出的二进制程序跑过的，一直跑到构建即将
> 开始拉取镜像或下载 base 磁盘的那一步为止。完整的构建**在本次复核中没有被执行**（它们
> 需要镜像仓库/网络访问，对虚拟机而言还需要 libguestfs 以及好几 GB 的内存和磁盘）。

---

## 第 1 部分 —— Delonixfile

### 代码在哪里

| 关注点 | 文件 | 符号 |
|---|---|---|
| 语法（解析器） | `crates/adapters/delonix-oci/src/build.rs` | `parse_dockerfile_with_args`、`parse_run_flags`、`parse_secret_mount`、`resolve_target_stage` |
| 构建编排 | `bins/delonix-runtime-bin/src/cmd/build.rs` | `run`、`build_from_spec`、`build_one_stage`、`default_build_file` |
| 镜像提交 | `crates/adapters/delonix-oci/src/build.rs` | `ImageStore::commit_flat_rootfs`（rootless）、`commit_upper` + `build_image`（root） |
| 项目模板 | `bins/delonix-runtime-bin/templates/<name>/Delonixfile` | 由 `delonix init` / `stack init` 渲染 |

### 文件查找

不带 `-f` 的 `delonix build [CONTEXT]` 会调用 `default_build_file`（`cmd/build.rs`）：
`<context>/Delonixfile` 存在就用它，否则用 `<context>/Dockerfile`。**两个名字用的是同一套
语法**——下面这些 Delonix 指令，在一个叫 `Dockerfile` 的文件里同样会被接受。`Delonixfile`
只是先被发现的那个名字而已。`-t/--tag` 是必填的。

```bash
delonix build -t myapp:dev .                 # Delonixfile, else Dockerfile
delonix build -t myapp:dev -f build/Other .  # explicit file
```

### 解析器接受的指令

`parse_dockerfile_with_args` 是 **fail-closed** 的：一个它不认识的指令是一个带行号的错误，
第一个 `FROM` 之前除了 `ARG` 以外的任何指令也是一个错误。以 `\` 结尾的行会被拼接；以 `#`
开头的行是注释。指令名字不区分大小写。

| 指令 | 发生了什么 | 备注 |
|---|---|---|
| `ARG NAME[=default]` | 声明一个构建变量；之后每一行里的 `${NAME}`/`$NAME` 都会被替换 | 允许出现在 `FROM` 之前（用来给它传参）。`--build-arg NAME=VALUE` 只能覆盖一个已声明的 `ARG`。简化之处：变量的作用域是**整个文件**，而不是按阶段划分。不支持 `${NAME:-default}` 这种形式（`substitute_vars`）。 |
| `FROM <image> [AS <name>]` | 打开一个阶段 | 更靠后的阶段也可以写 `FROM <earlier-stage>`（见多阶段构建）。 |
| `RUN <shell>` | 通过 `exec` 在一个工作容器里运行 | 作为参数只接受 `--mount=type=secret,...`/`--mount=type=cache,...`（见下文）。 |
| `COPY [--from=<stage>] <src> <dst>` | 写入该阶段磁盘上的根文件系统 | 被限制在 context/rootfs 之内（`safe_join`、`confine_to`）：`..`、绝对路径的逃逸，以及离开 base 的符号链接都会被拒绝。 |
| `ADD` | **和 `COPY` 一样** | 不支持 URL 下载，也不会自动解压归档。 |
| `ENV K=V [K2="v 2" …]` 或 `ENV K V` | 影响之后的各个 `RUN`；最后一个阶段的 `ENV` 会写进镜像配置 | 值会针对更早的 `ENV` 做展开（`expand_env_value`）。 |
| `WORKDIR <dir>` | 之后各个 `RUN` 的工作目录；最终的值会写进镜像配置 | |
| `USER <name\|uid[:gid]>` | 记录进镜像配置 | 留空 → 继承 base 镜像的设置。 |
| `CMD`、`ENTRYPOINT` | 记录进镜像配置 | 支持 exec（JSON）和 shell 两种形式。 |
| `HEALTHCHECK [opts] CMD <cmd>` / `HEALTHCHECK NONE` | `CMD` 之后的部分会被存为这个镜像的健康检查命令 | `CMD` 之前的选项会被忽略。被 `delonix container healthcheck <id>` 以及 compose 的 `depends_on: condition: service_healthy` 用到。 |
| `LABEL`、`EXPOSE`、`MAINTAINER`、`VOLUME`、`STOPSIGNAL`、`SHELL`、`ONBUILD` | **被接受，但会被忽略** | 只是元数据；对构建没有影响。 |

**Delonix 扩展**（被解析进同一个文件里的 `Dockerfile` 字段）：

| 指令 | 被解析进 | 在这棵树里的效果 |
|---|---|---|
| `SCAN fail-on=<sev>` | `scan_fail_on`（没有 `fail-on=` 时默认 `high`） | **会被解析，但 `delonix build` 不会强制执行它**——没有调用方去读这个字段。要给镜像加门禁，显式用 `delonix image scan --fail-on <sev> <image>`。 |
| `CPUS <n>` | `cpus` | 写进镜像配置（一个非标准的 `Cpus` 键）；缺省时继承 base 的值。本次复核没有找到任何运行时的消费者。 |
| `MEMORY <n>` | `memory` | 和 `CPUS` 一样（`Memory` 键）。 |
| `SECURITY <opt>...` | `security` | 和 `CPUS` 一样（`Security` 键）。 |

把 `CPUS`/`MEMORY`/`SECURITY` 当作*被记录下来的意图*就好；真正的限制要在运行时去设置
（`container run -m/--cpus`，或者清单里的 `resources`）。如果你把它们接上了实际效果，记得
更新这张表。

值得了解的解析器行为（是从代码里读出来的，不是一个设计上的承诺）：

- `COPY a b c /dst` 只保留**第一个**源和**最后一个**参数；中间的源会被悄悄丢掉。
- `COPY --chown=…`/`--chmod=…` 不被识别；那个参数 token 会被当成源路径来处理。
- `RUN --network=…`、`RUN --security=…`，以及其他任何 `RUN --<flag>` 都会被拒绝。

### 多阶段构建、`COPY --from` 与 `--target`

每一个阶段（包括最后一个）都会拿到**自己的工作容器和根文件系统**
（`build_one_stage`）。这个工作容器（在该阶段的根文件系统上跑 `sleep infinity`）由
`ensure_container` 创建，走的是和 `container run`/`start` 相同的那个 `WorkloadRuntime`
（`container::with_host_workload`），所以构建不可能偏离容器自己的启动规格。中间阶段会
一直留在磁盘上，直到整个构建结束，因此：

- `COPY --from=<name-or-index> <src> <dst>` 直接从那个阶段的根文件系统里读
  （`resolve_copy_source`、`copy_into_rootfs`）。
- `FROM <earlier-stage>` 用 `cp -a --reflink=auto`（`clone_rootfs`）克隆那个阶段的根文件
  系统——用 `cp -a` 是为了让 `/bin -> usr/bin` 这样的符号链接依然是符号链接。
- `--target <name-or-index>`（`resolve_target_stage`）会在那个阶段停下并打包它；更靠后的
  阶段不会被构建。一个未知的名字会被拒绝，错误信息里会列出实际存在的各个阶段。对一个
  中间阶段用 `--target` 时，`CMD`/`ENTRYPOINT`/`USER`/`ENV`/`WORKDIR`/`HEALTHCHECK` 来自
  **那一个**阶段，而不是文件里的最后一个阶段。

root 模式（overlay）下有两条限制，两条都会在一开始就带着明确的提示被拒绝：最后一个阶段
不能是 `FROM <earlier-stage>`（OCI 提交需要一个真实的 base 镜像来提供世系信息），并且对
一个中间阶段使用 `--target` 会被拒绝。Rootless 模式下这两条限制都不存在。

### 构建密钥

```bash
delonix build -t app:dev --secret id=npmrc,src=$HOME/.npmrc .
```

```dockerfile
RUN --mount=type=secret,id=npmrc,target=/root/.npmrc npm ci
```

- `--secret id=<name>,src=<path>` 可以重复给出；一条格式错误的条目或者一个缺失的 `src`
  文件是一个硬错误（`parse_build_secrets`）。
- 在文件里：`--mount=type=secret,id=<name>[,target=<path>][,required=true|false]`。
  `target` 默认是 `/run/secrets/<id>`；`required` 默认是 `false`（一个缺失的可选密钥会
  像 Docker 那样被跳过）。
- 这个密钥只在那**一次** `RUN` 期间，在工作容器的挂载命名空间里被实时绑定挂载
  （`mount_run_secrets`），然后就被卸载——提交和层缓存所读取的那份根文件系统的宿主机侧
  视图，从来不会包含它。密钥的内容也不会被哈希进缓存键。

### 缓存挂载（十三项改进计划中的 M03）

```dockerfile
RUN --mount=type=cache,target=/root/.cache/pip pip install -r requirements.txt
```

- `--mount=type=cache,target=<path>[,id=<name>]`。`target` 是必填的；`id` 默认就是
  `target` 本身（这是 Docker 自己的规则），所以两个没有指定 `id` 却挂载同一个 `target`
  的 `RUN`，共享的是同一个持久化目录。
- 和密钥不同，这个目录是**可读写**的，并且**会跨构建持久化**（也跨不同的 Dockerfile）：
  它位于 `<DELONIX_ROOT>/build-cache/mounts/<sha256(id)>`——是哈希值，绝不是原始的
  `id`/`target` 字符串，因为一个省略掉的 `id` 会默认成一个由 Dockerfile 任意决定的路径
  （`cache_mount_dir`）。目前还没有 GC/TTL，和下面的层缓存有着同样被如实写出来的缺口。
- 一次针对该指令的层缓存**命中**会完全跳过这条 `RUN`，所以它也就根本不会碰到这个缓存
  挂载的目录——只有未命中时才会碰（`mount_run_caches`）。
- `sharing=`/`ro`（Docker 自己额外加的字段）会被**拒绝**，而不是悄悄地接受并忽略：目前
  并发的 `delonix build` 之间共享同一个 cache id 时还没有跨进程加锁，而这里的每一个
  缓存挂载都是可读写的。
- `type=ssh` 和 `type=bind` 仍然会被**拒绝**（`parse_mount_flag`）——还没有实现。

### `--platform` 与 binfmt

`--platform linux/<arch>`（`parse_platform`：只接受 `linux/`）会解析出那个架构对应的
base 镜像，并把这个架构戳进构建结果里。**运行**一个跨架构的 `RUN` 需要宿主机自己注册好
`binfmt_misc` + `qemu-user-static`，这一步 Delonix 不负责管理。`build_from_spec` 会在构建
之前检查 `/proc/sys/fs/binfmt_misc/qemu-<arch>`，缺失或被禁用时会带着解释器的名字拒绝。

### Rootless 与 root 的差异，以及层缓存

| | Rootless（常规路径） | Root |
|---|---|---|
| 一个阶段的根文件系统 | 扁平目录（`prepare_rootfs_flat`） | overlay |
| 提交 | `commit_flat_rootfs`——在 base 的各层之上压成一层 | `commit_upper`（upperdir 的 tar）+ `build_image` |
| 层缓存 | **有** | **从来没有** |

这个缓存（`<DELONIX_ROOT>/build-cache/<hash>/rootfs`，`--no-cache` 可以绕过）是一条滚动
哈希链，一条指令对应链上一环。`RUN`/`COPY` 运行之后会给整个根文件系统拍一张快照；
`ENV`/`WORKDIR` 只是并入这条链，不会拍快照。一个 `COPY` 环节会对被复制的**字节内容**
做哈希，所以一个变动过的文件会让它之后的一切都失效。命中时，缓存下来的快照会被克隆进一个
全新容器的根文件系统——绝不会同步到一个活着的容器上（那样做会破坏
`/proc`/`/sys`/`/dev` 的挂载，这条路已经被放弃了）。模块文档里写明的取舍：拍的是整个
根文件系统的快照，而不是按层的差异（用 `--reflink=auto` 来缓解），并且**没有缓存 GC**
——`build-cache/` 只会不断变大。

因为每一条 `RUN` 都在一个真实的容器里执行，[准备你的环境](environment.md)里说的那些
宿主机前置条件（user 命名空间、subuid/subgid、Ubuntu 上的 AppArmor）对构建同样适用。

### 实例演示：一个模板

`delonix init -t <template> [DIR]`（或者 `delonix stack init --template <template>`）会渲染
`bins/delonix-runtime-bin/templates/<template>/`，替换掉 `__NAME__`、`__PORT__` 和
`__TEMPLATE_VERSION__`（`template.meta` 里 `version=` 的默认值，或者
`-v/--template-version`）。在一个空目录里，它会写出完整的项目；在一个非空目录里，它只会
加上 Delonix 的胶水代码（Delonixfile、清单、CI 文件）。下面是用 `httpd` 在一个临时目录里
生成出来的（*已运行*）：

```bash
$ delonix init -t httpd web
detected an empty directory → stack init --template httpd
  created: web/.dockerignore
  created: web/Delonixfile
  ...
```

```dockerfile
# Delonixfile — production-ready Apache httpd.
# Build with:  delonix build -t web:dev .
FROM httpd:2.4-alpine
# Listen on 8080 and harden a little (no version banner).
RUN sed -i 's/^Listen 80$/Listen 8080/' /usr/local/apache2/conf/httpd.conf && \
    printf '\nServerTokens Prod\nServerSignature Off\nTraceEnable Off\n' >> /usr/local/apache2/conf/httpd.conf
COPY public /usr/local/apache2/htdocs
EXPOSE 8080
HEALTHCHECK CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
CMD ["httpd-foreground"]
```

生成出来的 `delonix-manifest.yaml` 能通过 `delonix manifest validate`（*已运行*）。
`--up` 本该去构建、`stack apply`，再等待健康检查——这里没有执行过。

一个用到了大部分语法的多阶段文件，*已通过解析校验*（`--target nosuch` 的报错证明了
这个文件在任何拉取动作之前就已经解析完毕，并且列出了它的各个阶段）：

```dockerfile
ARG GO_VERSION=1.23
FROM golang:${GO_VERSION}-alpine AS builder
WORKDIR /src
COPY go.mod ./
RUN --mount=type=secret,id=netrc,target=/root/.netrc go mod download
COPY . .
RUN CGO_ENABLED=0 go build -o /out/app ./cmd/app

FROM alpine:3.20
COPY --from=builder /out/app /usr/local/bin/app
USER 65534
HEALTHCHECK CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
CMD ["/usr/local/bin/app"]
```

```text
$ delonix build -t demo:dev --target nosuch .
error invalid argument: no stage named 'nosuch' in this Dockerfile — known stages: builder
$ delonix build -t d -f Dockerfile.ssh .          # RUN --mount=type=ssh,...
error invalid argument: RUN --mount=type=ssh: only type=secret and type=cache are supported (ssh/bind not yet)
$ delonix build -t d --platform windows/amd64 .
error invalid argument: --platform 'windows/amd64': only 'linux/<arch>' is supported (this engine does not run another OS)
```

### 本仓库自己的 `Delonixfile` 不是用 `delonix` 构建的

仓库根目录下的那个 `Delonixfile` 把 `delonix` 这个 CLI 打包成一个容器镜像。它是用
**Docker 或 Podman**（`docker build -f Delonixfile …`，或者 `make image`）构建的，不是用
`delonix build`。它只用到了 `type=cache` 挂载（现在已经能被解析）和 `# syntax=` 指令
（对这个解析器来说就是一行注释，因为任何以 `#` 开头的行都是注释）——但用
`delonix build` 去构建它这件事从来没有试过：它要在工作容器里编译整个 Rust 工作区，这和
下面那些模板不是一个数量级，也超出了这项功能被验证过的范围。不要把它当作 Delonix
语法的例子；要用就用那些模板。

---

## 第 2 部分 —— VMfile

### 代码在哪里

| 关注点 | 文件 | 符号 |
|---|---|---|
| 语法、脚手架、构建器 | `bins/delonix-runtime-bin/src/cmd/vmfile.rs` | `parse`、`classify_base`、`resolve_base`、`stage_ops`、`build`、`finalize`、`scaffold` |
| CLI 入口、黄金配方 | `bins/delonix-runtime-bin/src/cmd/vmimage.rs` | `VmImageCmd::Build`、`VmImageStore`、`VmImage`、`customize_args`、`tool_failure_hint` |
| 声明式构建 | `bins/delonix-runtime-bin/src/cmd/vm.rs` | `VmBuildSpec`（`kind: VirtualMachine` 的 `spec.build`） |

### 脚手架

```bash
delonix vm init --vmfile [DIR] [--name <n>]   # or: delonix image vm init <name> [-d DIR]
```

两个命令都会写出 `VMfile` 和 `cloud-init/user-data.yaml`（*在一个临时目录里已运行过*）。
这份脚手架被设计成一份能用的配方，`parseia_o_scaffold_que_escrevemos` 这个测试保证它
始终能被解析。它打印出来的"Next:"提示里有两处需要手动改正：那个参数是
`vm create --disk`，不是 `--disk-image`；另外关于文件名，见下面 `CLOUDINIT` 那条说明。

### 构建

```bash
delonix vm build [-f vm.yaml|VMfile] [-t <tag>] [--target <image>] [--network] [--no-compress] [CONTEXT]
```

`delonix image vm build` 是同一个命令（两者共用一个 `BuildArgs`）。用哪一份配方，决定
方式和 `docker build` 在各文件之间做决定一样：一个显式给出的 `-f` 优先（`.yaml`/`.yml`
被当作 `vm.yaml` 来读，别的一律当作 `VMfile`）；不带 `-f` 时，context 里的 `vm.yaml` 优先
于 `VMfile`，`VMfile` 又优先于内置的黄金配方（见[构建 microVM](microvm-setup.md)）。黄金
配方专属的参数（`--k8s-version`、`--extra-package`、`--extra-run`、`--offline`、
`--no-k8s`、`--cri-bin`、`--delonix-bin`）在有 `vm.yaml` 或 `VMfile` 时会被**拒绝**，而
`--network` 在没有它们两者之一时也会被拒绝。

### `vm.yaml`：compose 风格的前端

`VMfile` 之于虚拟机镜像，就相当于 `Dockerfile` 之于容器镜像。`vm.yaml` 之于它，就相当于
`compose.yaml` 之于那一边：它给出一个目录里各个镜像的名字，并带上让 qcow2 完整起来所需的
参数——`size`、`packages.install`、`users`、`services`、`files`、`env`、`cloud_init`、
`run`，要**移除**什么（`remove.packages/paths/users/services`，在其他一切之后应用，
这样才能清理掉某个软件包或 `run:` 拉进来的东西），以及 `cleanup`（软件包缓存、日志、
历史记录、`/tmp`、machine-id）。

它只是一个前端（`cmd/vmspec.rs`）。每一个镜像都会编译成一个已经存在的构建器——一份
合成出来的 `VMfile`（`profile: custom`，默认值）、黄金配方（`profile: rootless|k8s`），
或者一个已有的文件（`build.file`）——所以并不存在第二套构建引擎。值得了解的规则：

- **严格**：一个未知的键是一个错误，一个所选路线无法兑现的字段也是一个错误
  （`profile: k8s` 配上一个 `hostname:` 会被按名字拒绝，而不是被忽略）。
- **`${TAG}` / `${VAR:-default}`** 会在已解析出的值上展开（注释里的 `${TAG}` 不会被
  求值）。`-t` 既是构建结果的 tag，**又**是 `${TAG}`。
- **相对路径是相对于 `vm.yaml` 自己所在的目录**，不管 context 说的是什么。
- **`packages` 需要 `network: true`**：一次能联网的构建，在不同的日子里会构建出不同的
  镜像，所以这是可选加入的，拒绝时也会说明这一点。
- `remove.paths` 必须是绝对路径，不能带 `..`，也绝不能是一个顶层系统目录。

**Appliance**（`appliance:`）。有些镜像没法描述成对一个云镜像的编辑：必须运行厂商自己的
安装程序（Proxmox 要从它的 ISO 装，OpenStack 要拉大约 20 GiB 的容器）。对这些镜像，配方
给出的是一个**构建器**的名字，而不是一个路径：`appliance: {builder: proxmox, args: [pve,
"9.2-1"]}` 会运行 `scripts/appliances/build-proxmox.sh`（在 `vm.yaml` 所在目录或它之上的
任何目录里查找），配一个和镜像存储放在一起的隔离 `OUT_DIR`，取走它留下的那唯一一个
`*.qcow2`（一个 `.raw.qcow2` 会被忽略），并按 `image vm import` 的语义注册它——除非
`cloud_init: true`，否则带上 `--appliance`。因为这个名字会被校验（`[a-z0-9-]`），并且
只在 `scripts/appliances/` 内部解析，所以一个 `vm.yaml` 没法让宿主机去运行一个它随便指定
的文件；`args` 和 `env` 也会被校验（不能以 `-` 开头，不能是 `PATH`/`LD_*`/`BASH_ENV`……）。
一个构建器自己决定的那些字段（`packages`、`users`、`hostname`、`network`、`profile`……）
会被按名字拒绝。

`images/` 下按发行版分的各个目录（`images/ubuntu/` 是第一个）各自带着一份 `vm.yaml`、
cloud-init 文件、产出物和一份 README。

### 指令

`parse` 是 fail-closed 的：一个未知指令是一个会说明支持哪些指令的错误。只有 `FROM` 可以
排在最前面。`\` 续行会被拼接；`#` 只有出现在一行的开头才算注释的开始（所以
`RUN sed 's/#x/y/'` 不会被搞乱）。

| 指令 | 作用范围 | 效果（`stage_ops` → `virt-customize`） |
|---|---|---|
| `FROM <ref> [AS <name>]` | 打开一个阶段 | 见 *`FROM` 接受什么*。`as` 不区分大小写。 |
| `RUN <shell>` | 步骤 | 客户机里的 `--run-command`；**离线**执行，除非有 `--network`。 |
| `COPY <src> <dst>` | 步骤 | 从构建 context 里拷入；`src` 被限制在 context 之内（`build::safe_join`）。恰好两个参数。 |
| `COPY --from=<stage> <src> <dst>` | 步骤 | 用 `virt-copy-out` 把那个阶段磁盘里的内容取到宿主机的一个暂存目录，再拷入。那个阶段必须**已被命名并且在更早的地方声明过**——在解析时就会被检查。 |
| `ENV KEY=value` | 步骤 | 把 `KEY=value` 追加进 `/etc/environment`（虚拟机没有镜像配置）。每行一对；键和值会被**不加引号**地传给 shell，所以要避免空格和 shell 的元字符。 |
| `USER <name>` | 步骤 | 如果这个账户不存在，就执行 `useradd -m -s /bin/bash <name>`。 |
| `PASSWORD <user>:<password>` | 步骤 | 设置那个账户的密码。它会被烤进这个镜像的每一份拷贝里。 |
| `ROOTPASSWORD <password>` | 步骤 | 设置 root 的密码。同样的警告。 |
| `SSHKEY <user> <path-or-key>` | 步骤 | 追加进 `/home/<user>/.ssh/authorized_keys`（`~/` 会被展开）。这个值必须是一个可读的文件，或者一个以 `ssh-`/`ecdsa-` 开头的密钥。这个账户必须已经存在（先用 `USER` 创建它）。 |
| `CLOUDINIT <path>` | 步骤 | 把那个文件（被限制在 context 之内）拷贝进 `/etc/cloud/cloud.cfg.d/`，**保留它原来的文件名**。cloud-init 只会从那个目录加载以 `.cfg` 结尾的文件，所以要给它起个比如 `99-myimage.cfg` 这样的名字——脚手架里 `user-data.yaml` 这个名字原样是不会被拾取的（本次复核没有实际执行过；这是从代码路径和 cloud-init 自己文档写明的行为推断出的）。`vm create` 仍然会在此之上加上它自己那份每实例专属的 NoCloud seed。 |
| `SIZE <n>G` | 阶段属性 | 在任何步骤运行**之前**执行 `qemu-img resize`——等 `RUN` 把磁盘填满了再扩容就太晚了，这也是它不是一个步骤的原因。 |
| `HOSTNAME <name>` | 阶段属性 | 写入 `/etc/hostname`（该阶段的第一个操作）。 |
| `VCPUS <n>` | 镜像 | 记录为 `default_vcpus`。 |
| `MEMORY <n>` | 镜像 | 记录为 `default_memory`。 |
| `HYPERVISOR <backend>` | 镜像 | 由 `delonix_vm::valid_backend_name` 校验并规范化（`ch` → `cloud-hypervisor`）；记录为 `default_backend`。 |
| `LABEL k=v` | 镜像 | 在这棵树里**只会被解析，不会被记录**进镜像元数据。 |

没有 `CMD`/`ENTRYPOINT`：虚拟机启动的是一个 init。`CLOUDINIT` 是最接近的等价物。

### `FROM` 接受什么

`classify_base` 是一个纯函数，负责判断引用的种类：

1. **一个已命名的更早阶段**——优先于一切（`resolve_base`）。
2. **`ubuntu:<rel>`、`debian:<rel>`、`rocky:<rel>`、`fedora:<rel>`**——先找项目自己为那个
   发行版/版本准备的 base 镜像（先本地副本，再官方镜像仓库——
   `vmimage::official_distro_base`）；如果没有，就用该发行版自己的云镜像，下载后对照
   发布者的校验和文件核实（`vmimage::download_base`）。
3. **`http://` / `https://` URL**——下载它；发布者提供了 `<url>.sha256` 时就对照校验，
   否则就只信任 TLS，构建过程会说明这一点。
4. **其他任何情况**——本地存储里已经有的一个虚拟机镜像（`delonix image vm ls`）。任何
   别的 `name:tag` 都会被当作一个本地 tag 来处理，而不是当作一个未知的发行版。

### 什么是一个阶段，结果落在哪里

一个阶段是**一整块磁盘**，而不是一层（`build`）：

1. base 会用 `qemu-img convert` **拍平**成 `<work>/<stage>.qcow2`（没有 backing file，所以
   这个产出物永远不会依赖 base 是否还存在）。
2. 应用 `SIZE`，然后所有步骤在一次 `virt-customize` 调用里跑完（默认 `--no-network`，除非
   有 `--network`）。`vmimage::customize_args` 总是把一次 SELinux relabel 追加成最后一条
   命令（并关掉 libguestfs 自己那次延后的 relabel），这样 SELinux 客户机就不会带着没打
   标签的文件启动。
3. 已命名的阶段会被保留，供 `COPY --from` 使用；未命名的则无法被寻址。只有**最后**一个
   阶段会成为一个镜像。
4. 除非有 `--no-compress`：先 `virt-sparsify --in-place`（尽力而为），再
   `qemu-img convert -c -o compression_type=zstd`。用 zstd 是因为这个镜像会成为每一台由
   它创建出来的虚拟机的只读 backing file，所以解压速度是一个运行时属性。
5. 这个 qcow2 会被移动到 `<DELONIX_ROOT>/vm-images/` 下的
   `VmImageStore::qcow2_path(tag)`，一条 `VmImage` 元数据记录会保存在它旁边。

记录下来的元数据：`digest` 和 `size`；来自文件的
`default_vcpus`/`default_memory`/`default_backend`；`cloud_init: true`；
`built_by: "delonix <version>"`；`distro` 和 `kernel_version` 只有在最后一个 `FROM` 是
一个本地镜像时才会被**继承**（一个 URL 没有什么可继承的，代码也拒绝去猜）。

`vm create` 只会在调用者没有自己决定的地方应用记录下来的默认值：`--vcpus`/`--memory`
参数优先于 `VCPUS`/`MEMORY`；对后端来说，`--backend` > 镜像自己的 `HYPERVISOR` >
`DELONIX_VM_BACKEND` > `vm default-backend` > 自动检测（`cmd/vm.rs` 里的
`resolve_vm_defaults`，以及 `delonix_vm::create_with`）。

**默认离线，以及为什么。** 一条能联网的 `RUN`，会因为运行的时间不同而产出不同的镜像。
`--network` 是可选加入的，因为一个 VMfile 最常见的诉求就是装一个软件包。带上
`--network` 时，libguestfs 的 appliance 会从 `passt` 那里拿到网络，而它的宿主机陷阱记录
在[构建 microVM](microvm-setup.md#6-troubleshooting)里。

**临时空间。** 工作目录被创建在 `std::env::temp_dir()`（`delonix-vmfile-<pid>`）之下，
也就是 `$TMPDIR` 或者 `/tmp`，每个阶段都会在里面放一整块拍平的磁盘。把 `TMPDIR` 指向一个
空间够用的文件系统——`/tmp` 往往是一个很小的 tmpfs，而且会在重启时被清空。一次失败的
构建会把那个目录留下来（本次复核中观察到了这一点）；要手动清理它。

### 声明式构建

`kind: VirtualMachine` 可以通过 `spec.build`（`VmBuildSpec`）构建自己的磁盘：`context`
（相对于**清单**所在的目录）、`file`（默认 `<context>/VMfile`）、`tag`（默认
`<metadata.name>:latest`）、`compress`、`network`。`apply` 调用的是同一个
`vmfile::build`。`disk` 和 `build` 是互斥的。

### 实例演示（已通过解析校验）

```dockerfile
FROM my-base:1.0 AS builder
RUN make -C /src

FROM my-base:1.0
SIZE 20G
HOSTNAME web
COPY --from=builder /src/app /usr/local/bin/app
ENV APP_ENV=production
USER app
VCPUS 2
MEMORY 2G
HYPERVISOR ch
LABEL org.opencontainers.image.title=web
```

临时存储里没有一个叫 `my-base:1.0` 的镜像时，这个文件能被解析，构建会停在解析 base
这一步——在任何磁盘操作之前：

```text
$ delonix image vm build -t web:1.0 .
[1/2] builder: FROM my-base:1.0
error invalid argument: FROM my-base:1.0: no such local VM image, and it is not a URL nor a known cloud image (ubuntu:/debian:/rocky:) — see `delonix vm ls`
```

以及解析器的各种拒绝：

```text
VMfile:2: unknown instruction 'CMD' — supported: FROM RUN COPY ENV USER PASSWORD ROOTPASSWORD CLOUDINIT SSHKEY SIZE HOSTNAME VCPUS MEMORY HYPERVISOR LABEL
VMfile:2: no earlier stage named 'nope'
VMfile:2: invalid argument: unknown VM backend: 'vmware' (use 'cloud-hypervisor', 'libvirt')
--offline belong to the built-in golden recipe and mean nothing with a VMfile — the VMfile describes all of that itself
```

一次完整的构建（`virt-customize`、下载、压缩）**在本次复核中没有被执行**。

---

## 对照

| | Dockerfile（Docker/BuildKit） | Delonixfile（`delonix build`） | VMfile（`delonix image vm build`） |
|---|---|---|---|
| 产出物 | OCI 镜像 | OCI 镜像（可推送，也能被 Docker 拉取） | 可启动的 qcow2 + `VmImage` 元数据 |
| 一个阶段的单位 | 文件系统的层 | 一个工作容器 + 根文件系统 | 一整块拍平的磁盘 |
| `FROM` | 镜像 | 镜像或更早的阶段 | distro:release、URL、本地虚拟机镜像，或更早的阶段 |
| `RUN` 在哪里执行 | 构建容器里 | 通过 `exec` 在工作容器里（rootless userns） | 客户机里，通过 `virt-customize`（默认离线） |
| `COPY --from` | 支持 | 支持（按名字或索引） | 支持（仅限已命名的更早阶段；`virt-copy-out`） |
| `COPY` 的 `--chown/--chmod` 参数 | 支持 | 不支持 | 不支持 |
| `ADD` 的 URL / 归档自动解压 | 支持 | 不支持（`ADD` 等同于 `COPY`） | 没有 `ADD` |
| `RUN --mount` | secret、ssh、cache、bind、tmpfs | 仅 `type=secret`/`type=cache` | 没有 |
| `--target` | 支持 | 支持 | 不支持 |
| `--platform` | 支持 | `linux/<arch>`，需要宿主机的 binfmt | 不支持（镜像只有 amd64 —— [ADR-0018](../adr/0018-vm-images-stay-amd64.md)） |
| 层缓存 | 支持 | 仅 rootless，且没有 GC | 没有 |
| `CMD`/`ENTRYPOINT`/`USER`/`ENV` | 镜像配置 | 镜像配置 | 没有 `CMD`；`USER` 会创建一个账户；`ENV` → `/etc/environment` |
| 资源提示 | — | `CPUS`/`MEMORY`/`SECURITY` 只是被记录，不会被应用 | `VCPUS`/`MEMORY`/`HYPERVISOR` 会被 `vm create` 当作默认值应用 |
| 漏洞门禁 | — | `SCAN` 会被解析，但不会被强制（用 `image scan --fail-on`） | — |
| 账户 / 密钥 / 密码 | — | — | `USER`、`SSHKEY`、`PASSWORD`、`ROOTPASSWORD` |
| 磁盘大小 | — | — | `SIZE`（在任何步骤之前） |
| 未知指令 | 报错 | 报错 | 报错 |

## 如果你要改动某种语法

- 让两个解析器始终 **fail-closed**：一个未知指令是一个错误，绝不是一行被跳过的内容。
- 一个新指令落地时，要在解析器的 `mod tests` 里带一个单元测试、在本页加一行——如果
  脚手架用到了它，还要让脚手架测试继续通过。
- 如果一个指令已经被解析，却还没接上任何效果（就像今天的 `SCAN`、`CPUS`/`MEMORY`/
  `SECURITY`，以及 VMfile 的 `LABEL` 那样），就要在这里说明；一个用户写了、引擎却
  忽略掉的字段，必须被这样记录下来，否则就该被去掉。
- Delonixfile 的解析器活在一个库 crate（`delonix-oci`）里，所以它绝不能打印任何东西；
  VMfile 的解析器则在 CLI 二进制程序里。见[各个 crate](crates.md)。

---

**下一篇：** [构建 microVM](microvm-setup.md)——启动和测试 microVM 所需要的宿主机前置条件、各个后端、镜像，以及 day-2 动词。
