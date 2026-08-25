# Programa das 13 melhorias — matriz de rastreabilidade

> **O que este documento é.** O registo único do estado de M01–M13, com a
> **baseline medida** de cada um. Uma célula que diz um número diz onde ele foi
> obtido; uma célula sem medição diz `por medir` — nunca uma estimativa.
>
> **O que não é.** Um plano de intenções. Um `DONE` aqui exige as quatro coisas
> ao mesmo tempo: código, testes, documentação e evidência reproduzível.
>
> **Porque é que ele existe.** O `docs/AUDITORIA-E2E.md` deste repo passou
> semanas a dar 27 problemas resolvidos por dívida viva, por não ter sido
> actualizado à medida que as correcções entravam. Uma tabela que não acompanha
> o código mente nos dois sentidos. Esta tem de ser actualizada no MESMO commit
> que muda o estado que descreve.

| Campo | Valor |
|---|---|
| Baseline medida em | **2026-08-25** |
| Contra | `origin/main` `b4653002b`, versão `0.63.1` |
| Onde | host de desenvolvimento, Linux 7.0, rootless, cgroup v2 |
| Binário | `cargo build -p delonix-runtime-bin` do próprio commit |

## Baseline factual do motor (medida hoje, não citada)

| Grandeza | Valor | Como foi obtida |
|---|---|---|
| Crates no workspace | **13** | `ls crates/` + `Cargo.toml` por directório |
| Comandos de topo | **28** | travessia de `--help` a partir do binário |
| Folhas invocáveis da CLI | **229** | a mesma travessia, contando as que não têm subcomandos |
| Testes no workspace | **1207** | `#[test]` + `#[tokio::test]` em `crates/` |
| Checks da bateria E2E | **270** | `scripts/e2e.sh` |
| Cenários de caos | **8** | `scripts/chaos.sh` |
| Rotas na Docker Engine API | **14 servidas, 12 recusadas com razão** | `API_MATRIX` / `API_UNIMPLEMENTED` em `cmd/dockerapi.rs` |
| Kinds no manifesto | **19** | tabela `cmd/kinds.rs` |
| ADRs | **18** (13 Accepted, 2 Proposed, 1 Rejected, 2 sem estado parseável) | `docs/adr/` |
| Jobs de CI | **7** (`fmt`, `lang`, `clippy`, `test`, `deny`, `docs` + caos à parte) | `.github/workflows/ci.yml` |

> **Correcção a esta baseline (2026-08-25).** A primeira versão desta linha dizia
> «21 rotas servidas», contadas por literais de rota no ficheiro. Estava errada:
> 21 é a soma das servidas com as **recusadas** — a `API_UNIMPLEMENTED` também
> guarda literais de path. O número certo é 14 servidas. Fica registado em vez de
> corrigido em silêncio, porque foi exactamente o erro que este programa existe
> para apanhar, cometido pelo próprio documento que o mede.

> **A superfície não é a cobertura.** As 229 folhas têm o `--help` verificado; o
> número das que a bateria **executa** não foi remedido nesta passagem e a
> fracção citada no `AGENTS.md` é de outra data. Fica `por medir` em M02/M11 até
> ser recontado com o `scripts/e2e.sh` do dia — citar um total que subiu faria a
> cobertura parecer melhor sem uma única folha nova exercitada.

## Matriz

Estados: `NOT_STARTED`, `IN_PROGRESS`, `PARTIAL`, `BLOCKED`, `DONE`.

