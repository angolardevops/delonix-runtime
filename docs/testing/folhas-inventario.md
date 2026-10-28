# Inventário de folhas — as sete superfícies

> **O que este documento é.** O denominador do trabalho de prova: cada unidade
> invocável de cada superfície pública, com a classe de prova que tem **hoje** e
> a fonte medida dessa classificação. Não é um plano — o plano por folha vive em
> [`plano-folhas.md`](plano-folhas.md), e a infra-estrutura que falta em
> [`lab-requisitos.md`](lab-requisitos.md).
>
> **O que não é.** Uma leitura do `--help`. Cada «executada» abaixo vem de um
> `results.jsonl` de uma corrida real, de um trace de rotas, ou de um nome de
> teste citado. Onde não há medição, diz **não medido** — nunca «provavelmente».

| Campo | Valor |
|---|---|
| Medido | **2026-10-20** |
| Commit | `2663c846` |
| Motor | `delonix 5.1.1` (`commit: 2663c846f (+49 commits since v5.1.1)`) |
| Host | `delonix`, kernel 7.0.0-38-generic, Intel i9-13900H, 20 threads |
| Binários | os seis de `target/release`. O motor reporta `2663c846f`; os sidecars foram construídos minutos antes, de uma árvore que difere **só em `scripts/`** — nenhuma linha de Rust entre os dois estados, logo são o mesmo binário em comportamento |

## As classes

| classe | significado | o que falta |
|---|---|---|
| **A** | executada e afirmada em ambiente real | só a matriz de casos da Fase 2 |
| **B** | existe, implementada, mas nenhuma corrida a executa | torná-la executável |
| **C** | executada só com mocks ou só numa codificação | levar a real |
| **D** | declarada mas `UNIMPLEMENTED`, ou com defeito conhecido | teste vermelho + correcção, ou ADR a remover/adiar |
| **E** | precisa de infra-estrutura que o laboratório não tem | ver `lab-requisitos.md` |

## Contagens — superfície × classe

<!-- folhas:counts:begin — verificado contra docs/testing/folhas.tsv pelo scripts/folhas_gate.py -->

| superfície | folhas | A | B | C | D | E | fonte do denominador |
|---|---:|---:|---:|---:|---:|---:|---|
| cli | **305** | 272 | 25 | 0 | 0 | 8 | `scripts/cli_baseline.tsv` (gate `rc=0`) |
| node-api | **65** | 15 | 6 | 15 | 29 | 0 | `proto/delonix/node/v1/*.proto` (`rpc <Name>`) |
| mgmt-api | **32** | 1 | 6 | 19 | 6 | 0 | `delonix-mgmt/src/lib.rs` (`.route("…"`) |
| mcp | **15** | 0 | 5 | 10 | 0 | 0 | `delonix-mcp/src/lib.rs` (`name = "<tool>"`) |
| cri | **34** | 24 | 3 | 5 | 2 | 0 | `delonix-cri/proto/api.proto` (`rpc <Name>`) |
| kinds | **135** | 41 | 55 | 8 | 23 | 8 | `kinds.rs` `FACTS` — 27 kinds × 5 verbos |
| **total** | **586** | **353** | **100** | **57** | **60** | **16** | — |

<!-- folhas:counts:end -->

> **Estes números são os do `folhas.tsv`, e o `scripts/folhas_gate.py` chumba se
> esta tabela deixar de lhes bater.** Escreveram-se à mão uma vez e divergiram
> à primeira revisão — a CLI dizia 7 `B` e 12 `E` onde a medição diz 9 e 10 —,
> que é precisamente o defeito de uma linha cujo estado vive em dois ficheiros
> sem ninguém a comparar.

Três notas de denominador, porque a contagem acima é a do **gate** e há leituras
legítimas diferentes:

- **Node API: 59 RPCs, não 116.** O âmbito conta gRPC e REST como folhas
  separadas, e por essa regra seriam 59 + 57 mapeadas. O gate conta a **RPC**,
  uma vez, porque é essa a unidade que o `proto` define e a que o
  `contract_gate.py` já guarda. A lacuna da codificação gRPC não desaparece por
  isso — está registada como trabalho **E1** no `plano-folhas.md`, e vale 57
  folhas.
- **Mgmt API: 32 caminhos, 40 operações.** O `.route()` dá o caminho; alguns
  carregam mais que um método (`/v1/volumes` serve `GET` e `POST`). 40 é a
  contagem por operação, e é a que o ADR-0042 e o `55_API_UNICA.md` publicam.
