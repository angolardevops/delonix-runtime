# A fronteira PaaS ↔ Runtime nos contratos actuais (2026-10-08)

Levantamento read-only, medido contra `delonix-runtime` na tag `v5.0.0` + 32 commits
(`1cbe9639`, worktree `naas-kaas-caas`). Objectivo: confirmar se o contrato
`delonix.node.v1` (crate `delonix-node-api`, `proto/delonix/node/v1/*.proto`) já permite o
caminho `PaaS → contrato → adaptador → backend` sem SSH, sem o PaaS construir regras de
firewall à mão, sem tocar em ficheiros internos de um provider, e sem ter de corrigir
recursos a meio-criados. Cada afirmação abaixo foi confirmada no código actual, não só na
prosa do `AGENTS.md`.

## 1. Separação de camadas

`proto/delonix/node/v1/common.proto` declara a regra na cabeça do ficheiro (guardrail #2):

> «No notion of tenant, account, plan, quota or billing (…). `namespace` is the ENGINE's
> isolation namespace — a client may map its own grouping onto it, the engine never
> learns what that grouping is.»

Confirmado por três caminhos independentes, nenhum deles o `scripts/arch_fitness.py`:

1. **`scripts/arch_fitness.py` existe e corre** (`python3 scripts/arch_fitness.py`, ok em
   todos os checks). A sua função `consumer_mentions()` (linhas 302-329) só recusa o
   **nome de um consumidor** (`delonix-paas`, `delonix-api`, `ngolacloud`, `paas`,
   `delonixctl`, …) em `crates/`, `bins/`, `proto/` — não varre por `tenant`/`billing`/
   `quota`/`customer`/`plan`/`account` como conceitos de domínio. Dito de outro modo: o
   gate garante «o motor não conhece o PaaS pelo nome», não «o motor não tem noção de
   inquilino».
2. **Procura manual independente** (`grep -rniE "tenant|billing|customer|\bplan\b|quota|
   account" crates/ bins/ proto/`) devolveu ~90 ocorrências, **todas** inofensivas depois
   de lidas uma a uma:
   - `plan`/`Plan` é sempre o verbo do reconciliador (`stack plan`, `fn plan(...)`,
     `PlanStack`), nunca um plano de facturação.
   - `quota` é sempre uma quota TÉCNICA de recurso: cgroup `cpu.max`/`quotactl` (kernel),
     quota de volume (`VolumeStore::set_quota`, bytes), quota de um dataset remoto
     (`delonix-truenas`). Nenhuma quota de inquilino/plano.
   - `account` é sempre a conta do SISTEMA OPERATIVO (a conta que recebe uma chave SSH
     num convidado, `/etc/subuid`), nunca uma conta de cliente.
   - `tenant`/`customer` aparecem só em **comentários de doutrina** e em **nomes de
     variável de teste** (`crates/contexts/delonix-compute/src/record.rs:290-319`, a
     testar que `safe_cgroup_segment` aceita `"tenant-acme"` como uma string qualquer e
     recusa `"Tenant"`/espaços — um teste sobre sanitização de string, não sobre
     inquilinos) — e no próprio doc-comment que explica a fronteira
     (`record.rs:431-439`: «the engine stays independent of tenants, plans and
     billing…»).
   - `crates/contexts/delonix-security-runtime` tem um TESTE PRÓPRIO
     (`no_file_in_this_crate_speaks_of_a_tenant`, `lib.rs:71-98`) que falha se um campo
     `tenant`/`project`/`environment` entrar nesse crate especificamente — mais estrito
     que o `arch_fitness.py`, mas âmbito a um único crate (o de eventos de segurança).
3. **`python3 scripts/contract_gate.py`** corre limpo (`buf format`/`buf lint`/`buf
   breaking against v5.0.0`/mapeamento HTTP das 57 rotas/OpenAPI gerado/45 caminhos REST
   distintos) — o contrato publicado está formalmente estável e sem reutilização de
   caminho.

**Veredicto**: a separação está genuinamente limpa HOJE — não há nenhum campo, struct ou
rota que carregue tenant/plano/quota-de-negócio/facturação. Mas a prova disso é feita por
**leitura humana após um `grep` amplo**, não por um gate automático que procure os termos
de domínio em si. O `arch_fitness.py` protege contra «o motor aprender o NOME do
consumidor»; não existe um gate equivalente que falhe se alguém introduzir um campo
`tenant_id`/`billing_plan` com um nome genérico (`owner`, `group`, `scope`) que não bata
com `CONSUMER_NAMES`. É uma lacuna de **gate**, não de **estado actual do código**.

## 2. O que o Runtime ainda confia no chamador

A pergunta do enunciado («nome de recurso que o chamador escolhe livremente sem
verificação de posse») foi verificada nos dois sentidos: traversal/injecção técnica, e
posse/âmbito lógico.

### 2a. Traversal e injecção técnica — fechado no que está servido

`CreateNetwork`/`DeleteNetwork` (`crates/interfaces/delonix-node-api/src/
network_ops.rs:174,207`) chamam `delonix_sdn::NetworkStore::validate_name`/
`validate_subnet` ANTES de qualquer efeito, com teste a provar a recusa de
`"../evil"`/`"a/b"`/`"bridge"` (`network_ops.rs:445-448`). As leituras (`GetVolume`/
`ListVolumes`, `volumes.rs:218-284`) nunca constroem um caminho a partir do `req.name` —
enumeram o store inteiro (`entries(root)` → `VolumeStore::list_all()`, que lê nomes já
sanitizados em disco) e filtram por igualdade de string. Não há path-traversal alcançável
pelo node-api nos quatro verbos hoje servidos (`Create/DeleteNetwork`, `Get/
ListVolumes`).

### 2b. Posse e âmbito lógico — NÃO verificado, por desenho, e isso é uma fronteira real

Nenhuma mensagem do contrato carrega um campo de identidade do chamador ou de posse do
recurso:

- `ResourceMeta` (`common.proto:39-50`) tem `name`/`namespace`/`uid`/`labels`/
  `annotations`/`etag` — nada que diga "quem pode tocar nisto".
- `DeleteNetworkRequest`/`DeleteVolumeRequest` (`infra.proto:202-211,302-308`) só têm
  `namespace`/`name`/`etag`/`force`/`request_id`. **Qualquer chamador que saiba (ou
  adivinhe) o nome apaga o recurso** — o único travão é o `etag` (se o chamador o passar)
  e a verificação de dependentes.
- O `delonix.io/stack`/`OwnerMark` que o AGENTS.md documenta (posse por etiqueta,
  `delonix-stack/src/reconcile.rs`; `OwnerMark` em `delonix-networking/src/
  ownership.rs`) é um mecanismo de **contabilidade de poda** (o que um `stack apply
  --prune`/`stack destroy` pode tocar) e de **GC em objectos remotos de provider**
  (Proxmox SDN vnet, OPNsense alias) — nunca é lido como controlo de acesso em nenhuma
  das RPCs do node-api. `CreateNetwork` aceita `labels`/`annotations` do PRÓPRIO
  chamador (`network_ops.rs:124-131`) e grava-os tal e qual; nada impede um segundo
  chamador de criar ou de apagar um recurso com a etiqueta de posse de outro.
- `ListOperationsRequest` (`operations.proto:82-86`) só tem `page`/`active_only` — **zero
  âmbito**. Qualquer chamador com acesso ao socket vê TODAS as operações do nó
  (`GetOperation`/`ListOperations`, `service.rs:268-294`), incluindo o `target`
  (`Network/lab`), o `request_id` e o erro de QUALQUER outro chamador. Não há
  `owner`/`caller_id` no `Record` persistido (`operations.rs:53-73`).
- `ListVolumesRequest.namespace` aceita o literal `"*"` (`volumes.rs:82-88`,
  `wanted()`), que devolve TODAS as namespaces de uma vez, sem gate nenhum — é uma
  escotilha deliberada para um operador humano (`delonix volume ls -A`), mas
  reexposta tal e qual no contrato remoto: um chamador que só devia ver a sua própria
  namespace e passe (ou receba de um PaaS com bug) `namespace: "*"` lê tudo.
- Em contraste, `NetworkService` fixa a namespace de rede sempre a `default`
  (`check_namespace`, `network_ops.rs:76-83`) — não há isolamento de rede por namespace
  no node-api hoje, mas também não há essa fuga específica, porque não há mais do que
  uma namespace para ler.
- À parte do node-api: `docs/adr/0069-kind-catalog-reassessment-and-strict-manifests.md`
  (linhas 147-150) já regista, para a camada `stack`/CLI, que a identidade de um recurso
  com âmbito (`namespace/nome`) é hoje **convenção de string** (`destroy_one` faz
  `find` sobre o nome do plano) e que um `ResourceKey` estrutural «tornaria isso
  estrutural em vez de convencional» — ninguém mediu precisar dele ainda, mas é a MESMA
  classe de fragilidade.

**Isolamento rede/socket**: o único controlo de identidade que existe é o do TRANSPORTE —
`SO_PEERCRED` contra o uid do próprio processo servidor, socket `0600`
(`crates/interfaces/delonix-node-api/src/lib.rs:46-47,117-134`). Ou seja: hoje o node-api
só é alcançável por um processo que corre com o MESMO uid do servidor no MESMO host — o
que implícitamente assume «um node-api por host, um chamador de confiança total (o
agente local do PaaS nesse host)». Não há noção de "chamador A" vs "chamador B" no
socket — o `SO_PEERCRED` garante só "é o dono do processo", não "é QUEM diz que é".

**Veredicto da pergunta 9**: as fronteiras TÉCNICAS de baixo nível (nomes de ficheiro,
subnets, ids de operação) já são validadas no servidor, independentemente do que o PaaS
tenha verificado a montante — isso está bem. Mas a fronteira LÓGICA («este chamador pode
tocar neste recurso, nesta namespace, ver esta operação») não existe no contrato: o
Runtime confia inteiramente que (a) só um agente de confiança fala com o socket, e (b)
esse agente só pede o que esse agente tem autoridade para pedir. É uma posição coerente
com a doutrina («o motor não conhece inquilinos»), mas significa que QUALQUER bug no lado
do PaaS que esqueça de filtrar `namespace`/`name`/`request_id` antes de os repassar dá
acesso cruzado sem que o Runtime o note ou recuse.

## 3. Matriz de maturidade dos contratos

| Área | Estado hoje (evidência) | Gap | Onde fechar |
|---|---|---|---|
| **Schemas/versionamento** | Pacote único `delonix.node.v1` (`common.proto` + `node/operations/infra/compute.proto`), `buf lint`/`buf breaking against v5.0.0`/mapeamento HTTP e OpenAPI gerados e verificados pelo `scripts/contract_gate.py` (corrido: `ok` nos 6 checks, 57 rotas mapeadas, 45 caminhos REST distintos). Sem versionamento por grupo de recurso (ao contrário dos manifestos da CLI, que usam `compute.delonix.io/v1alpha1` etc.) — é tudo `v1` plano. | Uma mudança incompatível em QUALQUER mensagem obriga a subir o pacote inteiro para `v2`, mesmo que só um recurso tenha mudado. Aceitável enquanto o contrato é jovem; cedo ou tarde fica caro. | Decidir, por ADR, se `delonix.node.v1` continua monolítico ou se cada `message`/serviço ganha o seu próprio ciclo de versão (seria mudança estrutural grande — não urgente hoje). |
| **Idempotência** | `request_id` em TODAS as mensagens de criação/eliminação do proto (`CreateNetworkRequest.request_id`, `DeleteVolumeRequest.request_id`, etc. — confirmado por grep em `infra.proto`/`compute.proto`). Implementado e testado para as 2 rotas servidas: `operations::replay`/`begin` (`network_ops.rs:212-224,253-291`; testes `the_same_create_sent_twice_does_the_work_once`, `the_same_delete_sent_twice_is_answered_once_not_with_not_found`). O REST lê `Idempotency-Key` e injecta-o em `request_id` (`service.rs:396-404`). | O CAMPO existe em todas as 22 mensagens que devolvem `Operation`, mas o MECANISMO (`operations::begin`/`replay`) só está ligado a 2 delas (`CreateNetwork`/`DeleteNetwork`). As outras 20 (Volume, Image, Stack, Container, Pod, VM, Snapshot) são `UNIMPLEMENTED` — o campo é aceite pelo parser e nunca usado. | `crates/interfaces/delonix-node-api/src/service.rs` — ligar cada verbo novo ao MESMO `operations::begin/finish/replay` já escrito, não reinventar. |
| **Operações assíncronas** | Ver secção 4 — `OperationService` (registo em disco, `Interrupted` por dono morto, retenção de 7 dias) está feito e testado para o que já existe. | Só 2 de 22 RPCs de mutação que devolvem `Operation` passam pelo mecanismo. `WatchOperation`/`CancelOperation` são `UNIMPLEMENTED` (`service.rs:295-316`) — um cliente só pode fazer *poll* por `GetOperation`, nunca subscrever nem cancelar. | `operations.rs` já tem o `Record`; falta o `stream` de `WatchOperation` (ver nota de design abaixo) e `CancelOperation` (hoje não há sequer um conceito de "pedir para parar" no meio de um `create_work`/`delete_work`, que são funções síncronas e não cooperativas). |
| **Erros estruturados** | RFC 9457 `application/problem+json` no REST (`service.rs:684-715`, `delonix_model::codes::problem`), `tonic::Status` com código de classe + metadata `dx-number` no gRPC (`network_ops.rs:42-73`), dicionário `DX-CDNN` central e estável (`delonix-model/src/codes.rs:1-10`: «a number never changes meaning and is never reused»). | O proto PROMETE (`common.proto:114-122`) um `ErrorDetail` (`reason`/`resource`/`metadata`/`retryable`/`retry_after`) «attached to every non-OK gRPC status as a binary detail» — mas isso só é construído dentro de `Operation.error` (`operations.rs:340-349`, confirmado por grep — é a ÚNICA construção de `ErrorDetail` em todo o crate). Um erro SÍNCRONO (`GetNetwork` not-found, `DeleteNetwork` em uso) nunca ganha o binary detail; carrega só a string `dx-number` em metadata ASCII. Um cliente gRPC genérico que espere `google.rpc.Status.details[]` (o padrão usado por `tonic_types`/outros serviços) não o encontra; tem de saber ler a metadata proprietária. | `network_ops.rs::status_of` / o equivalente para cada serviço novo — construir e anexar um `ErrorDetail` via `Status::with_details` (ou a convenção `grpc-status-details-bin`) também no caminho síncrono, não só dentro de `Operation.error`. |
| **Desired-vs-observed** | O reconciliador de 3 vias já existe e está maduro (`delonix-stack/src/reconcile.rs`, `kinds.rs` — doc-comment explica a tabela única `KindFacts` e os 3 bugs reais que a motivaram; `plan()` é puro, testado, compara manifesto/máquina/`delonix.io/last-applied`). `ResourceMeta.etag`/`Condition` no proto já modelam observado-vs-esperado ao nível do contrato (`common.proto:39-66`). `NetworkService.GetNetwork` já lê `Realized` do dataplane real (ADR-0042 slice E1). | O `plan`/`apply` DECLARATIVO (o que resolve "o que muda para bater com o manifesto") está TODO no `delonix-stack`/CLI local — `StackService.PlanStack`/`ApplyStack`/`DestroyStack` são as 3 RPCs do contrato que o expõem, e as 3 são `UNIMPLEMENTED` (nem registadas em `service.rs`). Um PaaS não pode pedir "convergir para este estado" pela rede; só pode pedir operações imperativas RPC-a-RPC (e mesmo essas, hoje, só Network create/delete). | `StackService` em `service.rs` + um novo `crates/interfaces/delonix-node-api/src/stack_ops.rs` que chame `delonix_stack::reconcile::plan`/o `apply` que o CLI já usa — ADR-0042 nomeia isto como dependente da P5 do ADR-0040 (ver AGENTS.md: «F6 do ADR-0059 ADIADA até à P5», mesma dependência). |
| **Criar/adoptar/actualizar/substituir/eliminar** | O proto já desenha bem o padrão para `Container` (`UpdateContainerRequest` com `FieldMask update_mask` + `etag`, `infra.proto`/`compute.proto:285-300`) e `etag`/`force` em todos os `Delete*Request`. | Por recurso: **Network** — Create/Delete servidos, **sem Update** no proto (não há `UpdateNetworkRequest` nenhum — labels só se definem na criação) e **sem adopção**: `CreateNetwork` sobre um nome já existente é sempre `AlreadyExists` (`network_ops.rs:215-220`), nunca a semântica de adopção que a CLI (`stack apply`) já tem via `delonix.io/last-applied`. **Volume** — só leitura; Create/Delete `UNIMPLEMENTED`. **Container/Pod/VM/Stack/Image** — nenhum verbo servido (nem Create nem Get). | Um ADR curto decide se `CreateNetwork` ganha um modo "adopt-if-exists-and-unowned" (replicando a regra do reconciliador) antes de o Container/VM wave chegar — senão cada wave reinventa a decisão. |
| **Eventos estáveis** | `WatchEvents` está NO contrato (`node.proto:38`) e tem handler registado — mas devolve sempre `Err(not_yet("WatchEvents"))` (`service.rs:40-46,147-153`). O crate `delonix-node::events` (mencionado na tabela de crates do AGENTS.md) existe como registo interno, mas não está ligado a este stream. | Zero eventos estáveis entregues pela rede hoje. Um PaaS não tem forma de saber "a VM X mudou de estado" sem *poll* contínuo de `Get*`. | Ligar `delonix_node::events` (o que já existe) ao `WatchEvents` — provavelmente a MESMA infra-estrutura de streaming que `WatchOperation` vai precisar; faz sentido desenhá-las juntas. |
| **Descoberta de capacidades por provider** | **Feita e servida** — `ListProviders` (`NodeService`, ADR-0050 D5) devolve `ProviderInfo{id, kind, available, capabilities[], health, catalog_version}` com `Capability{name, supported, detail, state}` de 6 estados (`supported`/`partial`/`unsupported-by-provider`/`requires-external-component`/`not-implemented`/`unavailable-on-host`) — confirmado em `service.rs:78-108,137-146` e testável hoje (gRPC + REST + `/docs`/`/redoc`). A regra «`supported` cita evidência que existe» é imposta por teste (`the_declared_providers_are_the_published_matrix`, catálogo documentado no AGENTS.md). | Nenhum. Esta é a área MAIS madura do contrato — o PaaS já pode perguntar "este nó aguenta snapshot com memória?" antes de tentar, em vez de descobrir por erro. | — |

## 4. Operações assíncronas

O `OperationService` (ADR-0042 D4/E2, `crates/interfaces/delonix-node-api/src/
operations.rs`) cobre bem, PARA O QUE ESTÁ LIGADO A ELE, as quatro perguntas do
enunciado:

1. **Criado com ID antes do trabalho.** `begin()` escreve o `Record` em
   `<root>/operations/<id>.json` com `state: Running` ANTES de o `work` closure correr
   (`network_ops.rs:221-229`); provado pelo teste
   `create_is_persisted_running_before_the_work_and_succeeds_with_the_target`, que
   espreita a lista DURANTE o `work` e confirma que já lá está.
2. **Consulta de progresso.** `GetOperation`/`ListOperations` servidos (gRPC + REST,
   `service.rs:268-294`), com `ETag`/paginação — um cliente que perdeu a resposta inicial
   volta e lê o estado pelo `id`.
3. **Resultado idempotente por `request_id`.** `operations::replay` (chamado ANTES de
   qualquer verificação que o próprio primeiro pedido tornou verdadeira — ex.: "já
   existe") devolve a MESMA `Operation` para o mesmo `(verb, target, request_id)`, sem
   repetir o efeito — testado (`the_same_create_sent_twice_does_the_work_once`: `runs`
   fica em 1).
4. **Cliente que perde a ligação e volta a perguntar depois, sem repetir o efeito.**
   Dois casos, os dois cobertos: (a) o cliente tem o `id` da `Operation` — faz
   `GetOperation`; se o processo que a fazia morreu a meio, `settled()`
   (`operations.rs:140-161`) marca-a `Failed/Interrupted` na PRÓXIMA leitura, nunca
   `Running` para sempre — e diz "leia o alvo para ver o que ficou, e reenvie o pedido".
   (b) o cliente só tem o `request_id` original — reenvia o MESMO pedido e
   `operations::replay` devolve a resposta já dada, sem voltar a executar `work`.

**O que falta, medido por RPC**: 24 RPCs do contrato devolvem `Operation` (grep em
`infra.proto`+`compute.proto`+`operations.proto`); dessas, `GetOperation` é a leitura que
já serve o mecanismo em si e `CancelOperation`/`WatchOperation` (esta devolve `stream
Operation`, por isso fica fora da contagem de 24, mas é a mesma família) são as outras 3
RPCs do `OperationService`, sempre `UNIMPLEMENTED`. Ficam **22 RPCs de mutação** (`Create`/
`Delete`/`Apply`/`Pull`/… que criam ou terminam um recurso) — e dessas só **2**
(`CreateNetwork`, `DeleteNetwork`) passam pelo `operations::begin`/`finish`/`replay`. As
outras 20 — `Create/DeleteVolume`, `Pull/Push/Build/Scan/DeleteImage`, `Apply/
DestroyStack`, `Create/DeleteContainer`, `Create/DeletePod`, `Create/Delete/Start/
StopVirtualMachine`, `Create/Restore/DeleteSnapshot` — respondem `UNIMPLEMENTED`
directamente no `service.rs` (nem chegam a criar um `Record`). Numericamente: **2 de 22
(≈9%)** das mutações assíncronas do contrato estão realmente ligadas ao mecanismo que as
torna seguras para um cliente com rede instável — o resto nem está acessível.

## 5. As 3 lacunas mais graves (para o PaaS poder delegar sem contornar o contrato)

1. **Container/Pod/VM/Stack/Image não têm NENHUM verbo servido no node-api — o PaaS não
   tem como criar nada "real" pela rede hoje.** `ContainerService` (12 RPCs),
   `PodService` (4), `VirtualMachineService` (13), `StackService` (3), `ImageService`
   (7) — 39 de 59 RPCs do contrato, 0 implementadas (`service.rs` não tem `impl
   ContainerService for NodeApi` nenhum; a REST cai sempre no `_ => Err(not_served)` da
   linha 414). O PaaS, hoje, só pode: criar/apagar uma rede, ler volumes, listar
   capacidades do nó. Para o resto (correr um container, criar uma VM, aplicar um
   stack) o caminho continua a ser o CLI local — exactamente o SSH/comando-à-mão que
   este desenho quer eliminar.
   - **Onde**: `crates/interfaces/delonix-node-api/src/service.rs` (registar os
     `impl *Service for NodeApi`) + um módulo `compute_ops.rs` que chame
     `delonix_compute::run`/o motor de containers já usado pela CLI (`cmd/
     container.rs`), tal como `network_ops.rs` chama `delonix_sdn::netops`.
   - **Teste que provaria**: um teste de integração gRPC real (como
     `crates/interfaces/delonix-cri/tests/grpc_status.rs` já faz para o CRI) que sobe o
     servidor num socket unix de teste, chama `CreateContainer` com um `request_id`,
     confirma `Operation.state == Succeeded` e que o container existe no `Store` real —
     e repete o MESMO `request_id` confirmando que não corre duas vezes.

2. **Nenhuma RPC síncrona ou assíncrona carrega uma noção de "quem pediu isto", e por
   isso `ListOperations`/`DeleteNetwork`/`ListVolumes(namespace="*")` são superfícies de
   acesso cruzado se o PaaS alguma vez repassar um `namespace`/`name` sem o filtrar
   primeiro.** Hoje isto é seguro PORQUE o socket só aceita o mesmo uid do servidor
   (`SO_PEERCRED`) — ou seja, assume-se 1 agente de confiança total por nó. No dia em
   que o desenho precisar de mais de um chamador por nó (vários processos do PaaS, um
   proxy multiplexado, um segundo agente), não há NADA no contrato que impeça um deles
   de ver/apagar o que outro criou.
   - **Onde**: `proto/delonix/node/v1/common.proto` — um campo opcional, por exemplo
     `string caller_scope` em `ResourceMeta`/`ListOperationsRequest`, verificado contra
     o `delonix.io/stack`/`OwnerMark` já existentes do lado do servidor (reaproveitar o
     mecanismo de posse que já existe para poda, em vez de inventar um segundo). Decisão
     de desenho: continuar "um agente de confiança total por nó" é aceitável e deve
     ficar ESCRITO como tal num ADR, para não ser descoberto tarde como suposição
     implícita.
   - **Teste que provaria**: dois `request_id`/chamadores distintos criam duas redes com
     etiquetas de posse diferentes; um `DeleteNetwork` do segundo chamador sobre o nome
     da PRIMEIRA falha com uma razão de posse — hoje esse teste não pode nem ser escrito,
     porque não há campo para o exprimir.

3. **O `ErrorDetail` estruturado que o contrato promete ("attached to every non-OK gRPC
   status") só existe dentro de `Operation.error` — o caminho de erro síncrono (a
   maioria dos erros reais: not-found, conflito, etag obsoleto) nunca o constrói, e por
   isso um cliente gRPC genérico (não escrito à mão contra esta API) não consegue
   classificar a falha sem saber ler a metadata ASCII proprietária `dx-number`.**
   - **Onde**: `network_ops.rs::status_of` (e o equivalente que qualquer serviço novo
     vai precisar) — construir um `ErrorDetail{reason, resource, metadata, retryable,
     retry_after}` e anexá-lo ao `tonic::Status` via `Status::with_details`/
     `grpc-status-details-bin`, ao lado (não em substituição) da metadata `dx-number`
     já usada pelo REST.
   - **Teste que provaria**: um teste que chama `GetNetwork` sobre um nome inexistente
     e decodifica o binary detail do `tonic::Status` (não a metadata ASCII) esperando um
     `ErrorDetail.reason == "DX_NOT_FOUND"` e `resource == "Network/<nome>"` — hoje esse
     teste falharia porque o detail nunca é anexado.
