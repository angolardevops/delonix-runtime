# Veredicto de release — `fa8c6297` (v3.1.0 + 26)

## Veredicto: **LAB READY**

Com excepção documentada, o caminho **CLI rootless de containers/stack num nó único** está em
**PILOT READY**: base de qualidade verde, E2E 729/1 (falha do harness), caos 29/0, storage fail-closed.

## Gates da §22

| Gate | Estado |
|---|---|
| Sem P0 abertos | ✔ nenhum confirmado (G-007 por reproduzir) |
| P1 resolvidos ou com excepção aprovada | ✘ 9 P1 abertos, 4 confirmados ao vivo |
| Rootless e rootful com evidência actual | ✘ só rootless |
| Upgrade e rollback testados | ✘ NOT TESTED |
| Security audit concluído | ✘ parcial (este documento) |
| Matrizes CRI/OCI/Docker API/Compose actuais | ◐ CRI, Docker API e Compose medidos; OCI dirigido, sem suite upstream |
| Chaos e failure injection | ✔ caos 29/0 + injecção G-008 + storage |
| SBOM e provenance presentes | ✔ publicados; ✘ não verificados nesta sessão |
| Artefactos assinados | ✔ minisign (não verificado) |
| Documentação corresponde ao comportamento | ✘ G-025, CLAUDE.md desactualizado |
| Limitações publicadas | ◐ parcial |
| Resultados ligados ao mesmo commit | ✔ `fa8c6297` (origin/main já em `b19fddcb`) |

## Por workload

| Workload | Estado |
|---|---|
| Containers rootless via CLI/`stack apply`, rede default, volumes locais | PILOT READY |
| `--secret-files` | BLOQUEADO até G-007/G-026 |
| Isolamento multi-inquilino por namespace como fronteira de segurança | BLOQUEADO até G-008 |
| Kubernetes via `delonix-cri` | DEVELOPMENT READY |
| Ferramentas via socket Docker (Compose, Testcontainers, Dev Containers) | NOT READY |
| VMs libvirt | LAB READY |
| VMs Cloud Hypervisor | LAB READY |
| VMs Proxmox / OpenStack | NOT READY |
| Rootful / SELinux | NÃO AVALIADO |

## Próxima release recomendada

**3.1.1** (patch de segurança/fiabilidade): G-008, G-007/G-026, G-029, G-003, G-030, e confirmar os
fixes já em `origin/main` (`4a4d029f` OOMKilled, `58fdc4e3` pause, Proxmox `a9c41cb9`).
Critério de saída: E2E 0 FAIL, caos 0 FAIL na CI (não skipped), critest re-medido sobre o commit da tag.

## Rollback

Nenhuma alteração foi aplicada ao produto. Para a 3.1.1: reinstalar o binário 3.1.0 assinado; como
G-008 corre no holder, o rollback exige `delonix net netns down/up` numa janela de manutenção
(os containers são recuperados por reinício).
