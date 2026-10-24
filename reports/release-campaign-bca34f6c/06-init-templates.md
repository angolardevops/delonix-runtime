# 06 — init / templates (geração)

SUT: `delonix 5.1.1` @ `bca34f6c3`. Templates disponíveis (13): django, fastapi, go, haproxy, httpd, laravel, nestjs, nextjs, nginx, node, odoo, rust, wordpress.

| # | Caso (§15) | Observado | Estado |
|---|---|---|---|
| INIT-1 | gerar não-interactivo, nome, template (`fastapi`, `nginx`) | ambos gerados, exit 0; estrutura completa (compose/Dockerfile/Delonixfile/manifest/Makefile/tests/CI/tunnel/TLS) | **PASSOU** |
| INIT-2 | sem placeholders por resolver | `grep` de `{{`/`__X__`/`CHANGEME`/`<REPLACE` em py/yaml/Dockerfile/Delonixfile → **0 ocorrências** | **PASSOU** |
| INIT-3 | sem segredos fixos | `POSTGRES_PASSWORD`/`RABBITMQ_DEFAULT_PASS` (32 chars) **diferem entre duas gerações** (hash distinto) → gerados por projecto | **PASSOU** |
| INIT-4 | `.env` protegido | `.env` em `.gitignore` (linha 12); `.env` (1396 B) distinto de `.env.example` (2763 B) | **PASSOU** |
| INIT-5 | manifesto gerado válido | `compose config` no projecto gerado → exit 0, 3 serviços resolvidos (offline) | **PASSOU** |
| INIT-6 | TLS mkcert no scaffold | `tls/tls.crt|key|pem` emitidos pela CA mkcert local p/ localhost/127.0.0.1/::1 | **PASSOU** (geração; confiança do cliente não testada) |

## Observações

- O `init` detecta/despacha para `stack init`/`vm init` e **adopta** projectos existentes
  (só escreve o glue Delonix/CI, não toca no código) — comportamento documentado e coerente.
- O projecto `nginx` fez `git init` (tem `.git/`); o `fastapi` não — diferença entre templates,
  não defeito.
- `WEBHOOK_INBOUND_SECRET` fica **vazio** no `.env` (a preencher pelo utilizador) — é um campo em
  branco, não um segredo hardcoded; aceitável.

## Cobertura e limites (honestidade)

- Testada a **geração** e a **validade estática** de 2 dos 13 templates (um monólito web+DB+fila,
  um estático). As combinações declaradas (stack×versão, monólito×microserviços, destino existente,
  nome com espaços, cancelamento, falha a meio) **não foram todas exercitadas** — estratégia
  baseada em risco, como o próprio âmbito admite. → cobertura parcial, não "catálogo validado".
- **TEMPLATE-1 (correr o projecto gerado: `make doctor`/`dev`/build/test/lint, pedido funcional,
  Redis, RabbitMQ)**: **NÃO EXECUTADO** nesta ronda (pesado: toolchains + containers de apoio;
  disco a 96%). Risco médio: a geração está provada; a execução ponta-a-ponta do stack gerado não.
