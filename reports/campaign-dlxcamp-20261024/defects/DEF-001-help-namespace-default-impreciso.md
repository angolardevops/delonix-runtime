# DEF-001 — Texto do `--help` da flag `--namespace` não menciona a excepção do `default`

- **Requisito/teste:** SCOPE-01 (§8 isolamento entre âmbitos)
- **Severidade:** P3 (documentação / texto ao utilizador) — NÃO é defeito de comportamento
- **Ambiente:** delonix v5.1.1 binário `55662f89…` (runtime de develop `558e5f20`), host rootless uid 1000

## Esperado vs observado
O `delonix run --help` descreve `--namespace`:
> "Logical ISOLATION namespace (default `default`). Containers in different namespaces cannot reach each other (even on the same network); only a `kind: Dependency` crosses the boundary"

Texto **categórico**. Mas o ADR-0011 (dns-namespace-scoping) define, por design:
- `| namespace default | yes | reachable from any namespace, by design |` (linha 50)
- "an open namespace is a shared space you opt into by not naming one — not a hole in isolation" (linha 122)
- código `crates/adapters/delonix-sdn/src/infra.rs:9262`: "The default (shared) namespace. Reachable from every namespace by design"

## Passos mínimos de reprodução
```
run -d --name srv --net N -v app:/www:ro busybox httpd -f -p 80 -h /www      # namespace default
run -d --name web --namespace beta --net N busybox sleep 300
exec beta/web wget -qO- http://srv/   # ALCANÇA (rc=0) — porque default é partilhado por design
run -d --name web --namespace alpha --net N busybox httpd ...
exec beta/web ping <alpha_ip>         # 100% packet loss — isolamento entre nomeados OK
```

## Impacto
Baixo. O comportamento está correcto e documentado no ADR-0011; é o texto do `--help` que ensina um modelo mental errado ("cannot reach each other" sem excepção), podendo levar a colocar workloads sensíveis em `default` julgando-os isolados.

## Critério de encerramento
O `--help` de `--namespace` passa a mencionar que o namespace `default` é um espaço partilhado alcançável de qualquer namespace (alinhado com ADR-0011).

## Hipótese de causa
Texto do campo clap escrito antes/independente do ADR-0011; drift documentação-vs-contrato.
