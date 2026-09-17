<!-- translated-from: README.md sha256:16bc925163a0f88ba4af46214d9a1ab7bbdd2860e46d2560626905e05d074659 -->
# Delonix Runtime — Manual do Contribuidor

Este manual é para quem quer **mudar o motor**: clonaste o repositório hoje e queres enviar um
primeiro pull request sem partir o teu host nem o motor. Se só queres *usar* o Delonix, começa
antes pelo [README](../../../README.rst) e pelo
[site de documentação do utilizador](https://angolardevops.github.io/delonix-runtime/).

<!-- dev-docs:begin crate-count -->
O workspace tem **22 crates** e produz **4 binários** (`delonix`, `delonix-cri`, `delonix-mcp`, `delonix-mgmt`).
<!-- dev-docs:end crate-count -->

## O que o motor é — e o que não é

O Delonix Runtime é uma abstracção de execução para **um nó**: corre **containers e microVMs**
e gere a rede e o armazenamento de que eles precisam. É declarativo (Kinds próprios, agrupados por
`apiVersion` — ver `delonix api-resources`), e fala com os providers (o kernel Linux, libvirt,
Cloud Hypervisor, Proxmox VE, o CRI do Kubernetes) só através de portas, nunca através de ramos
`if provider == …` espalhados pelo código.

Três princípios moldam quase todos os comentários de revisão que vais receber:

- **Cloud native** — plan / apply / deriva, API-first (a CLI, a API de nó, o CRI e o servidor MCP
  expõem as mesmas operações), observável através de padrões abertos.
- **Daemonless** — nenhum processo residente por omissão. O que tem de persistir pertence ao systemd
  ou a um processo por workload com um dono claro. Um daemon novo precisa de um ADR.
- **Rootless-first** — o caminho normal corre sem root; o privilégio é um opt-in explícito.

E uma fronteira que é imposta por um gate de CI: **o motor não conhece nenhum consumidor.** Não sabe
quem o chama, e não tem noção de inquilino, conta, plano ou facturação. Um requisito vindo de um
consumidor entra como uma capacidade genérica do motor, ou não entra. O texto canónico é a
secção *«Identidade e fronteira do motor»* no topo do [`AGENTS.md`](../../../AGENTS.md).

## Duas metades: factos gerados e narrativa

As páginas deste manual misturam dois tipos de conteúdo:

- **Factos** — que crates existem, a sua camada, quem depende de quem, os binários, a toolchain
  fixada, os jobs de CI. Vivem entre os marcadores `<!-- dev-docs:begin <key> -->` e
  `<!-- dev-docs:end <key> -->` e são **gerados** por `python3 scripts/dev_docs.py`
  a partir do `Cargo.toml`, do `scripts/arch_fitness.py`, do `rust-toolchain.toml` e do
  `.github/workflows/ci.yml`. Nunca os edites à mão — a CI corre `dev_docs.py --check` e falha.
  Se um facto estiver errado, corrige a fonte ou o gerador.
- **Narrativa** — porque é que as coisas são como são, como funcionam os fluxos, como contribuir. É
  escrita à mão e revista depois de cada release. Ver [11 — Publicar a documentação](11-publishing-docs.md).

## Percursos de leitura

| Se queres… | Lê, por esta ordem |
|---|---|
| **Começar do zero, sem ninguém a quem perguntar** | [00](00-start-here.md) (mantém o [14](14-glossary.md) aberto) → o percurso abaixo que corresponde à tua mudança |
| **Enviar um primeiro PR** (uma correcção na CLI, uma correcção na documentação, uma feature pequena) | [00](00-start-here.md) → [01](01-environment.md) → [02](02-build-and-test.md) (mantém o [15](15-environment-variables.md) à mão) → [10](10-contributing-workflow.md) → [12](12-coding-conventions.md) → [06](06-crates.md) para o crate em que mexes |
| **Compreender o motor em profundidade** | [03](03-rust-primer.md) → [04](04-cloud-native-primer.md) → [13](13-cloud-native-standards.md) → [05](05-architecture.md) → [06](06-crates.md) → [07](07-system-design-interview.md) |
| **Trabalhar em VMs ou imagens de VM** | [01](01-environment.md) → [02](02-build-and-test.md) → [08](08-delonixfile-and-vmfile.md) → [09](09-microvm-setup.md) → a secção `delonix-vm` do [06](06-crates.md) → a secção de afinação de VMs do [15](15-environment-variables.md) |
| **Mudar a forma como a documentação é produzida** | [11](11-publishing-docs.md) |
| **Configurar, isolar ou afinar uma corrida** (state roots, logs, escapatórias, providers) | [02 — Isolar o estado do motor](02-build-and-test.md#isolating-the-engines-state) → [15](15-environment-variables.md) |

## Páginas

| # | Página | O que responde |
|---|---|---|
| 00 | [Começa aqui](00-start-here.md) | Verificação do ambiente no dia 0, a tua primeira contribuição de ponta a ponta, onde vai uma mudança, as regras e as suas fontes, o que fazer quando ficas bloqueado |
| 01 | [Preparar o teu ambiente](01-environment.md) | Do que o kernel e o host precisam, a toolchain fixada, e as armadilhas do host que parecem bugs do motor |
| 02 | [Clonar, compilar e testar](02-build-and-test.md) | Compilar, correr testes, cada gate de CI como comando local, E2E e caos com isolamento |
| 03 | [Introdução ao Rust](03-rust-primer.md) | O Rust que esta base de código realmente usa |
| 04 | [Introdução ao cloud native](04-cloud-native-primer.md) | Namespaces, cgroups v2, OCI, CRI, CNI, nftables, KVM — e onde cada um aparece no motor |
| 05 | [Arquitectura](05-architecture.md) | Camadas, o grafo de crates, caminhos de controlo e de dados, estado em disco |
| 06 | [Os crates](06-crates.md) | Um bloco por crate: responsabilidade, tipos principais, por onde começar a ler |
| 07 | [System Design Interview](07-system-design-interview.md) | O motor desenhado como resposta de entrevista, e depois comparado com o que foi construído |
| 08 | [Delonixfile e VMfile](08-delonixfile-and-vmfile.md) | As gramáticas dos ficheiros de build e em que diferem de um Dockerfile |
| 09 | [Construir microVMs](09-microvm-setup.md) | KVM, Cloud Hypervisor e firmware, libvirt, imagens de VM |
| 10 | [Fluxo de contribuição](10-contributing-workflow.md) | Worktrees, versões, regra de língua, regras de arquitectura, ADRs, commits e PRs |
| 11 | [Publicar a documentação](11-publishing-docs.md) | Como o site e este manual são gerados, verificados por gates e publicados |
| 12 | [Convenções de código](12-coding-conventions.md) | Como se escreve código neste repositório, e a lista de verificação que os revisores aplicam |
| 13 | [Padrões cloud native](13-cloud-native-standards.md) | Os padrões cloud native pelos quais uma mudança é avaliada |
| 14 | [Glossário](14-glossary.md) | Os termos do motor e do cloud native que encontras aqui, com o seu significado no Delonix e onde ler mais |
| 15 | [Variáveis de ambiente](15-environment-variables.md) | Todas as variáveis `DELONIX_*` que o código lê: quem as lê, o que mudam, a sua omissão, e quais baixam uma fronteira |

Outras referências para onde vais ser encaminhado: [`ARCHITECTURE.md`](../../../ARCHITECTURE.md) (diagramas C4),
[`docs/adr/`](../../adr/README.md) (decisões de arquitectura), [`SECURITY.md`](../../../SECURITY.md)
(relatos privados de vulnerabilidades) e [`CONTRIBUTING.md`](../../../CONTRIBUTING.md) (a porta de entrada curta).
