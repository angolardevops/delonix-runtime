# KaaS (Kubernetes-as-a-Service) capability matrix — 2026-10-08

Measured against `main` at `1cbe9639` (tag `v5.0.0`), read-only, in the worktree
`naas-kaas-caas`. Scope: the KaaS slice of the larger NaaS/KaaS/CaaS discovery (CaaS/NaaS are
out of scope here). Every claim below is grounded in a file:line citation or an explicit test
name; `AGENTS.md` is used only as a map to find the current code, never as the source of truth.
Where a historical `AGENTS.md` entry describes a validation that predates or diverges from the
code paths read here, that is called out explicitly rather than inherited as fact.

**What this covers**: the `delonix cluster *` family (`bins/delonix-runtime-bin/src/cmd/cluster.rs`,
`k8s_recipes.rs`, `kubeadm_config.rs`, `lb.rs`, `etcd.rs`, `remote.rs`), the CRI server
(`crates/interfaces/delonix-cri`), the cgroup-hierarchy placement for CRI containers
(`crates/adapters/delonix-linux/src/lib.rs`), and the CRI-facing CNI/hostPort wiring
(`crates/adapters/delonix-sdn/src/cni.rs`, `lifecycle.rs`).

**What this explicitly leaves out**: `cluster create`/kind-mode (containers acting as nodes,
single-host, no SSH) is a *different* delivery mechanism from the `mode: ssh`/`mode: vm`
(kubeadm) path that a KaaS product would actually sell; it is mentioned only where it already
has a capability (e.g. `Destroy`/`Stop`/`Start`/`Health`) that the kubeadm path lacks, so the
gap is visible by contrast.

---

## 1. Capability matrix

