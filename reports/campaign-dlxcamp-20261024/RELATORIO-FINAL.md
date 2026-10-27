# Relatório final — Campanha de testes reais dlxcamp-20261024

Campanha de integração/fiabilidade que **precede** a avaliação de release. Não publicou nada.

## 1. Artefacto e estado exacto testados
- Binário **compilado do próprio código** (não o instalado, que estava 45 commits atrás e sem verbos Docker na raiz).
- Estado inicial `fix/stabilize-develop-after-merges` @ `257a0ee4` (working tree sujo, diff `c062d290`). O dono fez checkout para **`develop` @ `558e5f20`** a meio da campanha (operação concorrente — memória conhecida).
- Binário final recompilado **limpo de develop `558e5f20`**: digest `350a6320e67cb0f80bb37e74fff4ebe24abd6976ff64afdabe16d9c77bc80d50`, `--version` = `delonix 5.1.1 · 558e5f201 (+258 commits) · built 2026-10-24`.
- Delta do estado inicial para develop em código compilável = só `pt.po` (i18n) e `image.rs` dentro de `mod tests` (não entra no release) → **os resultados obtidos antes do checkout permanecem válidos** (runtime idêntico).

## 2. Ambientes e versões
- Zorin OS 18.1 (Ubuntu noble), kernel 7.0.0-38-generic, x86_64, 20 vCPU, 62 GiB RAM.
- Rootless (uid 1000); cgroup2 **delegado: cpu/memory/pids** (não cpuset/io).
- Ferramentas: docker, podman, buildah, kind v-node 1.34.0, kubectl, kubeadm, virsh/qemu, cloud-hypervisor, mkcert, cloudflared, ngrok.
- Imagens VM: `delonix-vm-base:ubuntu-24.04`, `k8s-golden`, appliance proxmox.
- **Disco: bloqueio inicial de 5 GB resolvido** com `cargo clean` reversível do delonix-paas (+35 GB) após autorização; 27 GB no fim.

## 3. Cobertura de requisitos (SEPARADA da taxa de sucesso)
37 casos na matriz, em 13 famílias.
- **Cobertura executada (PASSOU+FALHOU+PARCIAL): 24/37 = 65%.**
- **Não demonstrados: 13/37 = 35%** (9 NÃO EXECUTADO, 1 BLOQUEADO, 1 NÃO APLICÁVEL, + 2 parciais já contados na execução).

## 4. Taxa de sucesso dos casos EXECUTADOS
Dos 24 executados: **20 PASSOU · 1 FALHOU (lacuna) · 3 PARCIAL/com-defeito.**
- Taxa de sucesso limpa = 20/24 = **83%**; com parciais a contar como não-pleno, 20/24.
- (Não inflada: os 13 não-demonstrados estão listados explicitamente, não excluídos.)

## 5. Percursos completos demonstrados ponta-a-ponta
- **Container rootless**: run→ps→exec→logs→stop(gracioso)→start→rm, com processo e cgroup verificados no SO; porta publicada + HTTP externo real; bind mount `:ro`; limites cpu/mem efectivos na hierarquia.
- **Troca de porta a quente** (distintivo): porta trocada sem reiniciar o processo (pid inalterado).
- **Rede/scope**: DNS interno, comunicação por nome, egress; isolamento entre namespaces nomeados (ping 100% loss); `default` partilhado por design.
- **IaC**: validate/plan/apply/idempotência/destroy de um Pod.
- **VM nos dois backends** (libvirt + Cloud Hypervisor): create→guest vivo por SSH (cloud-init done, vcpu/mem efectivos)→stop/start/restart→destroy sem órfãos; VMs do dono intactas.
- **Cluster Kubernetes (kind-mode, sem Docker)**: control-plane Ready v1.34.0 → Deployment nginx + Service → **tráfego pod→service por nome DNS e por ClusterIP** → destroy limpo.
- **Túnel público pinggy**: URL `https://…free.pinggy.net` serviu a origem sintética pela internet.
- **init+template `go`**: projecto gerado → `go build` + `go test` verdes; segredos gerados; compose válido offline.
- **GC seguro**: dry-run distingue em-uso/declarado/debris sem destruir nada do dono.

