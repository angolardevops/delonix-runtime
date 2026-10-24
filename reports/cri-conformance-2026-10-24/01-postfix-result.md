# Resultado pós-correcção + verificação de regressão

## Suites

| Corrida | Binário (base) | Resultado |
|---|---|---|
| Baseline | `delonix-cri` campanha (base **develop** `ee615f3b`) | `84 Passed \| 20 Failed \| 18 Skipped` |
| Pós-fix (full) | fix (base **main** `ab4751ac` + fix) | `85 Passed \| 19 Failed \| 18 Skipped` |
| Focused `Image Manager` | fix | `10 Passed \| 0 Failed` |

Os dois specs corrigidos (**#18** digest, **#19** Uid/Username) saem da lista de
falhas em todas as corridas pós-fix. Confirmado por `grep` ao bloco
*Summarizing Failures* do `postfix.log`.

## A armadilha que o cross-base quase escondeu (e a sua resolução)

O `85/19` **não compara directamente** com o `84/20`: o baseline foi medido com
um binário de base **develop** e o fix está em cima de **main**. Entre as duas
bases há todo o delta main↔develop, não só o meu commit. O sintoma foi um spec
NOVO a falhar no pós-fix — **`Container OOM … exitCode 137 OOMKilled`** — que
passava no baseline.

O meu diff toca UM ficheiro (`lib.rs`, caminho de imagem + testes); a detecção de
OOM vive no `lifecycle.rs`, intocado. Teste decisivo (A/B na MESMA base `main`):

```
focused "Container OOM"  @ main ab4751ac  SEM fix  → FAIL
focused "Container OOM"  @ main ab4751ac  COM fix  → FAIL
focused "Container OOM"  @ develop (binário baseline) → PASS
```

→ O OOM falha na base **main** independentemente do meu commit: é uma lacuna de
`main` (um fix de OOM que existe em `develop` e ainda não propagou para `main`),
**não uma regressão desta alteração**. Na mesma base, o fix só melhora (image
#18/#19 fail→pass) e não regride nada.

## Conclusão de regressão

- **Sem regressão atribuível a este commit**: diff só de imagem + o único spec
  que "mudou" (OOM) falha igual na base sem o fix.
- **Nota de método para a próxima sessão**: medir baseline e pós-fix com binários
  da **mesma base**. O `84→85` fica como indicativo, não como medida directa; a
  medida válida é o A/B na base `main` acima e o focused `Image Manager` 10/0.
- **Follow-up destapado**: `Container OOM` falha em `main` e passa em `develop` —
  confirmar que o fix de OOM de `develop` chega a `main` (propagação GitFlow).
