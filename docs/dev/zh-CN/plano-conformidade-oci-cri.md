<!-- translated-from: plano-conformidade-oci-cri.md sha256:fd53478c1e7c11c6637436232549328ed2872216e71bd1550ffd75b59d30c549 -->
# OCI + CRI 合规计划——让 Delonix 与 Docker/Podman 对齐

> **目标。** 让 Delonix 通过与 containerd/CRI-O（CRI）以及任何 OCI 运行时
> （image/distribution/runtime）所通过的**相同的互操作契约**，从而成为 kubelet 和 registry 的
> drop-in 替代——并且，在一个单独且带保留的 track 中，覆盖第三方工具经由 `DOCKER_HOST` 调用的
> Docker Engine API。
>
> **度量原则。** 每个阶段都对着一个**带数字的上游 suite** 收尾，而不是"看起来做好了"。绝不静默
> 偏离（显式拒绝 + 原因）。如同项目其余部分一样：一个在没有修复时失败、有修复时通过的测试。
> 凡是一个测试不适用于本模型（daemonless、rootless）的地方，就带原因标注 **N/A**——不虚增百分比。
>
> **范围，以及 `paridade-docker-podman.md §5` 的保留。** OCI（image/distribution/runtime）和
> CRI 是值得追求完全合规的互操作契约——它们使 Delonix 成为真正的替代品。Docker Engine API
> （Track D）是生态系统的便利；项目本身不建议追求与 Docker CLI 的完全对等，所以 Track D 只覆盖
> 自动化工具所调用的内容，而非交互式的 `docker run`。

## 已测量基线（二进制 `ee615f3b`，2026-10-24）

| 契约 | 状态 | 来源 |
|---|---|---|
| CRI — critest v1.36.0 | **79/103 来自 v0.63（2026-08）——需在 v5.1.1 重新测量**（所有者正在 C2/C3 上运行 critest） | `scripts/critest.sh` |
| OCI Image | **在 v5.1.1 已证明互操作**：delonix→docker **和** delonix→podman + docker→delonix，按 digest（IMG-1 已于 2026-10-24 修复） | `reports/conformance/A1-oci-image-distribution.md` |
| OCI Distribution | 在 v5.1.1 跨引擎 push/pull OK；`distribution-spec` suite 是服务端 → **N/A**（Delonix 是客户端） | A1 |
| OCI Runtime — bundle | **v5.1.1**：ociVersion 1.3.0 ✅、seccomp ✅、`runc run` rootless rc=0 ✅（RT-1/2/3）—— `oci-runtime-tool validate` OK | B1/B2 报告 |
| OCI Runtime — CLI runc | **不存在**（唯一真正的 runtime 缺口）—— RT-4 / ADR-0080 | B1 |
| Docker Engine API | **15/26** 路由（在 v5.1.1 实测） | `delonix serve docker-api --matrix` |

> **来源提示（重要）。** 本计划的第一版沿用了 **v3.1.0** 报告（`fa8c6297`）的缺口矩阵。我们现在
> 处于 **v5.1.1**：对当前代码重新测量表明，若干"缺口"已经**过时（stale）**——mount-propagation
> 已经可用（B2），层的 `.tmp` 已在进程内清理（G-035），而 `tag@digest` 是有效格式，不是 bug
> （G-033）。**本计划由 v5.1.1 上的测量驱动，而非 v3.1.0 矩阵。** 一个"缺口"只有在 v5.1.1 上能
> **复现**时才会入列。

**CRI 的 24 项失败，按领域划分：** AppArmor 9 · Security Context 4 · Mount Propagation 3 ·
Image Manager 2 · Streaming 2 · Multiple Containers 1 · Networking 1 · RO-mount 1 · OOM 1。

## 所有者的决定（2026-10-24）

1. **优先级：CRI 优先**（kubelet drop-in——container+VM+k8s 的差异化点）。
2. **`NoNewPrivs=false`：** 兑现 CRI 的请求（使之合规），**同时在 CLI/正常路径上保持 NNP 默认
   常开**。默认保证不变；只有 CRI 可以放宽它。
