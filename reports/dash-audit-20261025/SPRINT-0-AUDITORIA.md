# Sprint 0 — Auditoria e baseline do `delonix dashboard`

Data: 2026-10-25 · repo `delonix-runtime` v5.1.1 · ratatui 0.29 / crossterm 0.28.1
Branch: `feat/dash-metrics-correctness` (off `develop` @ e8708a02)
Fonte de verdade do processo: `docs/dev/engineering-process.md` (develop-GitFlow).

## 1. Mapa de arquitetura (estado atual)

O dashboard é **um único ficheiro** de 1933 linhas:
`bins/delonix-runtime-bin/src/cmd/dash.rs`. Não há módulo/crate próprio.

Camadas, tal como existem hoje:

| Responsabilidade | Onde vive | Nota |
|---|---|---|
| Aquisição (counts, memória, io, net, storage) | `delonix-mgmt::dashstats::collect/_with_timeout/collect_container_net` | colector ÚNICO partilhado com `/metrics` e `/v1/dash` |
| CPU/mem/io por container | `delonix_linux::container_usage` / `ContainerUsage` | `Option<u64>` **sem razão** (`Why`) |
| CPU do slice | `delonix_linux::slice_cpu_usage_usec` / `slice_budget` | contador cru usec; `host_ncpu` lido mas descartado |
| Qualidade rica (PSI, throttle, OOM, `Why/Unavailable`) | `delonix-telemetry::workload` + `pressure.rs` | **só o servidor `/metrics` usa; o dashboard NÃO** |
| Snapshot puro (dados) | `DashData::collect_with` (dash.rs:232) | testável sem terminal |
| Render ANSI (`--once`/`--json`) | `render_snapshot` (dash.rs:612) | puro sobre `DashData` |
| TUI (event loop, foco, draw) | `mod tui` (dash.rs:747-1764) | ratatui; estado de taxa vive aqui |
| Operações por recurso | **inexistentes no dashboard** | ver §3 do plano |

Entrypoint: `run(scope, once, json)` (dash.rs:729) → `--json` | `--once`/não-tty | `tui::run_interactive`.

Cadência: tick de 1s (counts+memória, barato) + thread de fundo a 15s
(`SLOW_REFRESH`) com tecto 120s (`COLLECT_TIMEOUT`) para rede/storage (walk de
disco medido >1 min num host carregado). CPU% é recomputado por **frame**
(poll de 200 ms) — ver bug G3.

Pontos fortes a preservar: colector único (TUI/JSON/Prometheus não divergem na
aritmética); `Option` vs `-` vs `measuring…` (indisponível≠zero no render);
circuit-breaker `InFlight` para I/O pendurado (NFS); `network_unmeasured_containers`
(host/none ≠ 0); modelo `Why`/`Unavailable` em `pressure.rs`/`workload.rs`.

## 2. Baseline reproduzido (ao vivo, binário 5.1.1)

`delonix dashboard --once` (scope global), neste host:
- CONTAINERS 0/0 · VMs 1/1 · NETWORKS 7 · VOLUMES 27 · IMAGES 26 · SECRETS 17
- MEMORY 76 MiB / 53.07 GiB · TRAFFIC 0 B/0 B · STORAGE 26.93 GiB
- Tabela: 1 VM + 7 redes + volumes; **todas as linhas não-container a `-`** em
  CPU/MEM/I/O/NET; coluna STATUS mostra `bridge` (redes) e `local` (volumes) —
  i.e. o **driver**, não o estado.

Confirma, da lista do enunciado: coluna STATUS mistura estado/driver/id;
tabelas genéricas com colunas inadequadas e campos vazios; nomes/caminhos
longos (mountpoints) no INFO; `OK`/`no problems` sem cobertura de diagnóstico;
`UP`=`-` para quase tudo. (CPU%/mem live não observáveis agora: 0 containers a
correr — reservar prova para quando houver carga.)

Testes atuais: ~17 unitários inline em dash.rs + ~14 em dashstats.rs; smoke
`scripts/e2e.sh` valida `dashboard --once --json`. TUI interativo sem teste
automatizado (precisa de PTY).

## 3. Matriz de gaps

