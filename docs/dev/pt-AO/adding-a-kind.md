<!-- translated-from: adding-a-kind.md sha256:390b9b778fbc57a84fa46c20c5fbddf6e29ec5a6b7b476e1ed05169ecad6a056 -->
# Acrescentar um Kind

**Antes de leres:** [Convenções de código](coding-conventions.md), [Arquitectura](architecture.md) e [Os crates](crates.md#delonix-stack) — esta página assume que sabes o que o `delonix-stack` possui e porque é que o planeamento é puro.

Um Kind é um recurso declarativo que este motor conhece — `Container`, `Volume`, `Service`, e por
aí fora (o `delonix api-resources` lista-os todos). Acrescentar um toca em mais do que um simples
braço de `match`: a tabela que descreve o que o Kind É, o código que o aplica, e a fiação do
reconciliador que deixa o `stack plan`/`apply` tratá-lo como qualquer outro Kind. Esta página
percorre isso pela ordem, com um Kind real — **`Service`** (ADR-0032) — como exemplo trabalhado do
princípio ao fim, porque é o Kind acrescentado mais recentemente e todo o ficheiro citado abaixo é
lido da árvore, não da memória de uma disposição antiga.

## A única tabela a que um Kind tem de responder

`crates/contexts/delonix-stack/src/kinds.rs` é a fonte única do que um Kind é. O próprio
comentário do módulo explica porque existe: antes desta tabela, os mesmos factos sobre um Kind
viviam em seis listas separadas (`KINDS`, `CONVERGING_KINDS`, `TEARDOWN_KINDS`,
`kind_honors_namespace`, os `DECLARATIVOS` do teste do wait, e os braços do `presence()`), e
divergiam — o `Vm`, o `FirewallPolicy` e o `ShareVolume` ganharam uma vez um adaptador de
reconciliador e ficaram DE FORA de `CONVERGING_KINDS`, por isso o passo de convergência
saltava-os em silêncio enquanto o próprio `apply` deles mantinha o recurso a parecer correcto,
pelo caminho errado.

Acrescentar um Kind começa por uma linha `KindFacts`:

```rust
pub const SERVICE: &str = "Service";
```

```rust
KindFacts {
    kind: SERVICE,
    plural: "services",
    short: &["svc"],
    api_version: "networking.delonix.io/v1alpha1",
    domain: Domain::NetConnectivity,
    form: Form::Primary,
    in_stack: true,
    converges: true,
    teardown: true,
    namespaced: Namespaced::Always,
    presence: Presence::Registry,
},
```

Cada campo é uma decisão, não uma formalidade:

| Campo | Pergunta a que responde | Errá-lo |
|---|---|---|
| `kind` | O nome, como `const` — nunca um literal de string à solta. Uma renomeação a tocar em todos os sítios de chamada foi medida em **106 ocorrências em dez ficheiros** antes de esta tabela existir; um `const` transforma um typo num erro de compilação em vez de um braço de `match` silenciosamente inalcançável (um padrão `&'static str` mal escrito degrada para uma *binding* fica-com-tudo, o que o próprio comentário do `SECRET`/`NETWORK`/etc. na tabela nomeia explicitamente). | Um segundo Kind a responder a ninguém, ou uma renomeação que falha um sítio sem erro de build nenhum. |
| `plural` | A palavra que `delonix get <plural>` aceita. Campo próprio, não `kind` + `"s"` — `Dependency`→`dependencies` e `Ingress`→`ingresses` não seguem essa regra. | `delonix get services` (correcto) contra um `delonix get servicess` adivinhado. |
| `short` | Abreviaturas aceites. Escasso de propósito — um teste impõe unicidade global, por isso uma abreviatura que colida com outro Kind falha o build em vez de sombrear em silêncio. | Dois Kinds a responder às mesmas três letras. |
| `api_version` | O `apiVersion` que um manifesto escreve, dividido por domínio (`compute.delonix.io/…`, `networking.delonix.io/…`, …) desde a ADR-0020. Uma coluna, não uma constante partilhada, precisamente para os Kinds poderem migrar para o esquema dividido um de cada vez. | Um Kind preso à string legada `delonix.io/v1`, ou no grupo errado. |
| `domain` | A área de actuação, mostrada na coluna `DOMAIN` do `stack ls`/`plan --fields`. As três de rede estão separadas de propósito: `NetConnectivity` responde «existe um caminho», `NetPolicy` responde «o tráfego nele é permitido» — fundi-las esconderia que o `NetworkRoute` abre um caminho enquanto o `FirewallPolicy` decide se deixa o tráfego atravessá-lo. | Um domínio que responde a uma pergunta diferente da que o Kind de facto actua sobre. |
| `form` | O que um documento deste Kind se torna: `Primary` (o seu próprio apply, sobrevive ao load), `Sugar(alvo)` (reescrito para outro Kind no momento do load, desaparece), `Aggregate` (expande-se nos documentos que contém, como o `Stack`), `Compat(alvo)` (um schema estrangeiro — o `Ingress` é `networking.k8s.io/v1` — compilado sobre o mecanismo de outro Kind, e ao contrário do `Sugar` *sobrevive* ao load), ou `Sunset(alvo)` (ainda primário, sobrevive ao load, mas um sucessor é anunciado; usado quando reescrever mudaria em silêncio o que o motor *faz* — o `Container` não pode baixar para um `Pod` de um membro só porque um Pod constrói sempre uma netns partilhada, o que é uma forma de execução diferente). | Escolher `Sugar` para algo que tem de manter o seu próprio apply, ou vice-versa. |
| `in_stack` | Se o `stack apply` sequer trata dele. **As linhas com `in_stack: true` têm de ficar um prefixo contíguo da tabela** — o `destroy` deriva a sua ordem de teardown invertendo a ordem do stack, por isso uma linha colocada depois de um Kind fora do stack muda a ordem de apply sem ninguém editar uma «ordem» em lado nenhum. Um teste (`os_kinds_do_stack_sao_um_prefixo_contiguo`) impõe isto. | Um Kind aplicado fora de ordem de dependência, ou um Kind de procedimento remoto como o `KubernetesCluster` (SSH contra hosts que já existem, não um recurso local) fiado por engano no ciclo. |
| `converges` | Se um campo *mudado* é de facto aplicado, contra só «garante presente». `false` é legítimo — o estado do `Secret` são valores cifrados que um plano não vai decifrar para comparar — mas precisa de uma razão (vê o `not_converged_reason` abaixo); uma desculpa genérica falha um teste. | Um Kind que reporta `!` em todos os planos com uma razão que se lê como «ninguém chegou lá» quando a verdade é uma propriedade do recurso. |
| `teardown` | Se o `destroy_one` o consegue remover, para o `--prune` e o `destroy` o poderem prometer. Um teste (`so_um_kind_convergente_tem_teardown`) recusa um Kind com `teardown: true` e `converges: false` — prometer podar algo que o plano nem consegue representar como mudado. | O `--prune` a prometer remoção e o `destroy_one` a recusar a meio, depois de Kinds anteriores na ordem de teardown já terem desaparecido. |
| `namespaced` | `Never`, `Always`, ou `PerDocument`. Não é um `bool` — o `Volume` tem genuinamente três respostas: nenhuma para um volume simples, real para um com um bloco `share:`; modelá-lo como `true` avisaria «namespace sem efeito» em todo volume normal, e como `false` avisaria o mesmo, erradamente, numa share cujo namespace decide em que directório os seus dados vivem. | Um aviso de namespace que contradiz o que o próprio `apply` do Kind faz com o campo. |
| `presence` | Como `stack ls`/`wait` sabem se o recurso existe: `Registry` (um store responde sim/não), `Derived` (calculado a partir de outra coisa — um Pod são os seus membros com label), `Declarative` (nada para reler; o recurso é uma directiva aplicada a um alvo, e `presence()` responde `-`, que *não* é «ausente»), ou `NotObservable` (nunca chega ao `presence()` — não sobrevive ao load, ou não é sequer um recurso local). | O `NetworkRoute` já não teve braço nenhum no `presence()` e caía em `_ => ("?", "unsupported kind")` — impresso por `ls`/`describe`, e lido pelo `wait` como pendente para sempre. |

A linha do `Service` lê-se, numa frase: aplicado pelo stack, logo a seguir aos Kinds de compute
que selecciona; tem um caminho real (`NetConnectivity`); primário; converge sem recriar; pode ser
desfeito; sempre namespaced; e um registo real está por trás.

## O tipo de spec e o schema

Um Kind com spec de manifesto tipada (a maioria) precisa de uma struct
`#[derive(Deserialize, Serialize, JsonSchema)]` — `ServiceSpec` para este exemplo, em
`bins/delonix-runtime-bin/src/cmd/service.rs` — e um braço em `TYPED_KINDS`, em
`bins/delonix-runtime-bin/src/cmd/schema.rs`. Essa constante alimenta o `delonix manifest schema` e o
`delonix explain <Kind>.<campo>`, os dois gerados da mesma struct (ADR-0007), para o schema
publicado nunca poder divergir do que o código de facto aceita. Deixar um Kind de fora é um
estado real e permitido — o `Storage` e o `ShareVolume` não têm schema de propósito, porque são
reescritos para `Volume` no load e uma segunda struct seria só uma cópia mantida à mão dos campos
que o próprio `Volume` já tem — mas tem de ser *dito*: `untyped_hint(kind)` dá o redireccionamento
específico, e o `todo_kind_conhecido_tem_schema_ou_dica` (em `schema.rs`) falha o build se um Kind
que a tabela conhece não estiver nem em `TYPED_KINDS` nem tiver uma dica. A mensagem genérica
("no typed schema for X") lê-se como um bug do manifesto; a dica diz que é uma propriedade do
Kind.

## Fiar o Kind no reconciliador

O plano do reconciliador (`crates/contexts/delonix-stack/src/reconcile.rs::plan`) é puro — nunca
abre um store nem corre um comando. Tudo o que toca na máquina vive em
`bins/delonix-runtime-bin/src/cmd/stack.rs`, re-exportado como `cmd::kinds`/`cmd::reconcile` a
partir do `delonix-stack` (`pub use delonix_stack::kinds;` em `cmd/mod.rs`, por isso nada que já
chamasse `cmd::kinds::…` teve de mudar quando a tabela passou para o seu próprio crate). Quatro
funções em `stack.rs` precisam de um braço para um Kind novo que converge:

1. **`desired_of`** — um `reconcile::Desired` por documento, com `fields` chaveados pelo nome do
   campo **do manifesto** (`matchLabels`, `port`), nunca pelo nome do registo interno, porque o
   diff é lido por quem escreveu o YAML. `Service::desired` constrói isto a partir de
   `ServiceSpec`.
2. **`actual_of`** — todas as instâncias do Kind que existem na máquina, para o `--prune` ter
   com que comparar. `Service::actual` lê `delonix_sdn::infra::service_list()` e preenche os
   *mesmos* nomes de campo que o `desired` usou, senão o diff compara alhos com bugalhos.
3. **`converge_and_stamp`** — aplica um `Action::Update` a quente. O `Service` reaproveita aqui
   o seu próprio `apply_one` (`converge_doc`), porque essa função já sobrescreve por inteiro a
   entrada do registo — a mesma forma que `FirewallPolicy` e `NetworkAccessRule` usam, e a mesma
   razão: um caminho por-campo separado seria uma segunda maneira de escrever o mesmo registo, e
   duas maneiras começam a discordar. Um Kind sem caminho de actualização ao vivo nenhum (o `Pod`
   não tem campo quente nenhum) devolve o erro explícito "no live update path" em vez de cair em
   silêncio.
4. **`stamp_all`** — regista a posse (label `delonix.io/stack`) e o mapa de campos aplicado
   (anotação `delonix.io/last-applied`) depois de um apply com sucesso, o que é o que transforma
   o plano seguinte num diff de três vias em vez de duas. `Service::stamp` escreve-os
   directamente na própria entrada de registo do serviço — ao contrário do `NetworkAccessRule`,
   cuja regra vive num *container* que não é dele, um `Service` tem um registo inteiramente seu.

E o `destroy_one` precisa de um braço a chamar `remove_for_replace` quando `teardown: true` — o
`Service::remove_for_replace` só chama `delonix_sdn::infra::service_remove`.

**Nada disto é inventado por Kind.** O `run_layers` (também em `stack.rs`) é onde um documento é
de facto *criado* pela primeira vez, uma camada por Kind `in_stack`, pela ordem da tabela
(`layers.run(k::SERVICE, "🧭", || super::service::apply(docs))?`) — essa função `apply(docs)` é a
mesma que o grupo imperativo da CLI já tem, reaproveitada em vez de duplicada.

## Campos que convergem sem recriar

Se `converges: true`, acrescenta uma entrada a `hot_fields` em `reconcile.rs` a nomear
exactamente que campos do manifesto podem mudar *ao vivo*. Esta tabela é uma promessa que o
executor tem de cumprir — listar um campo que o passo de convergência não consegue de facto
aplicar transforma um `Replace` limpo num `Update` que falha a meio, o que o comentário do módulo
chama "estritamente pior do que declarar o replace à cabeça". O `Service` lista
`["matchLabels", "port"]`, porque `service::apply_one` já sobrescreve por inteiro o registo sem
precisar de reiniciar nada — nada num `Service` é frio. Um campo deixado fora de `hot_fields`
continua a aparecer no plano; só força `Action::Replace` (recusado sem `--replace <Kind>/<nome>`)
em vez de `Action::Update`.

Três testes em `cmd/stack.rs` mantêm esta tabela honesta contra a tabela de `kinds.rs` e umas
contra as outras:

- **`as_tres_listas_de_kinds_convergentes_concordam`** — todo Kind marcado `converges: true` tem
  de aparecer na saída de `--fields` (`compared_fields_table`), e vice-versa; e um Kind
  convergente nunca pode carregar a desculpa genérica de `not_converged_reason`.
- **`todo_kind_nao_convergente_tem_razao_especifica`** — o inverso: todo Kind que NÃO converge
  precisa da sua própria frase concreta em `not_converged_reason`, nunca o
  `NOT_CONVERGED_GENERIC` partilhado.
- **`todo_kind_convergente_tem_teardown_ou_razao`** — um Kind convergente ou é removível
  (`kinds::has_teardown`) ou tem uma entrada em `no_teardown_reason`; nunca os dois, nunca
  nenhum.

## Presença, para `ls`/`describe`/`wait`

Acrescenta um braço a `presence(kind, doc, containers)` em `stack.rs` que diga se uma instância
existe e, se sim, a sua string de estado. O braço do `Service` verifica
`delonix_sdn::infra::service_get(namespace, nome)` e, se encontrado, conta quantos containers
vivos o seu selector alcança neste momento (lendo o *mesmo* índice que o próprio resolvedor de
DNS lê, para o plano nunca poder discordar do que um cliente a pedir o nome de facto recebe).
Dois testes guardam esta coluna especificamente:

- **`todo_o_kind_declarativo_de_kinds_tem_braco_no_presence`** — todo Kind cujo `presence` seja
  `Declarative` responde de facto `-` a partir de `presence()` (não o `"?"`/"unsupported kind"
  de recurso, em que o `NetworkRoute` caía antes de ganhar um registo próprio).
- **`um_kind_declarativo_nao_fica_pendente_para_sempre`** — `is_pending("-", kind, estado)` é
  `false` para todo Kind declarativo. O `stack wait` costumava ler `present == "yes"` como o
  único sinal de prontidão, por isso um manifesto com *qualquer* Kind declarativo gastava todo o
  `--timeout` à espera de um marcador que esse Kind nunca pode produzir.

Se o Kind é namespaced (`namespaced != Namespaced::Never`), precisa também de uma entrada em
`NAMESPACE_SOURCES` (`bins/delonix-runtime-bin/src/cmd/complete.rs`) — ou `NsSource::Store(fn)` a
ler o próprio store do Kind, ou `NsSource::Via("OutroKind — razão")` quando o namespace viaja
para o registo de outro Kind (o namespace de um `Pod` está nos seus `Container`s membros, por
exemplo). O `every_namespaced_kind_declares_a_source` falha caso contrário — o efeito prático de
faltar isto é a completação da shell nunca poder oferecer um inquilino cujo único recurso seja o
Kind novo.

## A checklist

Para um Kind que se comporta como o `Service` (primário, converge, tem teardown, namespaced):

1. `kinds.rs` — um `pub const` com o nome, e uma linha `KindFacts`.
2. Uma struct de spec com `JsonSchema`, e um braço em `TYPED_KINDS` (`schema.rs`) — ou uma
   entrada em `untyped_hint` a explicar porque não.
3. `stack.rs` — braços em `desired_of`, `actual_of`, `converge_and_stamp`, `stamp_all`,
   `destroy_one`, `presence`, e uma camada em `run_layers` a chamar o próprio `apply(docs)` do
   Kind.
4. `reconcile.rs` — uma entrada em `hot_fields` a nomear os campos que convergem ao vivo.
5. Se o Kind não converge ou não pode ser desfeito: uma frase específica em
   `not_converged_reason` / `no_teardown_reason` (`stack.rs`).
6. Se namespaced: uma entrada em `NAMESPACE_SOURCES` (`complete.rs`).
7. `cargo test -p delonix-runtime-bin -p delonix-stack` — os testes nomeados acima são o que
   apanha um passo saltado, não um revisor a ler o diff a olho.

Um Kind que seja `Sugar`/`Aggregate` (reescrito ou expandido no load, como o `Workload` ou o
`Stack`) salta a maior parte disto: o `um_kind_que_baixa_para_outro_nao_pertence_ao_ciclo_do_stack`
exige `in_stack: false` e `converges: false` para essas formas, e o trabalho vive antes no
`manifest::load`, onde a reescrita acontece.

---

**A seguir:** [Fluxo de contribuição](contributing-workflow.md) — worktrees, alinhamento de versão, a regra de língua, quando escrever um ADR, e como enviar a mudança.
