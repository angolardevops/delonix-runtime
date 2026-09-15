# Conformidade CRI — medida sobre `fa8c6297`

## Medição

| Campo | Valor |
|---|---|
| Suite | cri-tools `critest` **v1.36.0** (upstream) |
| Motor | `delonix-cri` construído de `fa8c6297` (debug), **rootless** |
| Kernel / cgroups / LSM | Linux 7.0.0-31 / v2 / AppArmor (sem SELinux) |
| Estado | `DELONIX_ROOT` via `mktemp` + `DELONIX_NET_RUNTIME_DIR=/tmp/dxcri-n` isolados |
| Comando | `NO_BUILD=1 DELONIX_NET_RUNTIME_DIR=/tmp/dxcri-n scripts/critest.sh` |
| Data / duração | 2026-09-15, 1 129 s |
| Logs | `raw/critest.log`, `raw/critest.log.server` |

```
Ran 103 of 122 Specs in 1129.479 seconds
FAIL! -- 82 Passed | 21 Failed | 0 Pending | 19 Skipped
```

**Comparável** com o valor publicado (79/24 de 103, critest v1.36.0, v0.63.1): mesma suite,
mesmo total. **+3 passam** — todos em AppArmor (9 → 6 falhas). As outras 15 falhas são as
mesmas áreas de antes.

Rootful: **NOT RUN** (host de produção). SELinux: **ENVIRONMENT BLOCKED**.

## As 21 falhas, por causa

| # | Spec | Mensagem | Causa | Nota |
|---|---|---|---|---|
| 1–6 | AppArmor (6 specs) | `InvalidArgument: AppArmor profile '…deny-write' is not loaded on the host` | **HOST ENVIRONMENT** | O critest carrega o perfil com `apparmor_parser`, que exige root. O motor recusa um perfil não carregado — comportamento **fail-closed correcto**. Revalidar rootful. |
| 7–9 | Mount Propagation rprivate/rshared/rslave | `failed to mount "mntSource": operation not permitted` | **HOST ENVIRONMENT** | É o próprio critest que falha a montar no host sem privilégio; o motor nunca é exercitado. |
| 10 | Mount Readonly non-recursive | `failed to mount tmpfs … operation not permitted` | **HOST ENVIRONMENT** | Idem. Independente disto, rro não é suportado (G-010). |
| 11 | Networking — port mapping só com porta do container | `dial tcp 10.200.0.2:80: i/o timeout` | **ROOTLESS LIMITATION** | O critest liga ao IP do pod a partir do host; em rootless esse IP vive no netns do holder. |
| 12 | Multiple Containers — should support network | timeout 77 s | **ROOTLESS LIMITATION** (inferência) | Mesmo padrão de alcance a partir do host; não confirmado linha a linha. |
| 13 | Streaming — portforward | timeout 60 s | **UNKNOWN** | Log do servidor não regista nada (77 linhas no total — lacuna de observabilidade). |
| 14 | Streaming — portforward in host network | `failed to start port forward …` | **UNKNOWN** | Idem. |
| 15 | Security Context — PodPID | lista de processos não partilhada | **MISSING FEATURE** | `shareProcessNamespace` é a «Fase 3» documentada. |
| 16 | Security Context — RunAsUserName | `failed to start container` | **DELONIX BUG / MISSING FEATURE** | Arranque falha com utilizador por nome. |
| 17 | SeccompProfilePath nil ⇒ unconfined | `Seccomp: 2` (esperado 0) | **DELIBERATE SECURITY DIVERGENCE** (a confirmar por ADR) | O motor aplica o perfil por omissão onde a spec pede unconfined. |
| 18 | NoNewPrivs=false permite escalar | `Effective uid: 1000` (esperado 0) | **ROOTLESS LIMITATION** (inferência) | setuid não eleva no userns de mapeamento único. |
| 19 | Container OOM → 137/OOMKilled | timeout 60 s | **MISSING FEATURE** | Documentado: `oom_kill` não é capturado ao vivo. |
| 20 | Image — pull por digest | lista de imagens não corresponde | **DELONIX BUG** | |
| 21 | Image status Uid/Username | `Image Username should be www-data` | **DELONIX BUG** | |

## Resumo por causa

| Causa | N |
|---|---|
| HOST ENVIRONMENT | 10 |
| ROOTLESS LIMITATION | 3 |
| MISSING FEATURE | 2 |
| DELONIX BUG | 3 (inclui RunAsUserName) |
| DELIBERATE SECURITY DIVERGENCE | 1 |
| UNKNOWN | 2 |

## Problemas do harness (não do produto)

1. `scripts/critest.sh` isola `DELONIX_ROOT` mas **não** `DELONIX_NET_RUNTIME_DIR` — a
   meia-isolação que este repo já documentou como perigosa. Contornado de fora.
2. O `cleanup` chama `delonix netns down`, comando que já não existe (é `net netns down`);
   o erro é engolido por `|| true`. Verificado depois: nenhum processo ficou a apontar para o
   runtime dir isolado.
3. O servidor não regista as falhas de streaming/port-forward — o diagnóstico de 2 das 21
   falhas é impossível só com os logs.