Estado: **C**=Confirmado · **H**=Hipótese · **GF**=Gap funcional · **LP**=Limitação de provider · **BA**=Bloqueado pelo ambiente.

### A. Correção de métricas (foco do Sprint 1)

| # | Requisito | Evidência | Estado | Causa | Impacto | Correção | Teste de aceitação |
|---|---|---|---|---|---|---|---|
| G3 | Taxa = Δcontador / Δintervalo real | dash.rs:1083-1109,1166 | **C** | `cpu_prev` reescrito por frame (~200ms), contador muda por tick (1s): maioria dos frames `cur==u0`→0%, frame pós-tick divide Δ~1s por ~200ms → **~5× inflado** | CPU% falso (0% intermitente + picos 5×); engana triagem | Ancorar a amostra anterior ao **tick** (não ao frame): só recomputar quando o contador muda; manter último % entre ticks | teste de unidade do cálculo: 2 amostras 1s → % correcta; amostra intra-tick não altera |
| G4 | Identidade estável (ctx/provider/tipo/**id**) | dash.rs:1087 chave `kind:name` | **C** | chave por nome, não `id` (já disponível em `ContainerRow`) | nome reutilizado/duplicado colide amostra → Δ errado | chavear `cpu_prev` por `id` estável | teste: dois containers mesmo nome, ids distintos → % independentes |
| G5 | Denominador explícito p/ % | dashstats.rs:145 (`host_ncpu` descartado); `cpu.max` só em 4843-5769 | **C** | nenhuma % de CPU usa denominador; "% de 1 core" sem relação com limite | utilizador não sabe se 180% é mau (2 cores? quota?) | rotular claramente "% de 1 core"; quando `cpu.max`/`host_ncpu` determinável, expor também % do limite/host; nunca inventar denominador | teste: com quota conhecida → %limite; sem quota → só %core + label |
| G6 | Qualidade do limite ≠ sentinela | slice_budget lib.rs:5797; DashSummary:43 | **C** | `memory_bytes_limit=0` colapsa ilimitado/desconhecido/ilegível | gauge de memória desaparece sem dizer porquê | distinguir "sem limite" de "desconhecido"; texto explícito | teste: limit=0 → label "no limit" vs erro de leitura |
| G2 | Estado/qualidade da métrica visível | ContainerUsage lib.rs:10819 (sem `Why`) | **C** | dashboard usa tier pobre; ignora `Why`/PSI/throttle/OOM | não distingue "0" real de "não delegado/ilegível" | manter `-` para None (já ok); Sprint 1 mínimo: não regredir; expor PSI/OOM é Sprint 4 | teste: None→`-`, Some(0)→`0 B` |
| G7 | Rede como taxa (B/s), não só total | dash.rs:318,597 | **C** | só total acumulado; CPU tem taxa, rede não | "TRAFFIC" não mostra throughput | derivar taxa rx/tx no tick (Δ/intervalo), rotular; manter total disponível | teste: 2 amostras → B/s correcta; 1ª amostra → "a recolher" |
| G8 | Timestamp/idade no snapshot | DashSummary:30-66 sem `collected_at` | **C** | idade só no TUI (`slow_age`); `/v1/dash` não a leva | consumidor JSON não distingue fresco/stale | (Sprint 1 opcional) não regredir TUI; expor idade no JSON é candidato | teste: JSON tem campo de idade/age |

### B. Semântica/UI (confirmados; correção nos Sprints 2-4)

| # | Requisito | Evidência | Estado | Causa | Correção (sprint) |
|---|---|---|---|---|---|
| U1 | STATUS = estado de execução, não driver | --once: redes `bridge`, volumes `local` | **C** | `Row.status` recebe `n.driver`/`vol.driver` (dash.rs:456,476) | coluna STATUS só estado; driver/CIDR/mount em colunas próprias (S4) |
| U2 | Tabelas específicas por recurso | --once: colunas CPU/MEM/IO/NET `-` p/ não-container | **C** | tabela única 9-col para todos os tipos | colunas por scope/subtab (S4) |
| U3 | `UP` só onde aplicável | `up="-"` em quase tudo | **C** | só containers têm `pid_starttime` | VMs: uptime do backend; outros: ocultar coluna (S4) |
| U4 | `OK/no problems` com cobertura | draw_problems:1619 | **C** | ausência de problems = "OK" sem dizer quantas verificações | síntese "N verificações, M falhas, K indisp." (S4/S6) |
| U5 | `RUNNING CONTAINERS` sparkline só em scope relevante | dash.rs:1669-1680 | **C** | sparklines fixas global em todos os scopes | gráficos contextuais por scope (S4) |
| U6 | Nome/caminho truncado acessível | TUI widths dash.rs:1560 | **H** | truncagem visual sem painel de detalhe | detalhe/inspector com valor completo (S3) |
| U7 | Refresh ≠ atualidade | age_suffix só em TRAFFIC/STORAGE | **C/parcial** | counts/mem sempre "agora"; sem selo global | cabeçalho com atualidade por fonte (S4) |
| U8 | Estado de ligação/contexto no cabeçalho | inexistente | **GF** | sem cabeçalho persistente | cabeçalho ctx/host/rootless/ligação (S4) |
| U9 | Esc não sai da app toda | dash.rs:1187 Esc=quit | **C** | Esc mapeado a quit | Esc=fechar modal/voltar; `q` sai (S2) |

### C. Navegação / pesquisa / operações (Sprints 2-3)

| # | Requisito | Estado | Nota |
|---|---|---|---|
| N1 | PgUp/PgDn, foco entre painéis, Enter→detalhe | **GF** | só ↑↓/Home/End/Tab hoje |
| N2 | Preservar seleção por id após refresh/sort | **GF** | seleção por índice (dash.rs:1132) |
| N3 | Ctrl+K paleta + parser `kind:… state:…` | **GF** | só `/` filtra por substring de nome |
| O1 | Enter→detalhe (Overview/Metrics/Logs/…); start/stop/restart/exec/attach/remove/inspect | **GF** | **nenhuma operação hoje**; serviços existem (ver inventário ops) |
| O2 | Exec vs Attach distintos; PTY/resize/detach/restauro | **GF/LP** | `exec_with` usa fd 0/1/2+raw global (colide c/ TUI); SIGWINCH só no arranque (lib.rs:9512); `attach -i` não suportado pelo engine |
| O3 | Consola VM (libvirt virsh / CH socket) | **GF** | helpers existem (vm.rs:5530-5760) |
| O4 | Logs por ficheiro CRI (`parse_cri_log_line`) | **GF** | `cmd_logs` escreve stdout/bloqueia; ler ficheiro directo |

## 4. Plano por sprints (dependências)

- **S0 Auditoria** — este documento. ✓
- **S1 Métricas e estados** (esta sessão): G3→G4→G5→G6; G7 se couber. Sem zeros
  artificiais; testes de contador/reset/1ª amostra/denominador. Depende de: nada.
- **S2 Navegação+pesquisa**: modelo de foco, seleção por id (N2), Enter/Esc (U9),
  Ctrl+K+parser (N3). Depende de: refactor leve do estado da TUI (S1 toca o mesmo
  loop — fazer S1 primeiro reduz conflito).
- **S3 Detalhes+operações**: O1-O4. Depende de S2 (Enter→detalhe) e de expor
  `cmd_*` ou chamar `delonix_linux`/`delonix_vm` directo; suspender/retomar TUI
  à volta de `exec_with`.
- **S4 Visual+gráficos**: Overview/Health, tabelas por recurso (U1-U3,U5),
  cabeçalho (U8), layout 80x24/120x30/160x50, alto contraste. Depende de S1 (dados).
- **S5 Robustez+integração**: carga 1k/10k, resiliência, redação de secrets,
  docs, benchmarks p95<100ms. Depende de tudo.

## 5. Decisões e restrições

- Reutilizar `dashstats`/`delonix_linux`/`delonix_vm` — não duplicar regras.
- Sem daemon obrigatório, sem Prometheus obrigatório (daemonless-first).
- ratatui confinado a `delonix-runtime-bin` (arch_fitness impõe; AGENTS.md:1128).
- Integração é por PR squash do dono (hook recusa push/merge); gates: cli-surface
  (`cli_baseline.tsv`), docs_cli_gate, i18n `data/pt.po`, arch, clippy -D warnings.
  Sprint 1 **não** adiciona folhas/flags novas → não mexe em `cli_baseline.tsv`.
