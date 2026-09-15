# Compatibilidade Compose — `delonix compose` (nativo) sobre `fa8c6297`

Medição **estática** (`delonix compose config`, só parse e tradução, nada criado).
A execução `compose up` ao vivo de aplicações reais: **NOT TESTED** nesta fase — a
bateria E2E cobre `compose up/down/ps/logs` e passou (ver `raw/e2e-results.jsonl`).

## Allowlist (lida em `cmd/compose.rs`)

- Topo (7): `version name services networks volumes secrets configs`.
- Serviço (28): `image build extends environment env_file ports volumes command entrypoint depends_on
  healthcheck restart networks labels working_dir user cap_add cap_drop privileged tmpfs extra_hosts
  deploy container_name hostname read_only profiles secrets configs`.
- Recusados com razão: `include`, `extends.file`, `devices dns security_opt group_add logging network_mode pid ipc`,
  secret/config `external: true`, `target ≠ source`.

## Teste de chaves desconhecidas

| Caso | rc | Resultado |
|---|---|---|
| Controlo: `services.web.bogus_key` | 1 | **REJECTED** com mensagem clara |
| `deploy.placement.constraints` | 0 | **IGNORED — BUG** (desaparece do render) |
| `healthcheck.disable: true` | 0 | **IGNORED — BUG** (o bloco healthcheck desaparece do render) |
| `volumes[].bind.propagation: rshared` | 0 | **IGNORED — BUG** (render: `/tmp:/data`, propagação perdida) |
| `depends_on.<svc>.required: false` | 0 | **IGNORED — BUG** |
| `deploy.resources.reservations` | 0 | **IGNORED — BUG** |

Render de prova: `raw/compose-nested-config.txt`.

Causa-raiz (G-011): `check_unsupported_fields` verifica só o primeiro nível; os structs aninhados
são desserializados por serde sem `deny_unknown_fields`, logo sub-chaves desconhecidas são
descartadas em silêncio — a mesma classe de defeito que a allowlist do topo foi escrita para fechar.

## Não validado

Comparação lado a lado com `docker compose`/`podman-compose`; aplicações NGINX, PostgreSQL,
Redis, Odoo+PostgreSQL, multi-serviço com healthchecks, volumes persistentes e secrets.
