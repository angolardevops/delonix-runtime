# Campanha de conformidade de capacidades — baseline v5.1.1

Objectivo: tratar individualmente todas as entradas parciais, não implementadas, não suportadas, dependentes de componentes externos ou indisponíveis no host da matriz de capacidades do delonix-runtime, implementando suporte real (backend + contratos públicos + testes) onde tecnicamente aplicável — sem alterar apenas etiquetas e sem mexer contadores artificialmente.

## Checkpoint retomável

| campo | valor |
|---|---|
| Baseline | `v5.1.1` = `a0da90f50abccb8d80a14136263d549dce257837` |
| Branch de trabalho | `develop` (descendente linear da baseline, +258/−0) |
| Catálogo | 1.4.0 · Engine 5.1.1 |
| Fonte-de-verdade | `matrix-structured.json` (515 entradas, lossless) |
| Sprint actual | **Sprint 1 em curso** — auditoria §4 + reclassificação A1–A3 + 1.ª promoção real (OOM) a supported |
| Próximo | Sprint 1 — riscos restantes: namespaces partilhados por identidade de processo, restauro de snapshot com dados; depois candidatos B (CH bridge/ip.observed, net.apply.rollback) |

## Métricas antes/depois (medido @ develop)

| estado | relatório | pós-reconciliação (0894caf9) | agora (b6aa53ff) |
|---|---|---|---|
| supported | 103 | 112 | **119** |
| partial | 93 | 86 | 84 |
| not-implemented | 74 | 72 | 68 |
| unsupported-by-provider | 230 | 230 | 229 |
| requires-external-component | 12 | 12 | 12 |
| unavailable-on-host | 3 | 3 | 3 |
| proved/applicable (N2) | — | 112/273 | **119/274 (43.4%)** |

Nota de integridade: no caminho até aqui o `vm.snapshot-memory` (libvirt) foi DEMOVIDO supported→partial de propósito (RAM não observável sem guest com SO) — baixar a métrica por honestidade é progresso (§5/§17); depois `metrics.prometheus` subiu com prova real.

Separação do progresso (§17): 1 promoção real a supported (OOM, com teste live + evidência), 3 reclassificações de causa corrigidas (A1–A3), 2 lacunas completadas em classificação (lvm/zfs-btrfs: backend já existia, agora partial honesto). Sem promoção falsa — cada +1 em supported tem evidência verificada.

## Histórico de commits da campanha

- `0894caf9` docs — Sprint 0 reconciliação (baseline, matriz 515, diff explicado, plano).
- `e082a275` docs — Sprint 1 proposta de reclassificação verificada (auditoria §4, 2 especialistas).
- `8cd5c63b` fix — aplica A1–A3: libvirt `vm.migration.live` unsupported→not-implemented; linux `storage.lvm-thin`/`storage.zfs-btrfs` not-implemented→partial.
- `34f7afd5` docs — marca A1–A3 aplicadas no checkpoint.
- `b6aa53ff` feat — **`container.oom-detection` partial→supported (§9)**: `scen_oom` reforçado para asserir `crash_reason=oom_killed` (distingue OOM de término não-OOM); provado ao vivo. supported 112→113.
- `560939e0` docs — checkpoint pós-OOM.
- `fe3f529e` feat — **`pod.shared-ipc-uts` partial→supported (§9)**: novo `scen_pod_shared_ipc_uts` prova por IDENTIDADE que 2 membros de um pod leem a mesma `/proc/self/ns/{ipc,uts}` e um container fora lê outra; provado ao vivo. supported 113→114. Confirmado em paralelo que `pod.shared-pid` é genuinamente not-implemented (membros recebem `CLONE_NEWPID`; `shareProcessNamespace` não ligado) — feature de Sprint 2+.

