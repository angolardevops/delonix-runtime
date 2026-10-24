# DEF-002 — `rm` não aceita `--namespace`: container fora de `default` só removível por ID

- **Requisito / teste:** SCOPE-2 / §8 (isolamento por namespace), §5 (limpeza).
- **Severidade:** **P3 (baixa)** — usabilidade/assimetria de superfície; não há perda de dados nem órfãos (a remoção por ID funciona e limpa tudo).
- **Tipo:** defeito do produto (lacuna de superfície CLI).
- **Ambiente:** `delonix 5.1.1` @ `bca34f6c3`, rootless.

## Passos mínimos

```
delonix run -d --name dup --namespace teamA --net n nginx…
delonix run -d --name dup --namespace teamB --net n nginx…
delonix rm -f dup --namespace teamA     # error: usage (flag não existe)
delonix rm -f dup -n teamA              # error: usage
```

## Esperado vs observado

- **Esperado:** simetria com `ls`/`vm ls`, que aceitam `--namespace` (`delonix vm ls --namespace teamA` é exemplo documentado em `manual_entries.rs:2891`). Poder endereçar `rm <name> --namespace <ns>`.
- **Observado:** `rm`/`container rm` **não têm** `--namespace`. Com o mesmo nome em dois namespaces, o nome é ambíguo e só o **ID global** remove o recurso (`rm -f <id>` funciona e limpa).

## Impacto

Operador que siga o modelo mental de namespaces (criar por `--namespace`, listar por `--namespace`) não consegue remover por nome+namespace; tem de descobrir o ID. Fricção, não falha de dados.

## Critério de encerramento

Adicionar `--namespace` a `container rm` (e espelhar em `stop`/`restart` se aplicável), ou documentar explicitamente que a remoção fora de `default` é por ID. Reexecutar os passos mínimos.

## Estado

CORRIGIDO (local, working tree, verificado ao vivo em 2026-10-23). Ver bloco «Correcção» abaixo.

## Correcção

- `bins/delonix-runtime-bin/src/cmd/container.rs`: `RmArgs` ganha `-n/--namespace` (simétrico a `ps`);
  novo helper puro `resolve_rm_query(store, token, ns)` qualifica um nome simples como `<ns>/<name>`
  (a forma que `find` já entende), com fallback para o token cru (ids/prefixos continuam a funcionar).
- Testes: `rm_namespace_tests::namespace_disambiguates_a_shared_name` e
  `namespace_leaves_ids_and_qualified_forms_alone`. **PASSAM.**
- Live: `rm --namespace teamA relcfix-dup` e `rm -n teamB relcfix-dup` removem por nome o container
  certo em cada namespace (2→1→0), sem precisar do ID.

**Commit:** `5ce63079` (branch `fix/campaign-defects-doctor-rm-compose`, não empurrado).