| ID | Melhoria | Estado | Baseline medida | Entregáveis em falta | Testes | Dep. | Risco | PR | DoD |
|---|---|---|---|---|---|---|---|---|---|
| **M01** | Posicionamento e arquitectura | `IN_PROGRESS` | `ARCHITECTURE.md` tem C4 1–3 e mini-ADRs; **omitia 3 de 13 crates** e a contagem dizia 10. Nenhum gate arquitectural existia. | capability discovery; política de estabilidade por API (parcial em `cli-stability.md`); gate de dependência proibida (fronteira PaaS) | `tests/architecture.rs` — 3 gates, verdes; regressão verificada nos dois sentidos | — | baixo | — | 3 de 5 |
| **M02** | Compatibilidade e migração | `IN_PROGRESS` | CRI **77/103** (`critest` v1.36.0, motor v0.42.2 — **desactualizado**). Docker API: **14 servidas, 12 recusadas com razão**, e agora um **terceiro estado** — 21 rotas que o `kind`/compose usam, das quais **8 recusadas** (`serve docker-api --matrix`). `compose` nativo; `compatibility`/`migrate assess` **não existem**. | comandos `compatibility {compose,oci}` e `migrate assess`; recontagem do `critest` na versão actual; matriz do Compose | `tests/compat/` + 3 gates da matriz Docker | M01 | médio | #123 | 1 de 4 |
| **M03** | Build de produção | `PARTIAL` | `build` tem `--secret`, `--platform`, `--no-cache`, `--build-arg`, cache por instrução (rootless), multi-stage. **Sem** `--ssh`, cache distribuída, SBOM/provenance no artefacto de build. | mounts `type=ssh`/`cache`; cache em registry; SBOM+provenance por imagem construída; comparação medida com BuildKit | `crates/delonix-image/benches` existe | M04 | médio | — | 1 de 4 |
| **M04** | Segurança verificável | `PARTIAL` | Releases **assinadas** (minisign, `release.yml`) e `cargo-deny` no CI. **Sem** SBOM de release, **sem** provenance/SLSA, **sem** fuzzing no CI, **sem** `kind: RuntimePolicy` (0 ocorrências). 3 auditorias ofensivas anteriores registadas. | SBOM + attestation de release; `kind: RuntimePolicy`; job de fuzz; processo de advisory publicado | 1207 testes; auditorias em `AGENTS.md` | — | **alto** | — | 1 de 4 |
| **M05** | Desired State e GitOps | `IN_PROGRESS` | `stack` serve **11** verbos: `init apply destroy prune plan ls describe wait validate history rollback`. `plan` não muda estado e tem `--detailed-exitcode`; diff de 3 vias sem ficheiro de estado; **revisões persistidas** (ADR-0019). **Faltam `diff`, `drift`, `reconcile`** — `drift` e `diff` são hoje o `plan` com outro nome; o `reconcile` contínuo é o que resta a sério, e traz a pergunta do daemon. O `apply` continua fail-fast sem rollback, por desenho, e já não deixa órfãos invisíveis. | `drift`/`diff` como verbos próprios; reconciler opcional com rate-limit | caos `stack_converge` + `stack_partial_apply`; **19 checks E2E** de `history`+`rollback`, um a apagar `stacks/` e outro a exigir o ciclo completo | M01 | médio | #120, #121, #122 | 5 de 6 |
| **M06** | Gestão de frota | `BLOCKED` | `node`/`cordon`/`drain` **não existem** (0 ocorrências). **O ADR-0010 RECUSOU a API de gestão remota** (2026-08-10) e o `AGENTS.md` diz que `delonix node add` está bloqueado. | **ADR sucessor** que nomeie o consumidor concreto — ver a nota abaixo | — | ADR | — | — | bloqueado |
| **M07** | Observabilidade OTel | `PARTIAL` | OpenTelemetry **0.32** e `prometheus-client` na árvore (`delonix-runtime-core`); `/metrics` no `delonix-cri` e no `delonix-mgmt`; `system events`; `dash --json`. **Sem** `observe`/`trace`/`diagnose` como comandos; correlação e bundle sanitizado por fazer. | comandos `observe`/`diagnose`; schemas de evento versionados; overhead medido | — | M01 | médio | — | 2 de 4 |
| **M08** | SLOs e health | `NOT_STARTED` | `slo` tem **0 ocorrências** em toda a CLI. Existem `--health-*` no `run` e probes de compose. Sem SLI/SLO, sem error budget, sem reason codes. | tudo | — | M07 | médio | — | 0 de 5 |
| **M09** | MicroVMs e isolamento | `PARTIAL` | 3 backends (`cloud-hypervisor`, `libvirt`, `proxmox`) por trás de `VmBackend` **com registo**; ADR-0006 fixou `type: microvm`; snapshots nos dois backends locais. **Sem** `isolation: {container,microvm,vm,auto}` (0 ocorrências de `isolation` no `cmd/workload.rs`). | eixo `isolation` com `auto` explicável; Firecracker/Kata avaliados; capability model SEV-SNP/TDX | 28 folhas `vm`; secção CH no `e2e.sh` | M01 | médio | — | 2 de 5 |
| **M10** | Plugin SDK e providers | `PARTIAL` | **Um** trait de provider em todo o workspace: `VmBackend` (`delonix-vm`), com registo e `BackendRegistration`. Providers reais: Proxmox (ADR-0008), TrueNAS (ADR-0009). **Sem** interfaces versionadas para network/storage/secrets/tunnel/registry, sem contract tests. | SDK versionado; contract tests comuns; isolamento e assinatura de plugin | `crates/delonix-proxmox/tests/live.rs`, `delonix-truenas/tests/live.rs` | M01 | médio | — | 1 de 5 |
| **M11** | Performance comprovada | `PARTIAL` | `docs/comparacao-medida.md` compara as três ferramentas na mesma máquina no mesmo dia — mas **da v0.53.0, 2026-08-13**. `crates/delonix-image/benches` existe. **Sem** harness versionado nem thresholds de regressão em CI. | harness reproduzível; thresholds; recontagem na versão actual | benches de `delonix-image` | M01 | médio | — | 1 de 4 |
| **M12** | Developer Experience | `PARTIAL` | `completion`, `explain`, `man`, `syntax`, `init` com **11 templates**, `install.sh` com verificação de assinatura, exit codes com classe (v0.49.0). **`doctor` tem 0 ocorrências**; sem error IDs; sem hot reload/file sync. | `doctor`; error IDs estáveis; golden paths local→CI→k8s medidos | 270 checks E2E | M01 | baixo | — | 3 de 4 |
| **M13** | Maturidade e certificação | `PARTIAL` | `docs/cli-stability.md` define estável/não-estável e o contrato de exit codes; releases assinadas. **Sem** `feature status`, `release verify`, `conformance report`; sem níveis por capability. | os 3 comandos; níveis derivados de evidência; release gates | — | todos | médio | — | 1 de 5 |