3. **OOM post-mortem：** **投资于 supervisor** —— 按容器的 supervisor（它为 `--restart`/`wait`
   已经存在）在拆除（teardown）之前读取 cgroup 并记录 `OOMKilled`/137。它不是 N/A。

---

## Track C — CRI（优先）· gate：在 v5.1.1 重新测量，然后 → rootful 下 ~100/103

**第零步：在 v5.1.1 重新测量 critest**（`scripts/critest.sh`）。那 79/103 来自 v0.63（2026-08），
下面的失败分布是那次运行的——可能已不相符。所有者正在跑这个（worktree
`cri-image-repodigests-and-user` = C2、`cri-portforward-conformance` = C3）。下面各阶段作为**领域**
的地图保留，但那些数字（`+4` 等）只有在重新测量之后才有效。

每个阶段 = critest 的一个失败领域，并点名失败的 spec。

| 阶段 | 领域（spec 数） | 要做什么 | Gate |
|---|---|---|---|
| **C1** | Security Context (4) | `PodPID` 命名空间；`RunAsUserName`；按路径的 `SeccompProfilePath`（接到已经报错而非忽略的 seccomp-custom）；**`NoNewPrivs=false`**（决定 2——只能由 CRI 放宽） | +4 |
| **C2** | Image Manager (2) | 经 CRI 按 digest 拉取；`ImageStatus` 中的 `Uid`/`Username` | +2 |
| **C3** | Streaming (2) | CRI streaming 服务器：`PortForward` + 带 hijack 的 `Exec`/`Attach` 基础（**与 D3 共享**） | +2 |
| **C4** | Networking (1) + Mount Propagation (3) | 仅带容器端口的 port mapping；`rshared`/`rslave`（**来自 Track B2**） | +4 |
| **C5** | RO-mount (1) + Multiple Containers (1) | 非递归的只读挂载；多容器用例 | +2 |
| **C6** | OOM (1) | supervisor 在 cgroup 拆除之前捕获 OOM（决定 3） | +1 |
| **C7** | AppArmor (9) | 转译 CRI 的按容器 profile；在 **rootful** 下运行该 suite（加载 profile 需要特权——rootless 下 N/A，已点名）；证明那些非环境相关的 | 按 rootful/rootless 为 +0 到 +9 |

> **要在最终报告中诚实点名的 N/A：** 那些要求加载 profile（rootless 不具备的特权）的 AppArmor
> spec，在 rootless 下为 N/A，并在 rootful 下证明。其他任何都不算。

**Track B 的前置条件：** C4 依赖 B2（mount propagation）。在 Track C 启动时就做 B2。

---

## Track B — OCI Runtime Spec（为 CRI 赋能）

| 阶段 | 要做什么 | Gate |
|---|---|---|
| **B1** | 运行 `opencontainers/runtime-tools` validation，对着 (a) `image export` 的 bundle + runc/crun 以及 (b) Delonix 自己的 init（config.json → namespaces/caps/seccomp/mounts/hooks/rlimits）。可复现脚本。 | 已发布基线 |
| **B2** | Mount propagation `rshared`/`rslave`（**关闭 CRI 的 3 项——C4**） | runtime-tools + critest |
| **B3** | 递归 readonly + masked/readonly paths（决定 G-009） | runtime-tools |

## Track A — OCI Image + Distribution（镜像互操作的基础）

**在 v5.1.1 重新定基线（2026-10-24）：此 track 大部分已完成——其余是可选的。**
OCI 镜像互操作已在三个引擎上**实际证明**（IMG-1 已修复：`push` 发布 OCI 清单，可被 docker 与
podman 拉取）。v3.1.0 报告的"缺口" G-033/035/039 在重新测量后**出局**：

- **G-033**（tag+digest）—— **不是 bug**：`repo:tag@digest` 是有效且常见的 OCI/Docker 格式（kind
  会用它）；拒绝它会破坏 kind。若有朝一日想检测一个真正的*冲突*（tag 解析到另一个 digest），
  那是本地表示的决定——在被要求之前不在范围内。
- **G-035**（层的 `.tmp`）—— **已在进程内清理**（`overlay.rs:492-508`）。残留极少：只有一个来自
  中途**死亡**进程的孤儿 `.tmp`，`prune` 不会清扫它——这是小改进，可归并进未来一次 `prune`
  的扫荡，不是合规缺口。
