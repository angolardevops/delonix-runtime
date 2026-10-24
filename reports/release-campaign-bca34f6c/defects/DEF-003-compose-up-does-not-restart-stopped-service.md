# DEF-003 — `compose up` não reconcilia um serviço parado de volta a Running

- **Requisito / teste:** IAC-1 / §9 passos 10-11 ("introduzir drift; detectar e reconciliar"). Também §6 paridade docker-compose.
- **Severidade:** **P2 (média)** — divergência silenciosa do comportamento documentado e do docker-compose; impede recuperação por reaplicação. Sem perda de dados; há workaround.
- **Tipo:** defeito do produto (reconciliação incompleta).
- **Ambiente:** `delonix 5.1.1` @ `bca34f6c3`, rootless.

## Passos mínimos

```
# compose.yaml com services web (nginx) e cache (redis)
delonix compose up -d                 # ambos Running
delonix stop relc-stack-cache         # drift: cache -> Exited (0)
delonix compose up -d                 # "cache: already exists, nothing to do"
delonix compose ps                    # cache AINDA Stopped
delonix start relc-stack-cache        # workaround -> cache Running
```

## Esperado vs observado

- **Esperado:** `compose up` — cujo próprio `--help` diz *"Creates/**starts** every service"* e que
  visa compatibilidade com `docker compose up` (que reinicia containers parados do projecto) —
  devia trazer o serviço parado de volta a `Running`.
- **Observado:** a verificação de existência curto-circuita o arranque: "already exists, nothing
  to do". O serviço parado permanece `Stopped` em reaplicações sucessivas. Confirmado em 2
  invocações seguidas de `up -d`.

## Impacto

Num modelo daemonless sem supervisor por omissão, se um serviço cai/para, **reaplicar o
manifesto não o recupera** — o operador tem de descobrir e arrancar manualmente cada serviço
parado. Além disso quebra a expectativa de quem traz scripts de `docker compose`.

## Hipótese de causa

Na lógica de `compose up`, o ramo "container já existe" devolve cedo sem comparar/forçar o
estado `running` desejado. A reconciliação cobre criação, não execução.

## Critério de encerramento

`compose up` passa a arrancar serviços existentes mas parados (equiparando a `docker compose up`).
Reexecutar os passos mínimos: após o 2.º `up -d`, `cache` deve estar `Running` sem `start` manual.

## Estado

ABERTO. Reprodução preservada. Não corrigido nesta ronda (defeito de comportamento, candidato a
decisão: pode ser intencional — nesse caso o `--help` e a promessa de paridade têm de mudar).

## Correcção

- `bins/delonix-runtime-bin/src/cmd/compose.rs`: no `up`, um container existente mas NÃO a correr é
  reconciliado — `reconcile_status` + novo helper puro `compose_up_should_start(status)` (arranca
  `Stopped`/`Failed`/`Crashed`/`Created`; deixa `Running` e `Paused` em paz) → `cmd_start`. Nova
  mensagem `started`/`started, connected to …`.
- Teste: `tests::up_restarts_down_services_but_not_running_or_paused`. **PASSA.**
- Live: `up` → stop `cache` (Exited) → `up` de novo imprime `cache: started` e `web: already exists,
  nothing to do`; `compose ps` mostra `cache` **Running**.

## Nota de integração (bloqueio para commit)

Os três fixes foram construídos e verificados ao vivo a partir de `bca34f6c` + patch (binário digest
`a45e71a5…`). O `develop` actual (`69c349c3`, merge do #940 «develop-GitFlow») **não compila** por uma
razão ALHEIA a estes fixes: `bins/delonix-runtime-bin/src/cmd/prune.rs:370` chama
`delonix_linux::user_service_base` numa visibilidade que o merge deixou inconsistente (E0603) — é a
integração parcial das branches de prune que a memória do projecto avisou ter de ir «as duas juntas».
Logo as alterações ficam no working tree (fix local pedido), **não commitadas**: committar exige o
`develop` a compilar (correcção do dono ao #940) e consentimento para a mensagem de commit.

**Commit:** `ee615f3b` (branch `fix/campaign-defects-doctor-rm-compose`, não empurrado).
