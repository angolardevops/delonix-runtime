---
name: delonix-adr
description: Processo de decisão arquitectural do delonix-runtime — quando escrever um ADR (Architecture Decision Record), o formato, e os guarda-rios não-negociáveis que qualquer decisão tem de respeitar (daemonless por desenho, fronteira com o PaaS privado, sem dependência privada, spike GO/NO-GO antes de fronteiras novas). Complementa o agente `martin` (que desenha C4 e mantém o ARCHITECTURE.md). Usa sempre que uma mudança alterar uma fronteira estrutural, introduzir um backend/dependência/daemon, ou quando o utilizador pedir "ADR"/"decisão de arquitectura"/"design doc".
---

# Decisões arquitecturais do Delonix Runtime (ADR)

O repo não tinha `docs/adr/` até esta skill — mas **já pratica a disciplina**: os
spikes GO/NO-GO (kind/`--privileged`), as "decisões de desenho registadas" (A7 do
exit-code, volumes anónimos do compose adiados), e as limitações-documentadas-em-
vez-de-escondidas do `CLAUDE.md` são ADRs informais. Esta skill formaliza o
formato e, sobretudo, os **guarda-rios que uma decisão não pode violar**.

## Quando um ADR é obrigatório (e quando NÃO é)

Escreve um ADR quando a mudança:

- introduz ou muda uma **fronteira estrutural** (um crate novo, mover lógica entre
  crates, uma dependência entre crates numa direcção nova);
- adiciona um **backend** (`VmBackend` novo — Firecracker, Proxmox de nó único),
  uma **dependência externa** de peso, ou um **daemon** (ver guarda-rio 1);
- toma uma decisão de **filosofia** com alternativas reais (o modelo Workload,
  supervisor por-container vs. daemonless, event bus in-process vs. daemon);
- **fecha um GO/NO-GO** — o resultado do spike É a decisão, com a evidência.

NÃO escrevas ADR para: um bug fix, uma feature que só liga fios já existentes,
uma flag nova num comando (isso é `delonix-feature-dev`), ou uma limitação que já
está documentada no `CLAUDE.md`. ADR é para decisões com **consequência
estrutural e alternativas**, não para todo o commit.

## Os guarda-rios (uma decisão que viole qualquer um destes está errada)

1. **Daemonless por desenho.** O produto não tem daemon permanente. Um `delonixd`
   NÃO é um default — é uma mudança de filosofia que precisa do SEU PRÓPRIO ADR
   com necessidade provada (event bus/observabilidade contínua que nada mais
   resolve). O holder de rede e o slirp são infra persistente mínima, só existem
   quando há trabalho — não abras a porta a mais nada "porque seria conveniente".
2. **Fronteira com o `delonix-paas` privado (Regra de ouro).** Sem noção de
   tenant/licença/billing/quota/Console/IAM. Scheduler multi-nó, inventário
   multi-cluster e mapeamento tenant↔recurso são do PaaS, não daqui. Um
   `ProxmoxBackend` de baixo nível (um nó, sem tenant) pode caber; um "Proxmox
   Driver" com inventário/scheduler não. Se a decisão precisa de saber "quem é o
   cliente", está no repo errado.
3. **Sem dependência privada.** Nunca uma dep a `delonix-core`/`delonix-api`/
   `delonix-orchestrator` ou qualquer crate do monorepo privado. `cargo tree -e
   normal` não mostra nenhum `delonix-*` fora do `Cargo.toml` raiz. O repo
   compila sozinho.
4. **Crates de motor dep-limpos.** Dependências de UI/proxy/serialização de CLI
   ficam confinadas ao `-bin` (`ratatui`, `hyper`, `serde_yaml`). `cargo tree -e
   normal` de um crate de motor tem de continuar limpo. Uma dep nova num crate de
   motor é sempre matéria de ADR (superfície de supply-chain de um runtime de
   containers).
5. **Spike GO/NO-GO antes de uma fronteira nova de privilégio.** Qualquer coisa
   que toque userns/cgroup/netns/capabilities exige um spike empírico ANTES do
   desenho (o kind/`--privileged` foi assim — investigação, não suposição) e uma
   auditoria dedicada (`delonix-runtime-sec`) antes de fundir. Nada de "deve
   funcionar".
6. **Sem falha silenciosa.** Uma decisão que aceite uma opção e depois a ignore é
   pior que a feature em falta. Fail-closed (erro/aviso explícito), sempre. É um
   invariante do produto — um ADR não o pode relaxar.

## Formato do ADR

Um ficheiro por decisão em `docs/adr/NNNN-titulo-curto.md` (numeração
sequencial), fonte em EN como o resto da documentação de código:

```markdown
# ADR-NNNN: <título imperativo curto>

- **Status:** Proposed | Accepted | Superseded by ADR-MMMM | Rejected
- **Date:** YYYY-MM-DD
- **Deciders:** <quem>

## Context
O problema e as forças em jogo. Que guarda-rios (acima) tocam esta decisão?
Que evidência existe (spike, medição, bug real)? Sem contexto = sem ADR.

## Decision
O que foi decidido, em voz activa. A alternativa mais barata que resolve o
problema medido — não a mais completa.

## Alternatives considered
As opções reais e porque foram rejeitadas (incl. "não fazer nada"). Um ADR sem
alternativas é uma nota, não uma decisão.

## Consequences
O que fica mais fácil, o que fica mais difícil, que dívida se assume, o que
passa a precisar de manutenção. Limitações conhecidas explícitas — nunca
escondidas.
```

## Regras de higiene

- **Nunca reescrevas um ADR aceite** — se a decisão muda, escreve um novo com
  `Superseded by`/`Supersedes`. O registo é o valor; um ADR editável é um diário
  reescrito.
- **Rastreável ao código.** Como o `martin`, cada afirmação de estrutura confirma-
  se num ficheiro/símbolo real. Um ADR não inventa arquitectura que não existe.
- **O `martin` desenha, o ADR decide.** Se a decisão precisa de diagrama C4/
  sequência, chama o `martin` e liga ao `ARCHITECTURE.md`; não dupliques os
  diagramas dentro do ADR.
- **Roadmap Workload (Fases 1-3 do `CLAUDE.md`)**: cada fase que arranque merece o
  seu ADR — o shape do `kind: Workload`, a extracção do trait de `VmBackend` para
  `delonix-runtime-core` (confirmar ausência de dependência circular), e a decisão
  de daemon (guarda-rio 1). Não avances nenhuma sem o ADR correspondente aceite.

## No roteiro de auditoria

Conduz os pontos **3** (arquitectura e design) e parte do **11** do roteiro
completo — ver a skill âncora `delonix-auditoria`. Os guarda-rios acima são o
critério de aceitação de qualquer achado de arquitectura: um achado que proponha
violar um deles está errado, por mais elegante que seja. Para os princípios de
engenharia aplicados ao código (SOLID, patterns, núcleo puro, fronteiras de
crate) usa `delonix-engenharia`; o ADR entra quando a decisão move uma fronteira.
