<!-- translated-from: 12-coding-conventions.md sha256:43d981abb9a2261c0d996d221aa5ef0d44c29b0f981f6310e2696432ee29c195 -->
# 12. Convenções de código

Esta página diz-te como escrever código que passa a revisão neste repositório, para não teres de
adivinhar as regras nem de inventar as tuas. Cada regra abaixo leva uma etiqueta e uma fonte:

- **Imposto (gate)**: um job de CI falha se a quebrares. O gate é nomeado, para o poderes correr
  localmente (vê [02 — Construir e testar](02-build-and-test.md#the-gates-ci-runs)).
- **Decidido (ADR/AGENTS.md)**: um Architecture Decision Record **Aceite** em `docs/adr/` ou uma
  secção do `AGENTS.md` resolve-o. Nenhum gate o verifica ainda, por isso verifica-o a revisão.
- **Proposto (ADR, ainda não decidido)**: a única fonte escrita é um ADR cujo estado ainda é
  **Proposto** (vê a coluna de estado de [`docs/adr/README.md`](../../adr/README.md)). É a direcção
  para onde o código caminha, e a revisão aplica-o, mas ainda pode mudar; quando o ADR for aceite ou
  rejeitado, a etiqueta muda com ele.
- **Convenção (observada)**: o código fá-lo de forma consistente e nada o escreveu. Os exemplos são
  referências `path:symbol` reais. Copia-os.

Se uma pergunta não tiver resposta aqui, e o código à volta da tua mudança também não lha der, a
resposta honesta é **«não decidido — segue o código à volta»**. A lista das perguntas em aberto
conhecidas está em [Por decidir](#undecided). Não transformes uma preferência pessoal numa regra
num PR.

Páginas relacionadas: as camadas e as razões por trás delas em [05 — Arquitectura](05-architecture.md),
o que cada crate contém em [06 — Crates](06-crates.md), os idiomas de Rust em
[03 — Introdução ao Rust](03-rust-primer.md), e o fluxo de trabalho (worktree, versão, ADRs, PRs) em
[10 — Fluxo de contribuição](10-contributing-workflow.md).

---

## 1. As ferramentas que impõem o estilo

| Ferramenta | O que verifica | Etiqueta e fonte |
|---|---|---|
| **rustfmt** | Formatação. **Sem ficheiro de configuração**: não há `rustfmt.toml` nem `.rustfmt.toml` na raiz, por isso aplicam-se as omissões. | **Imposto (gate)**: o job de CI `fmt` corre `cargo fmt --all --check` (`.github/workflows/ci.yml`). `CONTRIBUTING.md` § Style: «`cargo fmt` defaults, no custom config». |
| **clippy** | Todo o lint do clippy **e todo o aviso do rustc** falham o build, testes incluídos (`--all-targets`). Sem `clippy.toml`, por isso aplicam-se as omissões dos lints. | **Imposto (gate)**: o job de CI `clippy` corre `cargo clippy --workspace --all-targets --locked -- -D warnings`. |
| **`[workspace.lints]`** | O `Cargo.toml` raiz declara exactamente um lint de workspace: `[workspace.lints.clippy] undocumented_unsafe_blocks = "deny"`. Todo o manifesto de membro tem `[lints] workspace = true`. | **Imposto (gate)**: clippy. Foi introduzido pela fase P0 do ADR-0040 («`[workspace.lints]` (`undocumented_unsafe_blocks = deny`)»), um ADR que ainda está **Proposto**; o gate vale na mesma. |
| **cargo-deny** | Avisos RUSTSEC e crates retirados (yanked), só licenças permissivas (sem GPL/AGPL), crates.io como único registo, sem fontes git. A secção `[bans]` do `deny.toml` (versões duplicadas e wildcards, em modo aviso) **não** é avaliada na CI. | **Imposto (gate)**: o job de CI `deny` corre `check advisories licenses sources` com o `deny.toml`. **Convenção (observada)**: todo o aviso ignorado no `deny.toml` leva um comentário com a sua razão — nenhum gate verifica que o comentário existe. |
| **`scripts/lang_ratchet.py`** | Português em identificadores, comentários e strings visíveis ao utilizador (LANG-01, vê [§2](#2-language-of-the-code)). | **Imposto (gate)**: job de CI `lang`, linha de base em `scripts/lang_baseline.json`. |
| **`scripts/arch_fitness.py`** | Direcção das camadas, directório do crate = camada, versões só na raiz, nomes de consumidores, e os ratchets de dívida listados em [05](05-architecture.md#layers-and-the-allowed-direction) (vê [§4](#4-structure-where-code-goes) e [§5](#5-internal-api-and-boundaries)). | **Imposto (gate)**: job de CI `arch`, linha de base em `scripts/arch_baseline.json`. |
| **`scripts/contract_gate.py`** | O contrato de nó em `proto/delonix/node/v1` (vê [§5.4](#54-the-node-contract)). | **Imposto (gate)**: job de CI `contract`. |

Tem presente uma mecânica dos **ratchets** (`lang_ratchet.py` e os números do
`arch_fitness.py`). Um ratchet falha quando o seu número **sobe**, e **também** falha quando o
número desce sem a linha de base ser baixada no mesmo commit (o `arch_fitness.py` imprime «debt was
paid; lower the baseline in the same commit (--update)»). Se pagares dívida, corre `--update` e faz
commit da nova linha de base juntamente com a correcção. Uma verificação `<=` deixaria a dívida a
ler-se como verde para sempre (docstrings de módulo dos dois scripts).

Uma consequência com que vais dar: **Imposto (gate)**. Como o `-D warnings` cobre os lints do
próprio rustc, os lints de nomes do rustc (`non_snake_case`, `non_camel_case_types`,
`non_upper_case_globals`) também são impostos. Funções e variáveis em `snake_case`, tipos em
`UpperCamelCase` e constantes em `SCREAMING_SNAKE_CASE` não são aqui uma escolha de estilo.

Existe `#[allow(clippy::…)]` na árvore, por exemplo `#[allow(clippy::too_many_arguments)]` em
`crates/contexts/delonix-compute/src/run.rs` e `crates/contexts/delonix-compute/src/pod.rs`.
**Não decidido**: nenhuma regra diz quando um `allow` é aceitável. Se acrescentares um, põe um
comentário `// why` ao lado, como farias para qualquer outra excepção (vê [§10](#10-comments-and-documentation)).

---

## 2. Língua do código

- **Identificadores, comentários e mensagens escrevem-se em inglês.** **Imposto (gate)**:
  `scripts/lang_ratchet.py`. **Decidido**: AGENTS.md § «Língua do código: inglês (LANG-01)». O
  ratchet percorre todo o ficheiro `.rs`, `.py`, `.ts`, `.go` e `.yml`/`.yaml` fora de `target/`,
  `third_party/` e semelhantes. Conta três coisas:
  - **identificadores**: qualquer nome declarado por `fn`, `struct`, `enum`, `trait`, `const`,
    `static`, `type`, `mod`, `union` ou `let` cujos segmentos `snake_case`/`camelCase` contenham uma
    palavra de `scripts/lang_pt_lexicon.txt`;
  - **comentários**: linhas `//`, `///` e `//!` que contenham uma palavra do léxico;
  - **texto ao utilizador**: literais de 8 ou mais caracteres dentro de `format!`, `println!`,
    `eprintln!`, `panic!`, `anyhow!`, `bail!`, `expect` ou `unimplemented!`.
- **O português chega ao operador só pelo catálogo.** **Decidido**: AGENTS.md §
  «i18n (fonte EN + catálogo pt.po embutido)» e `CONTRIBUTING.md` § «If you add or change a CLI
  command». O texto em inglês vai no código-fonte. O português vai em
  `bins/delonix-runtime-bin/data/pt.po`, que `bins/delonix-runtime-bin/src/cmd/po.rs` embute com
  `include_str!`.
  - `po::t("…")` para uma string fixa.
  - `po::tf("… {name} …", &[("name", value)])` para texto interpolado. Usa placeholders
    **nomeados**: uma tradução pode reordená-los, e o `format!` precisa de um literal em tempo de
    compilação, por isso o template é traduzido primeiro e os valores são substituídos depois
    (`po.rs`, doc comment do `tf`).

    ```rust
    // bins/delonix-runtime-bin/src/cmd/config.rs — refuse_unknown_key
    Err(Error::Invalid(super::po::tf(
        "'{key}' is not a config key — known: {known}",
        &[("key", key), ("known", &KNOWN_KEYS.join(", "))],
    )))
    ```
  - O texto do `--help` também está em inglês no código-fonte, traduzido em runtime por
    `po::translate_help`. **Imposto (gate)**: `help_i18n_tests` em
    `bins/delonix-runtime-bin/src/main.rs`. `todo_o_help_de_comando_tem_traducao_pt` é estrito para
    o help dos comandos, e `o_help_dos_argumentos_so_pode_encolher` é um ratchet sobre
    `ARG_HELP_PENDING` para o help das flags. Um comando ou flag novo precisa de uma entrada no
    `pt.po`.
  - Uma entrada em falta cai para o inglês, por isso a UI nunca fica em branco (`po::t`). Uma
    string em português escrita directamente no código é um bug, não um atalho.
  - Não reutilizes um `msgid` cujo português dependa do género gramatical do sujeito. «created»
    pode ser *criada* para uma rede e *criado* para um volume, por isso usa chaves separadas.
    **Decidido**: AGENTS.md § «v0.32.2 — 380+ strings PT hardcoded».
- **Armadilhas do léxico.** **Decidido**: AGENTS.md § LANG-01.
  - **Não chames `num` a nada.** Está no léxico de propósito, porque apanha português real
    («num apply falhado»). Um identificador `num` conta por isso como dívida portuguesa e falha o
    gate. Usa `count` ou `number`.
  - Acrescentar uma palavra ao léxico **sobe** a contagem e falha o gate. Baixa a linha de base no
    mesmo commit. Os homógrafos (`data`, `base`, `no`, `nas`…) ficam de fora, a menos que uma
    medição mostre que apanham português real; `num` é a excepção registada (AGENTS.md § LANG-01).

---

## 3. Nomes

### 3.1 Crates e directórios

- **O directório é a camada.** Um crate vive em `crates/foundation/`, `crates/contexts/`,
  `crates/adapters/`, `crates/providers/` ou `crates/interfaces/`, ou é um binário em `bins/`. O
  directório tem de bater com a entrada do crate na tabela `LAYERS`. **Imposto (gate)**:
  `scripts/arch_fitness.py` `misplaced()` / `LAYER_DIR`. Um crate novo entra na `LAYERS` **e** no
  directório certo no mesmo commit.
- **Convenção de nomes alvo para crates novos ou reestruturados.** **Proposto (ADR, ainda não
  decidido)**: ADR-0040 D2.1.

  | Papel | Nome |
  |---|---|
  | Fundação pura partilhada | `delonix-model` |
  | Contexto delimitado (bounded context) | `delonix-<context>`, com o nome do grupo de API publicado (D2.2): `delonix-compute`, `delonix-stack` |
  | Adapter de tecnologia | `delonix-<technology>`: `delonix-linux`, `delonix-sdn`, `delonix-oci` |
  | Provider conectável | `delonix-provider-<technology>` |
  | Biblioteca de interface | `delonix-<protocol>`: `delonix-cri`, `delonix-mcp` |

  Um crate só existe se for um contexto delimitado, se isolar uma dependência pesada ou
  privilegiada, ou se for um binário instalado à parte. **Sem sufixos `-core`, `-common`, `-utils`
  ou `-types`** (o ADR-0040 D2.1 regista como um crate `-core` se tornou «the sink of everything»).
  Alguns crates ainda têm nomes antigos: `delonix-runtime-core`, `delonix-proxmox`,
  `delonix-truenas`, `delonix-security-runtime`. O ADR-0040 renomeia cada um na fase que o
  reestrutura, «never twice». **Não renomeies um crate fora da sua fase.**
- **O caminho de cada crate escreve-se uma vez**, em `[workspace.dependencies]` do `Cargo.toml`
  raiz. Os membros dependem uns dos outros com `{ workspace = true }`. **Decidido**: AGENTS.md §
  «A direcção das dependências é um portão (ADR-0040, fase P0)», e o comentário no topo de
  `[workspace.dependencies]`.

### 3.2 Módulos e ficheiros

- **Grupos de comandos da CLI**: um módulo por grupo em `bins/delonix-runtime-bin/src/cmd/<group>.rs`,
  com um `pub enum <Group>Cmd` (subcomandos clap), um despachante
  `pub fn run(action: <Group>Cmd) -> Result<()>`, e uma função `cmd_<verb>` por subcomando.
  **Convenção (observada)**: `cmd/volume.rs:VolumeCmd` + `run` + `cmd_create`/`cmd_ls`/`cmd_describe`;
  `cmd/secret.rs:SecretCmd` + `run`; `cmd/container.rs:cmd_run`/`cmd_start`/`cmd_stop`.
  O AGENTS.md § «CLI (`delonix`)» enuncia a parte de um módulo por grupo.
- **Adapters de uma porta de compute** vão num ficheiro com o nome da preocupação da porta, não da
  tecnologia: `crates/adapters/delonix-linux/src/workload.rs`, `.../run_host.rs`,
  `crates/adapters/delonix-sdn/src/run_network.rs`, `.../vm_network.rs`,
  `crates/adapters/delonix-oci/src/run_images.rs`. **Convenção (observada)**.

### 3.3 Tipos, traits, funções, constantes

- **As portas têm o nome da capacidade, não da tecnologia.** **Proposto (ADR, ainda não
  decidido)**: o ADR-0040 D3 enumera as portas: `WorkloadRuntime`, `SandboxProvider`, `VmProvider`,
  `NetworkProvider`, `StorageProvider`, `ImageRegistry`, `ImageStore`, … As que existem hoje estão
  em `crates/contexts/delonix-compute/src/ports.rs` (`ImageStore`, `StorageProvider`,
  `DeviceResolver`, `RunHost`, `VmNetwork`, `NetworkProvider`) e em `.../launch.rs`
  (`WorkloadRuntime`). O `VmBackend` mais antigo em `crates/adapters/delonix-vm/src/lib.rs` deve
  passar a `VmProvider` na P4.
- **As implementações de host de uma porta chamam-se `Host<Thing>`.** **Convenção (observada)**:
  `delonix-linux/src/workload.rs:HostWorkload` (implementa `WorkloadRuntime`),
  `delonix-sdn/src/run_network.rs:HostNetwork` (implementa `NetworkProvider`),
  `delonix-oci/src/run_images.rs:HostImages`, `delonix-volume/src/lib.rs:HostVolumes`,
  `delonix-linux/src/cdi.rs:HostDevices`.
- **Funções de decisão puras** são verbos ou perguntas: `resolve_*`, `parse_*`, `valid_*`, `is_*`,
  `*_plan`. **Convenção (observada)**: `cmd/vm.rs:resolve_vm_defaults`,
  `delonix-oci/src/registry.rs:parse_content_range`, `cmd/stack.rs:is_pending`,
  `delonix-net-rules/src/lib.rs:bridge_name`.
- **As constantes** são `SCREAMING_SNAKE_CASE` (**Imposto (gate)**, lint do rustc sob o clippy
  `-D warnings`). **Os nomes de Kind também são constantes, nunca literais de string repetidos**
  (**Decidido**: AGENTS.md § «Os Kinds ganham grupos e nomes definitivos»; as constantes estão em
  `crates/contexts/delonix-stack/src/kinds.rs`: `pub const VM: &str = "VirtualMachine";`).

### 3.4 Nomes de testes

- **O ratchet conta os nomes de testes.** O `lang_ratchet.py` casa toda a declaração `fn` e não
  salta `#[cfg(test)]`, por isso um nome de teste em português sobe `identifiers` e falha o gate.
  **Imposto (gate)**.
- Muitos testes existentes têm nomes de frase em português, por exemplo
  `delonix-model/src/exitcode.rs:nao_existe_e_rebentou_deixam_de_ser_o_mesmo_numero`. Isso é dívida
  contada, não um estilo a copiar. Os testes novos são **frases em inglês que enunciam o
  comportamento que se prova**, como os testes mais recentes do mesmo ficheiro:
  `a_missing_capability_is_not_a_wrong_argument` e `the_text_class_and_the_number_cannot_diverge`.
  **Decidido**: LANG-01 (AGENTS.md); a forma de «frase» é uma **Convenção (observada)**.
- Se traduzires o nome de um teste existente, a contagem desce, por isso corre
  `python3 scripts/lang_ratchet.py --update` no mesmo commit.

### 3.5 Comandos e flags da CLI

- **Comandos agrupados, `delonix <group> <verb>`.** Sem atalhos planos de topo. **Decidido**:
  AGENTS.md § «Reorganização da raiz da CLI (v0.30.0)»; `docs/cli-stability.md` (os atalhos de
  topo foram removidos na v1.0.0).
- **Os verbos seguem o Docker/Podman/kubectl** quando esse verbo existe. **Decidido**: AGENTS.md §
  os sprints de «Reestruturação da CLI (semântica Docker/Podman/kubectl)»; `docs/cli-stability.md`
  § «Estável».
  - Os verbos de listagem usam `ls` (`network ls`, `volume ls`, `image ls`…). O `image list` voltou
    a `ls` na v2.0.0 (`docs/cli-stability.md`).
  - `create` só cria, e recusa um nome existente com exit 5 a menos que haja `--force`. O upsert é
    um verbo à parte (`secret set`). `apply` é o idempotente «garante presente». **Decidido**:
    AGENTS.md § «Sprint 1: `secret create` vs `secret set`».
  - `describe` é para humanos (estilo kubectl), `inspect` é JSON para scripts. **Decidido**:
    AGENTS.md § «Output: `ls` estilo docker, `describe` estilo kubectl».
  - A ordem e os nomes das flags copiam o Docker onde o Docker tem o conceito:
    `network connect <NETWORK> <CONTAINER>`, `-p [hostIp:]hostPort:containerPort`,
    `volume create --driver … --opt k=v`. **Decidido**: AGENTS.md § Sprints 5 e 6.
- **As mudanças incompatíveis são cortes limpos, sem aliases.** A grafia antiga tem de falhar com
  `unrecognized subcommand`, nunca fazer outra coisa em silêncio. Antes de cortares, faz grep dos
  chamadores internos em **todo** o workspace. **Decidido**: `docs/cli-stability.md` § «Como uma
  quebra é feita». Os grupos listados como estáveis nesse ficheiro só podem quebrar numa release
  major.
- **Um comando alcançável por vários caminhos tem de estar ligado em todos eles** (por exemplo
  `vm pull` / `image vm pull` / `image --vm pull`). **Decidido**: `CONTRIBUTING.md`; vê
  [10](10-contributing-workflow.md#adding-or-changing-a-cli-command).
- **As mudanças de folhas actualizam a linha de base da CLI** (`scripts/cli-tree.sh --update`) no
  mesmo commit. **Imposto (gate)**: vê [10](10-contributing-workflow.md#adding-or-changing-a-cli-command).

### 3.6 Kinds, grupos de API e campos de manifesto

- **Os Kinds são substantivos `UpperCamelCase`** num dos grupos publicados
  `core`, `compute`, `networking`, `gateway`, `storage`, `artifact`, `infrastructure`
  (`<group>.delonix.io/v1alpha1`). **Decidido**: AGENTS.md § «Identidade e fronteira do
  motor» e § «Os Kinds ganham grupos» (o ADR-0020, que introduziu os grupos, ainda está
  **Proposto**). Cada Kind é **uma linha** em
  `crates/contexts/delonix-stack/src/kinds.rs` (`KindFacts`: `kind`, `plural`, `short`,
  `api_version`, `domain`, `form`, `in_stack`, `converges`, …). O `delonix api-resources` imprime
  essa tabela. Acrescentar um Kind também mexe em tabelas que nada deriva do `kinds.rs`
  (`hot_fields`, `NAMESPACE_SOURCES`, `TYPED_KINDS`, o schema gerado). Os testes falham até cada
  uma estar feita. **Imposto (gate)**: o AGENTS.md § «`kind: Service`» lista que teste apanhou cada
  tabela.
- **Um Kind renomeado mantém o nome antigo como alias silencioso e insensível a maiúsculas.** Uma
  **fusão** avisa, porque o significado mudou. **Decidido**: AGENTS.md § «Os Kinds ganham grupos e
  nomes definitivos» («Alias silencioso, não depreciação»); implementado em
  `cmd/manifest.rs:KIND_ALIASES`. O ADR-0020 ainda está **Proposto**.
- **Os campos de manifesto são `camelCase`.** Se um campo tinha antes uma grafia `snake_case`, essa
  grafia continua aceite como `alias` do `serde`. **Convenção (observada)**, campo a campo em vez de
  `rename_all`:

  ```rust
  // bins/delonix-runtime-bin/src/cmd/vm.rs — VmSpec
  /// Canonical `cpuAffinity`; `cpu_affinity` stays accepted (back-compat).
  #[serde(rename = "cpuAffinity", alias = "cpu_affinity")]
  cpu_affinity: Option<String>,
  ```

  Outros exemplos: `delonix-compute/src/pod.rs:PodSpec.restart_policy` (`rename = "restartPolicy"`),
  `cmd/service.rs:ServiceSelector.match_labels` (`rename = "matchLabels"`). O schema publicado
  (`docs/schema/v1/delonix.json`) é **gerado** a partir destas structs e há testes a exigir que bata
  com elas (ADR-0007). O schema dos manifestos está declarado **estável**
  (`docs/cli-stability.md` § «O schema dos manifestos»).
- **Os registos internos** (o JSON debaixo do state root) mantêm os nomes de campo `snake_case` do
  Rust. Vê `crates/foundation/delonix-runtime-core/src/lib.rs` (`net_mode`, `namespace`).
  **Convenção (observada)**.

### 3.7 Variáveis de ambiente

- **Prefixo `DELONIX_`**, em maiúsculas: `DELONIX_ROOT`, `DELONIX_NET_RUNTIME_DIR`, `DELONIX_L18N`,
  `DELONIX_LOG_FORMAT`, `DELONIX_CRI_CAP_CEILING`. **Convenção (observada)** em `crates/` e
  `bins/`.
- **Para a telemetria, lê as variáveis padrão `OTEL_*`**, não um alias `DELONIX_*` novo.
  **Proposto (ADR, ainda não decidido)**: ADR-0040 D6. Isto é o alvo, não o código de hoje:
  `crates/adapters/delonix-telemetry/src/telemetry.rs` lê `DELONIX_OTLP_ENDPOINT` para o exporter
  OTLP, e do conjunto padrão só `OTEL_SERVICE_NAME`. Não acrescentes uma variável de telemetria
  `DELONIX_*` nova, e não removas `DELONIX_OTLP_ENDPOINT` fora da fase que a migra.
- **Uma escapatória que enfraquece uma omissão de segurança é ruidosa e explícita.** Está desligada
  a menos que valha `1`, e avisa: `DELONIX_ENABLE_IPV6=1`, `DELONIX_ALLOW_LINK_LOCAL=1`.
  **Decidido**: AGENTS.md § «Bloco 0 do plano 33 (v0.37.1)».
- Uma flag ganha à variável de ambiente, que ganha à omissão (`serve cri --cap-ceiling` vs
  `DELONIX_CRI_CAP_CEILING`). **Decidido**: AGENTS.md § «Tecto de capabilities no CRI».

### 3.8 Códigos de saída e códigos `DX_*`

- **Os códigos de saída derivam do tipo de erro num só sítio**: o exaustivo
  `crates/foundation/delonix-model/src/exitcode.rs:for_error`. As classes são 1 genérico,
  2 uso, 3 `NOT_RUNNING`, 4 `NOT_FOUND`, 5 `CONFLICT`, 69 `UNAVAILABLE`, 74 `IO`,
  77 `NO_PERMISSION`, 124 `TIMEOUT`. Cada erro tem também uma identidade textual estável,
  `Error::code()`, que devolve uma string `DX_*`. **Imposto (gate)**: o match não tem braço `_ =>`,
  por isso uma variante nova pára o build até alguém a classificar. O teste
  `the_text_class_and_the_number_cannot_diverge` mantém os dois em sintonia. **Decidido**: AGENTS.md
  § «Códigos de saída com classe (v0.49.0)»; `docs/cli-stability.md` § «Códigos de saída».
- **Não escolhes um número, devolves a variante certa.** «Não existe» é `Error::NotFound`, «já
  existe» é `Error::Conflict`, «falta uma ferramenta a este host» é `Error::Unavailable`. Um número
  novo precisa de um produtor real. **Decidido**: docs de módulo do `exitcode.rs` («every extra
  number is a promise»).

---

## 4. Estrutura: onde vai o código

### 4.1 As camadas e a direcção

A direcção permitida está escrita num só sítio, `ALLOWED` em `scripts/arch_fitness.py`.
**Imposto (gate)**:

| Camada | Pode depender de |
|---|---|
| foundation | foundation |
| context | foundation, context |
| adapter / provider | foundation, context |
| interface | foundation, context, adapter, provider |
| bin | tudo |

As dependências de desenvolvimento e de build não contam. Uma excepção declarada tem de nomear a
fase do ADR-0040 que a remove, e uma excepção que já não se aplica também falha (`EXCEPTIONS`). A
tabela de camadas gerada e as excepções actuais estão em [05](05-architecture.md#layers-and-the-allowed-direction).

Mais regras estruturais, cada uma **Imposto (gate)** pelo `scripts/arch_fitness.py`:

- **A fundação e os contextos ficam livres de dependências pesadas.** `tokio`, `axum`, `hyper`,
  `tonic`, `reqwest`, `clap`, `ratatui`, `serde_yaml`, `rmcp`, OpenTelemetry e `prometheus-client`
  são recusados aí (`HEAVY`).
- **Um binário compõe exactamente uma interface.** Imposto em `rule_failures()` (introduzido pelo
  ADR-0040 D2.4, ainda **Proposto**; também escrito no AGENTS.md § «A direcção das dependências é
  um portão»).
- **As versões das dependências vivem só no `[workspace.dependencies]` raiz.** Um membro escreve
  `{ workspace = true, features = [...] }` e nada mais. O `default-features = false` fica na raiz,
  porque um membro não consegue desligar o que a raiz liga (`inline_versions()`).
- **As bibliotecas não imprimem.** O ratchet `library_prints` conta `println!`/`eprintln!`/`print!`
  fora de `bins/`. Emite antes `tracing` (por exemplo `tracing::warn!` em
  `crates/adapters/delonix-sdn/src/lib.rs`) e deixa a interface apresentar a saída.
  **Decidido**: AGENTS.md § «A direcção das dependências é um portão (ADR-0040, fase P0)» («Uma
  biblioteca não escreve para o terminal; emite `tracing`»).
- **As bibliotecas não voltam a correr o binário do próprio motor.** O ratchet `self_exec_sites`
  conta `current_exe()`, `cli_bin()` e `delonix_bin()` fora de `bins/`. Chama antes uma função ou
  um caso de uso. `Command::new("ip")`, `nft`, `qemu-img` e `ssh` **não** são contados, porque
  correr essas ferramentas é exactamente para o que um adapter existe (comentário acima de
  `SELF_EXEC`).
- **Não escrevas no ambiente do processo.** O ratchet `env_writes` conta `env::set_var`/`remove_var`
  em todo o lado, testes incluídos. Os testes correm em threads paralelas e uma escrita entra em
  corrida com todos os leitores. Passa antes os valores como argumento (comentário acima de
  `ENV_WRITES`).

### 4.2 «A minha mudança é X → vai para Y»

Esta tabela usa os crates tal como existem hoje. Consulta [06](06-crates.md) para ver o conteúdo de
cada crate antes de lhe acrescentares algo. Onde a coluna de fonte cita o ADR-0040 ou o ADR-0026, a
etiqueta é **Proposto (ADR, ainda não decidido)**: os dois ADRs ainda estão Propostos, embora os
crates que descrevem já existam.

| A tua mudança | Crate (camada) | Fonte |
|---|---|---|
| Uma regra pura sobre CIDRs, nomes de bridge ou aritmética de IPAM que os dois lados têm de calcular de forma idêntica | `delonix-net-rules` (foundation) | 06; AGENTS.md § Arquitetura |
| Uma classe de erro, código de saída ou código `DX_*` nova; nomes gerados | `delonix-model` (foundation) | ADR-0040 D1 |
| Um tipo de registo persistido (`Container`, `Vm`, …) | `delonix-runtime-core` (foundation) — o que sobrou da divisão da P3 do ADR-0040. Acrescenta aqui só o que pertence a registos, nunca helpers gerais | ADR-0040 D2.1 «no `-core`» |
| Uma regra pura do modelo de segredos (`Secret`, nomes e chaves válidos, análise de env-file) | `delonix-model` (foundation): `secret.rs` | ADR-0040 P3 (o PR que moveu os stores) |
| Um store, o lock de ficheiro, `write_atomic*`/`write_private_temp`, o store de segredos cifrado ou o cofre de credenciais | `delonix-state` (adapter) | ADR-0040 D2.3 |
| Factos de Kind, o planeador/diff, condições, revisões | `delonix-stack` (context) | AGENTS.md § Arquitetura |
| A especificação de execução, a sua validação pura, uma porta de que o caso de uso de run precisa | `delonix-compute` (context): `run_opts.rs`, `preflight.rs`, `ports.rs` | ADR-0040 D2.2 |
| Política de segurança, admissão, score, redacção | `delonix-security-runtime` (context) | ADR-0026 |
| Namespaces, cgroups, mounts, capabilities, seccomp, dispositivos | `delonix-linux` (adapter) | ADR-0040 D2.3 |
| netns holder, nftables, slirp, DNS, DHCP, overlay, WireGuard, CNI | `delonix-sdn` (adapter) | ADR-0040 D2.3 |
| Cliente de registo, CAS, layers, overlay, build de imagens | `delonix-oci` (adapter) | ADR-0040 D2.3 |
| SBOM / CVE | `delonix-scanner` (adapter) | ADR-0040 D2.3 |
| Tracing, OpenTelemetry, configuração do registo Prometheus | `delonix-telemetry` (adapter) | ADR-0040 D2.3 |
| Um backend de VM local (Cloud Hypervisor, libvirt) | `delonix-vm` (adapter) | ADR-0008 |
| Um provider remoto ou conectável (API de hypervisor, API de NAS) | um crate de provider em `crates/providers/`. **Escreve primeiro um ADR** | ADR-0008, ADR-0009; [10](10-contributing-workflow.md#when-to-write-an-adr) |
| Um RPC do CRI | `delonix-cri` (interface) | AGENTS.md |
| A API de gestão local, `/metrics` | `delonix-mgmt` (interface) | ADR-0010 |
| Uma tool MCP | `delonix-mcp` (interface) | ADR-0025 |
| Um comando da CLI, a sua apresentação e traduções | `bins/delonix-runtime-bin/src/cmd/<group>.rs` + `data/pt.po` | AGENTS.md § CLI |
| Que adapter serve que porta (composição) | a raiz de composição do binário. Sem lógica de negócio aí | ADR-0040 D1 «Binaries» |

### 4.3 Núcleo puro, I/O nas pontas

- **As decisões são funções puras sobre dados já lidos.** Não recebem store, não correm comandos e
  não precisam de privilégio, por isso um teste pode chamá-las com valores simples. **Proposto (ADR,
  ainda não decidido)**: ADR-0040 D1 (o `domain/` de um contexto não tem «no I/O, no `tokio`,
  `libc`, `nix`, `std::fs`»). **Convenção (observada)**: `delonix-stack/src/reconcile.rs` («decides
  it WITHOUT touching the machine»), `cmd/vm.rs:resolve_vm_defaults`,
  `delonix-sdn/src/infra.rs:vmtap_line`, `delonix-oci/src/registry.rs:parse_content_range`.

  ```rust
  // bins/delonix-runtime-bin/src/cmd/stack.rs — is_pending
  fn is_pending(present: &str, kind: &str, status: &str) -> bool {
      match present {
          // Declarative: nothing to observe, so nothing to wait for.
          "-" => false,
          "yes" => !ready_status(kind, status),
          // "no" (absent) and "?" (unknown/unreadable) both keep waiting.
          _ => true,
      }
  }
  ```

- **Se uma função pura precisar de algo de fora, recebe-o como parâmetro.** Por exemplo,
  `resolve_image_ref` recebe o store de imagens em vez de abrir o real, para o teste poder passar
  um directório temporário. **Decidido**: AGENTS.md § «O manifesto de VM resolvia a imagem de outra
  maneira que a CLI».
- **Uma regra, um dono.** Quando dois pontos de chamada precisam da mesma derivação, extrai uma
  função e chama-a dos dois. Uma segunda cópia deriva. Exemplos: `delonix_net_rules::bridge_name`,
  reexportada pelo `delonix-sdn` (o nome da bridge tinha duas fórmulas e imprimia um dispositivo
  que não existia), `infra::dhcp_lease_ip`, `effective_entrypoints`. **Decidido**: AGENTS.md §
  «`delonix network`», § «Isolamento de namespace», § «Reverse-proxy L7».

---

## 5. API interna e fronteiras

### 5.1 Portas e adapters

- **Um backend novo implementa uma porta. Nunca é um `if provider == …` noutro sítio qualquer.**
  **Decidido**: AGENTS.md § «Identidade e fronteira do motor» («Um provider novo entra como
  implementação de uma porta, nunca como um `if provider == …`»). O ADR-0040 D3 regra 3 («No string
  matching on provider names outside the composition root») reafirma-o e ainda está **Proposto**.
  A D3 planeia um fitness test para isto, mas **ainda não existe no `arch_fitness.py`**, por isso
  por agora é a revisão que o impõe.
- **O conhecimento específico de um backend vive no backend.** Por exemplo,
  `VmBackend::ip_is_predicted()` responde se o IP de uma VM foi previsto, em vez de o ponto de
  chamada verificar `backend.contains("cloud-hypervisor")`. **Decidido**: ADR-0008, citado no doc
  comment em `crates/adapters/delonix-vm/src/lib.rs`.
- **Um adapter não depende de outro adapter.** O que precisa de outra preocupação entra como hook ou
  como porta, ligado pela raiz de composição. **Imposto (gate)**: `ALLOWED` (adapter → foundation,
  context). **Convenção (observada)**: o doc comment de
  `delonix-linux/src/workload.rs:HostWorkload` explica os seus hooks `addresses`/`attach_slirp`
  desta forma.

  ```rust
  // crates/contexts/delonix-compute/src/ports.rs — NetworkProvider (excerpt)
  pub trait NetworkProvider {
      /// Refuses a network that does not exist.
      fn check_network(&self, name: &str) -> Result<()>;
      /// Undoes an attach; best effort, used on the way out of a failure.
      fn detach(&self, id: &str, ip: &str);
      /// Publishes one `-p` specification on a container's address.
      fn publish(&self, ip: &str, spec: &str) -> Result<()>;
  }
  ```

- **Um trait precisa de um consumidor real quando entra.** Não escrevas andaimes à espera do
  primeiro chamador. Todo o método tem de ter um chamador. **Decidido**: AGENTS.md § «`delonix
  workload`» (ADR-0002). Uma função pública sem chamadores é também um perigo conhecido: várias
  revelaram esconder um bug latente (`mount_live`, `set_net_rate`, `update_limits`,
  `publish_port_allow`), e algumas foram apagadas em vez de ligadas (AGENTS.md § «Endurecimento do
  ingress/egress»).

### 5.2 Erros por crate (ADR-0040 P3)

- **Um adapter ou provider define o seu próprio `Error` e converte-o na classe partilhada**
  (`delonix_model::Error`), que carrega o código `DX_*`. **Proposto (ADR, ainda não decidido)**:
  ADR-0040 P3.
  **Imposto (gate)**: o ratchet `shared_error_imports` no `arch_fitness.py` (`SHARED_ERROR`,
  limitado a `crates/adapters/` e `crates/providers/`) conta os imports
  `use delonix_runtime_core::{…Error/Result…}` ou `use delonix_model::{…}` que tornam o tipo
  partilhado o tipo de resultado do próprio crate. O tipo partilhado ainda pode ser nomeado dentro
  de um impl `From`. A implementação de referência é `crates/adapters/delonix-scanner/src/error.rs`:

  ```rust
  impl From<Error> for Dx {
      fn from(e: Error) -> Self {
          match e {
              e @ (Error::EmptySbom | Error::OsvShape | /* … */ Error::NoModule) => Dx::Invalid(e.to_string()),
              Error::ModuleScan(io) => Dx::Runtime { context: "module scan", message: io.to_string() },
              Error::Engine(e) => e,
          }
      }
  }
  ```

  Três coisas nesse ficheiro são o padrão: a conversão **decide** a classe; o `code()` pergunta à
  conversão em vez de manter uma segunda tabela; e um teste
  (`the_code_is_the_code_of_the_class_it_converts_into`) mantém os dois em sintonia. A mensagem
  convertida também se mantém byte a byte idêntica ao que a CLI imprimia antes
  (`the_converted_message_is_the_one_printed_before`).
- **Pergunta ao erro a sua classe; não faças match numa variante do erro partilhado.** Fora da
  fundação, escreve `e.is_not_found()` ou `e.class()`, ou faz match em `e.root()` quando precisas
  do conteúdo — nunca `Err(Error::NotFound(_))` nem `matches!(…, Error::NotFound(_))`. O erro
  próprio de um crate viaja dentro da classe partilhada com o seu código, por isso um match numa
  variante deixa de o apanhar sem uma palavra do compilador. **Decidido**: ADR-0043 D4 (Accepted).
  **Imposto (gate)**: o ratchet `raw_error_variant_matches` do `arch_fitness.py`
  (`RAW_VARIANT_MATCH`, que salta `crates/foundation/delonix-model/`), com linha de base 0 —
  qualquer match novo deste tipo faz falhar o CI. Os métodos estão em
  `crates/foundation/delonix-model/src/codes.rs`.

### 5.3 Visibilidade

- **Privado por omissão.** Dentro do crate da CLI, usa `pub(crate)` para algo que outro módulo
  `cmd` precise de chamar. **Convenção (observada)**: `cmd/container.rs:cmd_run`, `cmd_start` e
  `cmd_stop` são `pub(crate)` para que `pod`, `compose` e `stack` lhes possam delegar;
  `cmd/firewall.rs:update_locked`; `cmd/manifest.rs:KIND_ALIASES`.
- **`pub` num crate de biblioteca é uma promessa a outros crates.** Remover um item público é uma
  mudança incompatível para quem usa a biblioteca, mesmo com zero chamadores neste workspace. O lint
  `dead_code` do rustc não vê itens `pub` sem uso, por isso quando apagares um, conta à mão os itens
  públicos órfãos. **Decidido**: AGENTS.md § «`delonix_sdn::Net` foi APAGADO — e é breaking para
  quem usa a biblioteca».

### 5.4 O contrato de nó

`proto/delonix/node/v1` é a fonte de verdade das duas codificações, gRPC e HTTP/JSON.
`docs/api/openapi.yaml` é gerado a partir dele. **Nunca edites o ficheiro OpenAPI à mão.**
**Imposto (gate)**: `scripts/contract_gate.py` corre `buf format`, `buf lint`, `buf breaking`
contra a última tag que contenha `proto/`, verifica o mapeamento HTTP de cada RPC, e verifica que o
ficheiro OpenAPI é igual ao gerado. **Decidido**: AGENTS.md § «O contrato de nó é um portão» (que
cita o ADR-0040 P1; o ADR-0040 D4 ainda está **Proposto**).

- **Uma mensagem de pedido por RPC**, com o nome `<Rpc>Request`. **Imposto (gate)**: `buf lint`
  `RPC_REQUEST_STANDARD_NAME` (vê `buf.yaml`). Um pedido partilhado deixa um campo pensado para um
  método aparecer em cinco.
- **Identidade explícita no pedido**: campos `name` e `namespace`, nunca uma mensagem genérica de
  metadados com campos que o motor ignoraria. **Decidido**: AGENTS.md. **Convenção (observada)**:
  `compute.proto:GetContainerRequest { string name = 1; string namespace = 2; }`.
- **As imagens são endereçadas por query, não no caminho**, porque num caminho como `alpine:3.20` o
  `:` seria lido como verbo personalizado. **Decidido**: AGENTS.md. **Convenção (observada)**:
  `infra.proto` `GetImage` → `get: "/v1/images:get"`, com `GetImageRequest { string reference = 1; }`.
- **As respostas** devolvem o recurso, ou uma `Operation` para mutações longas. As regras de nomes
  de resposta do `buf lint` estão desligadas de propósito (comentário no `buf.yaml`).
- **Todo o RPC tem um mapeamento HTTP excepto os streams bidireccionais** (`Exec`, `Console`), que
  não o podem ter. **Imposto (gate)**: verificação 4 do `contract_gate.py`.

### 5.5 O motor não conhece nenhum consumidor

O motor não sabe quem o usa. Nenhum nome de plataforma, control plane, consola ou agente, e nenhuma
noção de inquilino, conta, plano ou faturação, pode aparecer em `crates/`, `bins/`, `proto/`, no
`Cargo.toml` raiz ou no `Makefile`, **comentários incluídos**.

- **Nomes de consumidores.** **Imposto (gate)**: `scripts/arch_fitness.py` `consumer_mentions()`
  casa a expressão regular `CONSUMER_NAMES`, uma lista fixa de nomes, nesses caminhos. Um nome que
  não esteja na lista não é apanhado.
- **Conceitos de inquilino, conta, plano e faturação.** **Decidido**: AGENTS.md § «Identidade e
  fronteira do motor». Nenhum gate os casa; verifica-o a revisão.

Se um consumidor precisar de algo, escreve-o como uma capacidade
genérica no vocabulário próprio do motor (Kinds e recursos), e acrescenta-o só se fizer sentido
para qualquer cliente. O motor valida o seu próprio contrato e nunca confia num chamador para
recusar o que ele próprio não suporta.

---

## 6. Tratamento de erros e mensagens

- **Nenhuma falha silenciosa.** Se uma opção é aceite e depois ignorada, isso é pior que uma
  funcionalidade em falta, porque o utilizador julga que teve efeito. Recusa-a com um erro claro, e
  nomeia a flag. **Decidido**: AGENTS.md § «Falhas silenciosas corrigidas (fail-closed)»; a
  auditoria da v0.37.0 (§ «Auditoria sistemática dos 208 subcomandos») chama a esta classe «relato
  desonesto». Os padrões que essa secção lista são:
  - **Não destruas nada antes de saberes que o objecto é teu para destruir, e apaga a contabilidade
    em último lugar.** Se o registo é removido primeiro e a remoção dos dados falha a seguir, os
    dados ficam órfãos e um `create` posterior entrega-os a outra pessoa.
  - **Uma medição ilegível é *desconhecida*, nunca zero.** Um `read_dir` que falha não é um
    directório vazio. É por isso que existe `Usage { bytes, unreadable }`.
  - **Atenção aos padrões que transformam falhas em sucesso**: `let _ =` sobre um resultado que
    importa (entropia, uma leitura de socket), `as u64` sobre um `f64` (satura), `capture()` lido
    pelo seu `Result` em vez da sua saída. O AGENTS.md § «A classe «X não é Y»» cataloga-os.
- **Desconhecido ou impossível de medir não é um palpite.** Quando o motor não consegue ler um
  valor, reporta que não sabe, ou recusa. Não escolhe a resposta mais provável. Exemplo: o
  `system prune --auto` recusa se a ocupação do disco não puder ser lida, e o ceifador do IPAM falha
  fechado quando um store é ilegível (uma lista vazia ler-se-ia como «nada está vivo»).
  **Decidido**: AGENTS.md § CLI (`system prune`), § «O IPAM vaza».
- **Forma da mensagem: o facto primeiro, depois o que fazer**, com o comando exacto quando existe.
  **Decidido**: AGENTS.md § «`-p 80:80` respondia com o JSON cru do slirp» («facto primeiro, depois
  os comandos prontos a copiar»). **Convenção (observada)**:
  `delonix-model/src/error.rs:Error::VmNotFound` → `"no such VM: {0} (see \`delonix vm ls\`)"`;
  `cmd/config.rs:refuse_unknown_key` nomeia as chaves válidas. Nomeia a **ferramenta em falta e o
  seu pacote** em vez de passar um `ENOENT` cru, que se lê como um ficheiro em falta (AGENTS.md §
  «A bateria mede o `--help` de tudo», achado 1).
- **Uma medição parcial não é um sucesso.** O `--wait` tem de observar o que afirma, e o
  `✓ … is up` só é impresso depois de verificar. **Decidido**: AGENTS.md § «O `--wait` de uma VM CH».
- **Nunca faças parsing de uma mensagem de erro para decidir o que fazer.** As mensagens são
  traduzidas (`--l18n=pt`/`DELONIX_L18N`), por isso um `grep 'no such'` classifica numa máquina e
  deixa em silêncio de classificar noutra. Usa a classe de saída ou o código `DX_*`. **Decidido**:
  docs de módulo de `delonix-model/src/exitcode.rs`.
- **Devolve a variante que corresponde à classe** (`NotFound`, `Conflict`, `NotRunning`,
  `Unavailable`, `Timeout`), não `Invalid` para tudo. **Decidido**: AGENTS.md § «Códigos de
  saída com classe»: o `util::find` a devolver `Invalid` para «não encontrado» tornava o recurso
  mais usado impossível de classificar.

---

## 7. `unsafe`, syscalls e processos

- **Todo o bloco `unsafe` tem um comentário `// SAFETY:` imediatamente acima.** **Imposto (gate)**:
  `undocumented_unsafe_blocks = "deny"` em `[workspace.lints.clippy]`.

  ```rust
  // crates/adapters/delonix-linux/src/lib.rs — apply_filter_logged
  // SAFETY: `fprog` points to a valid BPF program; NO_NEW_PRIVS is already set.
  let rc = unsafe {
      libc::syscall(libc::SYS_seccomp, SET_MODE_FILTER, FLAG_LOG, &fprog as *const _)
  };
  ```

  O comentário tem de enunciar o invariante que torna a chamada correcta. «same» só é aceitável
  imediatamente a seguir a uma chamada idêntica e justificada (como em `delonix-linux/src/lib.rs`
  logo a seguir ao primeiro `_exit(126)`).
- **Nenhum `clone()`/`fork()` cru num processo multi-thread** (os servidores tokio, o shim da API
  Docker). O `clone` não corre os handlers `pthread_atfork`, por isso o filho pode ficar em deadlock
  no lock do malloc. Re-executa antes um spec tipado, entregue por um ficheiro `0600`/`O_EXCL` em
  vez do argv. **Decidido**: AGENTS.md § «Auditoria de segurança #3», item 5. Contexto:
  [03](03-rust-primer.md#why-forkclone-in-a-multi-threaded-process-is-dangerous).
- **Um hook `pre_exec` não pode bloquear à espera de algo que o pai faz depois de o `spawn`
  devolver.** O `Command::spawn` só devolve depois do `exec`, por isso os dois processos esperam um
  pelo outro para sempre. Usa um `fork` cru para handshakes. **Decidido**: AGENTS.md § «A classe
  «X não é Y»» (a entrada do `reexec_mapped_hold`).
- **Ficheiros temporários: usa `delonix_state::write_private_temp`.** Abre com um nome
  único, `O_EXCL` e modo `0600`, por isso nunca segue um symlink plantado. Não uses um nome fixo ou
  derivado do pid em `/tmp`. **Decidido**: AGENTS.md § «Auditoria de segurança #3», passagem 2; doc
  comment em `crates/adapters/delonix-state/src/store.rs`. **Convenção (observada)**: `delonix-sdn/src/bpf.rs`,
  `delonix-linux/src/run_host.rs`.
- **Ficheiros que têm de ser privados ou atómicos: usa `write_atomic_mode(path, bytes, Some(0o600))`.**
  Define o modo na criação e publica com um rename atómico. Nunca escrevas o ficheiro para depois
  lhe fazer `chmod`, porque outro utilizador pode abri-lo nesse intervalo. **Decidido**: doc comment
  de `delonix-state/src/store.rs:write_atomic_mode`; AGENTS.md (TOCTOU do kubeconfig).
- **Antes de sinalizar um pid lido de um ficheiro, confirma que ainda é o mesmo processo.** Usa
  `delonix_runtime_core::safe_to_signal(pid, starttime)`, que compara o instante de arranque para um
  pid reciclado não ser morto. **Decidido**: AGENTS.md § «A classe «X não é Y»» (as entradas de pid).
- **O argv de um processo não prova que ele é nosso.** Outros state roots do mesmo utilizador, e
  outras ferramentas, correm com o mesmo argv. Verifica um token que só nós escolhemos: um caminho
  derivado da nossa raiz, ou uma variável de ambiente que pinámos no spawn. **Decidido**: AGENTS.md
  § «A classe «X não é Y»» (a entrada «o argv de um processo»).
- **Passa `--` antes dos argumentos posicionais que vêm de input** no argv de ferramentas externas
  (`ssh`, `scp`, `virsh`, `mount`, `qemu-img`), e valida contra uma whitelist de caracteres
  qualquer valor que acabe numa shell remota. O `shell_quote` não sanitiza o conteúdo.
  **Decidido**: AGENTS.md § «Auditoria de segurança (skill `delonix-runtime-sec`)» e § «#2».
- **Os caminhos construídos a partir de nomes vindos do utilizador ou do manifesto são
  confinados.** Usa uma verificação de nome `valid_*` na fronteira do motor
  (`delonix_vm::valid_vm_name`), e uma junção segura que recuse componentes `..`/absolutos e
  symlinks (`safe_join`, `safe_bind_target`). **Decidido**: as mesmas secções do AGENTS.md.

---

## 8. Estado e concorrência

- **Ler–modificar–escrever passa por `update`, nunca por `load` → mutar → `save`.**
  `Store::update` e `JsonStore::update` (`crates/adapters/delonix-state/src/store.rs`)
  tomam um `flock`, **voltam a ler debaixo do lock**, aplicam o teu closure e escrevem de forma
  atómica. Um closure que devolve `false` aborta a escrita. A CLI, o servidor CRI e as actualizações
  em background mexem todos nos mesmos registos em concorrência, e sem o lock uma escrita perde-se
  em silêncio. **Decidido**: doc comments das duas funções; AGENTS.md § «Revisão ampla de
  código/arquitectura (2026-07-27)», bugs 5 e o item do `JsonStore`. **Convenção (observada)**: uma
  mutação que pode ela própria falhar é embrulhada assim:

  ```rust
  // bins/delonix-runtime-bin/src/cmd/firewall.rs — update_locked
  let c = store.update(id_or_name, |c| match f(c) {
      Ok(commit) => commit,
      Err(e) => {
          err = Some(e);
          false
      }
  })?;
  ```

- **Persiste cada passo assim que o dataplane o confirma.** Se uma mudança de vários passos falhar a
  meio, o registo tem de continuar a bater com o que o kernel tem de facto. **Decidido**: AGENTS.md §
  «Reconfiguração a quente» («Persistência»).
- **Os campos novos em registos persistidos levam `#[serde(default)]`** (ou `default = "fn"`), para
  os registos escritos por versões anteriores continuarem a carregar. A omissão tem de descrever o
  que os registos antigos eram de facto, não um palpite. **Decidido**: AGENTS.md (por exemplo
  `Vm.namespace`, `VmImage.cloud_init`). **Convenção (observada)**: `delonix-runtime-core/src/lib.rs`,
  o doc comment de `Vm.namespace` («the default is a statement of fact and not a guess»).
- **Tudo o que é preciso para reconstruir um recurso tem de ser persistido, não só usado na
  criação.** Quando mexeres num caminho de `start`/`restart`, compara campo a campo o que a criação
  usa com o que o registo guarda. **Decidido**: AGENTS.md § «BUG GRAVE corrigido… `-v` nunca era
  persistido» (listado aí como o terceiro bug da mesma família).
- **Os ficheiros de lock nunca são apagados.** Apagar um abre uma janela em que dois processos
  bloqueiam inodes diferentes. **Decidido**: doc comment de `store.rs:lock_path` (`delonix-state`).
- **O `SecretStore::update` é o único `update` cujo lock é best-effort.** O seu `FileLock::acquire`
  devolve `Option` e avança sem lock se o ficheiro de lock não puder ser aberto, ao contrário do `Store` e
  do `JsonStore`. **Não decidido**: se deve recusar como os outros; segue o código à volta e
  di-lo no PR se lhe tocares.

---

## 9. Testes

- **Onde vão.** Os testes unitários vão num `#[cfg(test)] mod tests` no fim do ficheiro. Os testes
  de integração vão em `crates/<layer>/<crate>/tests/` e usam só a API pública. Os testes contra
  providers vivos são opt-in. **Convenção (observada)**; detalhes em [03 § 3.9](03-rust-primer.md#39-tests).
- **Primeiro o puro.** Põe a decisão numa função pura e testa-a como dados. Tudo o que precise de
  namespaces, cgroups ou de um holder de rede reais valida-se ao vivo ou com `scripts/e2e.sh`.
  **Decidido**: `CONTRIBUTING.md` («Write a unit test for any new pure function»); AGENTS.md §
  «IaC nativo» (o `reconcile.rs` é puro para poder ser testado como dados).
- **Nomes**: frases em inglês que enunciam o comportamento (vê [§3.4](#34-test-names)).
- **Os testes nunca mexem no estado real do host.** Dá aos stores uma raiz temporária. Não chames
  código que resolva o state root real. Não faças `set_var` (o ratchet `env_writes`,
  [§4.1](#41-the-layers-and-the-direction)).
  **Decidido**: AGENTS.md § «IaC nativo», a nota da fusão do `ShareVolume` («Nota de método: um
  teste que chamasse `apply_share` … escreveria no estado REAL da máquina»). **Convenção
  (observada)**: os testes de `delonix-state/src/store.rs` usam um helper `tmp_dir(tag)`.
  Para corridas manuais e E2E, isola **os dois**, `DELONIX_ROOT` e `DELONIX_NET_RUNTIME_DIR`. Isolar
  só um é pior que nenhum (AGENTS.md § «Meia-isolação é pior que nenhuma»;
  [02](02-build-and-test.md#isolating-the-engines-state)).
- **Um teste de regressão tem de falhar com a correcção revertida.** Reverte a correcção, vê o teste
  falhar, e repõe a correcção. Um teste que passa nos dois casos não prova nada, e o AGENTS.md
  regista vários (uma verificação de código de saída que o `1` não conseguia distinguir; um cenário
  de caos que ficou verde com uma reversão). **Decidido**: AGENTS.md, «verificado pela regra do
  repo» ao longo de todo o ficheiro, por exemplo § «IaC nativo» (`stack_converge`) e § «A bateria
  mede o `--help`».
- **Testa o caminho que a produção usa.** Se a produção passa caminhos relativos, o teste usa
  caminhos relativos. Um teste pode codificar o bug. **Decidido**: AGENTS.md § «Auditoria
  sistemática dos 208 subcomandos» (`default_project_name`).
- **Os bugs de concorrência levam uma corrida real.** Usa threads e um sleep explícito dentro da
  janela crítica. **Convenção (observada)**: `delonix-state/src/store.rs:jsonstore_update_concorrente_nao_perde_escritas`.
- **Prefere propriedades a tempos amostrados.** Quando uma corrida só pode ser amostrada, gera carga
  e repete. **Decidido**: AGENTS.md § «Um `exec` logo a seguir ao `run -d` corria no HOST».
- **As regiões geradas não levam contagens voláteis** (linhas, testes, commits). **Decidido**:
  docstring do `scripts/dev_docs.py` («Deliberately NOT generated: line counts, test counts, commit
  counts»). Uma **medição escrita à mão** cita o valor medido juntamente com a data em que foi
  medido, nunca um total corrente. **Decidido**: AGENTS.md § «A bateria mede o `--help` de tudo e
  EXECUTA um quarto» («Cita-se a fracção medida e a data, nunca o total»). Se as contagens podem de
  todo aparecer na prosa ou em comentários de código **não está decidido** — segue o texto à volta.

---

## 10. Comentários e documentação

- **Os comentários explicam o *porquê*, não o *quê*.** Escreve um para uma restrição escondida, um
  contorno de um bug concreto, ou um invariante que o código não torna óbvio. Os comentários que
  repetem o código são removidos na revisão. **Decidido**: `CONTRIBUTING.md` § Style.
- **Quando uma decisão foi medida, diz o que foi medido.** «Medido: …» vale mais que «deveria». As
  docs de módulo em `exitcode.rs` e `reconcile.rs` são o modelo. **Convenção (observada)**.
- **Os doc comments (`///`, `//!`) em itens públicos e cabeçalhos de módulo** dizem o que o item
  promete e porque existe. **Convenção (observada)**: todas as portas em
  `delonix-compute/src/ports.rs`, `delonix-state/src/store.rs:write_private_temp`, `exitcode.rs`. **Não decidido**:
  nenhum lint `missing_docs` está activo.
- **Os comentários estão em inglês e não nomeiam nenhum consumidor.** O ratchet de língua e o gate
  de consumidores percorrem ambos os comentários. **Imposto (gate)**.
- **Não acrescentes abstracções, flags de configuração nem tratamento de erros para casos que não
  podem acontecer.** **Decidido**: `CONTRIBUTING.md` § Style.
- **Quando moves uma fronteira, actualiza os seus registos na mesma mudança**:
  - um ADR **antes** do código, para um provider, porta ou daemon novo, uma dependência externa num
    crate de motor, uma fronteira de privilégio, ou uma mudança ao contrato ou às camadas. Os ADRs
    aceites nunca são reescritos; um novo substitui-os (**Decidido**: `docs/adr/README.md`;
    [10](10-contributing-workflow.md#when-to-write-an-adr));
  - a secção do `AGENTS.md` que descreve a área. Uma secção desactualizada aí engana a próxima
    pessoa, e **os conflitos no `AGENTS.md` resolvem-se mantendo os dois lados** (**Decidido**:
    AGENTS.md § «Método: um worktree por sessão»);
  - os artefactos gerados: `docs/schema/v1/delonix.json`, `docs/api/openapi.yaml`, a linha de base
    da CLI, `docs/gen.py`, e as regiões geradas de `docs/dev/` via
    `python3 scripts/dev_docs.py` (**Imposto (gate)**; vê
    [11](11-publishing-docs.md)).

---

## Por decidir

Nada no repositório resolve estas questões. Segue o código à volta e menciona a escolha no teu PR:

- **Quando é aceitável um `#[allow(clippy::…)]`.** É usado (`too_many_arguments`) sem uma política
  escrita.
- **O tipo `Result` das portas de compute de hoje.** As portas em
  `delonix-compute/src/ports.rs` devolvem `delonix_runtime_core::Result`, por isso os adapters que
  as implementam (`HostWorkload`, `HostNetwork`, …) importam o tipo de resultado partilhado, e esses
  imports contam para `shared_error_imports`. A P3 decide os erros por crate. Nenhum documento diz
  como muda a assinatura de uma porta, e acrescentar um import desses num ficheiro novo falha o
  ratchet. Se a tua mudança precisar de um, levanta a questão no PR. Não contornes a regex.
- **Nomes de crates durante a transição.** O ADR-0040 D2 (ainda Proposto) fixa os nomes alvo, mas um
  crate totalmente novo que entre antes de o seu contexto existir (por exemplo um segundo provider
  antes das renomeações `delonix-provider-*`) não tem regra escrita. Pergunta primeiro na issue.
- **Doc comments obrigatórios.** Não há lint `missing_docs`, só o hábito observado.
- **O fitness test para o casamento de nomes de provider** (ADR-0040 D3 regra 3) está proposto mas
  ainda não implementado.

---

## Lista de verificação da revisão

Antes de abrires o PR, percorre a lista:

1. `cargo fmt --all --check` e `cargo clippy --workspace --all-targets --locked -- -D warnings`
   estão limpos. → [§1](#1-the-tooling-that-enforces-style)
2. `python3 scripts/lang_ratchet.py` e `python3 scripts/arch_fitness.py` passam, e qualquer linha de
   base que baixaste está neste commit. → [§1](#1-the-tooling-that-enforces-style)
3. Os identificadores, comentários, mensagens **e nomes de testes** novos estão em inglês. O texto ao
   utilizador passa por `po::t`/`po::tf`, com entradas no `pt.po`. Nada de `num`. →
   [§2](#2-language-of-the-code), [§3.4](#34-test-names)
4. O código está no crate e na camada certos, as versões estão só na raiz, a biblioteca não tem
   `println!` e não volta a correr o seu próprio binário. → [§4](#4-structure-where-code-goes)
5. As decisões são funções puras com testes unitários, e o I/O fica nas pontas. →
   [§4.3](#43-pure-core-io-at-the-edges)
6. Os backends novos implementam uma porta, sem casamento de nomes de provider. Os adapters não
   dependem de adapters. → [§5.1](#51-ports-and-adapters)
7. As falhas de adapter/provider usam um `Error` do crate que converte em `delonix_model::Error`. →
   [§5.2](#52-errors-per-crate-adr-0040-p3)
8. Se mexeste em `proto/`: o `scripts/contract_gate.py` passa, com um pedido por RPC, identidade
   explícita, e um OpenAPI regenerado. → [§5.4](#54-the-node-contract)
9. Nenhum nome de consumidor em lado nenhum de `crates/`, `bins/` ou `proto/`, comentários
   incluídos. → [§5.5](#55-the-engine-knows-no-consumer)
10. Nada é aceite para depois ser ignorado. Os erros enunciam o facto e depois a correcção, e usam a
    variante da classe certa. → [§6](#6-error-handling-and-messages), [§3.8](#38-exit-codes-and-dx_-codes)
11. Todo o bloco `unsafe` tem um comentário `// SAFETY:`. Não há fork cru num processo
    multi-thread, os ficheiros temporários usam `write_private_temp`, e os segredos usam
    `write_atomic_mode`. → [§7](#7-unsafe-syscalls-and-processes)
12. As mutações de registos passam por `update`, e os campos novos de registos têm
    `#[serde(default)]`. → [§8](#8-state-and-concurrency)
13. Os testes usam raízes isoladas, o teste de regressão falha com a correcção revertida, e um
    número medido leva a sua data. → [§9](#9-tests)
14. Mudanças na CLI: todos os pontos de entrada estão ligados, os verbos alinham com o Docker, os
    cortes não têm aliases, e a linha de base da CLI está actualizada. → [§3.5](#35-cli-commands-and-flags)
15. Kinds e campos de manifesto: uma linha em `kinds.rs`, campos `camelCase` com as grafias antigas
    como aliases, e o schema regenerado. → [§3.6](#36-kinds-api-groups-and-manifest-fields)
16. O ADR, o `AGENTS.md` e a documentação gerada estão actualizados se uma fronteira mudou. →
    [§10](#10-comments-and-documentation)
