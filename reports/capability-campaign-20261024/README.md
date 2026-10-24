# Campanha de conformidade de capacidades — baseline v5.1.1

Objectivo: tratar individualmente todas as entradas parciais, não implementadas, não suportadas, dependentes de componentes externos ou indisponíveis no host da matriz de capacidades do delonix-runtime, implementando suporte real (backend + contratos públicos + testes) onde tecnicamente aplicável — sem alterar apenas etiquetas e sem mexer contadores artificialmente.

## Checkpoint retomável

| campo | valor |
|---|---|
| Baseline | `v5.1.1` = `a0da90f50abccb8d80a14136263d549dce257837` |
| Branch de trabalho | `develop` (descendente linear da baseline, +258/−0) |
| Catálogo | 1.4.0 · Engine 5.1.1 |
| Fonte-de-verdade | `matrix-structured.json` (515 entradas, lossless) |
| Sprint actual | **Sprint 1 em curso** — auditoria §4 (`e082a275`) + reclassificação A1–A3 aplicada (`8cd5c63b`, gates verdes) |
| Próximo | Sprint 1 — riscos de isolamento/dados (OOM distinto, namespaces partilhados por identidade de processo, restauro de snapshot) |

## Histórico de commits da campanha

- `0894caf9` docs — Sprint 0 reconciliação (baseline, matriz 515, diff explicado, plano).
- `e082a275` docs — Sprint 1 proposta de reclassificação verificada (auditoria §4, 2 especialistas).
- `8cd5c63b` fix — aplica A1–A3: libvirt `vm.migration.live` unsupported→not-implemented; linux `storage.lvm-thin`/`storage.zfs-btrfs` not-implemented→partial. Matriz regenerada, ratchet applicable 273→274, supported inalterado (sem promoção falsa). Contagens: not-impl 72→71, partial 86→88, unsupported 230→229.

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
