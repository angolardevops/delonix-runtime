# Matriz de rastreabilidade RPC — CRI `runtime.v1` (34 RPCs)

Legenda de estado: **IV** implementado e validado (critest/crictl/kubelet) ·
**IN** implementado, validado só por round-trip gRPC/unit · **P** parcial ·
**U** unimplemented (devolve `UNIMPLEMENTED`, não finge sucesso) · **N/A** não
aplicável com fundamento.

Fonte: `crates/interfaces/delonix-cri/src/{lib.rs,runtime_svc.rs,runtime_svc/lifecycle.rs}`.

## RuntimeService

| RPC | Estado | Validação / nota |
|---|---|---|
| Version | IV | `runtime_api_version=v1`, nome `delonix`; `grpc_status.rs` + crictl |
| RunPodSandbox | IV | netns rootless (holder/slirp); critest sandbox lifecycle |
| StopPodSandbox | IV | idempotente; força `-t 0` (contrato: grace já dado); corrige órfãos do `kubeadm reset` |
| RemovePodSandbox | IV | só apaga registo se o `rm -f` do motor vencer (regra do RemoveContainer) |
| PodSandboxStatus | IV | critest |
| ListPodSandbox | IV | filtro por id/estado (teste unitário) |
| CreateContainer | IV | security context completo; cap-ceiling do nó aplicado aqui |
| StartContainer | IV | critest |
| StopContainer | IV | critest; timeout honrado |
| RemoveContainer | IV | idempotente |
| ListContainers | IV | filtro por sandbox (teste unitário) |
| ContainerStatus | IV | exit code/137 OOM, timestamps estáveis, writable_layer real |
| UpdateContainerResources | IV | honour-or-refuse cpuset/unified/hugepages (ADR 0038) |
| ReopenContainerLog | IV | recria o ficheiro + shim segue o inode (rotação) |
| ExecSync | IV | stdout/stderr separados, exit, timeout; entra em todos os ns incl. IPC |
| Exec | IV | SPDY/WebSocket; token com entropia (fail-closed) |
| Attach | IV | `logs -f`; stdin a PID 1 não suportado (por desenho) |
| PortForward | **P** | servidor prepara URL; 2 specs de portforward falham (gap real #12/#17) |
| ContainerStats | IV | cgroup v2 cpu/mem; eviction manager alimentado |
| ListContainerStats | IV | filtro |
| PodSandboxStats | IV | cgroup v2 |
| ListPodSandboxStats | IV | filtro |
| UpdateRuntimeConfig | IV | pod CIDR: honra CNI, avisa na SDN nativa (nunca `Ok` cego) |
| Status | IV | `RuntimeReady`+`NetworkReady` reais (ref-count); `capabilityCeiling` em verbose |
| CheckpointContainer | **U** | exige CRIU; opcional; não finge sucesso |
| GetContainerEvents | **U** | Evented PLEG (opcional); kubelet cai no PLEG genérico |
| ListMetricDescriptors | IN | real (cgroup v2); critest v1.36.0 não exercita — `grpc_status.rs` |
| ListPodSandboxMetrics | IN | idem |
| RuntimeConfig | IV | anuncia `cgroupfs` (teste `the_kubelet_is_told_cgroupfs`) |

## ImageService

| RPC | Estado | Validação / nota |
|---|---|---|
| ListImages | IV | `repo_tags`/`repo_digests` partidos por tipo de referência (**corrigido** #18); `uid`/`username` (**corrigido** #19) |
| ImageStatus | IV | idem; mapeamento partilhado `to_cri_image` |
| PullImage | IV | creds do `imagePullSecrets` (user+pass); por tag e por digest |
| RemoveImage | IV | idempotente (GC do kubelet) |
| ImageFsInfo | **P** | `used_bytes` real; `inodes_used` = 0 (follow-up); devolve image+container fs |

## Cobertura

- **IV**: 27 · **IN**: 2 · **P**: 3 (PortForward, ImageFsInfo, e o conjunto de
  rede rootless) · **U**: 2 (opcionais).
- Nenhuma operação não implementada devolve `Ok`. Os `U` são `UNIMPLEMENTED`
  explícito — o kubelet trata-os como ausência de feature.
