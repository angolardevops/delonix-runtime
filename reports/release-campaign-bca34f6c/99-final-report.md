# Relatório final — campanha de testes pré-release `relcamp-bca34f6c`

> Esta campanha fornece **evidências** para a decisão de release; **não** publica o produto.
> Cobertura de requisitos e taxa de sucesso dos testes executados são apresentadas **em separado**.

## 1. Artefacto e estado exacto testados

| | |
|---|---|
| Binário | `target/release/delonix` construído **neste commit** (`cargo build --release`, 3m17s) |
| Versão | `delonix 5.1.1` · `commit: bca34f6c3 (+243 since v5.1.1)` · built 2026-10-23 |
| Digest sha256 | `725112b6454f6431bf6229137f01a10538e012cf64483a89fb9edd986213e328` |
| Branch/commit | `develop` @ `bca34f6c3cfc366e32e4fb5650fbaf541cb46eed`, **working tree limpo** |
| Sidecar MCP | `delonix-mcp` construído no mesmo commit (para §18) |
| Anti-mistura | binário instalado em `~/.local/bin` (`f49d3570c`, stale) **não** usado como SUT |

## 2. Ambientes e versões

Zorin OS 18.1 (Ubuntu 24.04), kernel 7.0.0-38, 20 threads, 62 GiB RAM, disco **96%/44 G livres**.
cgroup v2 (`cpu memory pids` delegados; io/cpuset **não**). Rootless OK (subuid/userns).
Auxiliares: runc, nft, slirp4netns, kind, kubectl, kubeadm, virsh/qemu, cloud-hypervisor, mkcert,
cloudflared, ngrok, pinggy. **docker/podman ausentes** (delonix é o runtime). Imagens em cache
(nginx-unprivileged, redis-alpine, ubuntu, kindest/node v1.34.0, k8s-golden). Online.
Providers: libvirt+cloud-hypervisor available/Ok; proxmox/opnsense NotConfigured.

## 3 & 4. Cobertura de requisitos vs taxa de sucesso (SEPARADAS)

**Cobertura** (dos requisitos do prompt, não "percentagem de verdes"):

| Estado | IDs | Nº |
|---|---|---:|
| **PASSOU** | CLI-1..4, CONTAINER-1..5, IMAGE-1..3, NETWORK-1..4, SCOPE-1..2, IAC(validate/reject/apply/reapply/idempotent/down), INIT-1..6, CLUSTER-1, VM-1, TUNNEL(pinggy full, ngrok/cloudflare establish), API-2(MCP), RECOVERY-1..2 | ~30 |
| **FALHOU** | IAC drift-reconcile (DEF-003); TUNNEL cloudflare edge round-trip (ambiente) | 2 |
| **BLOQUEADO (ambiente)** | CLI-5 (docker/podman ausentes), CLUSTER-3 (kubeadm offline HA — disco/tempo), VM-3 (proxmox NotConfigured), VM-4 (build imagem cloud — disco) | 4 |
| **PARCIAL / NÃO EXECUTADO** | IAC(stack kind apply), TEMPLATE-1 (correr projecto gerado), CLUSTER-2 (workers/config kind), CLUSTER-4 (multi-cluster), VM-2 (cloud-hypervisor), API-1 (parity 4 superfícies), PERFORMANCE-1, §16 security scan/sbom/sign, §16 observabilidade | ~9 |

**Taxa de sucesso dos testes EXECUTADOS:** dos ~32 casos efectivamente corridos, **~30 PASSOU, 1 FALHOU
produto (DEF-003), 1 FALHOU ambiente (cloudflare edge)**. A taxa **não** foi inflacionada excluindo os
bloqueados — estão listados acima e contam contra a cobertura.

## 5. Percursos completos demonstrados (ponta-a-ponta, com verificação independente)

1. **Container rootless**: pull→run→exec→**HTTP 200**→limites **efectivos no kernel** (mem 128 MiB/cpu 0.5)→logs→bind-mount RW→wait(42)→stop→start→restart→**rm sem órfãos** (cgroup/porta/proc).
2. **Imagem**: build→push p/ registry local→**digest igual ao do registry**→rm→**pull por digest**→conteúdo idêntico.
3. **Rede**: **republicação de porta a quente sem reiniciar o PID**; isolamento entre namespaces **por nome E por IP** (nft drop); DNS; cgroup delegate efectivo.
4. **IaC compose**: validate(fail-closed)→apply→**reapply idempotente (mesmo id)**→down com ownership.
5. **init**: 2 templates gerados, 0 placeholders, **segredos aleatórios por projecto**, `.env` gitignored, manifest válido offline.
6. **Cluster kind (sem Docker)**: create→nó Ready→**deploy app + tráfego (port-forward 200 + DNS pod→service)**→destroy limpo.
7. **VM libvirt**: create k8s-golden→**IP via DHCP + ping 2/2**, confirmado em `virsh`→stop/start→**destroy (qcow2/overlay/xml) verificado em libvirt**.
8. **Túnel pinggy**: expose→URL público→**fetch público 200 correlacionado no log de origem (XFF do edge)**→delete.
9. **GC daemonless**: leaf de cgroup órfão **reclamado oportunisticamente** (até por um `ps`) sem tocar nos vivos.
10. **MCP**: initialize+tools/list por protocolo (15 tools, risco classificado).