## M06 — porque está `BLOCKED` e não `NOT_STARTED`

A diferença não é de grau. `NOT_STARTED` diz «ninguém chegou lá»; aqui alguém
chegou, mediu e **decidiu que não**: o **ADR-0010 está `Rejected`** (2026-08-10),
com a razão escrita — dos consumidores que enumerava, a evidência apontava para
o control-plane de frota, e isso é o `delonix-paas`; *remoteness* sem identidade,
autorização e auditoria não é *remoteness* que valha a pena. O `AGENTS.md`
repete-o e acrescenta a condição de reabertura: **um ADR sucessor que nomeie o
consumidor concreto**, nunca um comando.

Duas consequências, e nenhuma é negociável por este programa:

1. **Implementar `node add`/`cordon`/`drain` agora contrariaria uma decisão
   aceite do repositório**, e a regra da casa é que um ADR se sucede com outro
   ADR — não se revoga por uma linha noutro documento nem por um pedido de
   funcionalidade. É também o que a regra 9 do próprio programa manda («preservar
   contratos») e o que o guarda-rio da fronteira com o PaaS exige (o motor não
   tem noção de inquilino).
2. **O trabalho de M06 que NÃO depende da decisão pode avançar** e não está
   bloqueado: identidade de nó, mTLS e o modelo de capacidades já têm o ADR-0003
   em `Proposed`. O que fica travado é a superfície de comando e o controller.