## 6. Defeitos e severidade
| ID | Sev | Resumo |
|----|-----|--------|
| DEF-002 | P2 | `stack plan`/`apply` diz "unchanged" ao alterar `command` de um Pod — não converge nem sinaliza (convergência declarativa incompleta; `--replace` resolve) |
| DEF-003 | P2 | `tunnel expose` deixa `ssh -R` órfão ao matar o pai — exposição pública persiste até SIGKILL |
| ACHADO-1 | P3 | `run` sem flag de limite de PIDs (lacuna de paridade Docker, §7) |
| DEF-001 | P3 | `--help` de `--namespace` não documenta a excepção do `default` partilhado (ADR-0011) |

Nenhum defeito P0/P1 (bloqueante) encontrado nos percursos executados.

## 7. Correcções e regressões
- Nenhuma correcção de código aplicada (o âmbito priorizou execução/evidência; os defeitos ficam com repro mínima para correcção dirigida).
- Falsos-positivos descartados por investigação (evitaram defeitos inexistentes): isolamento `default` (ADR-0011), exit-codes mascarados por `| tail`/`| head` (cpuset exit 69, strict exit 1), prova OOM mal-desenhada, drift de estado vs config, VM `permission denied` por falta de `-i`.

## 8. Limitações de compatibilidade
- `--cpuset` e IO cgroup: controllers não delegados neste host (rootless) → recusa explícita e correcta (fail-closed), com fix documentado.
- SSH direto host→guest não roteia em CH rootless (topologia slirp); a via é `delonix vm ssh` (confirmado na auditoria em memória).

## 9. Desempenho (medições pontuais, NÃO capacidade absoluta)
- VM Cloud Hypervisor create→running: **7.8 s**; libvirt: **14 s**.
- Cluster kind single-node até control-plane Ready: **~2 min** (coerente com a memória ~2m45s).
- Container stop gracioso de PID1 sem handler: 10.3 s (grace→SIGKILL, = docker).
- Build release incremental do estado: 7m16s (inicial) / 4m40s (develop).

## 10. Estado da limpeza
- **0** containers/redes/clusters/VMs `dlxcamp-*` restantes.
- **0** processos órfãos (dlxcamp, ssh pinggy, cloud-hypervisor), **0** leaves cgroup, **0** ficheiros VM no disco.
- Recursos do dono **intactos**: 10 imagens, 5 redes, VMs opnsense/pve92.
- `cargo clean` do delonix-paas é reversível (recompila quando o dono voltar ao paas).

## 11. Riscos ainda abertos (não demonstrados nesta campanha)
- **KUBEADM offline + HA + kube-vip + CNI** (§13) — o percurso mais pesado, NÃO executado.
- **API/MCP paridade REST/gRPC** (§18), **observabilidade/HTTPS/stress/SonarQube** (§16), **IMAGE build+push/pull por digest** (§7), **múltiplos clusters** (§14), **recuperação por kill do supervisor** (§19), **NETWORK-04 persistência pós-restart**, **túneis cloudflare/ngrok** — todos NÃO EXECUTADOS.
- **Proxmox** (§10) BLOQUEADO (appliance desligado).

## 12. Recomendações para a avaliação de release
1. Tratar **DEF-002** antes do release: a convergência declarativa silenciosamente incompleta é um risco operacional real (o operador confia num "unchanged" falso).
2. Tratar **DEF-003**: exposição pública que sobrevive à paragem é um risco de segurança; ligar o ciclo de vida do `ssh -R` ao do processo pai e adicionar `tunnel stop`/`ls`.
3. Corrigir os P3 (texto `--namespace`; avaliar flag de PIDs) — baixo custo.
4. Agendar uma segunda ronda focada nos 35% não demonstrados, com prioridade a **kubeadm offline** e **API/MCP**, que são load-bearing para a proposta de valor.
5. O núcleo (container/imagem/rede/scope/VM/cluster/init/túnel/GC) demonstrou-se **sólido e sem bloqueantes** nos percursos executados — base aceitável para avançar, condicionada aos dois P2.
