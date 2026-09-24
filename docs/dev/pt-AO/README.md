<!-- translated-from: README.md sha256:e9205b57d61260f2d44b7c78c60bc5ee9e79f9781fa0948ce1738e55a800ee77 -->
# Delonix Runtime — Manual do Contribuidor

Este manual é para quem quer **mudar o motor**: clonaste o repositório hoje e queres enviar um
primeiro pull request sem partir o teu host nem o motor. Depois desta página vais saber como o
manual está sequenciado e que páginas ler, por que ordem, para o teu papel. Se só queres *usar*
o Delonix, começa antes pelo [README](../../../README.rst) e pelo
[site de documentação do utilizador](https://angolardevops.github.io/delonix-runtime/).

<!-- dev-docs:begin crate-count -->
O workspace tem **23 crates** e produz **4 binários** (`delonix`, `delonix-cri`, `delonix-mcp`, `delonix-mgmt`).
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
  escrita à mão e revista depois de cada release. Ver [Publicar a documentação](publishing-docs.md).

## Como este manual está organizado

As páginas formam **um único curso**, lido de cima a baixo. Cada página abre com uma linha
**Antes de leres** que nomeia as páginas anteriores que assume, e termina com uma linha
**Seguinte** que aponta para a página que constrói em cima dela. O número junto a uma página na
barra lateral do site é a sua posição nesta ordem. Números de secção *dentro* de uma página (por
exemplo §3.8 na introdução ao Rust, ou 13.4 na página de padrões) são rótulos locais mantidos
estáveis para ligações; não são posições de página.

O curso está agrupado em oito partes:

| Parte | Páginas | O que ganhas com ela |
|---|---|---|
| **Começar** | [Começa aqui](start-here.md) · [IaaS e cloud native](iaas-and-cloud-native.md) | Um checkout a funcionar, um primeiro caminho de contribuição, e o modelo mental de onde um motor de nó se situa numa cloud |
| **Fundações** | [Fundações de Linux](linux-foundations.md) · [Introdução ao cloud native](cloud-native-primer.md) · [Introdução ao Rust](rust-primer.md) | Os primitivos do kernel à mão, como o motor usa cada um, e o Rust em que este código está escrito |
| **Preparar e compilar** | [Preparar o teu ambiente](environment.md) · [Clonar, compilar e testar](build-and-test.md) | Um host que consegue correr os caminhos ao vivo, e cada gate de CI como comando local |
| **Arquitectura** | [Estrutura do projecto](project-structure.md) · [Arquitectura](architecture.md) · [Os crates](crates.md) · [System Design Interview](system-design-interview.md) | Onde as coisas estão, porque é que estão divididas assim, o que cada crate possui, e o raciocínio por trás do desenho |
| **Imagens e microVMs** | [Delonixfile e VMfile](delonixfile-and-vmfile.md) · [Construir microVMs](microvm-setup.md) | As duas gramáticas de build, e VMs desde os pré-requisitos do host até ao arranque |
| **Operar e depurar** | [Diagnóstico de problemas](troubleshooting.md) · [Como os nomes chegam ao `/etc/hosts`](service-names-and-hosts.md) | Um índice por sintoma do que um gate ou uma corrida ao vivo imprime, e o mecanismo que publica os nomes de serviço e os hosts de rota na máquina do operador |
| **Contribuir** | [Convenções de código](coding-conventions.md) · [Adicionar um Kind](adding-a-kind.md) · [Fluxo de contribuição](contributing-workflow.md) · [Releases e estabilidade](releases-and-stability.md) · [Publicar a documentação](publishing-docs.md) | Como o código tem de ser escrito, como se acrescenta um Kind declarativo, como se envia uma mudança, o que uma release promete não partir, e como a documentação a acompanha |
| **Referência** | [Padrões cloud native](cloud-native-standards.md) · [Variáveis de ambiente](environment-variables.md) · [Glossário](glossary.md) | Páginas onde procuras coisas: conformidade por padrão, cada nome `DELONIX_*`, cada termo |

Os conceitos são **ensinados uma vez**: um primitivo do kernel em [Fundações de Linux](linux-foundations.md),
como o motor o usa em [Introdução ao cloud native](cloud-native-primer.md), e o padrão que segue
com o seu estado de conformidade em [Padrões cloud native](cloud-native-standards.md). Onde uma
página menciona algo ensinado noutro sítio, liga para lá em vez de o repetir.

## Percursos de leitura por papel

Ninguém tem de ler as vinte e duas páginas antes de uma primeira mudança. Escolhe a linha que te
descreve e lê as suas páginas pela ordem dada; mantém o [Glossário](glossary.md) aberto.

| Papel | Lê, por esta ordem — e porquê |
|---|---|
| **Primeiro PR, sem tempo** | 1. [Começa aqui](start-here.md) — a verificação do dia 0 e os oito passos de um primeiro PR. 2. [Preparar o teu ambiente](environment.md#known-host-traps) — só *Armadilhas conhecidas do host*. 3. [Clonar, compilar e testar](build-and-test.md#the-gates-ci-runs) — os gates que tens de passar. 4. [Os crates](crates.md) — só a secção do crate em que mexes. 5. [Fluxo de contribuição](contributing-workflow.md) — como o PR é julgado. |
| **Engenheiro DevOps** (CI, empacotamento, instalação, releases) | 1. [Começa aqui](start-here.md) — a configuração e as regras. 2. [Preparar o teu ambiente](environment.md) — do que um host precisa e as armadilhas que parecem bugs do motor. 3. [Clonar, compilar e testar](build-and-test.md) — instalar um build, cada job de CI como comando local, E2E e caos. 4. [Estrutura do projecto](project-structure.md) — o que é gerado, o que a CI verifica, o que o `release.yml` actualiza. 5. [Diagnóstico de problemas](troubleshooting.md) — reconhecer a falha de um gate pela sua mensagem. 6. [Releases e estabilidade](releases-and-stability.md) — o gate de versão, o que uma tag empurrada faz, o que é estável. 7. [Publicar a documentação](publishing-docs.md) — o que acontece no momento da release. 8. [Variáveis de ambiente](environment-variables.md) — cada botão e quais baixam uma fronteira. |
| **Engenheiro de plataforma** (a construir sobre as interfaces do motor) | 1. [IaaS e cloud native](iaas-and-cloud-native.md) — que camada é o motor e o que deixa para um control plane. 2. [Introdução ao cloud native](cloud-native-primer.md#48-declarative-reconciliation) — Kinds e o reconciliador de três vias. 3. [Arquitectura](architecture.md) — as interfaces (CLI, CRI, API de gestão, MCP, contrato de nó) e as camadas. 4. [Os crates](crates.md) — `delonix-stack`, `delonix-cri`, `delonix-mgmt`, `delonix-mcp`. 5. [Adicionar um Kind](adding-a-kind.md) — a tabela e a ligação ao reconciliador que um Kind novo precisa. 6. [System Design Interview](system-design-interview.md) — as escolhas de API e os seus compromissos. 7. [Padrões cloud native](cloud-native-standards.md) — o que é conforme, parcial ou ausente, com datas. |
| **SRE** (a operar nós, a diagnosticar falhas) | 1. [Fundações de Linux](linux-foundations.md) — responde "que namespace, que cgroup, quem segura este fd" com um comando. 2. [Preparar o teu ambiente](environment.md#diagnosing-the-host) — diagnosticar um host e as suas armadilhas. 3. [Diagnóstico de problemas](troubleshooting.md) — um índice por sintoma para falhas de gate e de runtime. 4. [Arquitectura](architecture.md#level-2-containers-executables-and-processes) — que processos existem em runtime, estado em disco, limitações conhecidas. 5. [System Design Interview](system-design-interview.md#7-failure-modes-and-the-limits-of-one-node) — modos de falha e os limites de um nó. 6. [Convenções de código](coding-conventions.md#38-exit-codes-and-dx_-codes) — o que um código de saída significa. 7. [Variáveis de ambiente](environment-variables.md#observability) — logging, OTLP e as escapatórias. 8. [Padrões cloud native](cloud-native-standards.md#1311-opentelemetry) — OpenTelemetry e Prometheus. |
| **Programador cloud** (Kinds, manifestos, imagens, compatibilidade Compose/Docker) | 1. [IaaS e cloud native](iaas-and-cloud-native.md) — os princípios tal como aparecem no código. 2. [Introdução ao cloud native](cloud-native-primer.md) — imagens OCI e reconciliação declarativa. 3. [Clonar, compilar e testar](build-and-test.md) — compilar e correr isolado. 4. [Os crates](crates.md#delonix-stack) — `delonix-stack` e `delonix-oci`. 5. [Delonixfile e VMfile](delonixfile-and-vmfile.md) — as gramáticas de build. 6. [Convenções de código](coding-conventions.md#36-kinds-api-groups-and-manifest-fields) — regras para Kinds e campos. 7. [Adicionar um Kind](adding-a-kind.md) — ligar um Kind ao reconciliador de ponta a ponta. 8. [Padrões cloud native](cloud-native-standards.md#138-the-workload-api-own-kinds-and-the-node-contract) — Kinds próprios, API Docker e subconjuntos do Compose. |
| **Programador Linux** (namespaces, cgroups, rede, VMs) | 1. [Fundações de Linux](linux-foundations.md) — os primitivos à mão. 2. [Introdução ao cloud native](cloud-native-primer.md) — onde cada primitivo vive no código. 3. [Introdução ao Rust](rust-primer.md#34-unsafe-ffi-and-linux-syscalls) — `unsafe`, syscalls, `fork`/`clone` em processos com threads. 4. [Preparar o teu ambiente](environment.md) — armadilhas de AppArmor e de delegação de cgroup. 5. [Arquitectura](architecture.md) — a infra-estrutura de rede rootless e os dois fluxos como sequências. 6. [Os crates](crates.md#delonix-linux) — `delonix-linux`, `delonix-sdn`, `delonix-vm`. 7. [Construir microVMs](microvm-setup.md) — KVM, Cloud Hypervisor, libvirt. 8. [Convenções de código](coding-conventions.md#7-unsafe-syscalls-and-processes) — as regras para `unsafe` e processos. |

Duas tarefas mais estreitas têm o seu próprio atalho: **mudar como a documentação é produzida**
começa em [Publicar a documentação](publishing-docs.md); **configurar ou isolar uma corrida**
começa em [Isolar o estado do motor](build-and-test.md#isolating-the-engines-state) e depois
[Variáveis de ambiente](environment-variables.md).

## Páginas

| Página | O que responde |
|---|---|
| [Começa aqui](start-here.md) | Verificação da configuração no dia 0, a tua primeira contribuição de ponta a ponta, onde vai uma mudança, as regras e as suas fontes, o que fazer quando ficas bloqueado |
| [IaaS e cloud native](iaas-and-cloud-native.md) | Do que é feita uma IaaS, que camada é este motor, o que deixa para um control plane, e como os princípios cloud native aparecem nos seus ficheiros |
| [Fundações de Linux](linux-foundations.md) | Processos, namespaces, cgroups v2, descritores de ficheiro e sinais — à mão, com os comandos para inspeccionar cada um |
| [Introdução ao cloud native](cloud-native-primer.md) | Como o motor usa namespaces, cgroups, capabilities, OCI, rede, CRI, KVM e reconciliação — com ficheiros e símbolos |
| [Introdução ao Rust para esta base de código](rust-primer.md) | O Rust que esta base de código realmente usa |
| [Preparar o teu ambiente](environment.md) | Do que o kernel e o host precisam, a toolchain fixada, e as armadilhas do host que parecem bugs do motor |
| [Clonar, compilar e testar](build-and-test.md) | Compilar, instalar, correr testes, cada gate de CI como comando local, E2E e caos com isolamento |
| [Estrutura do projecto](project-structure.md) | O que é cada ficheiro e directório de topo, quem o muda, e o que é gerado |
| [Arquitectura](architecture.md) | Camadas, o grafo de crates, processos em runtime, caminhos de controlo e de dados, estado em disco |
| [Os crates](crates.md) | Um bloco por crate: responsabilidade, tipos principais, por onde começar a ler |
| [System Design Interview](system-design-interview.md) | O motor desenhado como resposta de entrevista, e depois comparado com o que foi construído |
| [Delonixfile e VMfile](delonixfile-and-vmfile.md) | As gramáticas dos ficheiros de build e em que diferem de um Dockerfile |
| [Construir microVMs](microvm-setup.md) | KVM, Cloud Hypervisor e firmware, libvirt, imagens de VM |
| [Diagnóstico de problemas](troubleshooting.md) | Um índice por sintoma: mensagens de falha de gate, armadilhas do host e as suas correcções, num só sítio |
| [Como os nomes chegam ao `/etc/hosts`](service-names-and-hosts.md) | O bloco delimitado único, as duas vias por onde um nome entra nele (`hosts: [host]` e `delonix hosts sync`), o que ele recusa, e como o testar sem root |
| [Convenções de código](coding-conventions.md) | Como se escreve código neste repositório, e a lista de verificação que os revisores aplicam |
| [Adicionar um Kind](adding-a-kind.md) | A tabela, o schema e a ligação ao reconciliador que um Kind declarativo novo precisa, trabalhado através de `Service` |
| [Fluxo de contribuição](contributing-workflow.md) | Worktrees, versões, regra de língua, regras de arquitectura, ADRs, commits e PRs |
| [Releases e estabilidade](releases-and-stability.md) | O gate de versão, o que uma tag empurrada faz, e o que a CLI e o schema de manifesto prometem não partir |
| [Publicar a documentação](publishing-docs.md) | Como o site e este manual são gerados, verificados por gates e publicados |
| [Padrões cloud native](cloud-native-standards.md) | Cada padrão, o que exige, como o Delonix o implementa, e o seu estado de conformidade |
| [Variáveis de ambiente](environment-variables.md) | Cada variável `DELONIX_*` que o código lê: quem a lê, o que muda, a sua omissão, e quais baixam uma fronteira |
| [Glossário](glossary.md) | Os termos do motor e do cloud native que encontras aqui, com o seu significado no Delonix e onde ler mais |

Outras referências para onde vais ser encaminhado: [`ARCHITECTURE.md`](../../../ARCHITECTURE.md) (diagramas C4),
[`docs/adr/`](../../adr/README.md) (decisões de arquitectura), [`SECURITY.md`](../../../SECURITY.md)
(relatos privados de vulnerabilidades) e [`CONTRIBUTING.md`](../../../CONTRIBUTING.md) (a porta de entrada curta).

---

**Seguinte:** [Começa aqui](start-here.md) — verifica a tua configuração em trinta minutos e percorre uma primeira contribuição de ponta a ponta.
