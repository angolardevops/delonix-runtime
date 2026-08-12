# Template do relatório de auditoria

Um relatório por auditoria, em `docs/discovery/NN_<ASSUNTO>.md` (a convenção que
o repo já usa) ou no corpo da resposta se o âmbito for pequeno. **Cada linha
carrega a evidência medida.**

---

## Cabeçalho — sem isto o relatório não é verificável

| Campo | Valor |
|---|---|
| Data | YYYY-MM-DD |
| Binário | `delonix X.Y.Z` (`git rev-parse --short HEAD`) |
| Host | distro, kernel, rootless/root, cgroup delegado sim/não |
| Âmbito | que pontos do roteiro foram corridos, e quais NÃO |
| Isolamento | `DELONIX_ROOT` usado, prefixo dos nomes criados |

## 1. Achados

Mais severo primeiro. Uma linha por achado, e o comando tem de ser repetível.

| # | Classe | Grupo/crate | O que se mediu | Esperado | Porque importa |
|---|---|---|---|---|---|
| 1 | BUG | `container` | `<comando>` → `<saída real>` | `<X>` | consequência concreta em produção |
| 2 | GAP | `net` | … | … | … |

**Classes:** BUG (comportamento errado, reproduzido) · GAP (ausência que alguém
vai procurar) · MELHORIA (funciona e podia ser melhor) · RISCO (não falhou aqui,
mas falha sob carga/concorrência/falha parcial).

Para CRÍTICO/ALTO, um bloco por achado com: reprodução passo a passo, o efeito
observado no kernel/disco/registo, e a correcção concreta proposta (não «isto é
perigoso»).

## 2. Desempenho e recursos

Números, com a linha de base ao lado. Sem base, um número não diz nada.

| Métrica | Base | Medido | Delta | Como se mediu |
|---|---|---|---|---|
| latência `container run` (p50/p95) | | | | |
| RSS do pin/controlo após N ciclos | | | | |
| fds do processo de longa vida | | | | |
| entradas nft / regras por pacote | | | | |
| disco não devolvido após limpeza | | | | |

## 3. Não coberto — e porquê

O mais importante da lista, porque é onde a próxima sessão começa.

- `<caminho>` — sem `<pré-requisito>` neste host (2.º nó, GPU, alvo remoto,
  privilégio, holder não respawnável com produção viva).

## 4. Confirmado sem achado

O que foi exercitado e está bem. Sem esta secção, um relatório curto lê-se como
uma varredura preguiçosa.

## 5. Recomendação

Por ordem de risco:

1. **Fechar antes de produção** — …
2. **Fechar antes da próxima release** — …
3. **Decisão de desenho, não trabalho** — … (e a razão de ficar assim)

## 6. Aprendizados a registar

O que passa para o `CLAUDE.md`/memória e que teste trava a regressão. Ver a
skill `delonix-aprendizados`. Um achado corrigido sem teste que falhe com a
correcção revertida **não está fechado**.
