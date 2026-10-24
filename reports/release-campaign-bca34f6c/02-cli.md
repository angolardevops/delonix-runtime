# 02 — CLI, descoberta e tratamento de erros

SUT: `delonix 5.1.1` @ `bca34f6c3` (digest `725112b6…`), `target/release/delonix`.

## Resultados

| Caso | Comando | Esperado | Observado | Estado |
|---|---|---|---|---|
| CLI-1 | `--version` | exit 0, commit=HEAD | exit 0; `commit: bca34f6c3` | **PASSOU** |
| CLI-1 | `--help`, `<grp> --help` | help coerente | help completo, verbos directos+agrupados listados | **PASSOU** |
| CLI-2 | subcomando inválido | exit≠0, sugestão | `exit=2`, "tip: a similar subcommand exists: 'init'" | **PASSOU** |
| CLI-2 | flag inválida | exit≠0 | `exit=2`, "unexpected argument" | **PASSOU** |
| CLI-2 | arg obrigatório em falta (`run`) | exit≠0 | `exit=2`, "required arguments were not provided: <IMAGE>" | **PASSOU** |
| CLI-2 | porta inválida `99999:80` | `DX-` + exit, sem efeito | `error[DX-1307] invalid port`, exit=1, **0 containers criados** | **PASSOU** |
| CLI-2 | nomes inválidos `../evil`,`/abs`,`"a b"`,`""` | `DX-` + sem efeito | todos `error[DX-1000]`, exit=1, **0 leaks** | **PASSOU** |
| CLI-3 | `ps -a -o json` | JSON com schema estável | lista JSON válida; campos `id,image,command,created_unix,status,ports,name,restarts` | **PASSOU** |
| CLI-3 | `-o` valor inválido | recusa, lista valores | exit=2, "possible values: table, json" | **PASSOU** |
| CLI-4 | `ps -a` vs `container ls -a` | equivalentes | **output byte-idêntico** (`diff` vazio) | **PASSOU** |
| CLI-4 | `images` vs `image ls` | equivalentes | **output byte-idêntico** | **PASSOU** |

## Observações / discrepâncias

- **Convenção de exit codes consistente e correcta:** erros de uso (clap) → **exit 2**;
  erros de domínio → **exit 1** com código `DX-NNNN` estável (`DX-1000` nome inválido,
  `DX-1307` porta inválida). Entradas inválidas **não deixam efeito** (verificado com `ps -a`).
- **Rationale de segurança embebido:** nomes com `.` são recusados para impedir
  hijack de DNS interno — a mensagem explica o porquê. Boa prática.
- **DISCREPÂNCIA (baixa, DOC/CONTRATO):** `docs/testing/plano-folhas.md §8` (caso 8,
  "Saída") lista `-o json`/`yaml`/tabela como contrato, mas `ps -o` só aceita
  `table|json` — `yaml` é recusado. A recusa em si é correcta (exit 2, lista os valores),
  mas o contrato escrito promete `yaml`. A classificar: ou o plano sobre-promete, ou
  falta `yaml` na superfície de saída. **Registar para verificação** (não bloqueia).

## Requisito CLI-5 (Docker/Podman side-by-side) — **BLOQUEADO**

`docker` e `podman` não estão instalados (delonix é o runtime deste host). Comparação
ao vivo não executável. O repo tem comparação medida anterior em
`docs/comparacao-medida.md` / `docs/paridade-docker-podman.md` (não re-medida aqui).
