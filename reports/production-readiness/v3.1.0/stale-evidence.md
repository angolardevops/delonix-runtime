# Matriz de evidências desactualizadas

Commit analisado: `fa8c6297` (3.1.0 + 26). Critério: **CURRENT** = medido em 3.1.x;
**STALE** = medido numa versão anterior; **UNDATED**; **DOCUMENTATION ONLY**.

## Conclusão

Quase nenhum número publicado foi medido em 3.x. A conformidade CRI, os benchmarks, a
bateria E2E e as comparações Docker/Compose são de v0.x (Julho–Agosto 2026). **Nenhum
workflow corre critest, bench, coverage, o smoke da Docker API ou a matriz de
compatibilidade.** O único trabalho medido em 3.1.x é `docs/sovereignty-engine.md` e as
notas da v3.1.0.

## Evidência de CI no próprio commit (verificada via `gh run view`)

| Workflow | Resultado reportado | Realidade |
|---|---|---|
| `ci.yml` em `fa8c6297` | success | jobs executados |
| `chaos.yml` em `fa8c6297` (run 34988281401) | **success** | `environment probe (userns)` success; **`chaos harness` = skipped** → o arnês de caos **e a bateria E2E não correram** |

## Afirmações contraditas pelo código actual

| Afirmação | Onde | O que o código diz |
|---|---|---|
| Docker API 15 servidas / 7 recusadas; exec/logs/attach/stats/images-create «em lado nenhum» | `../CLAUDE.md` | 14 servidas, **12 recusadas com razão**, as 5 incluídas (`dockerapi.rs:426-532`) |
| critest 77/103 (v0.42.2) | `../CLAUDE.md` | doc diz 79/103 em v0.63.1 — **ainda 3 majors atrás** |
| Compose engole chaves desconhecidas | `../CLAUDE.md` | allowlist no topo/serviço recusa (`compose.rs:1576`); **mas sub-chaves aninhadas continuam ignoradas** |
| Releases só x86_64 | `../CLAUDE.md` | **confirmado** (`release.yml`; assets da v3.1.0) |
| critest 65/103 | `paridade-docker-podman.md:216`, `tests/compat/cri-conformance.sh:6` | 79 |
| profiles/extends/configs/secrets/multi-file «por fazer» | `COMPARACAO-DOCKER-PODMAN.md:130` | implementados |
| `--pids-limit` ausente | `COMPARACAO-DOCKER-PODMAN.md:169` | existe |
| `vm create` sem `--disk-size` | `comparacao-medida.md:126` | existe (`cmd/vm.rs:552`) |
| `cri-conformance.md` | cabeçalho 24 falhas vs «26 falhas» (`:212`) vs «13 das 31» (`:177`) | **documento contradiz-se** |
| `comparacao-medida.md` | recontagem v0.63.1 (`:136`) vs «tabela é v0.53.0, recontagem por fazer» (`:183`) | **documento contradiz-se** |

## Matriz (resumo; detalhe por linha no inventário bruto)

| Classe | Evidência | Número | Medido em |
|---|---|---|---|
| STALE | Conformidade CRI | 79/103 (24 falhas, 19 skip) | critest 1.36.0, **v0.63.1**, 2026-08-25 |
| STALE | Latência run --rm Docker/Podman/Delonix | 299/277/80 ms (só 4a nos três) | v0.63.1 |
| STALE | Latência (tabela principal) | 208/268/91 ms | v0.53.0 |
| STALE | Bateria E2E (relatório) | 198/0/0 | v0.47.0 |
| STALE | Bateria E2E (cabeçalho do script) | **PASS 529 / FAIL 36 / SKIP 5**, executa 91/244 folhas (37%) | v3.0.0, 2026-09-09 |
| STALE | Docker API matriz / SDK smoke | 14/7; 14/14 | v0.42.2 |
| STALE | Auditoria E2E / de segurança | ~50k LOC, 9 crates | 2026-07-21 |
| STALE | Inventário `docs/runtime/current-state.md` | 10 crates, ~71k LOC | 2026-07-30 (hoje 15 crates, 156k LOC) |
| STALE | Code-quality reports | 138.5k LOC, scores | v1.0.0 |
| STALE | SBOM (roadmap) | 380 pacotes | v0.63.1 (Cargo.lock hoje: 412) |
| UNDATED | README / site — CRI 79/103 | 79/103 | sem versão |
| CURRENT | Soberania (0 acessos à rede no start) | — | 3.1.0, 2026-09-15 |
| CURRENT | 19 Kinds, 15 crates, 245 folhas | — | 3.1.0 |
| DOC ONLY | «múltiplas rondas de revisão de segurança» | — | `SECURITY.md:59` |

## Evidência regenerável e quem a corre

| Evidência | Script | CI? |
|---|---|---|
| CRI | `scripts/critest.sh`, `tests/compat/cri-conformance.sh` | **não** |
| Latência | `scripts/bench.sh` | **não** |
| Caos + E2E | `scripts/chaos.sh`, `scripts/e2e.sh` | sim, **mas saltado neste commit** |
| Cobertura | `scripts/coverage.sh` | **não** |
| SBOM | `scripts/sbom.py` | sim, só em tag |
| Matriz Docker API | `delonix serve docker-api --matrix`, `delonix compatibility docker` | **não** (testes de consistência sim) |
| Smoke Docker SDK | `tests/compat/docker_api_smoke.py` | **não** |