| Capacidade | Backend(s) responsável(is) | Implementação | Evidência | Estado | Gap concreto | Teste de aceitação sugerido |
|---|---|---|---|---|---|---|
| Criação de nós (VM) | `delonix-vm` via `cluster.rs` | `create_and_wait` (`cluster.rs:2350`), chama `delonix_vm::create`/`VmEngine` | Unit tests only; no e2e.sh coverage for `cluster kubeadm` | PASS por leitura | Node creation path itself is the same one `delonix vm create` already ships and is validated elsewhere; this plumbing is untested in isolation | `delonix cluster kubeadm --control-plane 1 --workers 1` against a real libvirt/CH host, assert N VMs exist and answer SSH |
| Selecção de imagem validada | `cluster.rs` | `resolve_vm_image`/`resolve_or_pull_vm_image` (`cluster.rs:1914`, `1974`) | Unit-tested name resolution only | PARTIAL | `resolve_vm_image` only matches by NAME CONVENTION (`delonix-vm-k8s:<v>`); it never checks the image's own `k8s_version`/`cloud_init` metadata (which `VmImage` does carry — see AGENTS.md "A imagem base não leva credenciais"/"`HYPERVISOR` no VMfile"). An arbitrary non-k8s image with a matching tag name is accepted silently and only fails later, deep into host prep | `resolve_vm_image` test: an image whose `.json` metadata says `k8s_version: None` but whose tag happens to match the convention is refused with a named reason |
| Bootstrap inicial (`kubeadm init`) | `delonix-cri` (runtime) + `cluster.rs` | `kubeadm_init` (`cluster.rs:2546`), stacked and `--config` (external etcd) paths | Unit tests (`parse_join_info`, `recover_join_info`); **zero** `scripts/e2e.sh` checks (`grep -c "cluster\|kubeadm" scripts/e2e.sh` → 0 hits in this exact code path) | PASS por leitura, não medido ao vivo nesta sessão | ADR-0038 itself states end-to-end validation under this exact `kubeadm_init` invocation (not the earlier, hand-run session logged in `AGENTS.md` under "CLUSTER KUBERNETES REAL A CORRER", which used a *different*, manually-flagged `kubeadm init` and predates this function) "is not claimed" | A VM-based `scripts/e2e.sh` (or a dedicated `scripts/cluster_e2e.sh`) section that runs `cluster kubeadm --control-plane 1 --workers 0` end to end on this host's own hypervisor |
| Papel control-plane vs worker | `cluster.rs` | `ClusterSpec.control_plane`/`.workers` (`HostSpec` lists), `kubeadm_join(..., as_control_plane: bool)` (`cluster.rs:2682`) | Unit tests: `validate_exige_endpoint_com_multiplos_control_planes` etc. | PASS por leitura | none found | n/a |
| Topologia HA — múltiplos control-planes | `cluster.rs` | requires `spec.controlPlaneEndpoint` when `count()>1` (`cluster.rs:379`); `kubeadm_join --control-plane --certificate-key` | Unit-tested validation; not live | PASS por leitura | none beyond the general "not live-validated" caveat | A real 3-control-plane kubeadm cluster, `kubectl get nodes` showing 3 `control-plane` roles |
| Topologia HA — etcd externo | `etcd.rs` (`bootstrap_etcd_cluster`, `push_etcd_client_pki`), `kubeadm_config.rs` (`render_init_config`) | `cluster.rs:1692-1727`; own CA/leaf certs via `rcgen` | Unit-tested quorum validation (`validate_recusa_etcd_external_numero_par_de_hosts`, etc.); not live | PASS por leitura | External-etcd is `mode: ssh`-only by design (`validate()` rejects it for `kind`/`vm`, `cluster.rs:306-330`); member add/remove after bootstrap, cert rotation and migrating a stacked cluster to external are explicitly documented as not built (`AGENTS.md`, "Cluster kubeadm" §etcd) — confirmed by `grep -rn "etcd.*(add\|remove\|rotate)" cmd/etcd.rs` → no hits | A chaos scenario that kills one etcd VM out of 3 and confirms quorum survives + `etcdctl member list` stays consistent |
| HAProxy automático (LB) | `lb.rs` | `build_haproxy_cfg`/`ensure_haproxy` (`lb.rs:15`, `35`), wired from `provision_and_apply` (`cluster.rs:2266-2325`) only for `cluster kubeadm` (VM path) | 3 unit tests on `build_haproxy_cfg`; AGENTS.md records one live run on 2026-07-24 that reached the HAProxy-config-push step (not the full bootstrap) | PASS por leitura, PARTIAL ao vivo (histórico) | `cluster apply -f` (the `mode: ssh` manifest path, pre-existing hosts) accepts a manual `controlPlaneEndpoint` but never auto-provisions an LB — only `cluster kubeadm` (VM-creating path) does | A `cluster kubeadm --control-plane 3` run that reaches `kubectl get nodes` with all 3 control-planes `Ready` behind the generated LB |
| Endpoint do control-plane | `cluster.rs` | `valid_endpoint` (`cluster.rs:270`), validated in `validate()` (`cluster.rs:379-394`) | `validate_recusa_endpoint_malicioso_no_manifesto_completo` (`cluster.rs:3641`) | PASS por leitura | none | n/a |
| CNI — caminho ROOT (CRI real, kubeadm node) | `delonix-cri` + `delonix-sdn::cni` | Node-level CNI chain in the HOST netns, `lifecycle.rs:719-786` (the "ROOT" branch) | `root_cni_readiness`; `scripts/e2e.sh:4750+` section "CRI root/CNI: o hostPort é publicado" (skips without `crictl`) | PASS por leitura, PARTIAL ao vivo (gated on `crictl`) | **`cluster.rs`/`kubeadm_init` never installs/applies ANY CNI plugin or manifest** — `spec.cni` is parsed, validated by round-trip (`cluster.rs:3892`) and then **never read again** (confirmed: `grep -n "\.cni\b" cluster.rs` → only the declaration and the one round-trip-test assertion). `kindnet`'s template-rendering + `kubectl apply` exists ONLY in `kindmode.rs:864-869` (the single-host, container-based "kind mode" cluster), never in the SSH/kubeadm path. A node bootstrapped via `cluster apply`/`cluster kubeadm` is `NotReady` forever unless the operator manually SSHes in and applies a CNI manifest by hand — exactly what the 2026-07-17 `AGENTS.md` session did out-of-band | `kubeadm_init`/`apply_ssh` applies a CNI manifest automatically when `spec.cni == "default"` (reusing the image's own `/kind/manifests/default-cni.yaml`, the same substitution `kindmode.rs` already does), and a test asserting `spec.cni` is actually consumed, not just round-tripped |
| CNI — caminho ROOTLESS opt-in (`DELONIX_CNI=1`) | `delonix-sdn::cni` | `lifecycle.rs:682-710` ("Rootless, opt-in… plugins run in the holder") | Unit tests in `lifecycle.rs` (e.g. `refuse_chain_without_port_mappings` tests at `2885-2920`); no e2e coverage found tying this specifically to a `cluster *` command (this mode is a raw-CRI/`crictl` feature, not surfaced by `cluster.rs` at all) | PASS por leitura, não ligado ao caminho `cluster` | This path exists for `delonix serve cri` under rootless, but `cluster.rs`'s own node-creation paths (`mode: vm`/`mode: ssh`) always provision ROOT VMs running `delonix-cri` as root (systemd service) — so in practice no `cluster *`-created node ever uses this branch. It is a capability of the CRI server, orthogonal to the KaaS command surface | n/a for KaaS specifically — note as "exists, not reachable from `cluster.rs`" |
| `hostPort`/portas publicadas | `delonix-cri` + `delonix-sdn::cni` | `refuse_chain_without_port_mappings`/`with_port_mappings` (`lifecycle.rs:2533-2630`), applied on both the root-CNI and rootless-CNI branches since ADR-0074's "bigger half" fix | `scripts/e2e.sh:4687-4762` ("CRI: hostPort SCTP recusado", "CRI root/CNI: o hostPort é publicado") — gated on `crictl` being installed | PASS por leitura, PARTIAL ao vivo (gated on `crictl`) | SCTP `hostPort` is refused by name on BOTH CNI paths (the engine's own rootless/native ingress never carried SCTP at all — ADR-0074); TCP/UDP flow through `portmap`'s `runtimeConfig.portMappings` capability on both. No gap beyond "not proven without `crictl` present and a kubelet attached" | Run the existing `scripts/e2e.sh` section on a host with `crictl` installed and capture the output as committed evidence (the AGENTS.md convention used for the Proxmox trace files) |
| cgroup driver reportado ao kubelet | `delonix-cri` | `engine_cgroup_driver()` **hardcoded** to `CgroupDriver::Cgroupfs` (`runtime_svc.rs:558-560`) | `the_kubelet_is_told_cgroupfs`-style test at `runtime_svc.rs:764-771` (`assert_eq!(linux.cgroup_driver, CgroupDriver::Cgroupfs as i32)`) | PASS medido (unit), não medido contra kubelet real nesta sessão | The value is a compile-time constant, not a probe of the host — if a future host/VM is deployed with a systemd-only cgroup hierarchy this is still correctly `Cgroupfs` (deliberate per ADR-0038's spike finding), so this is not actually a gap, just worth flagging as a *fixed* answer rather than a *measured* one | n/a — behaviour is intentional and tested |
| `cgroup_parent` honoured (ADR 0038 item 1) | `delonix-linux` | `setup_kube_cgroup` (`lib.rs:8679`), called from the resource-placement dispatcher BEFORE every other cgroup path (`lib.rs:5522`, with an explicit comment citing ADR 0038's precedence) | `KubeCgroupParent::parse` unit tests (`record.rs:1496-1538`); `setup_kube_cgroup`'s own cgroupfs/systemd branches have no dedicated unit test exercising a real `/sys/fs/cgroup` write (needs root); ADR-0038's own spike table (manual `crictl` + `busctl`, 2026-09-15) is the only live evidence, and it predates this exact function | PASS por leitura, PARTIAL ao vivo (spike-level evidence, not regression-tested) | `KubeCgroupDriver::Systemd` is effectively DEAD in practice: since `engine_cgroup_driver()` always answers `Cgroupfs`, a well-behaved kubelet (1.26+, "Using cgroup driver setting received from the CRI runtime") will never send a `*.slice` parent, so the `busctl`/`StartTransientUnit` branch (`lib.rs:8687-8747`) has no live caller under the documented product flow | A live-host chaos/e2e test: real kubeadm node, `crictl runp`+`runc` with `linux.cgroup_parent` set to a `/kubepods/burstable/pod<uid>` path, assert the container's leaf under it with the right `memory.max`/`cpu.max`, survives `daemon-reload` |
| Limite "unspecified = no limit" só sob `cgroup_parent` (item 2) | `delonix-linux` | `kube_limits` (`lib.rs:8525`) | Unit tests at `lib.rs:8828-8846` (memory/cpu `"max"`→`None`, invalid inputs rejected) | PASS medido (unit) | none found in the memory/cpu/weight/cpuset/io subset it covers | n/a |
| `oom_score_adj` / `cpuset_mems` / `unified` / `hugepage_limits` honoured-or-refused (item 3) | — | **Not implemented.** `grep -rn "oom_score_adj\|cpuset_mems\|hugepage" crates/interfaces/delonix-cri/src crates/adapters/delonix-linux/src` → zero hits outside the raw `.proto` struct definition | None | **GAP (confirmed violation of ADR-0038 item 3)** | The CRI proto field `LinuxContainerResources.oom_score_adj` is read from the wire, never once referenced afterwards — it is **silently dropped**, not honoured and not refused. This directly contradicts the ADR's own decision text ("Honour or refuse, never ignore") and the repo-wide "no silent failure" doctrine. Same for `cpuset_mems` (only `cpuset_cpus` is wired) and `unified`/hugepage limits | A pod asking for `oom_score_adj: -998` (what the kubelet sets for Guaranteed QoS pods) is either applied to the leaf (`echo -998 > /proc/<pid>/oom_score_adj`) or the `CreateContainer` call is refused with a named reason — never silently accepted and ignored |
| `UpdateContainerResources` (item 4, in-place pod resize) | `delonix-cri` | `runtime_svc.rs:333-338`, `todo("update_container_resources")` | None (stub) | **GAP** (explicitly a `todo!()`) | In-place vertical pod resize (KEP-1287, stable since k8s 1.33) cannot work on this runtime at all; any attempt returns `UNIMPLEMENTED` | A `crictl updatecontainer` (or a real in-place resize via the apiserver) that successfully changes a running container's `memory.max` with the same PID |
| Eviction-manager stats (item 5) | `delonix-cri` | `writable_layer_dir`/`writable_layer_usage` (`lifecycle.rs:1972-2041`) | Not individually unit-tested in isolation (no dedicated test function found for `writable_layer_usage`); AGENTS.md records a live fix+measurement in a prior session (#316) | PASS por leitura (fix present), não re-medido nesta sessão | none found in the code itself | A live kubelet with `--v=4`, confirm `stat` on the reported `writable_layer` path succeeds (no "no such file" in the kubelet log) |
| Gestão de tokens de join / CA cert hash | `cluster.rs` | `valid_kubeadm_token`/`valid_ca_cert_hash`/`valid_certificate_key` (`cluster.rs:2497-2514`), re-checked at the shell boundary inside `kubeadm_join` itself (`cluster.rs:2692-2697`, "re-check even though the constructors did") | `join_info_rejects_shell_injection_from_remote_output` (`cluster.rs:3675`) | PASS medido (unit) | none found | n/a |
| Gestão de certificados (upload-certs, etcd client PKI) | `cluster.rs`/`etcd.rs` | `kubeadm init --upload-certs`, `kubeadm init phase upload-certs` on recovery (`recover_join_info`, `cluster.rs:2607`), own CA for external etcd via `rcgen` | Not live-tested; certs are re-derived on every `cluster apply` run (no state file) | PASS por leitura | Certificate **rotation** after the initial bootstrap (kubeadm's own `--config` cert renewal, or rotating the delonix-generated etcd CA) is not implemented — matches AGENTS.md's own "por fazer" note for etcd | A `kubeadm certs check-expiration`-equivalent command/flow and a documented rotation procedure |
| kubeconfig — obtenção/cache/merge | `cluster.rs` | `fetch_kubeconfig` (`cluster.rs:2723`, `write_private_temp`+atomic mode, ACH-fixed TOCTOU per AGENTS.md), `merge_into_local_kubeconfig`/`safe_cluster_entry`/`safe_user_entry` (`cluster.rs:2789-2825`, hardened against a compromised control-plane's `admin.conf` per the historical security audit) | Unit tests on `safe_cluster_entry`/`safe_user_entry` exist (grep shows their names used in `#[test]` blocks nearby); the TOCTOU fix itself was validated live per AGENTS.md | PASS medido (unit) + histórico ao vivo | none found | n/a |
| Join de nós (adicionar um nó a um cluster já de pé) | `cluster.rs` | `kubeadm_join` is idempotent — `test -f /etc/kubernetes/kubelet.conf` short-circuits (`cluster.rs:2689-2691`) — so re-running `cluster apply -f` with a manifest that has grown its `hosts` list joins only the new ones | No dedicated "incremental join" test found; the idempotency check itself is exercised only indirectly | PASS por leitura, não medido nesta forma específica | None in the mechanism itself; the GAP is discoverability — there is no `cluster join-node`/`cluster add-node` verb, the operator must know to re-edit the manifest and re-run `apply` | A test: run `apply_ssh` once with 1 worker, then again with 2, assert the first worker's `kubeadm_join` call is skipped (no SSH command sent) and only the second is joined |
| Remoção de nós | — | **Not implemented.** `grep -n "kubeadm reset\|kubectl delete node"  cluster.rs` → zero hits | None | **GAP** | There is no `cluster remove-node`/`cluster drain --delete` path. `Drain` cordons+evicts but never runs `kubeadm reset` on the host nor `kubectl delete node` — the node stays a cluster member, forever marked unschedulable, after a drain. An operator must do this entirely by hand over SSH | `cluster remove-node <name> <node>`: drains, SSHes `kubeadm reset -f`, then `kubectl delete node`, removing it from both the manifest-visible and the apiserver-visible node lists |
| Drain / eliminação controlada de carga de um nó | `cluster.rs` | `drain_node`/`uncordon_node` (`cluster.rs:1187-1241`), pure passthrough to the system `kubectl` | No dedicated test for the shell-out itself (by design — "never reinvent kubectl") | PASS por leitura | none found | n/a |
| Actualização de versão k8s (upgrade) | `cluster.rs` | `cmd_upgrade`/`upgrade_control_plane_leader`/`upgrade_one_node`/`bump_k8s_packages` (`cluster.rs:1306-1445`), drains before / uncordons after each node, mirrors `kubeadm upgrade apply`/`kubeadm upgrade node` manually, one node at a time (explicitly NOT an unattended whole-cluster cascade, per the command's own doc comment) | Not found as a dedicated integration test; relies on `kubectl`/`kubeadm`/`apt` shelled-out commands | PASS por leitura | Only `mode: ssh` is upgradeable this way (the `file` argument loads a `kind: KubernetesCluster` manifest, and `find_upgrade_target` implies the `hosts` list — `mode: vm`/`kind` are not wired to this verb); no automatic rollback if a mid-sequence node upgrade fails | A 3-node cluster upgrade run where one worker's `apt`/`kubeadm upgrade node` is made to fail, confirming the command stops and reports exactly which nodes are upgraded vs not |
| Backup/recuperação do cluster (etcd incluído) | — | **Not implemented.** `etcd.rs` only installs `etcd`/`etcdctl` for health probing (`download_and_cache_etcd`, `etcd.rs:149`) and never calls `etcdctl snapshot save`/`restore`; no `cluster backup`/`cluster restore` verb exists | None | **GAP** | There is no automated etcd snapshot, no cluster-level backup verb, and no documented restore procedure specific to a `KubernetesCluster`. The only overlapping capability is generic VM-level `vm snapshot create` (per-VM qcow2 checkpoint, `AGENTS.md` "vm snapshot create|ls|rm|restore"), which is NOT etcd-consistent across a multi-node cluster and is not wired into `cluster.rs` at all | `cluster backup create <name>`: runs `etcdctl snapshot save` on one etcd member over SSH, pulls the snapshot file, and a matching `cluster backup restore` that re-bootstraps from it (the standard kubeadm DR procedure) |
| Diagnóstico (describe/health) | `cluster.rs` | `cmd_health` (`cluster.rs:1065`, **kind-mode only**, exits non-zero on anything short of fully healthy), `cmd_describe`, `cmd_ls` | Not dedicated unit tests for the live-probe behaviour (by design, exec-based) | PASS por leitura, limitado | `cluster health` explicitly does NOT work for `mode: ssh`/`mode: vm` clusters — its own doc comment says so ("an SSH-provisioned `cluster apply` target has no container to exec into"). A kubeadm/VM cluster's only health signal is `cluster kubectl <name> get nodes` via the passthrough, with no single "is this cluster fully healthy" verdict | `cluster health` extended to the SSH/VM family via the cached kubeconfig (same resolution `drain`/`uncordon` already use), checking node Ready + core-dns Running, not just node count |
| Eliminação controlada do cluster inteiro | `kindmode.rs` (kind-mode) only | `ClusterCmd::Destroy` → `kindmode::destroy` (`cluster.rs:838`) | — | **GAP for `mode: vm`/`mode: ssh`** | There is **no `cluster kubeadm --destroy`/`cluster apply --prune`-equivalent** for VM-provisioned or SSH-provisioned kubeadm clusters. `Destroy`/`Stop`/`Start`/`Prune` are all wired exclusively to kind-mode (`cluster.rs:790-852`, each one calls into `kindmode::*`). Tearing down a `cluster kubeadm`-created cluster today means the operator must `vm rm` every VM by hand, and nothing un-registers the kubeconfig/context or the etcd PKI directory left in `<root>/clusters/<name>/` | A `cluster kubeadm --destroy <name>` (or extending `ClusterCmd::Destroy` to recognise a VM-based cluster by its `<name>-cp*`/`<name>-w*`/`<name>-lb` VM naming convention) that removes the VMs, the kubeconfig cache, and the `~/.kube/config` context |

---

## 2. The four states — what is actually proven for each

`AGENTS.md`'s own historical sessions (and this investigation's own reading) distinguish four
states a "cluster" can be in. For each, this is what exists TODAY in the code, and what evidence
backs it:

1. **Nó ligado por SSH** (reachable, before any k8s command runs). — `wait_for_vm_ssh_ready`
   (VM path, `cluster.rs:2079`) and the implicit connectivity check inside `prepare_host`
   (`cluster.rs:2397`, `mode: ssh`). **Evidence**: none live in this session; `AGENTS.md` records
   that a real SSH attempt against a nonexistent host fails cleanly ("No route to host") in a
   prior session — that is evidence the failure path works, not that the success path was
   exercised against a real reachable host by this exact code.

2. **Bootstrap concluído** (`kubeadm init`/`kubeadm join` returned success). — `kubeadm_init`/
   `kubeadm_join` (`cluster.rs:2546`, `2682`). **Evidence**: unit-tested for the string-parsing/
   validation halves only (`parse_join_info`, `recover_join_info`'s token extraction). The one
   piece of genuinely live evidence in this repo's history (`AGENTS.md`, "CLUSTER KUBERNETES REAL
   A CORRER", 2026-07-17) used a **hand-run** `kubeadm init` with extra flags
   (`featureGates: KubeletInUserNamespace`, `fail-swap-on=false`, a patched kube-proxy ConfigMap)
   that **do not appear anywhere in `kubeadm_init`'s generated command line** today. That session
   validated "can a kubeadm control-plane run on top of this engine's CRI at all" — a real and
   valuable fact — but it is evidence for the CRI, not for the `cluster.rs` code path a user would
   actually invoke. **No e2e test exercises `kubeadm_init`/`kubeadm_join` today.**

3. **Nó `Ready` no cluster** (CNI converged, `kubectl get nodes` shows `Ready`). —
   `wait_for_cluster_ready` (`cluster.rs:1789`) polls for this and degrades to a WARNING, never a
   hard failure, on timeout. **Evidence**: none. And, as the CNI row above establishes, this state
   is **structurally unreachable** through `cluster apply`/`cluster kubeadm` as written today,
   because nothing in that code path ever applies a CNI plugin — a node stays `NotReady`
   indefinitely unless an operator manually SSHes in and runs `kubectl apply -f <cni>.yaml`
   (exactly the manual step the 2026-07-17 session took, and exactly the step `cluster.rs` never
   automates). This is the single biggest gap in the whole matrix: **states 1-2 are reachable by
   code; state 3 requires a manual step the CLI's own `--cni` flag promises but does not deliver.**

4. **Cluster operacional** (a real workload scheduled and `Running`). — Nothing in `cluster.rs`
   attempts this. No smoke-test Deployment, no post-bootstrap pod. **Evidence**: none in this
   code path. The only workload-level evidence anywhere in the project's history is, again, the
   2026-07-17 hand-run session (kube-proxy programming real nftables rules, a node registering) —
   not something the current `cluster *` commands do or verify.

**Honest summary**: states 1 and 2 have working, validated (at the unit level) *code*. States 3
and 4 are either structurally broken by a missing feature (CNI auto-apply) or entirely unattempted
by the current commands. Treating "the CLI printed `cluster "x" ready`" as proof of states 3-4
would be exactly the "relato desonesto" this repo's own culture names and rejects — `apply_ssh`
prints that line (`cluster.rs:1774-1779`) unconditionally once `kubeadm_join` returns, regardless
of whether `wait_for_cluster_ready` ever saw a `Ready` node (that function only warns, never
errors — `cluster.rs:1810-1819`).

---

## 3. O que falta para um cliente comprar isto

Ordered roughly by how much it blocks a sellable product, each pointing at the file where the
equivalent container/VM capability already exists as a pattern to reuse:

1. **CNI auto-apply on bootstrap.** Without it, state 3 (the node even being `Ready`) is
   unreachable by the documented `--cni default` flag. Pattern to reuse: `kindmode.rs:864-869`
   already does exactly this substitution+`kubectl apply` for kind-mode; porting it into
   `apply_ssh`/`kubeadm_init` is mechanical, not a new design.

2. **A declarative `Kind` for the cluster's own desired state (node count, roles, CNI, HA) with
   drift detection**, instead of a one-shot `kubeadm init`/`join` imperative script. Today
   `KubernetesCluster` is explicitly excluded from the stack reconciler
   (`crates/contexts/delonix-stack/src/kinds.rs:793-796`, "a remote procedure over SSH… `stack
   apply` never runs it — grouping it would promise an apply that does not happen"). Pattern to
   reuse: the `stack plan`/`apply`/`destroy` 3-way diff machinery (`delonix-stack`'s
   `reconcile.rs`) that every other Kind in this repo gets — a `KubernetesCluster` whose desired
   node list diffs against the live `kubectl get nodes` output could get real `plan`/`drift`/
   `destroy` support the same way `kind: Volume`/`kind: Network` do.

3. **Node pools.** There is no concept at all (confirmed: zero occurrences of "NodePool"/"node
   pool" anywhere in the Rust source). `spec.workers.hosts`/`spec.workers.replicas` is a single
   flat list/count with one shared image and one shared VM size — no way to express "5 cheap
   workers + 2 GPU workers" as two declared, independently-scaled groups. A customer-facing KaaS
   (unlike this engine's current single-tenant-host CLI) needs this as a first-class concept, not
   a second manifest.

4. **Teardown of VM/SSH-provisioned clusters.** `ClusterCmd::Destroy`/`Stop`/`Start`/`Prune` only
   exist for kind-mode (`cluster.rs:790-852`). A customer who created a cluster with `cluster
   kubeadm` has no supported way to delete it short of manually `vm rm`-ing every VM. Pattern to
   reuse: `kindmode::destroy` already has the right shape (remove nodes, kubeconfig, `~/.kube/
   config` context) — it just never learned the VM-based cluster's naming convention
   (`<name>-cp*`/`<name>-w*`/`<name>-lb`).

5. **Node removal / scale-down.** `Drain` exists; `kubeadm reset` + `kubectl delete node` does
   not (see matrix row). Join (scale-up) is incrementally supported almost by accident
   (`kubeadm_join`'s idempotency check) — there is no symmetric "remove this one node" verb.

6. **etcd/cluster backup and restore.** No `etcdctl snapshot save/restore` wiring exists at all
   (see matrix row) — a customer cannot be told their control-plane data is backed up in any way
   that survives a host failure, beyond ad-hoc VM-level snapshots that are not etcd-consistent.

7. **CSI / dynamically-provisioned storage for workloads.** `docs/adr/0034-csi-daemon-conflict.md`
   is **Status: Proposed** (not Accepted — a deliberate non-decision, correctly documented as such
   rather than silently absent). The documented fallback (`kind: Volume`'s NFS/CIFS/TrueNAS
   provisioning, reused by a community NFS-subdir external-provisioner Deployment) is a real,
   usable answer today, but it requires the customer/operator to deploy that community chart
   themselves — nothing in `cluster.rs` wires it up as part of cluster creation.

8. **`UpdateContainerResources` / in-place pod resize.** `todo!()` (see matrix row). Not fatal to
   a v1 KaaS, but any customer running recent `kubectl` tooling that defaults to in-place resize
   will see an opaque `UNIMPLEMENTED` from the runtime.

9. **A single cross-mode health verdict.** `cluster health` is kind-mode only; the SSH/VM family
   has to be checked by hand via `cluster kubectl <name> get nodes`. A customer-facing product
   needs ONE command/API that answers "is this cluster healthy" regardless of how it was created.

---

## 4. 3 gaps mais graves

1. **CNI is never applied by the code a customer would run — `spec.cni` is a promise with no
   implementation.** (`cluster.rs`: the field is declared at line 156/554, validated by round-trip
   at line 3892, and **never read again** anywhere in `kubeadm_init`/`apply_ssh`/`provision_and_apply`.)
   This is not a cosmetic gap: it means every `cluster apply`/`cluster kubeadm` run today produces
   a cluster whose nodes are permanently `NotReady`, with `wait_for_cluster_ready` silently
   degrading to a warning (`cluster.rs:1810-1819`) and `apply_ssh` printing "cluster \"x\" ready"
   regardless (`cluster.rs:1774`). A customer who trusts that line is being told something false.
   **Fix**: port `kindmode.rs:864-869`'s template substitution + `kubectl apply` into
   `kubeadm_init`/`apply_ssh`, gated on `spec.cni == "default"`, before `wait_for_cluster_ready`
   runs. **Test**: a live or VM-based e2e check that a fresh `cluster kubeadm --control-plane 1`
   reaches `kubectl get nodes` → `Ready` without any manual step, within the existing 180s
   timeout.

2. **`oom_score_adj` (and `cpuset_mems`/`unified`/hugepages) are silently dropped on the CRI path,
   directly contradicting ADR-0038's own decision text ("Honour or refuse, never ignore") and this
   repo's "no silent failure" doctrine.** (Confirmed: zero references to `oom_score_adj` outside
   the raw `.proto` struct, in either `crates/interfaces/delonix-cri/src` or
   `crates/adapters/delonix-linux/src`.) This is a correctness/risk gap, not a crash: on a real
   kubelet, Guaranteed-QoS pods get `oom_score_adj: -998` and BestEffort pods get `+1000`; without
   it, this runtime's OOM-kill ordering under memory pressure does not match what the kubelet
   (and any operator reading `kubectl describe node`'s conditions) believes is true — the node can
   kill the wrong process first. **Fix**: either write `/proc/<pid>/oom_score_adj` for the
   container's PID in `setup_kube_cgroup`/the general spawn path, or refuse `CreateContainer` when
   a non-zero value cannot be honoured (ADR-0038's own stated contract). **Test**: a container
   created via `crictl`/the gRPC API with `oom_score_adj: -998` has that value readable in
   `/proc/<pid>/oom_score_adj` after creation, or the call is refused with a named `DX-` code.

3. **No teardown and no backup for the only cluster topology a customer would actually buy
   (VM-provisioned, multi-node, kubeadm-based).** `ClusterCmd::Destroy`/`Stop`/`Start`/`Prune` are
   wired exclusively to kind-mode (`cluster.rs:790-852`); there is no `etcdctl snapshot`
   anywhere (`etcd.rs` only installs the binaries for health probing). Together these mean: (a) a
   customer cannot cleanly delete what they created without manual, undocumented SSH/`vm rm`
   work, leaving orphaned VMs, kubeconfig cache files and (for external etcd) a PKI directory
   behind; and (b) there is no path to recovering a control-plane after a host failure beyond
   whatever ad-hoc, non-etcd-consistent VM snapshot an operator happened to take. This is the
   "perda de estado do cluster" risk the task brief names directly. **Fix**: extend
   `ClusterCmd::Destroy` to recognise the `<name>-cp*`/`<name>-w*`/`<name>-lb` VM naming
   convention `cluster kubeadm` itself uses and tear those down + the cached kubeconfig/PKI;
   add a `cluster backup create/restore` verb around `etcdctl snapshot save/restore` over SSH.
   **Test**: create a cluster, destroy it, assert zero VMs/kubeconfig/PKI directory remain; create
   a cluster, `cluster backup create`, kill all VMs, re-provision fresh ones, `cluster backup
   restore`, assert the apiserver answers with the same object state as before.

---

## Method notes

- No code was written or modified. No `cargo build`/`test`/VM/root commands were run.
- Every "GAP (confirmed)" claim above is backed by a `grep`/`Read` that found zero matches for the
  expected wiring, quoted with the exact search used, not inferred from `AGENTS.md` prose.
- Where `AGENTS.md` describes a historical live validation, this document distinguishes whether
  that validation exercised the *current* code path (cited by file:line) or a now-divergent manual
  procedure (the 2026-07-17 kubeadm session, which predates and differs from today's
  `kubeadm_init`).
- `scripts/e2e.sh` was checked for `cluster`/`kubeadm`/`etcd`/`cgroup_parent` sections and found to
  have none; the CRI/hostPort sections that DO exist (`crictl`-gated) were cited by line number.