- `d8f7390b` fix — `net.apply.rollback` not-impl→unsupported (§4.6, ADR-0059).
- `8838a15d`+`d8ff1c2b` feat+merge — **`pod.shared-pid` not-impl→supported (§9)**: implementa `shareProcessNamespace` (join do PID ns do infra feito no *pai* antes do `clone`, pois `setns(CLONE_NEWPID)` só afeta filhos; `PidnsGuard` restaura). Cenário `scen_pod_shared_pid` prova por identidade + visibilidade de processos. Feito em worktree paralela, provado ao vivo.
- `ca2d7d88`+`bcc2e47e` fix+merge — **CH `vm.ip.observed` unsupported→partial (§4)**: observação por ARP (`--wait`→`sdn_reachable`) validada ao vivo (guest respondeu ARP), mas o IP reportado continua computado do MAC (não lido de lease/agent) → partial honesto, não supported. Feito em worktree paralela.

Integração: as duas frentes paralelas correram cada uma na sua worktree off develop; integradas em série com regeneração única da matriz/ratchet (115/274); worktrees e branches limpas. develop ganhou também, em paralelo (dono), 2 fixes CRI (`91771918`, `7fbd385f`).

- `85c308c9`+`2bbc1cb7` feat+merge — **restauro de snapshot com DADOS (libvirt)** §7/§14: nova secção e2e escreve `DADO-ORIGINAL-v1`→snapshot→corrompe→restore→relê o original (8/8 PASS live). `vm.snapshot-disk`/`vm.snapshot-restore` ganham evidência de recuperação real; `vm.snapshot-memory` DEMOVIDO supported→partial (RAM não observável sem guest com SO). Worktree paralela.
- `2b857465`+`387eefea` fix+merge — CH `vm.network.bridge`: razão corrigida (§13); mantém-se `unsupported-by-provider` por ser decisão arquitetural espelhada (CH rootless→SDN; libvirt→host bridge), não incapacidade do VMM. Sem mudança de estado. Worktree paralela.

- `9b35fffd`+`e28ea711` feat+merge — **cobertura CRI de `shareProcessNamespace`**: `lifecycle.rs` lê `namespace_options.pid==POD`, persiste `SandboxRec.shared_pid` e liga `pod_shared_pid`+`pod_infra_pid` (pid-âncora = 1.º membro vivo) nos membros SDN nativos. 95 testes delonix-cri + 2 novos. Matriz intocada (cobertura; já era supported via CLI). Provado por teste de unidade da decisão (não por pod CRI real — precisaria de kubelet).
- `288b0f48`+`9a2ca746` feat+merge — **`metrics.prometheus` partial→supported (§14)**: os gauges por-container eram só de contenção (throttling/PSI); acrescentei `workload_cpu_usage_seconds_total` + `workload_memory_current_bytes` (sempre presentes no cgroup v2). Scrape live provou as séries por container (mem 331776 B). supported 114→115.

- `e6624da7`+`d6f024c4` feat+merge — **PID partilhado em `--net host`/CNI**: provado empiricamente ao vivo que é IMPOSSÍVEL rootless (`setns(CLONE_NEWPID)` exige `CAP_SYS_ADMIN` na userns dona da pid ns; userns irmã nem abre `ns/pid`). Em vez de silêncio, `lifecycle.rs` passa a RECUSAR com erro accionável (nomeia o fix: largar hostNetwork). pod.shared-pid mantém-se supported pelo caminho nativo. CNI rootless (`DELONIX_CNI=1`) não afetado. Matriz intocada.
- `ad2e7358`+`cb89aee6` feat+merge — **`host.capacity` not-impl→supported (§4/§9/§14)**: o reporte já existia (`node.rs:362 capacity()`, servido em `/v1/node/capacity`, e2e-testado) — só a classificação estava errada. Live: allocatable < total (CPU 17000/20000 millis; mem 53/62 GiB) confirma que reflete o cgroup delegado rootless, não a máquina. supported 115→116.

