# D7 re-run — `cni: default` → Cilium num cluster kubeadm multi-nó (2026-10-27)

Host: este host rootless. Binários: `delonix`/`delonix-cri` compilados da árvore
`develop @ 9177cd6c` + a alteração de orquestração `default`→cilium (não commitada).

## Comando

    delonix cluster kubeadm --name d7cil --control-plane 1 --workers 1 \
        --memory 4G --vm-image k8s-golden

`--cni` ficou no default (`default`). **Sem `--experimental-cni`.**

## O que FUNCIONOU (escopo #799 — orquestração)

- `default` resolveu para **cilium** (não a bridge node-local): o plano anunciou
  «Installing the cilium pod network», não o passo single-node. — `create.log`
- Bootstrap: kubeadm init → instala cilium (helm template + kubectl apply) →
  join do worker → espera. **Exit 0.**
- `cilium agent Ready on 2/2 node(s); 2/2 node(s) Ready` em 2m12s; cluster ready.
- `kubectl get nodes`: **ambos Ready** (cp1 control-plane, w1), runtime
  `delonix://5.1.1`. — `kubectl-verify.txt`
- cilium DaemonSet 2/2, operator 2/2.
- `cilium status --brief` = OK; `cilium-health`: **Cluster health 2/2 reachable**
  (Node 1/1 + Endpoints 1/1 nos dois nós). — `cilium-health.txt`
- **Confirma o fix bpffs #869 (c95a3b19) ao vivo** — o Cilium ARRANCA onde a
  ADR-0078 D7 original morria com «multiple mount points detected at /sys/fs/bpf».
  A pendência nº1 da ADR-0078 está resolvida.

## O que NÃO funcionou (fora do escopo #799 — defeitos do CRI)

### 1. Tráfego pod-a-pod ENTRE NÓS bloqueado (CNI_ARGS em falta no CRI)

Dois pods busybox fixados a nós distintos (`ping-cp1` 10.244.0.89,
`ping-w1` 10.244.1.122): ICMP e TCP (httpd/wget) falham nos dois sentidos.
— `crossnode-connectivity.txt`

Causa-raiz (logs do agente): os endpoints dos pods normais ficam em
**`reserved:init`** (identity 5) com policy enforcement Enabled, por isso a
política init bloqueia-os. Os logs mostram `k8sPodName=/` e `ciliumEndpointName=/`
**vazios** — o agente não associa o endpoint CNI ao pod Kubernetes, logo não vai
buscar as labels ao apiserver e a identidade nunca resolve. — `diag-identity.txt`,
`diag-apiserver.txt`

O delonix-cri **nunca passa `CNI_ARGS`** ao plugin CNI:
`crates/adapters/delonix-sdn/src/cni.rs:294 build_env()` define apenas
`CNI_COMMAND/CONTAINERID/NETNS/IFNAME/PATH`; o `Target` nem carrega
namespace/nome do pod. O contrato CNI pede `CNI_ARGS` com
`K8S_POD_NAMESPACE`/`K8S_POD_NAME`/`K8S_POD_INFRA_CONTAINER_ID` — que plugins com
identidade/política (Cilium, Calico) leem. Plugins puros (bridge, host-local,
flannel) não precisam, e é por isso que o single-node `default` e o overlay
simples funcionam. O Flannel, sem conceito de identidade, deverá funcionar
cross-node SEM este fix.