- **Providers ficam fora do gate.** ~430 operações nos nove crates, das quais
  ~196 com prova live. O denominador deles é um schema HTTP publicado ou um
  `virsh help` de versão nomeada, e já tem instrumento próprio (ADR-0049); um
  segundo extractor pior produziria um segundo número discordante. Ver §7.

---

## 1. CLI — 300 folhas

`scripts/cli-tree.sh --count` → **316 comandos · 273 folhas · 40 grupos**;
classes de reestruturação 169 `~`, 85 `=`, 17 `→`, 2 `!`. `--gate` **rc=0**.

`scripts/cli_exec_ratchet.py` **rc=0** → **263 de 273 invocadas sob asserção —
96,3 %**. Proveniência do trace: 2026-10-20, commit `2663c846`, root isolado,
`PASS=1340 FAIL=0 SKIP=16`.

> **Actualização de 2026-10-22 (merge de `origin/main` `4b860b51`).** A CLI
> cresceu para **280 folhas** (`gitops reconcile/resume/status/suspend`,
> `network vip-pool clear/set/show`), e o trace commitado passou a ser o da
> `main`: 2026-10-21, `6dc31582`, `PASS=1537 FAIL=0 SKIP=15`, **268 de 280**.
> As sete folhas novas estão nesse trace e são **A**; `vm pause`/`vm unpause`
> também (a bateria da `main` tinha uma VM libvirt a correr) e saíram de **E**.
> As quatro que este ramo promoveu — `net l4guard set/clear` e `system boot
> enable/disable` — voltaram a **B**: os checks que as conduzem estão em
> `scripts/e2e.sh`, mas o trace de `6dc31582` é anterior a eles, e o trace só se
> regrava de uma corrida real. Sobem a **A** na próxima gravação. **Feito no mesmo dia**: o trace foi regravado da bateria corrida sobre o
> merge (`fa34383b`, `PASS=1577 FAIL=0 SKIP=14`), **272 de 280**, e as quatro
> estão em **A**. O texto abaixo
> descreve o estado de 2026-10-20 e fica como estava.
>
> **Actualização de 2026-10-22 (verbos de raiz Docker/Podman, #930/#933).** A
> CLI cresceu para **291 folhas**: os 10 atalhos de raiz
> `run/create/ps/start/stop/restart/rm/exec/logs/pull` e o canónico `container
> create`. As 11 estão em **B**, não em **A**: o trace commitado (`fa34383b`) é
> anterior a elas e a bateria exerce só a forma canónica `container
> <verb>`/`image pull` (essas são **A**), nunca o atalho de raiz. Sobem a **A**
> quando um check conduzir a forma de raiz e o trace for regravado de uma
> corrida real.
>
> **Actualização de 2026-10-22 (tabela Docker/Podman completada).** A CLI
> cresceu para **300 folhas**: os 9 atalhos de raiz que faltavam —
> `kill/attach/stats` (container) e `images/rmi/tag/save/load/push` (imagem).
> Mesma razão das anteriores: estão em **B**, partilham a struct e o handler da
> forma canónica `container <verb>`/`image <verb>` (essas são **A**, ou **E**
> no caso de `image push`), e a bateria exerce a canónica, não o atalho. Sobem a
> **A** quando um check conduzir a forma de raiz e o trace for regravado.

As 10 que nenhuma corrida executa, e o que **cada uma** precisa — medido, não
presumido. Os irmãos de cada grupo já correm, e é isso que as torna baratas:
`net netns` (10/10), `net l4guard` (3/3) e `system boot` (3/3) estão
**completos** desde 2026-10-20, `vm image import/convert/describe/ls/rm/init` e
**19 das 21** folhas de `cluster` já são executadas. As 10 que restam são
**todas** classe `E` — nenhuma é lacuna de cobertura, todas esperam
infra-estrutura.

| folha | precisa | classe |
|---|---|---|
| `vm pause` | VM a correr + imagem arrancável no store isolado | **E** (só a imagem) |
| `vm unpause` | idem | **E** (só a imagem) |
| `vm image build` | `virt-customize` (presente) + descarga da cloud image | **E** (rede) |
| `cluster create` | imagem `kindest/node`; `kind`+`kubeadm` presentes, corre os nós **no próprio motor** (sem Docker) | **E** (imagem) |
| `cluster kubeadm` | imagem golden + VMs reais | **E** |
| `image push` | registry OCI | **E** |
| `image sign` | registry OCI (a assinatura publica-se ao lado da imagem) | **E** |
| `vm image pull` | registry OCI | **E** |
| `vm image push` | registry OCI | **E** |
| `vm image ls-remote` | registry OCI (listagem de tags) | **E** |

**Cinco destas folhas eram um pacote em falta, e já não são.** A bateria **já
tinha** checks para `net netns exec/detach/firewall/publish/unpublish`, em
`scripts/e2e.sh`, desde `5c62db13` (2026-10-11), e são dos melhores que aqui
existem — julgam no **fio**, com um token que A responde e B consome, porque «a
listener de uma porta publicada aceita a ligação haja ou não alguém a responder
atrás dela». Contavam zero porque o bloco inteiro saltava num `command -v
socat` e o `socat` não estava no host, não por não existirem: o bloco já lá
estava quando o trace de `e5be0903` (2026-10-16) foi gravado.

Medido a 2026-10-20: o `socat` **está** em `/usr/bin/socat`, o bloco corre, e as
cinco folhas passaram a **A** sem uma linha de código — converteram-se pela
instalação de um pacote, tal como o `lab-requisitos.md` previa ser o item mais
barato de toda a lista.

**Nenhuma das 10 é um stub.** `bins/delonix-runtime-bin/src/` não tem um
`unimplemented!` nem um `todo!` — logo nenhuma é classe `D`. E `vm pause`/`vm
unpause` são reais nos **três** backends: `delonix-provider-cloud-hypervisor`
(`:847`/`:851`), `delonix-provider-libvirt` (`:3254`/`:3268`) e `delonix-proxmox`
(`:6488`/`:6497`); o default do trait falha fechado de propósito
(`vm_backend.rs:570` — «never as a silent no-op»).

**`image sign` não precisa de `cosign`**: tem ECDSA-P256 próprio, com a chave
gerada em `<DELONIX_ROOT>/keys/image-signing.key`. Só a *interoperabilidade* da
assinatura o exigiria.

---

## 2. Node API — 59 RPCs, nas duas codificações

Contrato `proto/delonix/node/v1/{node,infra,compute,operations,common}.proto`.
As rotas REST são **geradas**, não escritas à mão
(`delonix-node-api/build.rs:283-356` → `$OUT_DIR/rest_routes.rs`), e o
`scripts/contract_gate.py:rpc_mappings()` exige `google.api.http` em cada RPC
não-client-streaming e **proíbe-o** nos dois streams bidireccionais.

| | n |
|---|---:|
| RPCs | **59** em 9 serviços |
| mapeadas em REST | 57 (2 sem mapa de propósito: `Exec`, `Console`) |
| server-streaming que respondem 501 em REST | 3 (`WatchEvents`, `WatchOperation`, `Logs`) |
| implementadas | **30** |
| **não implementadas** | **29** |
| com ≥1 teste que as invoca | 35 |
| **executadas numa corrida real** | **16** (15 positivas + `StartContainer` só como 501/405) |
| **nunca invocadas por teste algum** | **24** |

As 29 não implementadas são serviços **inteiros**: `ContainerService` (12),
`PodService` (4), `ImageService` (7), `StackService` (3) — sem handler e sequer
registados em `service.rs:router()` (:519-549) —, mais `ConnectContainer`,
`DisconnectContainer` e `Console`, que estão registados e devolvem
`service.rs:unserved`. **Classe D, e é a maior decisão em aberto deste
inventário** (ver «Decisões que preciso de ti»).

**Lacuna estrutural (classe C, superfície inteira):** a bateria exercita **só a
codificação REST**. Cada chamada é `curl --unix-socket`; não existe `grpcurl`
nem cliente gRPC em `scripts/` ou `tests/`. O gRPC só é exercitado por testes de
componente do `cargo test`. Logo as 57 RPCs mapeadas não têm a perna gRPC numa
corrida real.

Implementadas mas com o caminho de sucesso **sem teste em camada alguma** (só
passam por `T2`, onde cada asserção é uma recusa `NOT_FOUND`):
`ResumeVirtualMachine`, `ListSnapshots`, `RestoreSnapshot`, `DeleteSnapshot`.
E o **`VirtualMachineService` inteiro** (12 RPCs implementadas) nunca entra na
bateria real.

Parciais (classe D mais estreita): `CreateNetwork` recusa topologia overlay;
`CreateVolume`/`DeleteVolume` recusam share/remote/pool/`destroy_remote`;
`WatchEvents` recusa um filtro de namespace; `CancelOperation` está
implementada mas **por desenho nunca cancela** (só `NOT_FOUND` ou
`FAILED_PRECONDITION` DX-5002).

`Idempotency-Key` **é** honrada aqui (`service.rs:595`) e medida de verdade por
`scripts/e2e_node_network_ops.py` (`scenario_replay`).

---

## 3. Mgmt API — 40 operações

Router e handlers num ficheiro só: `delonix-mgmt/src/lib.rs:router` (:188-278),
32 `.route()` planos, sem `nest`, sem `merge`, sem middleware. Arranca por
**`delonix serve api`** (não `serve mgmt`), que faz `execve` do irmão.

| | n |
|---|---:|
| caminhos / operações | 32 / **40** |
| implementadas | 33 |
| retiradas (501 `DX-6302`) | **7** |
| `Idempotency-Key` | **0 de 40** — a string não existe no crate |
| com ≥1 teste | 33 |
| **executadas sobre HTTP real** | **1 de 40** (`GET /v1/dash`) |
| sem teste algum | **7** |

**Lacuna estrutural (classe C, superfície inteira):** `test_state()` (:1463) põe
`bin: PathBuf::from("/bin/false")` (:1470). Todo o teste de componente cobre
**só validação pré-exec e recusas** — **nenhuma rota que faz shell-out tem um
caminho de sucesso testado em lado algum**.

As 7 sem teste algum, por ordem de risco:
`POST /v1/containers/{id}/exec` (**a rota de maior privilégio do motor**),
`GET /v1/containers/{id}/logs`, `GET /v1/images/scan`, `GET /v1/images/sbom`,
`DELETE /v1/networks/{name}`, `GET /v1/net/dhcp6/{net}/{mac}`,
`POST /v1/vms/{name}/action`.

**Segurança:** sem TLS e sem TCP — HTTP sobre `AF_UNIX`, socket a `0600` e
`SO_PEERCRED` em cada accept (`:167-169`); um par que não bate é descartado em
silêncio, logo não existe 401/403 a nível HTTP. Sem tokens e **sem autorização
por rota**: qualquer processo com o euid do servidor tem a superfície toda,
incluindo o `exec`.

**Nota de âmbito que muda a prioridade:** o ADR-0042, passos D-E/F (:131-132,
:148), planeia migrar cada caminho do mgmt para o contrato de nó e **apagar o
`delonix-mgmt`**. Investir 40 folhas de prova numa superfície marcada para
remoção é uma decisão tua, não minha.

---

## 4. MCP — 15 ferramentas

Tudo em `crates/interfaces/delonix-mcp/src/lib.rs` (router `#[tool_router]`,
:297-768). Transporte **só stdio** — `delonix-mcp-bin/src/main.rs:28-33` recusa
qualquer outro («ADR-0025 defers loopback HTTP»).

| | n |
|---|---:|
| ferramentas | **15** |
| implementadas | **15** (nenhuma é stub) |
| **invocadas pelo protocolo real** | **0** |
| com teste que invoca o handler | **1** (`audit.query`) |
| sem teste algum | **5** |

**`tools/call` não existe em lugar algum do repositório.** O único teste de
protocolo (`scripts/e2e.sh:6974`) manda `initialize`,
`notifications/initialized` e `tools/list`, e afirma que `serverInfo` existe e
que `"logs.query"` está na lista de nomes. **Nenhum corpo de ferramenta correu
alguma vez pelo protocolo.** Classe B/C para a superfície inteira.

Sem teste algum: `runtime.info`, `runtime.health`, `resources.get`,
`metrics.query`, `network.inspect`.

Sub-superfície de **resources**, igualmente descoberta: `read_delonix_uri`
(:789-817) e `resources/list` não são tocados por teste algum;
`delonix://schema/{kind}` é um `NOT_IMPLEMENTED` explícito (:839-846).

**Nenhum ratchet guarda a lista de ferramentas** — uma ferramenta pode entrar ou
sair sem gate nenhum dar por isso (`cli_baseline.tsv` só tem as 3 folhas da CLI
`mcp capabilities|doctor|serve`).

Dois achados a confirmar: `task.get` e `task.cancel` **nunca chamam
`self.log(...)`** (:723-746), contra a promessa do ADR-0025 §8 de uma linha de
auditoria por chamada — e o comentário do teste
`audit_query_logs_its_own_call` afirma que `audit.query` «era a única das
catorze» sem `self.log`, o que já não é verdade. E o ADR-0025 diz que
`events.list`/`security.explain` «devolvem um erro claro de não implementado»:
não existem, grep repo-wide dá zero.

---

## 5. CRI — 34 RPCs

| | n |
|---|---:|
| RPCs (29 Runtime + 5 Image) | **34** |
| completas | 22 |
| parciais | 10 |
| `UNIMPLEMENTED` | 2 (`CheckpointContainer`, `GetContainerEvents`) |
| sem handler | **0** — o trait gerado é exaustivo |
| exercitadas pelo `critest` | 24 |
| **sem cobertura de nenhuma fonte** | **5** |

Sem cobertura alguma: `PodSandboxStats`, `ListPodSandboxStats`,
`CheckpointContainer`, `GetContainerEvents`, `ImageFsInfo`.

**Dois baselines gravados, e contradizem-se** — o próprio repo já o registou
como lacuna **G-025 / STALE EVIDENCE**:

| | publicado | com logs crus |
|---|---|---|
| ficheiro | `docs/cri-conformance.md` | `reports/production-readiness/v3.1.0/cri-conformance.md` |
| motor | **v0.63.1** | commit `fa8c6297` |
| data | 2026-08-25 | 2026-09-15 |
| resultado | **79 Passed · 24 Failed · 19 Skipped** | **82 Passed · 21 Failed · 19 Skipped** |

O 79/24 é o que o `README.rst`, o `docs/cri.html` e o `docs/gen.py` publicam.
**Nenhum dos dois é da versão actual (5.1.1)**, e entre os dois o
`ListMetricDescriptors`/`ListPodSandboxMetrics` passou de stub a real (commit
`343e0447`). Das 21 falhas nomeadas: ambiente do host 10, limitação rootless 3,
**defeito do Delonix 3**, funcionalidade em falta 2, divergência deliberada 1,
**desconhecido 2**.

**O `critest` não é corrido por automação nenhuma**: não aparece no `Makefile`
nem em nenhum workflow. É uma medição manual.

**Deriva de contrato:** 7 RPCs do `cri-api` do `critest` v1.36.0 não existem no
proto do repo (`StreamContainers`, `StreamContainerStats`, `StreamPodSandboxes`,
`StreamPodSandboxStats`, `StreamPodSandboxMetrics`, `UpdatePodSandboxResources`,
`ImageService/StreamImages`). Nenhum documento o reconhece.

---

## 6. Kinds — 26 kinds × 5 verbos = 130 células

> **Actualização de 2026-10-22.** O `GitOpsSource` (ADR-0021) entrou no
> catálogo depois desta medição: `apply`, `plan` e `delete` são **A** (a secção
> `gitops:` do `scripts/e2e.sh`), `diff` e `drift` são **B** (alcançáveis —
> o `actual()` traz `last_applied` — mas nenhuma corrida os nomeia). O resto
> desta secção é a medição de 2026-10-20, com 25 kinds.

Medido com `delonix api-resources` contra o binário da árvore.

| forma | kinds |
|---|---|
| `primary` | **19** — RuntimePolicy, Secret, Network, NetworkRoute, StoragePool, Volume, Image, App, VirtualMachine, SystemContainer, Pod, Service, NetworkPolicy, NetworkAccessRule, NetworkGateway, IPPool, HTTPRoute, Gateway, KubernetesCluster |
| `sunset` | 2 — NetworkZone → Network, Container → Pod |
| `compat` | 1 — Ingress → HTTPRoute |
| `sugar` | 2 — Workload → Container/Pod/VirtualMachine, Dependency → NetworkPolicy |
| `aggregate` | 1 — Stack |

Grupos publicados: `core`, `compute`, `networking`, `gateway`, `storage`,
`artifact`, `infrastructure`, `security` — **oito**, não os sete que o
`AGENTS.md` enumera (`security.delonix.io/v1alpha1 RuntimePolicy` existe e não
está na lista da secção «Identidade e fronteira do motor»).

A fonte de verdade é o `const FACTS: &[KindFacts]` de
`crates/contexts/delonix-stack/src/kinds.rs` (:298-714), reexportado por
`cmd/mod.rs:37`; o `api-resources` deriva dele (`cmd/resource.rs:113`) e não há
segunda lista — o teste
`every_registry_kind_is_listed_and_resolvable_by_its_listed_name` impõe-o.

`in_stack: true` em 21 kinds, todos `converges: true`; **15** têm
`teardown: true`. `DELETE_ROUTES` = 16 (`verbs.rs:80`); `destroy_one` = 15
(`stack.rs:2383`). Blocos `provider` em **5** kinds (VirtualMachine, Network,
SystemContainer, NetworkGateway, Gateway) mais o equivalente por vendor do
Volume (`provision.truenas`).

**Nenhum kind está sem teste**: a CI (`ci.yml:476-490`) corre
`stack apply --dry-run` + `stack validate` sobre cada `examples/*.yaml`, e o
`examples/core/stack.yaml` carrega os 23 grupos. A lacuna é a de *corrida real*.

### Os 9 kinds cujo `apply` nenhuma corrida real exercita

**App, Service, Ingress, IPPool, Gateway, Workload, Dependency,
KubernetesCluster, NetworkZone.** Parciais: **SystemContainer** (só recusas,
e2e.sh §5879) e **NetworkGateway** (o ADR-0071 di-lo por escrito: «Not proven
live: the lab OPNsense VM does not boot»). Condicional: **VirtualMachine**
(e2e.sh:3278, saltado sem hipervisor).

O `scripts/e2e-all-templates.sh` **não exercita kind nenhum** — conduz
`init`/`build`/`container run`/`container rm`, a superfície imperativa.

### Verbos realmente exercitados

O **`Container` é o único kind com os cinco** (`apply`, `diff`, `plan`,
`delete`, `drift`). Os únicos sujeitos *nomeados* de `diff`/`drift` em toda a
bateria são `diff containers` (e2e.sh:6724-6726) e o ciclo de drift do
Container (§2476-2523) — inspeccionados os 18 `drift` e 2 `diff` do `e2e.sh`,
nenhum nomeia outro kind. Logo o `diff`/`drift` de `Network` e `Volume` é
**co-ocorrência na mesma secção, não medição**: fica **não determinado**.

Nove kinds não têm **nenhum** dos cinco verbos exercitado: RuntimePolicy,
Service, Ingress, IPPool, Gateway, App, Workload, Dependency e
KubernetesCluster.

### Defeitos encontrados nos Kinds

1. **`Pod.spec.shareProcessNamespace` é lido por ninguém e documentado como
   funcional.** `delonix-compute/src/pod.rs:149-150`, aceite em
   `POD_SPEC_FIELDS:308`. O comentário do próprio campo diz «Honored by
   `kind: Pod` (see `cmd::pod`)», mas o `cmd/pod.rs:18` diz «Shared **PID** …
   lands next» e o `delonix-linux/src/lib.rs:6290` concorda que não está
   implementado. É o **único** campo parsed-but-ignored que é simultaneamente
   não-lido **e** documentado como a funcionar — os outros são deliberados e
   rotulados. Casa com a falha #15 do `critest` (`Security Context PodPID`).
2. **O `FACTS` declara-se a ordem do apply e o `run_layers_inner` discorda em
   duas linhas.** O `kinds.rs:269` diz «The order of the rows below is the order
   of the apply», e o `teardown_order` (`stack.rs:2429`) deriva a remoção
   invertendo-a. Mas o `run_layers_inner` (`stack.rs:2237`) corre o **IPPool
   antes** de NetworkPolicy/NetworkAccessRule/NetworkGateway, e a tabela
   põe-no depois dos três; e o **Ingress não tem camada própria**. As
   dependências que os comentários citam continuam a valer, logo o impacto
   prático é **não determinado** — mas as duas listas não concordam, que é
   exactamente a classe de defeito que o `kinds.rs` existe para eliminar, e
   **nada o testa**.
3. **Dois `delete` genéricos sem razão escrita.** `NetworkAccessRule` e
   `HTTPRoute` faltam no `DELETE_ROUTES` **e** no `no_verb_reason`
   (`verbs.rs:226` devolve `""`), logo respondem «not wired yet». O guarda
   `a_kind_never_both_routes_and_claims_it_cannot` (`verbs.rs:602`) só faz
   `eprintln!` — **nunca afirma** —, portanto esta dívida não pode chumbar a
   CI. O `NetworkAccessRule` também não tem `get` nem `describe`.
4. **O `drift` é estruturalmente silencioso em 5 kinds** (RuntimePolicy,
   Secret, Image, App, Gateway): o `actual()` devolve `last_applied: None`,
   logo contam sempre como `unstamped` e nunca como deriva — e isso **não
   está dito** no mecanismo de honestidade `DOC_SCOPED`/`not_checked` do
   `drift.rs`.
5. **O `IPPool.spec.interface` é inalcançável em qualquer documento aceito**:
   só vale com `announce: l2` (`ippool.rs:234`), e o `l2` é recusado como não
   construído (`:242`, ADR-0046 fase 4).

### Campos efectivos — o número que a Fase 2 tem de cobrir

Os `RECONCILED_*_FIELDS` são o que `plan`/`diff`/`drift` comparam. A diferença
face ao schema é grande e é o que a condição `unconverged_fields_condition`
agora **nomeia** em vez de descartar em silêncio:

| kind | props no schema | efectivos |
|---|---:|---:|
| VirtualMachine | 60 | **9** |
| Container | 50 | **12** |
| Volume | 13 | 8 (só `quota` é a quente) |
| RuntimePolicy | 12 | 12 |
| Network | 9 | 7 (só `peers` é a quente) |
| Pod | 9 | 3 |
| SystemContainer | 9 | 8 |
| NetworkAccessRule | 6 | 6 |
| IPPool | 3 | 3 |

Faltam cinco ficheiros `examples/full-*.yaml`, que são a referência por campo:
`full-storagepool.yaml`, `full-systemcontainer.yaml`, `full-networkzone.yaml`,
`full-networkgateway.yaml` e as variantes de share do `full-volume`.

---

## 7. Providers — 9 crates

| provider | ops | live.rs | ops cobertas live | trace de rotas | failure injection |
|---|---:|---|---|---|---|
| Proxmox | ~300 | 45 casos | ~179 rotas | **sim** — 184/675 (27,3 %) | **sim**, ~80 casos |
| OPNsense | ~24 | 4 casos | 15 ops | não | **sim**, ~45 casos |
| TrueNAS | 5 | 1 caso | 5 ops | não | **não** |
| PowerDNS | 2 | 1 caso | 2 ops | não | parcial (`tests/mock.rs`) |
| libvirt | ~34 | 1 caso | **3**: `boot`, `is_running`, `guest_info` | **inutilizável** | **não** |
| Cloud Hypervisor | ~21 | 1 caso | **2**: `hotplug_add_cpu/memory` | não | **não** |
| LVM | ~11 | **não** | **0** | não | **não** |
| ZFS | ~11 | **não** | **0** | não | **não** |
| btrfs | ~11 | **não** | **0** | não | **não** |

Métrica de células do motor: **103 de 273 aplicáveis provadas (37,7 %)**
(`capability_ratchet.py` **rc=0**).

**Sete dos nove providers não têm failure injection.** Só Proxmox e OPNsense
têm, e as duas suites afirmam a partir do **registo de pedidos** do mock — «a
chamada devolveu Ok» nunca é a asserção inteira.

**Nenhum gate de CI** regenera ou verifica o `docs/proxmox/matrix-9.2.2.md`, e
o `lab.yml` nunca corre um `--test live`: cada caso live é `#[ignore]` e corrido
à mão.

### Defeitos verificados por mim neste inventário

1. **A etiqueta `delonix-test-run=<uuid>` não existe em lado nenhum.**
   `grep -rn 'delonix-test-run'` no repo todo → **0 ocorrências**. É um
   requisito do próprio briefing desta missão, e não está implementado em
   nenhum dos nove providers. O que existe é o `fresh_mark()` do OPNsense
   (`live.rs:50-61`) — mas essa é a marca de posse **de produção** (ADR-0059
   D1.5), não uma etiqueta de teste; e nomes derivados do PID nos outros.
2. **O `scripts/libvirt_virsh_inventory.py` lê os ficheiros errados.** O
   `SOURCES` (:54-57) aponta para `crates/adapters/delonix-vm/src/lib.rs` e
   `bins/delonix-runtime-bin/src/cmd/vm.rs`. Medido: o crate do provider tem
   **199** ocorrências de `virsh`, o adaptador que o script varre tem **2**.
   Logo o numerador do libvirt vem do sítio errado e o **31/276 (11,2 %)** do
   ADR-0050 não é reproduzível contra a árvore de hoje.
3. **O cliente Proxmox não tem política de redirect.** O `connect_with`
   (`src/lib.rs:1201-1225`) define `connect_timeout`, `timeout`,
   `pool_max_idle_per_host`, `danger_accept_invalid_certs` e
   `add_root_certificate` — e **nenhum `redirect::Policy`**, logo vale o default
   do `reqwest` (seguir até 10). O OPNsense tem a política
   (`src/lib.rs:245 .redirect(Policy::none())`), a verificação (`:292`) e o
   teste (`a_302_with_no_body_is_unauthorized_and_never_followed`), e o
   comentário do módulo (:17-21) explica porque importa: um cliente que segue
   redirects **esconde** uma das falhas, redirigindo para o login do GUI. O caso
   11 da matriz da Fase 2 exige exactamente «redirect (nunca seguido)».

### Outros achados dos providers, a confirmar antes de afirmar

- **Salto silencioso** em `delonix-proxmox/tests/live.rs:2150`: um
  `if let Ok(callback) = env::var(...)` salta metade de um caso e o caso continua
  a dar **passed** — contra a regra «variável em falta FALHA». E há defaults que
  mascaram ausência (`_NODE`→`pve`, `_STORAGE`→`local-lvm` em 18 sítios,
  `_BRIDGE`→`vmbr0`; TrueNAS `_POOL`→`tank`).
- **Teardown não é à prova de falha** no OPNsense (0 guardas `Drop`), no TrueNAS
  (0) e em ~41 dos 45 casos do Proxmox (desmontagem inline com `.expect()`): um
  pânico a meio deixa o objecto no alvo.
- **LVM/ZFS/btrfs são enviados e ligados** (`delonix-storage-helper/src/main.rs:120-133`,
  `cmd/storage_pool.rs:125-134`) mas o `capability-matrix.md:196-197` declara
  `storage.lvm-thin` e `storage.zfs-btrfs` **`not-implemented`**.
- **9 casos live do Proxmox nunca passaram contra um nó** (auto-declarado em
  `live.rs:33-50`): os três `move`, o `move` do system container, o IP pelo
  agente, o DNS, os vCPUs/memória do agente, a release do nó e a reparação do
  gateway. Logo `move_to_node`, `move_to` e `DnsProvider::prepare_zone/observe`
  estão implementados e **não provados**.
- `ListPodSandboxStats` do CRI lê só `f.id` e **descarta
  `PodSandboxStatsFilter.label_selector`** — exactamente o defeito que o irmão
  `list_container_stats` diz ter corrigido (`lifecycle.rs:2686`). Sem cobertura,
  nada o apanharia.
- O `scripts/critest.sh` isola o `DELONIX_ROOT` mas **não** o
  `DELONIX_NET_RUNTIME_DIR`, e o `cleanup()` (:49) invoca
  `"$CLI_BIN" netns down` — uma grafia que **já não existe** (é `net netns down`
  desde a reorganização da CLI). Medido: `delonix netns down` responde
  `error: unrecognized subcommand 'netns'`. O erro é engolido por `|| true`.

---

## Decisões que preciso de ti

1. **As 29 RPCs não implementadas do contrato de nó.** `ContainerService`,
   `PodService`, `ImageService` e `StackService` são serviços inteiros
   declarados e não servidos. Implementar está muito além de um ciclo TDD por
   folha; a alternativa do briefing é «decisão explícita de remover/adiar com
   ADR». Qual?
2. **O `delonix-mgmt` está marcado para remoção** pelo ADR-0042 D-E/F. Provar
   as suas 40 operações ou deixá-las como estão até à migração?
3. **O baseline do CRI contradiz-se** (79/24 publicado vs 82/21 com logs), e
   nenhum é da 5.1.1. Remedir agora e republicar, ou tratar como Fase 5?
4. **A comparação da Fase 5 não corre neste host**: não há `docker` nem
   `podman`, e o `bench_gate.py` **recusa julgar** esta máquina (a baseline é da
   AMD Ryzen 9 8940HX/32 threads; esta é Intel i9-13900H/20 threads). Precisa de
   uma VM de laboratório, como a corrida de 2026-08-13 fez.