- `b3323619`+`ba25af84` feat+merge — **`provider.events` partial→supported (§14)**: `WatchEvents` já estava servido (stream com tail incremental); provado ao vivo (`system events --follow` captou um `secret/remove` externo; teste de integração gRPC novo). supported→+1.
- `f210ee3f`+`4b092a0c` feat+merge — **`vm.cloud-init` partial→supported (§7)**: guest declara `DataSourceNoCloud [seed=/dev/vdb]`, marcador `write_files` + hostname aplicados; lidos do disco com guestfish (e2e 5/5 PASS). supported→+1.
- `af3cb654` docs — regenera matriz combinada (118/274, 43.1%).
- `fe6093ad` chore(lang) — **reconciliação lang_ratchet**: a campanha introduziu 16 comentários + 2 nomes de teste em pt-AO; o gate é um ratchet só-descendente (código em inglês). Traduzidos → inglês; lang_ratchet verde (3181/1044/116). Comportamento/evidência intactos.

### Constrangimento de ambiente
Disco do host a **99–100%** (~12G livres após limpeza de worktrees). Cada build release custa ~2–3G de churn; o próximo trabalho build-pesado tem risco de ENOSPC. Mitigado removendo worktrees concluídas; a resolução de fundo é do dono (libertar disco).

- `5519a696`+`1b337766`+`9b00e60d` feat+merge — **`metrics.network.per-workload` partial→supported (§14)**: `/metrics` passa a exportar `workload_network_receive_bytes_total`/`workload_network_transmit_bytes_total` por-container (lidos na netns), cache populada pelo refresh de fundo (reads de netns fora do scrape inline). Scrape live: rx=2306/tx=524 batem com `/proc/net/dev`. Código em inglês (lição lang_ratchet); lang verde. supported 118→119.

## Verificado correcto-como-está (NÃO reperseguir — read-only, sem promoção falsa)

- **CH/libvirt `host.capacity` = not-implemented** está CERTO. HostHealth é o único host-level que os providers VM expõem (teste libvirt:5149). `host.capacity` para um provider VM significaria capacidade VM-alocável do hipervisor (vCPU/RAM para VMs), distinta da capacidade cgroup-delegada de containers que o `/v1/node/capacity` reporta (a que o linux promoveu). Não é o mesmo número → não é mimetizável.
- **CH/libvirt `provider.events` = not-implemented** está CERTO. Confirmado por grep: nenhum `events::emit(..., "vm", ...)` existe — as ações de VM não escrevem no log de eventos; só o linux (container/volume/stack/pod/security) emite. Por isso linux `provider.events` é supported e os VM não.

## Próximas frentes (fila)

1. `provider.async-operations` (linux not-impl; proxmox partial — job handle) (`lifecycle.rs` lê `namespace_options.pid==POD` e injeta `pod_infra_pid`) — hoje pod.shared-pid/ipc-uts são provados pelo caminho público CLI `pod create`; extensão consistente.
2. Restauro de snapshot com dados (§7/§14): provar recuperabilidade, não só criação.
3. Candidatos B restantes: CH `vm.network.bridge` (adapter ignora `net_mode`).
4. Frentes com ambiente dedicado: Proxmox via nó `pve`, rede multi-node, storage, GPU (sem hardware → pendente).

## Ficheiros

- `00-baseline-reconciliation.md` — baseline fixada, matriz regenerada, diff vs relatório explicado, modelo do catálogo, limite OpenStack.
- `01-sprint-plan.md` — plano por sprint/frente, especialistas, critério de promoção (#16), ambientes autorizados.
- `matrix-structured.json` — importação estruturada das 515 entradas (chave `provider|capability`).
- `matrix-declared-5.1.1-descendant.md` — snapshot da matriz declarada regenerada do binário.

## Como regenerar a matriz

```sh
delonix provider matrix              # vista declarada (Markdown, == docs/providers/capability-matrix.md)
delonix provider ls -o json          # vista medida neste host (JSON)
delonix provider describe <prov> -o json
```

## Totais (reconciliados)

supported 112 · partial 86 · not-implemented 72 · unsupported-by-provider 230 · requires-external-component 12 · unavailable-on-host 3 = **515**.
Accionáveis (partial+not-implemented): **158**.
