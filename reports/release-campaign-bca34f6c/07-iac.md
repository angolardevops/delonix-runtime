# 07 — IaC / manifests (compose)

SUT: `delonix 5.1.1` @ `bca34f6c3`, rootless. Projecto: `relc-stack` (web=nginx-unprivileged, cache=redis-alpine).

| # | Caso (§9) | Comando | Observado | Estado |
|---|---|---|---|---|
| 1 | validar manifesto correcto | `compose config` | resolve projecto, normaliza serviços, rede `relc-stack_default`, exit 0 | **PASSOU** |
| 2 | rejeitar campo inválido | `compose config` com `porz:` | `error[DX-1000] 'porz:' is not understood and was refused rather than ignored` + lista de chaves suportadas, exit 1 | **PASSOU** (fail-closed) |
| 4 | aplicar | `compose up -d` | rede criada, 2 serviços `Running`; `web` responde `HTTP 200` em 18090 | **PASSOU** |
| 5 | confirmar estado real | `compose ps` + curl | ambos Running; HTTP 200 | **PASSOU** |
| 6/7 | reaplicar sem alterações / sem recriação | `compose up -d` 2ª vez | "already exists, nothing to do"; **id do web inalterado** (`40f5505569ec0bb0`) | **PASSOU** |
| 10/11 | introduzir drift + reconciliar | stop manual de `cache` → `compose up -d` | **`cache` fica `Stopped`** apesar do `up` dizer "Creates/starts every service" | **FALHOU** → DEF-003 |
| 12 | remover com ownership | `compose down` | remove exactamente os 2 containers do projecto + a rede (por labels); porta 18090 libertada | **PASSOU** |

## Resultado principal

- **Validação fail-closed exemplar:** campos desconhecidos são **recusados** (não ignorados), com a lista completa de chaves aceites — exactamente o comportamento que §9 exige.
- **Idempotência real:** reaplicar não recria (mesmo id), não só "não falha".
- **Ownership na remoção:** `down` remove só o que o `up` criou (derivado de labels); `kaeso-*` e redes pré-existentes intactos.
- **DEFEITO DE RECONCILIAÇÃO (DEF-003, P2):** `compose up` reconcilia **existência** mas **não o estado de execução** — um serviço parado/caído (drift) não volta a `Running`, contradizendo o próprio help ("Creates/starts every service") e o docker-compose (alvo de compatibilidade declarado). Workaround: `delonix start <svc>`.

## Cobertura e limites

- Testado o **percurso compose** (a superfície IaC primária de single-node). O
  `stack apply/plan` com documentos `kind:` e `delonix-model` (drift com campos mutáveis
  vs substituição, adopção de recurso externo, plano desactualizado, interrupção durante
  aplicação — §9 passos 3,8,9,13-17) **não foi exercitado ao vivo** nesta ronda →
  **NÃO EXECUTADO** (risco médio para release: a reconciliação declarativa completa fica por provar; ver DEF-003 que já indicia reconciliação parcial).

## Limpeza

Projecto `relc-stack` removido na totalidade; porta libertada; directório temporário apagado; `kaeso-*` preservados.
