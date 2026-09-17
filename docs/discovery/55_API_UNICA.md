# 55 — Uma só API do motor: o mapa medido (ADR-0042)

Medido em `origin/main` a 2026-09-17. É a base de decisão do
[ADR-0042](../adr/0042-one-engine-api-maturity-and-docs.md) e a lista de trabalho das ondas
de cobertura (passo E). Cada operação da `delonix-mgmt` tem de acabar numa de três
colunas — servida pelo contrato, lacuna que o contrato fecha, ou recusada com razão —, e
nenhuma sai em silêncio.

## O que existe hoje

| Superfície | Versão | Forma | Estado |
|---|---|---|---|
| `delonix-mgmt` (`delonix serve api` → `delonix-mgmt`) | `/v1` | 32 caminhos, 40 operações escritas à mão, em forma de RPC | servida; sem OpenAPI, paginação, idempotência nem ETag; as mutações devolvem o stdout da CLI |
| Contrato `delonix.node.v1` (`proto/`) | `/v1` | ~45 operações orientadas a recursos (AIP), `Operation` assíncrona | **não servida**; OpenAPI gerada e mantida por portão no CI (P1) |
| `http_post_json`/`http_get_auth`/`http_post_stream` (`delonix-image`) | `/v2/cli`, `/v2/studio/designs` | cliente HTTP para a API de uma plataforma | **zero chamadores neste repositório**; é API de um consumidor dentro do motor → sai (passo B) |
| Registo OCI (`/v2/<repo>/…`), TrueNAS (`/api/v2.0`) | de terceiros | protocolos alheios | ficam — não são versões nossas |

## Nível de maturidade (Richardson) de cada superfície, hoje

| Nível | `delonix-mgmt` | contrato `node.v1` |
|---|---|---|
| 0 — HTTP como transporte | sim | sim |
| 1 — recursos | parcial: `/v1/containers/:id`, mas `/v1/net/publish`, `/v1/net/dhcp/:net/:mac` são procedimentos | sim, `/v1/namespaces/{ns}/{kind}/{name}` |
| 2 — verbos e códigos | parcial: `POST …/action` com o verbo no corpo; erros sem forma única | sim no desenho (`201`/`202`/`204`, `PATCH`+`update_mask`, `DX_*`); por provar num servidor |
| 3 — hipermedia | não | não — o ADR-0042 acrescenta `links` ao contrato |

## A `delonix-mgmt`, operação a operação

| Operação | Destino no contrato | Estado |
|---|---|---|
| `GET /_ping` | `GET /v1/node/health` | servida no contrato |
| `GET /metrics` | `/metrics` (sem versão, convenção Prometheus) | mantém-se no servidor da API |
| `GET /v1/dash` | `GET /v1/node/capacity` + contagens por `List` | **lacuna**: o resumo de contagens não tem RPC; decidir se é `GetNodeInfo` ou um `Summary` |
| `GET/POST /v1/volumes`, `GET/DELETE /v1/volumes/:name` | `VolumeService` List/Create/Get/Delete | servida no contrato |
| `GET/POST /v1/containers`, `GET/DELETE /v1/containers/:id` | `ContainerService` List/Create/Get/Delete | servida no contrato |
| `POST /v1/containers/:id/action` (start/stop/kill/restart/pause/unpause) | `:start`, `:stop`, `:kill`, `:wait` | **lacuna**: `restart`, `pause`, `unpause` |
| `GET /v1/containers/:id/logs` | `ContainerService.Logs` | servida no contrato |
| `POST /v1/containers/:id/exec` | `Exec` (stream dos dois lados) | **lacuna no REST**: sem mapeamento HTTP; WebSocket (ADR-0040 D4) |
| `POST /v1/containers/:id/reconfig` | `PATCH …/containers/{name}` com `update_mask` | servida no contrato |
| `PUT /v1/containers/:id/rate` | `PATCH …/containers/{name}` (`update_mask: network.rate`) | **lacuna**: confirmar o campo no `Container` |
| `GET/DELETE /v1/images`, `POST /v1/images/pull`, `POST /v1/images/build` | `ImageService` List/Delete/Pull/Build | servida no contrato |
| `GET /v1/images/scan` | `POST /v1/images:scan` | servida no contrato (muda de `GET` para `POST`: um scan é trabalho, devolve `Operation`) |
| `GET /v1/images/sbom` | — | **lacuna**: SBOM como recurso do scan ou RPC próprio |
| `GET/POST /v1/networks`, `GET/DELETE /v1/networks/:name` | `NetworkService` List/Create/Get/Delete | servida no contrato |
| `POST /v1/net/attach-extra`, `DELETE /v1/net/attach-extra/:id/:idx/:ip` | `…/networks/{network}:connect` / `:disconnect` | servida no contrato |
| `DELETE /v1/net/attach/:id/:ip` | `:disconnect` | servida no contrato |
| `GET /v1/net/status` | condição `network` de `GET /v1/node/health` | servida no contrato |
| `POST /v1/net/publish`, `DELETE /v1/net/publish/:host_port` | portas do `Container` por `PATCH` (`update_mask: ports`) | servida no contrato; a publicação por IP cru é **recusada** — é canalização, não recurso |
| `PUT/DELETE /v1/net/firewall/:ip` | Kinds `NetworkPolicy`/`NetworkAccessRule` | **lacuna**: o contrato não tem serviço de política; a regra por IP cru é **recusada** |
| `PUT /v1/net/egress`, `PUT /v1/net/egress/:bridge` | `NetworkPolicy` de egress | **lacuna** (mesma do firewall) |
| `GET /v1/net/dhcp/:net/:mac`, `GET /v1/net/dhcp6/:net/:mac`, `GET /v1/net/container-ip/:id` | endereços no estado do `Container`/`VirtualMachine` | **recusadas**: canalização interna; o endereço é campo do recurso |
| `POST /v1/vms/:name/action` | `VirtualMachineService` `:start`/`:stop`/`:pause`/`:resume` | servida no contrato (e o contrato acrescenta List/Get/Create/Delete e snapshots, que a `mgmt` não tinha) |

**Resumo**: das 40 operações, 28 têm destino servido no contrato, 7 são lacunas que o
contrato fecha (resumo de contagens, restart/pause/unpause, exec em REST, SBOM, limite de
banda, política de firewall e egress) e 5 são recusadas com razão (endereços DHCP e IP de
contentor, regras e publicações por IP cru).

## O que isto não mediu

- Se o `prost`/`tonic` compila o contrato tal como está (risco registado no ADR-0040, fechado
  no passo C).
- A completude da OpenAPI gerada (descrições, exemplos, erros por operação): o portão de
  completude (passo G) mede-a antes de entrar, para começar numa linha de base verdadeira.
