# DEF-001 — `system doctor`/`system resources` anunciam a flag `--pids-limit`, que não existe

- **Requisito / teste:** CLI-1/§6 (help coerente), SCOPE-1/§8 (limites). CONTAINER-3.
- **Severidade:** **P3 (baixa)** — defeito de documentação/UX, não funcional. Não há perda de dados nem recusa indevida.
- **Tipo:** defeito do produto (texto ao utilizador incorrecto).
- **Ambiente:** `delonix 5.1.1` @ `bca34f6c3`, `target/release/delonix`, rootless.

## Passos mínimos

```
$ delonix system doctor
  ✓ ok       cgroup2 delegation (--memory/--cpus/--pids-limit)
```

E o doc-comment de `cmd_doctor` (`bins/delonix-runtime-bin/src/cmd/system.rs:2189`):
> "Without it `-m`/`--cpus`/`--pids-limit` are accepted and ignored"

## Esperado vs observado

- **Esperado:** o texto ao utilizador só nomeia flags que existem.
- **Observado:** `--pids-limit` é anunciada em `system.rs:2189` e `:2232`, mas `container run`/`update` **não têm** essa flag. É deliberadamente a grafia do Docker — `bins/delonix-runtime-bin/src/main.rs:1688-1697` tem um teste (`the_docker_spellings_are_not_ours`) que garante que `--pids-limit` **nunca** existe como flag.

## Hipótese de causa

O comentário em `main.rs:1640-1647` identifica a classe exacta: "they are string literals in another crate, and the compiler has no opinion about the inside of a string." O teste `named_flags_all_exist` cobre `flags_of_controller`, mas **não** o texto livre do `doctor`/`resources` em `system.rs`. Quando a flag foi removida do `run`, estes dois literais em `system.rs` ficaram para trás.

## Critério de encerramento

- Trocar `--pids-limit` por `--memory`/`--cpus` (as que existem) nos dois literais de `system.rs`, OU estender o gate para varrer também o texto do `system doctor`/`resources` contra o conjunto de flags reais.
- Reexecutar `delonix system doctor` e confirmar que não aparece `--pids-limit`.

## Estado

CORRIGIDO (local, working tree, verificado ao vivo em 2026-10-23). Ver bloco «Correcção» abaixo.

## Correcção

- `bins/delonix-runtime-bin/src/cmd/system.rs`: introduzidos 3 consts testáveis
  (`DELEGATION_CHECK_LABEL`, `DELEGATION_LIMITS_APPLY`, `DELEGATION_LIMITS_INERT`),
  todos com `--memory/--cpus` (sem `--pids-limit`), usados no `doctor` e no `resources`;
  corrigidos ainda o help do `Setup`, o doc-comment de `cmd_doctor` e o `DELEGATE_BODY`.
- `bins/delonix-runtime-bin/data/pt.po`: `--pids-limit` removido das traduções correspondentes.
- Teste: `main.rs::advisory_flag_tests::delegation_labels_name_only_real_flags` — assere que
  os 3 consts não contêm nenhuma das 4 grafias do Docker. **PASSA.**
- Live: `delonix system doctor` → `cgroup2 delegation (--memory/--cpus)`; `grep -c pids-limit`
  em `doctor`+`resources` = **0**.

**Commit:** `e489852b` (branch `fix/campaign-defects-doctor-rm-compose`, não empurrado).