O desbloqueio é uma peça de M01 (fronteiras), não de M06: escrever o ADR
sucessor com o consumidor nomeado, ou registar formalmente que M06 sai do âmbito
deste motor. Enquanto nenhuma das duas acontecer, M06 permanece `BLOCKED` — e
**não** contará como `DONE` por omissão.

## M05 — o que a Fase B fechou, e o que fica

**Fechado:** um `apply` que morre a meio deixava, sem dono, tudo o que as
camadas anteriores tinham criado — o carimbo só era escrito no fim. Medido
contra `b465300`: o `stack plan` seguinte não mencionava o recurso de todo, e o
`stack destroy` levava os irmãos carimbados e deixava-o para trás. Numa
ferramenta cujo `destroy` promete «remove tudo o que esta stack possui», isso é
uma fuga de recursos conduzida por manifesto. Gate: cenário de caos
`stack_partial_apply`, que corre o **ciclo** — cada comando isolado devolve o
mesmo com e sem o defeito; o que distingue é se o `destroy` alcança o órfão.

**Não fechado, e é decisão de desenho e não dívida:** o `apply` continua
fail-fast **sem rollback**. Desfazer a meio é pior do que parar, e o próprio
código o diz em três sítios. O que falta para dar um caminho de volta a sério
não é rollback transaccional — é `history` + `rollback`, ou seja **revisões
persistidas**, que é o próximo slice de M05 e é superfície nova (onde vivem, o
que uma revisão guarda, o que é reversível e o que não é).

**Correcção de raciocínio registada no código.** A primeira justificação escrita
para excluir os `Update` do carimbo de salvamento estava errada — dizia que a
mudança se leria como «já aplicada» e se perderia. Ler o `diff_fields` mostrou
que não: para um campo que o manifesto ainda declara, o desejado é comparado com
a máquina e o `last-applied` nunca é consultado. A razão verdadeira é outra e
chega mais tarde — o `last-applied` é o que autoriza **reverter** um campo que
saiu do manifesto, por isso carimbá-lo sem ter aplicado faz o motor reclamar
autoria de um valor que não pôs.

## M05 — revisões (ADR-0019)

Uma stack passa a gravar **uma revisão por apply**, em
`<root>/stacks/<stack>/revisions/`: o manifesto renderizado mais um cabeçalho
(número, instante, caminho, se correu bem, contagens do plano). Applies falhados
são gravados **e marcados** — depois de um incidente a pergunta é o que a máquina
foi MANDADA fazer, não o que conseguiu.

**A propriedade que separa isto de um `terraform.tfstate`, e é a única razão de
o ADR ter sido aceite:** um `.tfstate` é a fonte de verdade sobre o que existe,
por isso quando deriva a ferramenta age sobre uma mentira. Aqui nada é lido para
decidir o que existe — a posse e o diff de 3 vias continuam a vir do carimbo no
próprio recurso. **Apagar `<root>/stacks/` e o `plan`/`apply`/`prune`/`destroy`
funcionam na mesma**, e isso não é uma afirmação: são três checks da bateria E2E
que correm logo a seguir a um `rm -rf`.

Retenção de 20, podada pelo escritor (não há daemon), e **best-effort**: uma
revisão que não se consiga escrever nunca faz falhar um apply que funcionou.

## M05 — `rollback` (o slice a seguir às revisões)

Um rollback **É um apply**: repete o manifesto da revisão pelo `apply_docs`, o
mesmo caminho que um apply normal segue — e não um segundo que dele divergiria.
Ganha revisão própria, marcada com a que replicou, senão o histórico mostraria
duas entradas iguais sem dizer que a segunda foi um passo atrás deliberado.

