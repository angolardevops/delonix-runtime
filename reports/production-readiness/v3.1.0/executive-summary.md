# Prontidão de produção — sumário executivo (FASES 1–2)

**Commit:** `fa8c6297` (v3.1.0 + 26). **Data:** 2026-09-15. Nenhum comportamento do runtime foi
alterado. Todas as medições ao vivo usaram `DELONIX_ROOT` e `DELONIX_NET_RUNTIME_DIR` isolados.

## Veredicto: **LAB READY** — candidato a PILOT READY para o caminho CLI rootless

Não sobe para PILOT/PRODUCTION porque há **quatro P1 confirmados ao vivo** (G-008, G-027, G-029
e G-003 na CI) e porque rootful, upgrade/rollback, OCI e a verificação dos artefactos continuam
`NOT TESTED`.

## FACTO VERIFICADO (medido sobre `fa8c6297`)

| Evidência | Resultado |
|---|---|
| Qualidade base (fmt, lang, check, clippy -D warnings, test, doctest, deny) | 7/7 exit 0; **1 626 pass / 0 fail / 1 ignored** |
| Bateria E2E (`scripts/e2e.sh`) | **729 PASS / 1 FAIL / 5 SKIP** — a falha é o limiar do próprio check (G-004), não uma regressão |
| Caos (`scripts/chaos.sh`) | **29 PASS / 0 FAIL / 1 SKIP** |
| CRI (`critest` v1.36.0, rootless) | **82 / 21 / 19 de 103** — +3 face a v0.63.1 (79/24, mesma suite e total) |
| Docker CLI 29.8 real contra `serve docker-api` | ciclo manual create/start/stop/kill/wait/restart/rename/rm funciona; **`docker run` falha em todas as formas** |
| Compose (`compose config`) | topo recusa chaves desconhecidas; **5/5 sub-chaves aninhadas engolidas** |
| CI no próprio commit | `chaos harness` **skipped** com workflow `success` |

## Confirmados ao vivo nesta ronda

| ID | Sev | Achado |
|---|---|---|
| **G-008** | P1 | Anti-spoof fail-open: com o insert `nft` a falhar, `container run` dá rc=0 e o container fica sem regra anti-spoof (controlo: com regra). |
| **G-027** | P1 | `docker run -d` pendura (rc=124 em 60 s). |
| **G-029** | P1 | A Docker API reporta `Up About a minute` para um container já terminado; a CLI diz `Exited`/`Dead` para o mesmo id. |
| **G-026** | P2 | `--secret-files` com `/run` não gravável: container arranca **sem segredos**, rc=0, sem aviso. |
| **G-028** | P2 | `docker run --rm` falha e deixa órfãos (`AutoRemove` ignorado). |
| **G-011** | P2 | Compose ignora sub-chaves desconhecidas (`deploy.placement`, `healthcheck.disable`, `bind.propagation`, …). |
| **G-030** | P2 | `scripts/critest.sh`: meia-isolação e cleanup com comando inexistente (harness). |

## INFERÊNCIA (lida no código, sem reprodução)

- **G-007 (P1)** — falha do mount tmpfs em `--secret-files` escreve os segredos no rootfs do container.
- **G-009 (P1)** — falhas de masked/readonly paths e `setrlimit` só avisam ou são ignoradas.
- **G-010 (P2)** — read-only recursivo não suportado.
- CRI: 2 falhas de port-forward com causa **UNKNOWN** (o servidor não regista erros — G-031).

## Respostas às 10 perguntas

| # | Pergunta | Resposta |
|---|---|---|
| 1 | Substituir Docker em dev local? | **Com a CLI `delonix`, sim para o fluxo coberto pela bateria.** Como drop-in do socket Docker, **não**: `docker run`, pull, logs, exec e Compose falham. |
| 2 | Substituir Podman em servidores rootless? | **Piloto possível** para workloads CLI/stack sem `--secret-files` em `/run` só-leitura e sem depender do anti-spoof sob falha de nft. Rootful e reboot não validados. |
| 3 | CRI de Kubernetes? | **Não para produção.** 82/103 rootless; 3 bugs de imagem/utilizador, OOMKilled e port-forward em falta; sem kubelet real. |
| 4 | Workloads para produção? | Nenhum sem excepção formal. Piloto: serviços stateless em containers/stack rootless, rede default. |
| 5 | Bloqueados? | Kubernetes via CRI; ferramentas via Docker API; isolamento por IP como fronteira de segurança multi-inquilino; `--secret-files` sem teste de `/run`. |
| 6 | Riscos de segurança | G-008 (isolamento fail-open), G-007/G-009 (hardening fail-open), G-020 (actions não pinadas). |
| 7 | Cinco bloqueios | G-008, G-029, G-027, G-005 (CRI), G-024 (rootful não validado). |
| 8 | Próxima release | **3.1.1**: G-008 e G-026 fail-closed com teste; G-029; G-003 (job de caos falha em vez de saltar); G-030. **3.2.0**: `docker run`/pull/logs na API; G-011. |
| 9 | Rollback | Nenhuma mudança aplicada. Para 3.1.1: binário 3.1.0 assinado permanece; o holder novo exige respawn — rollback = reinstalar 3.1.0 e `net netns down/up` numa janela. |
| 10 | Veredicto | **LAB READY** |

## FASE 3 (OCI, storage, providers, PaaS)

| Área | Resultado | Relatório |
|---|---|---|
| OCI pull/digest/load | digest honrado e digest inexistente recusado; traversal de layer contido; **tag+digest em conflito grava etiqueta errada** (G-033); `.tmp` órfãos (G-035) | `oci-conformance.md` |
| Storage sob falha | volume em uso não é removido; restore sem `--force` recusado; restore de arquivo truncado falha sem tocar em dados nem reiniciar; restore válido devolve os dados | `storage-reliability.md` |
| VM providers | libvirt 51 PASS / 1 FAIL (pause→Stopped, G-041); CH núcleo PASS; Proxmox ENVIRONMENT BLOCKED; OpenStack ABSENT; live migration NO-GO | `vm-provider-matrix.md` |
| delonix-paas | bug histórico do reaper **FIXED**; dois gaps novos (G-036 P1, G-037 P2) por leitura de código | `cni-networking.md` |
| origin/main | `b19fddcb` (+9 commits): os achados abertos continuam no código | gap G-040 |

Veredicto consolidado: ver `release-verdict.md` (**LAB READY**, com o caminho CLI rootless de
containers em PILOT READY).

## NÃO VALIDADO

Rootful; SELinux (ambiente); suites OCI upstream; aplicações Compose reais comparadas com Docker
Compose/Podman; CNI ADD/DEL/CHECK com plugins externos; backup interrompido a meio; Proxmox e
OpenStack; upgrade in-place e rollback; reboot do host; assinatura minisign e provenance dos assets
v3.1.0 (exige descarregar ~100 MB, a aguardar autorização); benchmarks (host de produção);
cargo-audit; cobertura; observabilidade (Prometheus/OTel/JSON estável).