- **G-039**（**遗留的** docker-save 归档 `<hash>/layer.tar`）—— 真实但**只影响 pre-OCI 归档**；
  现代 `docker save` 产生 `load` 已接受的 `blobs/sha256/` 布局。低优先级；其修复（path→digest
  映射）在层上有微妙之处。

| 可选 | 剩下的 | Gate |
|---|---|---|
| A-sbom/scan/sign | `image sbom`/`scan`/`verify`/`sign` —— 在 v5.1.1 测量真实覆盖（本轮未测量） | 已发布基线 |
| A-manifest-list | `push` 的 multi-arch（今天每次调用 1 arch） | multi-arch round-trip |

> `oci-image-tool`（image-spec）**无法编译**（`Sirupsen/logrus` 损坏），且 `distribution-spec`
> 是服务端——所以 v5.1.1 上的镜像证明是**引擎间的互操作**，而非一个 suite。

## Track D — Docker Engine API（生态系统；§5 保留）

| 阶段 | 路由 | 为什么 |
|---|---|---|
| **D1** | `GET /containers/{id}/logs`（stdout/stderr framing） | 为 testcontainers 的 `wait_for_logs` 解锁——最大的孤立价值 |
| **D2** | `/events`、`/build`、`/networks`、`/volumes`、`GET …/stats` | compose/kind/工具会触及的表面 |
| **D3** | `POST …/exec` + `…/attach`（HTTP hijack） | **与 C3 共享 streaming 服务器** |

> 刻意不在范围内：对服务器的交互式 `docker run`（Go CLI 的内部行为），以及 `§5` 不建议的
> `--format`/CLI 对等。

---

## 顺序与依赖

**v5.1.1 上的状态：** Track A（镜像）**已证明**（IMG-1）；Track B RT-1/2/3 + B2 **已完成**——只差
**RT-4**（CLI runc，ADR-0080，受 GO 门控）。剩下的真实工作是 RT-4、Track C（待重新测量）和
Track D。

```
[v5.1.1 feito]  A (interop) ✅   B2 mount-prop ✅   RT-1/2/3 bundle ✅
[real restante]
   RT-4 (ADR-0080, GO) ─► runtime-tools validation suite contra o Delonix
   C0 re-medir critest ─► C1 ─► C2 ─► C3 ─┬─► C4 ─► C5 ─► C6 ─► C7
                                          └─(C3 streaming)─► D3 ─► D1 ─► D2
```

1. **Track C**：在 v5.1.1 重新测量 critest（C0），然后按领域收尾——战略回报（由所有者进行中：
   C2/C3）。
2. **RT-4**（当所有者给出 GO 时）：解锁用上游 suite 来测量 runtime。
3. **Track D** 放到最后，带着保留；D3 随 C3 顺带完成（同一个 streaming 服务器）。
4. **Track A**：只剩可选项（sbom/scan/sign、manifest-list）——互操作已经证明。

## 如何测量（可复现，按阶段）

- CRI：`scripts/critest.sh` → `Ran N of 122 ... X Passed | Y Failed`。每个阶段发布 X 的 diff。
- OCI runtime：`runtime-tools` validation（在 A1/B1 安装）；按 spec 的各节发布 pass/fail。
- OCI image/distribution：上游 suite `image-spec`/`distribution-spec`；发布基线和增量。
- Docker API：`delonix serve docker-api --matrix`（已服务/已拒绝的路由）+ `tests/compat/docker_api_smoke.py` 以及一次 testcontainers 运行。
- 每个阶段都带来一个在没有修复时失败的测试（项目的方法）。

## 已知风险 / 张力

- **NNP vs CRI**（决定 2）：放宽 NNP 的那条路径只能经由 CRI 到达，绝不经由 CLI。
- **AppArmor rootless**（C7）：9 项 spec 中有一部分是特权限制，而非缺口——用 rootful 测量以作区分。
- **OOM daemonless**（C6）：supervisor 必须在死亡与拆除之间的窗口内读取 cgroup；真实的竞态。
- **宿主机磁盘 96%**：OCI/CRI suite 会拉取镜像并写入制品——在 A1/B1 之前释放空间。