## 6. Defeitos e severidade

| ID | Sev | Resumo | Estado |
|---|---|---|---|
| [DEF-001](defects/DEF-001-doctor-advertises-pids-limit-flag.md) | P3 | `system doctor`/`resources` anunciam flag `--pids-limit` inexistente (grafia do Docker, guardada por teste no `run`, mas texto livre em `system.rs` ficou para trás) | ABERTO |
| [DEF-002](defects/DEF-002-rm-cannot-target-namespaced-container-by-name.md) | P3 | `rm` não aceita `--namespace`; container fora de `default` só removível por ID (assimetria com `ls`) | ABERTO |
| [DEF-003](defects/DEF-003-compose-up-does-not-restart-stopped-service.md) | **P2** | `compose up` não reconcilia serviço parado p/ Running (contradiz o próprio help e docker-compose) — reconcilia existência, não execução | ABERTO |

Observações menores (não formalizadas como defeito): `cluster delete <name>` dá erro confuso em vez de
sugerir `destroy`; parar túnel com `pkill` do wrapper deixa filhos `ssh`/`ngrok` (a via limpa é `delete gateway`);
SIGKILL a `cluster create` a meio deixou uma rede SDN órfã (`dlx-relc-k8s`) não auto-reclamada (removida à mão).

## 7. Correcções e regressões executadas

Nenhuma correcção de código aplicada — por desenho desta ronda: preservar a reprodução e **não misturar
alteração com medição**. Os 3 defeitos têm reprodução mínima e critério de encerramento registados.

## 8. Limitações de compatibilidade

- **Docker/Podman side-by-side não medível** (ausentes). Paridade medida anterior vive em `docs/comparacao-medida.md` (não re-medida ao vivo).
- `-o yaml` prometido em `plano-folhas.md §8` mas `ps -o` só oferece `table|json` (DISCREPÂNCIA de contrato — baixa).
- `rmi` é single-image (docker é variádico); `compose` sem `-f` global (usa discovery por directório).
- **cloudflare edge** não fiável neste sandbox (525/timeout) — ngrok/pinggy fiáveis.

## 9. Resultados de desempenho

**NÃO EXECUTADO** (`make stress`/`bench.sh` com SLO). Única medição lateral: control-plane kind Ready
em ~1m07s (arranque), VM libvirt a IP em ~34s, pull nginx ~6s. **Não** representam capacidade absoluta.

## 10. Estado da limpeza

**Limpo e verificado.** 0 recursos `relc-*` (containers/redes/vms/clusters/túneis/imagens/leaves de cgroup);
0 processos de túnel/ssh/ngrok/cloudflared pendentes; portas libertadas. **Preservados**: `kaeso-odoo`/`kaeso-db`
up, 5 redes pré-existentes, 6 túneis pré-existentes, VM `opnsense` running, serviço `delonix-engine prod`.
Disco não cheio (mantido ~96%, nunca empurrado). Imagens em cache (`registry:2`, etc.) reutilizáveis — não são órfãos.

## 11. Riscos ainda abertos (para a decisão de release)

1. **Reconciliação IaC incompleta (DEF-003, P2):** reaplicar não recupera serviços parados — afecta recuperação declarativa e paridade compose.
2. **kubeadm offline HA (§13) por provar:** imagem `k8s-golden` existe e arranca, mas o percurso completo (multi-VM, kube-vip, CNI profiles, falha de CP, egress bloqueado) **não foi exercitado** — a capacidade mais complexa do produto fica sem evidência live.
3. **Segurança/supply-chain (§16) não medidos nesta ronda:** `image scan/sbom/sign/verify`, SonarQube, `make sec` — por correr. O scanner existe mas a cobertura/base CVE não foi verificada.
4. **Observabilidade e carga (§16) não medidas:** Prometheus/Grafana/traces, `make stress`/SLO.
5. **Parity de APIs (§18) parcial:** só MCP validado pelo protocolo; REST/gRPC/docker-api do SUT não servidos nesta ronda.
6. **Robustez a interrupção mid-mutation (§19) assenta no arnês do repo** (`chaos.sh`), não re-corrido aqui; SIGKILL a `cluster create` deixou rede órfã — a resiliência de criação de cluster a kill abrupto merece verificação.

## 12. Recomendações para a avaliação de release

- **Tratar DEF-003 antes do release** (ou reclassificar `compose up` e corrigir o help/promessa de paridade).
- **Correr o percurso §13 (kubeadm offline) com folga de disco** — é a lacuna de maior risco; a infra (golden image, `cluster kubeadm`) está presente.
- **Executar a bateria de segurança/supply-chain e observabilidade** antes de qualquer selo de "production-ready" — hoje são lacunas de evidência, não negativos.
- DEF-001/DEF-002 são P3 cosméticos/UX — corrigíveis em conjunto, baixo risco.
- **Núcleo forte e demonstrado:** container/imagem/rede/scope/GC rootless, VM libvirt, cluster kind com tráfego, túnel pinggy, init — todos com verificação independente. O caminho **single-node rootless (containers + kind + VM libvirt)** tem evidência live sólida neste commit.

---
*Evidência: `reports/release-campaign-bca34f6c/` (`00`–`12` folhas, `defects/`, `evidence/` logs). Matriz: `01-traceability-matrix.md`.*
