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
| supported | 103 | 112 | **113** |
| partial | 93 | 86 | 87 |
| not-implemented | 74 | 72 | 71 |
| unsupported-by-provider | 230 | 230 | 229 |
| requires-external-component | 12 | 12 | 12 |
| unavailable-on-host | 3 | 3 | 3 |
| proved/applicable (N2) | — | 112/273 | **113/274 (41.2%)** |

Separação do progresso (§17): 1 promoção real a supported (OOM, com teste live + evidência), 3 reclassificações de causa corrigidas (A1–A3), 2 lacunas completadas em classificação (lvm/zfs-btrfs: backend já existia, agora partial honesto). Sem promoção falsa — cada +1 em supported tem evidência verificada.

## Histórico de commits da campanha

- `0894caf9` docs — Sprint 0 reconciliação (baseline, matriz 515, diff explicado, plano).
- `e082a275` docs — Sprint 1 proposta de reclassificação verificada (auditoria §4, 2 especialistas).
- `8cd5c63b` fix — aplica A1–A3: libvirt `vm.migration.live` unsupported→not-implemented; linux `storage.lvm-thin`/`storage.zfs-btrfs` not-implemented→partial.
- `34f7afd5` docs — marca A1–A3 aplicadas no checkpoint.
- `b6aa53ff` feat — **`container.oom-detection` partial→supported (§9)**: `scen_oom` reforçado para asserir `crash_reason=oom_killed` (distingue OOM de término não-OOM); provado ao vivo. supported 112→113.

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