**É um defeito do delonix-cri/sdn, não da orquestração (#799).** Candidato a
nova pendência nº1 da ADR-0078 (sucede ao bpffs, já corrigido).

### 2. cilium-envoy em CrashLoopBackOff (/dev/shm em falta)

`panic: cannot open shared memory region /envoy_shared_memory_0 — No such file
or directory` (`hot_restart_impl.cc`). O container do cilium-envoy (proxy L7)
não tem `/dev/shm` (tmpfs). Afeta só features L7 (política L7/ingress), não o
agente/dataplane core nem a prontidão dos nós. — `envoy-crash.txt`.
Segundo defeito estreito do CRI.

## Fix aplicado e RE-PROVADO (CNI_ARGS no CRI)

O dono pediu para corrigir o CNI_ARGS nesta sessão. Feito:
`crates/adapters/delonix-sdn/src/cni.rs` (`Target.args` + `build_env` emite
`CNI_ARGS`; `add`/`attach_named_netns` passam-no) e
`crates/interfaces/delonix-cri/src/runtime_svc/lifecycle.rs` (`cni_k8s_args`
monta `IgnoreUnknown=1;K8S_POD_NAMESPACE=…;K8S_POD_NAME=…;
K8S_POD_INFRA_CONTAINER_ID=…;K8S_POD_UID=…` a partir do `PodSandboxConfig`, no
caminho ROOT). Reconstruído, cluster d7cil **destruído e recriado**.

Resultado (`crossnode-connectivity-FIXED.txt`), mesmo comando
`cluster kubeadm --control-plane 1 --workers 1 --memory 4G` (cni default):

- Cluster ready, `cilium agent Ready on 2/2 node(s); 2/2 node(s) Ready` em 2m07s.
- Identidade do pod RESOLVIDA: o endpoint do `x-w1` (10.244.0.51) ganha
  `identity 5728  k8s:app=xt`, policy enforcement Disabled — já **não**
  `reserved:init`. **0 endpoints em `reserved:init`** no cp1.
- **Pod-a-pod ENTRE NÓS: OK nos dois sentidos** — `x-cp1`(@cp1, 10.244.1.92)
  ⇄ `x-w1`(@w1, 10.244.0.51), HTTP via busybox httpd (`OK-w1`/`OK-cp1`).
- **Serviço/DNS**: `nslookup kubernetes.default` via ClusterIP 10.96.0.10
  resolve 10.96.0.1 — kube-proxy + coredns + datapath cross-node ponta-a-ponta.

## cilium-envoy CORRIGIDO (dois defeitos de mount do CRI)

O `cilium-envoy` (proxy L7) ficava em CrashLoopBackOff por DOIS defeitos, um a
esconder o outro:

1. **`/dev/shm` em falta** (`envoy-crash.txt`): o `setup_dev`/`setup_dev_userns`
   nunca montava o tmpfs `/dev/shm`. Fix: `mount_dev_shm` (`mode=1777,
   size=65536k`) em `delonix-linux/src/lib.rs`, ambos os caminhos. Provado num
   container rootless (`/dev/shm` tmpfs 64m gravável).
2. **ordenação de mounts aninhados**: o pod monta `/var/run/cilium/envoy/` RO e
   `.../sockets` RW; o kubelet lista o filho RW antes do pai RO, e o
   `setup_rootfs` aplicava por ordem de chegada → o pai RO enterrava o filho RW
   → «cannot bind '.../admin.sock': Read-only file system». Fix: sort estável por
   profundidade do destino (`mount_target_depth`), como o runc.

Provado ao vivo (cluster d7env): **cilium-envoy Running 1/1 nos dois nós**,
`/dev/shm` e `/run/cilium/envoy/sockets` ambos `rw` — `envoy-FIXED.txt`. O Cilium
sobe COMPLETO (agent + envoy + operator 2/2).

## Conclusão

Cadeia completa PROVADA ao vivo: orquestração `default`→cilium (#799) +
fix bpffs #869 (cilium arranca) + fix CNI_ARGS (identidade de pod resolve →
pod-a-pod cross-node e serviços funcionam). Um cluster kubeadm multi-nó sobe
funcional com um só `cluster kubeadm --workers 1`, sem opt-in. Resolve a
pendência nº1 da ADR-0078 (bpffs) e acrescenta a pendência nova que a revelava
(CNI_ARGS), já corrigida.
