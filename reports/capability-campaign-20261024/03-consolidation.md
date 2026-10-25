# Consolidação — campanha de capacidades (checkpoint 126/274)

Data: 2026-10-25. Verificação integrada sobre develop HEAD de todo o corpo acumulado (67 commits desde `558e5f20`), antes de prosseguir. Cumpre o §17 do mandato (matriz antes/depois + contagens separadas, sem medir progresso só pela redução de negativos).

## Estado verificado (todos os gates de CI verdes ao mesmo tempo)

- `make gates`: lang_ratchet **3181/1044/116**, arch_fitness (library_prints **82**, layer direction, env_writes 23, self_exec 29), version_gate (5.1.1=v5.1.1), adr_status (85 ADR/81 linhas), dev_docs/handbook, leaves 353/570 — **todos ok**.
- `capability_ratchet`: **126 of 274 (46.0 %)**.
- Não-drift da matriz publicada + evidência-existe (cada `supported` cita teste real): **ok**.
- Testes dos crates da campanha, juntos em HEAD: delonix-runtime-bin 233, delonix-proxmox 134, delonix-provider-libvirt 71, delonix-mgmt 36, delonix-telemetry 14, delonix-cri 95, delonix-compute 126, delonix-linux — **0 falhas**.
- Worktrees e branches da campanha: **limpas**. Tag `v5.1.1`: **intacta**.

Durante a consolidação apanhou-se (e corrigiu-se, `d7c517ca`) uma dívida cross-pair que as verificações por-par tinham deixado passar: 3 `eprintln` de debug nos testes live do proxmox subiram `arch_fitness.library_prints` 82→85. Lição: correr `make gates` **completo** (não só a última linha) por frente.

## Matriz antes/depois

| estado | relatório inicial | agora | Δ |
|---|---|---|---|
| supported | 103 | **126** | **+23** |
| partial | 93 | 78 | −15 |
| not-implemented | 74 | 67 | −7 |
| unsupported-by-provider | 230 | 229 | −1 |
| requires-external-component | 12 | 12 | 0 |
| unavailable-on-host | 3 | 3 | 0 |
| **total** | **515** | **515** | **0** |
| proved/applicable (N2) | — | **126/274 (46.0 %)** | — |

(Reconciliado: o relatório inicial media 103 supported sobre um build anterior à v5.1.1; a baseline regenerada deu 112. Dos 112, a campanha levou a 126.)

## Contagens separadas (§17) — progresso qualificado

**Novas capacidades implementadas (backend novo):**
- `pod.shared-pid` (linux) — `shareProcessNamespace`: join do PID ns do infra no pai antes do clone + guard; cablado CLI e CRI.
- `metrics.network.per-workload` (linux) — séries rx/tx por-container no `/metrics`.
- `metrics.prometheus` (linux) — gauges de uso cpu/mem por-container (antes só contenção).
- `host.capacity` (proxmox) — método `host_capacity` lendo `/nodes/{node}/status`.
- `vm.snapshot.memory` (proxmox) — método `snapshot_config` expõe `vmstate`.

**Funcionalidades completadas + validadas ao vivo (backend existia, faltava prova):**
- libvirt hotplug add: `disk` (R/W no guest), `nic` (tráfego real), `cpu` (nproc↑), `memory` (MemTotal↑) — 4 níveis cada.
- libvirt `vm.cloud-init` (guest consome NoCloud), `vm.snapshot-disk`/`vm.snapshot-restore` (recuperação de dados por guestfish).
- linux `container.oom-detection` (crash_reason=oom_killed), `pod.shared-ipc-uts` (identidade de ns).
- proxmox `vm.snapshot.persistent` (sobrevive a stop/start, nó real).

**Validações acrescentadas:** cenários chaos (`scen_oom`, `scen_pod_shared_ipc_uts`, `scen_pod_shared_pid`), secções e2e (6 novas), testes de integração (gRPC WatchEvents) e testes live proxmox (3).

**Erros de classificação corrigidos (causa, não feature):** libvirt `vm.migration.live` unsupported→not-implemented; linux `storage.lvm-thin`/`storage.zfs-btrfs` not-impl→partial; linux `net.apply.rollback` not-impl→unsupported; CH `vm.ip.observed` unsupported→partial; CH `vm.network.bridge` razão corrigida; linux/proxmox `host.capacity`+`metrics` reclass.

**Demoção por honestidade (métrica desce, confiança sobe):** libvirt `vm.snapshot.memory` supported→partial (recuperação de RAM não observável sem guest com SO).

**Cobertura/robustez:** `shareProcessNamespace` cablado no caminho CRI (kubelet); recusa acionável de shared-pid em `--net host`/CNI (em vez de silêncio).

**Bloqueios de ambiente / limites upstream comprovados:** GPU/CDI — sem hardware, fica "implementado não validado". CH/libvirt `host.capacity` e `provider.events` — verificados correctos como not-implemented (capacidade VM ≠ cgroup; VMs não emitem eventos). Proxmox consola serial/VNC — websocket remoto, graduação exige mudança transversal (fica partial/not-impl honesto).

## Invariante mantido
Cada `+1` em supported passou pelo gate que exige evidência real (teste `check:`/`e2e:`/`live:`/`chaos:`/`test:` existente) — zero promoções só-etiqueta. 15 promoções líquidas a supported, todas com prova; progresso não medido só pela redução de negativos.
