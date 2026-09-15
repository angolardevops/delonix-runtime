# Matriz de providers de VM — medida sobre `fa8c6297`

Binário de `fa8c6297`, roots isolados, recursos `audvm-*`, verificação só-leitura com `virsh -r` e
`qemu-img snapshot -l`. 2026-09-15. Logs: `scratchpad/aud/` (libvirt.log, report.tsv, provider-calls.log).

| Provider | Resultado | Estado |
|---|---|---|
| libvirt (`qemu:///system`) | **51 PASS · 1 FAIL** (CLI, `kind: VirtualMachine`, guest Ubuntu 24.04 real com SSH) | PARTIAL |
| Cloud Hypervisor (EDK2 `CLOUDHV.fd`) | create/stop/start/restart/snapshots/delete PASS; guest responde ARP+ping | PARTIAL (subconjunto) |
| Proxmox VE | `DELONIX_PROXMOX_URL` ausente; `--backend proxmox` recusa com as variáveis a definir | ENVIRONMENT BLOCKED |
| OpenStack | zero ocorrências em `crates/*/src`; `--backend openstack` → `unknown VM backend` rc=1, sem fallback | ABSENT |
| Live migration | ADR-0031 Accepted: NO-GO; só `vm migrate` stop-copy-start (não corrido) | UNSUPPORTED |

## libvirt

| Área | Verificações | Estado |
|---|---|---|
| create idempotente, describe (rc=4 inexistente), stop, start, restart ×2 | confirmados pelo libvirt | PASS |
| snapshot create/ls/duplicado rc=5/restore/stop+start pós-restore/restore inexistente rc=4/rm | `snapshot-list` do libvirt | PASS |
| `kind: VirtualMachine`: validate, plan 2→0, apply idempotente, `spec.backend`, drift de memory rc=2, `get`, destroy | | PASS |
| Guest real `--ssh-key --hostname --wait` + `vm ssh -- hostname` | respondeu o hostname de dentro | PASS |
| Opção não honrável (`--namespace` em libvirt) | recusada explicitamente, nada criado | PASS |
| Nenhuma acção fora do delonix | 427 chamadas às ferramentas do provider, todas com o delonix como pai | PASS |
| **`vm pause` → `vm ls` diz `Stopped`** (libvirt diz `paused`) | reproduzido à mão | **FAIL (G-041)** — corrigido em `origin/main` (`58fdc4e3`), fora de `fa8c6297` |

## Cloud Hypervisor

| Área | Estado |
|---|---|
| create `--wait` com IP verificado por ARP/ping de dentro do holder | PASS |
| snapshot com VM a correr → recusa explícita a nomear a alternativa | PASS |
| snapshots com VM parada (create, duplicado rc=5, restore, rm confirmado no qcow2) | PASS |
| start/restart com guest a responder ~4 s depois | PASS |
| `vm stop`/`restart` imprimem saída do `qemu-img check` no stdout antes do nome | PARTIAL (G-042, cosmético) |
| `kind: VirtualMachine`, pause, `--ssh-key`/`vm ssh` | NOT TESTED |

## Proxmox — defeito conhecido não medido aqui

O `ipconfig0` duplicado que fazia todo o create falhar (`got ARRAY`) entrou em `23af3c45`, presente
em `fa8c6297`, e foi corrigido em `a9c41cb9`/`d66dd486` (`origin/main`). Não verificado ao vivo
(sem nó Proxmox) — **INFERÊNCIA** a partir do histórico git.

## Incidente de host a registar

Durante esta fase o domínio `vmip-ctl` de `qemu:///system` desapareceu. **Não foi criado nem tocado
por esta auditoria**: todas as chamadas destrutivas intermediadas visaram só `audvm-*`. Evidência
circunstancial de outra sessão: worktree `/tmp/wt-integra-vmip` (branch `integra/vm-create-stale-ip`),
`target-vmip`, e `vmip-ctl.log` (21:42) seguido de `vmip-fix.log` (21:47) no libvirt. **Não provado** —
a confirmar com o dono dessa sessão.
