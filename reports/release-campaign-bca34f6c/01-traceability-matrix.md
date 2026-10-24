# Matriz de rastreabilidade — campanha `relcamp-bca34f6c`

Consolida os requisitos do prompt de campanha com os contratos do repositório
(`docs/testing/folhas-inventario.md`, medido 2026-10-20 @ `2663c846`; esta
campanha re-mede @ `bca34f6c`). **Cobertura de requisitos ≠ taxa de sucesso** —
ver secção final do relatório.

Estados: `PASSOU` · `FALHOU` · `BLOQUEADO` · `NÃO EXECUTADO` · `N/A (justificada)`.
Este ficheiro é vivo: o estado é actualizado à medida que cada percurso corre.
Evidência = ficheiro sob `evidence/` ou folha do relatório.

## Estados resolvidos no fim da campanha (ver `99-final-report.md`)

| ID | Estado final | Evidência |
|---|---|---|
| CLI-1,2,3,4 | **PASSOU** | `02-cli.md` |
| CLI-5 | **BLOQUEADO** (docker/podman ausentes) | `02-cli.md` |
| CONTAINER-1..5 | **PASSOU** | `03-containers.md` |
| IMAGE-1,2,3 | **PASSOU** | `04-images.md` |
| NETWORK-1,2,3,4 · SCOPE-1,2 | **PASSOU** | `05-network.md` |
| IAC-1 (validate/reject/apply/reapply/idempotente/down) | **PASSOU** | `07-iac.md` |
| IAC-1 (drift→reconcile) | **FALHOU** (DEF-003 P2) | `07-iac.md` |
| IAC-1 (stack `kind:` apply) | NÃO EXECUTADO | — |
| INIT-1 (+estrutura/segredos/manifest) | **PASSOU** | `06-init-templates.md` |
| TEMPLATE-1 (correr projecto gerado) | NÃO EXECUTADO | — |
| CLUSTER-1 (kind + app + tráfego) | **PASSOU** | `08-cluster.md` |
| CLUSTER-2 (workers/config kind) | PARCIAL/NÃO EXECUTADO | `08-cluster.md` |
| CLUSTER-3 (kubeadm offline HA) | **BLOQUEADO** (disco/tempo) | `08-cluster.md` |
| CLUSTER-4 (multi-cluster) | NÃO EXECUTADO | `08-cluster.md` |
| VM-1 (libvirt ciclo) | **PASSOU** (verif. em virsh) | `09-vm-and-api.md` |
| VM-2 (cloud-hypervisor) | NÃO EXECUTADO | `09-vm-and-api.md` |
| VM-3 (proxmox) | **BLOQUEADO** (NotConfigured) | `09-vm-and-api.md` |
| VM-4 (build imagem cloud) | **BLOQUEADO** (disco) | `09-vm-and-api.md` |
| TUNNEL-1 (pinggy round-trip) | **PASSOU** | `10-tunnels.md` |
| TUNNEL-1 (ngrok/cloudflare establish) | **PASSOU**/edge-FALHOU(ambiente) | `10-tunnels.md` |
| TUNNEL-2 (stop/remove) | **PASSOU** | `10-tunnels.md` |
| API-2 (MCP tools/list + risco) | **PASSOU** | `09-vm-and-api.md` |
| API-1 (parity 4 superfícies) | PARCIAL | `09-vm-and-api.md` |
| RECOVERY-1 (resiliência) · RECOVERY-2 (GC) | **PASSOU** | `12-recovery-gc.md` |
| PERFORMANCE-1 · §16 security/observ. | NÃO EXECUTADO | `99-final-report.md` §11 |