**O que ele não consegue desfazer é impresso ANTES de correr**, e contado a
partir do plano desta invocação em vez de ser um aviso genérico: recursos
criados depois do alvo ficam (só saem com `--prune`), um recurso recriado vem
**vazio** (o registo guarda o manifesto, nunca os bytes), e um campo frio
continua a precisar de `--replace`. Uma revisão FALHADA é recusada como alvo.

**A verificação do gate encontrou um defeito no próprio gate.** O check da
recusa media o código de saída, e com a recusa desactivada continuava a passar —
repetir um manifesto que não aplica também falha, por isso o `rc` não distingue
«recusado à cabeça» de «tentou e rebentou a meio», que é a funcionalidade
inteira. Passou a exigir a frase que só a recusa produz. Sem a passagem de
reversão, ficava um gate verde a guardar nada.

## M02 — o terceiro estado da matriz Docker

A matriz publicada tinha **dois** estados: servido, e recusado com razão. Faltava
o que este repo escreve como regra e não estava a cumprir — o **em falta**. Um
leitor das duas listas não distingue «não implementado» de «ninguém pensou
nisto», e a diferença decide se ele espera ou vai a outro lado.

Medido: `POST /images/create` (o pull que quase toda a ferramenta faz PRIMEIRO)
e `GET /containers/{id}/stats` não apareciam em lista nenhuma. Duas rotas de rede
que o `kind` chama também não, porque o `/networks` genérico não casa com
`/networks/create` nem com `/networks/{id}`.

A `API_UPSTREAM_USED` fecha-o com uma fonte por linha — a captura das 52
invocações do `docker` real durante um `kind create cluster`, transcrita no
`AGENTS.md`, e a sequência do compose contra a qual esta camada foi construída.
Uma linha sem captura própria (o `stats`) **diz que não a tem**. Um gate exige
que cada rota dessa lista esteja classificada; falhou com 4 quando foi escrito.

O que a tabela passou a mostrar, e antes não mostrava: das **21** rotas que
ferramentas reais chamam, **8 estão recusadas** — incluindo o pull. Isso é o que
separa «tem uma API Docker» de «o Testcontainers corre contra isto».

## Ordem de execução

A do programa, com uma alteração justificada: M01 entrega primeiro os **gates**
e só depois a prosa. Um documento de arquitectura sem gate foi exactamente o que
deixou 3 crates fora do C4 sem ninguém dar por isso.

1. **Fase A** — M01 + esta matriz.
2. **Fase B** — M04 e os P0 de reliability (o `apply` sem rollback é o maior).
3. **Fase C** — M02, M03 e a fundação de M13.
4. **Fase D** — M05, M07, M08.
5. **Fase E** — M09, M10.
6. **Fase F** — M06, **se e quando** o ADR sucessor existir.
7. **Fase G** — M11, M12 e o fecho de M13.

## Registo de alterações

| Data | Alteração |
|---|---|
| 2026-08-25 | Baseline inicial medida contra `b4653002b`/v0.63.1. M01 passa a `IN_PROGRESS`: `tests/architecture.rs` instalado, 3 crates repostos no C4, contagem corrigida em dois documentos. |
| 2026-08-25 | M02 passa a `IN_PROGRESS`: a matriz Docker ganha o terceiro estado (o «em falta») e um gate que o obriga. Baseline corrigida — eram 14 servidas e não 21. |
| 2026-08-25 | `stack rollback`: as revisões passam a ter caminho de volta. M05 fica com 11 verbos e o `reconcile` contínuo como único item a sério em falta. |
| 2026-08-25 | Fase D (adiantada). `stack history` + ADR-0019: revisões persistidas, com a propriedade «apagar `stacks/` não parte nada» fixada por gate. M05 sobe para 5 de 6. |
| 2026-08-25 | Fase B. M05 passa a `IN_PROGRESS`: fechada a fuga de recursos de um `apply` parcial (`salvage_ownership`), com o cenário de caos `stack_partial_apply` a fixá-la. O risco de M05 baixa de **alto** para médio — o que resta (`history`/`rollback`) é superfície em falta, não perda de dados. |
