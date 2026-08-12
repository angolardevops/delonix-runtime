---
name: delonix-aprendizados
description: Fechar o ciclo de um achado no delonix-runtime — como escrever o aprendizado para não voltar a repetir o erro, onde o registar (CLAUDE.md, notas de release, ADR, memória, skills), e que gate automático trava a regressão de algo que já funcionava. Usa depois de corrigir um bug, no fim de uma auditoria ou sessão longa, quando o utilizador pedir para documentar aprendizados, ou quando suspeitares de regressão de uma funcionalidade antiga.
---

# Fechar o ciclo — o aprendizado e o gate

Um bug corrigido sem aprendizado registado volta noutra forma. Um aprendizado
registado sem gate automático volta na mesma, mais tarde. **São as duas metades,
e nenhuma sozinha chega.**

## O que faz um aprendizado valer alguma coisa

O melhor formato deste repo é uma frase da classe **«X não é Y»** — porque
generaliza para código que ainda não existe:

> um ficheiro de socket **não é** um listener · `/sys/class/net` **não é** a netns
> do processo · `capture()` devolver `Ok` **não é** o comando ter passado · uma
> label **não é** o estado persistido · `holder_pid.is_some()` **não é** «o holder
> é alcançável» · `container.userns` **não é** «está num userns diferente do meu»
> · um directório ilegível **não é** um directório vazio · um rootfs já extraído
> **não é** um rootfs a extrair · um `read` que FALHA **não é** uma resposta vazia

Ao escreveres um achado novo, tenta reduzi-lo a essa forma. Se conseguires,
acrescenta-o ao catálogo no `CLAUDE.md` («A classe X não é Y»). Se não
conseguires, o aprendizado leva três partes obrigatórias:

1. **O sintoma como apareceu** — o que o operador viu, não o que o código fazia.
2. **A causa, medida.** Uma hipótese não é um aprendizado.
3. **A regra generalizável** — o que perguntar da próxima vez, em código que
   ainda não existe. «Sempre que uma função de rede receber UM ip vindo do
   registo, pergunta o que acontece com `extra_networks` não vazio.»

E, quando aplicável: **quantas vezes já aconteceu**. «Quarta ocorrência da mesma
armadilha» é a informação que faz alguém parar e mudar o processo, não só o
código.

## Onde registar — cada sítio tem um propósito diferente

| Sítio | O que lá vai | O que NÃO vai |
|---|---|---|
| `CLAUDE.md`, secção do domínio | a regra generalizável, com o número medido | o diário da sessão |
| `.claude/skills/delonix-*` | a mesma regra, se muda **como se trabalha** naquele domínio | achados pontuais |
| `docs/releases/vX.Y.Z.md` | o que mudou para o utilizador, **incluindo o que não foi validado** | teoria |
| `docs/adr/NNNN-*.md` | a decisão, se moveu uma fronteira estrutural | um bug fix |
| memória do agente | preferência do utilizador, gatilho, armadilha do ambiente | o que o repo já regista |
| `docs/discovery/NN_*.md` | o levantamento completo de uma auditoria | conclusões sem evidência |

**Não dupliques.** Se o `CLAUDE.md` já tem, a skill aponta; se a skill já tem, o
`CLAUDE.md` não repete. Duas cópias divergem, e a que estiver errada é lida com a
mesma confiança que a certa.

## A lição sobre as próprias notas: uma tabela desactualizada mente nos dois sentidos

Aconteceu duas vezes, e é o defeito mais insidioso desta categoria:

- O `docs/AUDITORIA-E2E.md` nunca foi actualizado à medida que as correcções
  entravam — fez **27 problemas resolvidos parecerem dívida viva durante
  semanas**, e ao mesmo tempo listava como aberto o que já estava fechado.
- A secção «Estado para a próxima sessão» do `CLAUDE.md` esteve parada **onze
  versões** — era a primeira coisa que uma sessão lia, e dava por fazer o que
  estava feito.

**Regra:** quem fecha um achado actualiza a linha que o declarava aberto, no
mesmo commit. E ao começar uma sessão, **a tabela não é a fonte** — o código e o
binário são; a tabela é uma pista a verificar.

## O gate — sem isto, não está fechado

**A pergunta que decide:** *que teste falha se eu reverter o fix?* Se não souberes
responder, escreveste um teste que passa por acaso. Reverte de facto, confirma que
apanha, e só depois confirma que passa com o fix.

Escolhe o gate pelo tipo de achado:

| Tipo | Gate |
|---|---|
| lógica pura (parser, validador, plano) | `#[cfg(test)]` no módulo |
| invariante sobre input grande | `proptest` (já dev-dep no `-net`/`-image`) |
| read-modify-write sobre estado partilhado | teste de concorrência com `sleep` na janela — sem `flock` perde escritas |
| comportamento sob falha real | cenário em `scripts/chaos.sh` |
| superfície da CLI | verificação em `scripts/e2e.sh` (aceita expectativa **numérica**: `check <nome> 4 …`) |
| documentação/schema | os jobs de CI já existentes (`docs`, `chaos.yml`) |

**Dívida contada com ratchet, nunca com `<=`.** O teste de i18n de argumentos
falha se o número **subir** (flag nova sem tradução) **e se descer** (traduziu-se
e não se baixou a constante no mesmo commit). Um `<=` deixaria a dívida a ler-se
como verde para sempre.

## Regressão: como não partir o que já funcionava

- **Quando uma correcção faz um teste antigo falhar, a primeira hipótese é que o
  teste fixava o comportamento errado.** Aconteceu: das nove falhas da bateria
  E2E que não corria há 44 versões, **oito eram testes a codificar bugs já
  corrigidos**. Confirma qual dos dois está certo antes de mexer em qualquer um.
- **Um teste pode codificar o bug** de origem: `default_project_name_normaliza_o_
  directorio` afirmava exactamente o comportamento que colapsava projectos
  compose — passava só porque usava caminhos absolutos, e a invocação real é
  relativa. **Passa à função a forma que a produção lhe dá.**
- **Correr os gates DEPOIS da última edição.** Um gate verde antes do último
  commit não diz nada sobre o que foi comitado.
- **`$?` depois de um pipe é do último comando do pipe**, e um comando cancelado
  não é um comando que passou. Mede a coisa, não um proxy.
- **Ao apagar uma API pública**, a cascata privada é do compilador; a pública
  conta-se à mão — e se for uma **biblioteca**, «zero chamadores no workspace»
  não é o critério todo (o `delonix-paas` consome estes crates por tag).

## Formato do registo (curto, e é de propósito)

```markdown
### <a frase «X não é Y», ou o sintoma numa linha> (vX.Y.Z)

**Sintoma:** o que se viu.
**Causa:** o que era, medido — com o número.
**Regra:** o que perguntar da próxima vez, em código que ainda não existe.
**Gate:** <teste/cenário> — falha com a correcção revertida (verificado).
```

Se o achado tocou várias vezes na mesma armadilha, diz **qual ocorrência é**. É
essa contagem que faz mudar o processo, e não só a linha de código.