| ID | Requisito (prompt §) | Fonte repo | Caso de teste | Pré-condições | Resultado esperado | Evidência | Estado |
|---|---|---|---|---|---|---|---|
| CLI-1 | §6 `--version`, help, help por recurso | folhas CLI (300) | `delonix --version`, `--help`, `<grp> --help` | binário fresco | versão=commit HEAD; help coerente | `02-cli.md` | NÃO EXECUTADO |
| CLI-2 | §6 comandos/flags inválidos, exit codes | plano §2 | argumentos inválidos → `DX-` + exit code | — | código DX estável, exit≠0, sem efeito | `02-cli.md` | NÃO EXECUTADO |
| CLI-3 | §6 output estruturado `-o json/yaml` | plano §8 | `-o json` contra `docs/schema/v1` | — | schema estável | `02-cli.md` | NÃO EXECUTADO |
| CLI-4 | §6 equivalência directa/agrupada (`ps`↔`container ls`) | memória `root-docker-verbs` | comparar saídas | — | equivalentes | `02-cli.md` | NÃO EXECUTADO |
| CLI-5 | §6 comparação Docker/Podman | `docs/paridade-docker-podman.md` | trocar executável | docker/podman | paridade medida | — | **BLOQUEADO** (docker/podman ausentes) |
| CONTAINER-1 | §7 ciclo pull→rm rootless | folhas CLI classe A | nginx-unprivileged ciclo completo | online/cache | estado real no kernel/netns/fs | `03-containers.md` | NÃO EXECUTADO |
| CONTAINER-2 | §7 exec, logs, wait, exit code | folhas CLI | exec echo; logs; wait | container up | resultado observado | `03-containers.md` | NÃO EXECUTADO |
| CONTAINER-3 | §7 limites CPU/mem/PIDs efectivos | plano §1 | `--memory/--cpus/--pids` + ler hierarquia | cgroup v2 | valores efectivos na hierarquia | `03-containers.md` | NÃO EXECUTADO |
| CONTAINER-4 | §7 casos de erro (img inexistente, conflito nome/porta, sinal ignorado) | plano §2/3 | provocar cada falha | — | classe de recusa correcta, sem órfãos | `03-containers.md` | NÃO EXECUTADO |
| CONTAINER-5 | §7 `--rm`, detached, env, user, entrypoint | folhas CLI | variações | — | comportamento documentado | `03-containers.md` | NÃO EXECUTADO |
| IMAGE-1 | §7 inspect/digest | folhas CLI | `image inspect` | imagem cache | digest estável | `04-images.md` | NÃO EXECUTADO |
| IMAGE-2 | §7 build | folhas CLI | `delonix build` Dockerfile mínimo | — | artefacto final correcto | `04-images.md` | NÃO EXECUTADO |
| IMAGE-3 | §7 push+pull por digest (registry local) | plano §1 | registry local → push → pull | registry local | recupera por digest | `04-images.md` | NÃO EXECUTADO |
| NETWORK-1 | §8 topologia real (`delonix0`/bridges) | `docs/guia-rede.md` | reconstruir topologia | — | bridge/veth/nft observados | `05-network.md` | NÃO EXECUTADO |
| NETWORK-2 | §8 publicação de porta + HTTP externo | folhas CLI | `-p` + curl externo | — | HTTP 200 fora do container | `05-network.md` | NÃO EXECUTADO |
| NETWORK-3 | §8 republicação de porta em execução | `e2e_ports.sh` | alterar porta sem reiniciar proc | container up | processo intacto, porta nova/velha | `05-network.md` | NÃO EXECUTADO |
| NETWORK-4 | §8 DNS, isolamento entre scopes | folhas CLI | 2 redes, resolução + bloqueio | — | DNS ok; cross-scope bloqueado | `05-network.md` | NÃO EXECUTADO |
| SCOPE-1 | §8 cgroups v2 delegate efectivos | plano §1 | carga limitada, ler hierarquia | cpu/mem/pids delegados | valores efectivos | `05-network.md` | NÃO EXECUTADO |
| SCOPE-2 | §8 namespace lógico vs Linux ns vs context | folhas kinds | nomes iguais em scopes diferentes | — | resolução correcta | `05-network.md` | NÃO EXECUTADO |
| INIT-1 | §15 `init` wizard/não-interactivo/nome | folhas CLI + `init-accept.sh` | gerar projecto | — | estrutura sem placeholders/segredos | `06-init-templates.md` | NÃO EXECUTADO |
| TEMPLATE-1 | §15/16 projecto gerado corre (build/test/doctor/dev) | `e2e-all-templates.sh` | make doctor/dev + pedido funcional | toolchains | app responde; Redis/RabbitMQ | `06-init-templates.md` | NÃO EXECUTADO (risco-based) |
| IAC-1 | §9 compose/stack validate→apply→reapply→drift→rm | `docs/gitops.md` | ciclo IaC | — | estado real, sem recriação, ownership | `07-iac.md` | NÃO EXECUTADO |
| CLUSTER-1 | §12 kind simples + app + tráfego | `e2e_cluster_kind.sh` | criar cluster, deployar, curl Service | kindest/node cache | tráfego servido | `08-cluster.md` | NÃO EXECUTADO |
| CLUSTER-2 | §12 workers/mounts/portmap/kubeconfig | `kindconfig.rs` | config kind real | — | campos honrados/recusados por nome | `08-cluster.md` | NÃO EXECUTADO |
| CLUSTER-3 | §13 kubeadm offline (k8s-golden, kube-vip, CNI, HA) | `docs/guia-vm-lab.md` | VMs + init + join + CNI + falha CP | imagem k8s-golden, provider VM | HA dentro do contrato | — | **BLOQUEADO** (imagem golden + disco 45G) |
| CLUSTER-4 | §14 multi-cluster + kubeconfig preservado | — | 2 clusters, contextos | recursos | entradas preservadas | `08-cluster.md` | NÃO EXECUTADO |
| VM-1 | §10 libvirt/KVM ciclo criar→arrancar→rede→rm | `delonix-provider-libvirt` | VM pequena | virsh + imagem | guest arranca, cloud-init, rm limpo | `09-vm.md` | NÃO EXECUTADO |
| VM-2 | §10 Cloud Hypervisor ciclo | `delonix-provider-cloud-hypervisor` | VM CH | CH binário + imagem | idem | `09-vm.md` | NÃO EXECUTADO |
| VM-3 | §10 Proxmox provider | `delonix-proxmox` | create/snapshot/destroy | PVE live | sem órfãos no provider | `09-vm.md` | NÃO EXECUTADO (pve92 desligada) |
| VM-4 | §11 build imagem cloud + boot + cloud-init | `images/` | fluxo público build | disco | arranca, sem segredos do builder | — | **BLOQUEADO** (disco 45G) |
| TUNNEL-1 | §17 cloudflare/pinggy/ngrok HTTP(S) + origem | memória `tunnel-audit` | app sintética + túnel + curl público | clientes presentes | endpoint público responde | `10-tunnels.md` | NÃO EXECUTADO |
| TUNNEL-2 | §17 credencial inválida, origem indisponível, reconexão, stop | `tunnel.rs` | provocar falhas | — | erro claro, supervisão | `10-tunnels.md` | NÃO EXECUTADO |
| API-1 | §18 CLI vs REST vs gRPC vs MCP (op permitida/negada) | folhas node-api/mgmt/mcp | mesma op nas superfícies | servidores | identidade/autz/validação coerentes | `11-api-mcp.md` | NÃO EXECUTADO |
| API-2 | §18 MCP `tools/call` não contorna o núcleo | folhas mcp (E3) | cliente MCP | — | sem privilégio arbitrário | `11-api-mcp.md` | NÃO EXECUTADO |
| RECOVERY-1 | §19 interrupção (SIGINT/SIGKILL) a meio de mutação | folhas caso 6 + `chaos.sh` | matar a meio, correr de novo | — | recupera/limpa, zero órfãos | `12-recovery-gc.md` | NÃO EXECUTADO |
| RECOVERY-2 | §19 GC com recursos activos | memória `startup-cgroup-gc` | `system prune` com containers vivos | containers up | usados não removidos | `12-recovery-gc.md` | NÃO EXECUTADO |
| PERFORMANCE-1 | §16 `make stress`/bench com SLO | `bench.sh` | workload definido | — | RPS medido (não absoluto) | `13-performance.md` | NÃO EXECUTADO (risco-based) |

## Capacidades prometidas mas potencialmente ausentes (a classificar como FALHA de requisito, não N/A)

- CLI-5 (Docker/Podman side-by-side): os binários de referência não estão instalados → **BLOQUEADO** (ambiente), não N/A. O repo tem comparação medida anterior (`docs/comparacao-medida.md`); esta campanha não a re-mede ao vivo.
- CLUSTER-3 / VM-4: dependem de imagem `k8s-golden` e de espaço em disco (45 G livres). **BLOQUEADO (ambiente)** até haver a imagem e espaço; não é N/A.
