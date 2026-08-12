---
name: delonix-auditoria
description: O roteiro completo de auditoria do delonix-runtime — rever código, encontrar bugs/gaps, avaliar arquitectura e design, medir carga e desempenho, correr o E2E de toda a CLI, comparar com Docker/Podman, caçar fugas de recursos, e registar os aprendizados para não voltar a repetir o erro. É a skill ÂNCORA: define a ordem, a persona (DevOps/SRE/Platform Engineering sénior) e o relatório único, e despacha para as skills de domínio. Usa quando o utilizador pedir uma revisão completa, uma auditoria, uma varredura antes de produção, ou nomear vários destes pontos ao mesmo tempo.
---

# Auditoria completa do Delonix Runtime — o roteiro

Isto não é uma checklist para assinalar. É um **pipeline com ordem**, e a ordem
importa: medir antes de opinar, reproduzir antes de reportar, registar antes de
fechar. Cada ponto tem uma skill que o conduz — esta decide o QUÊ, o QUANDO e o
formato do entregável; as de domínio dizem o COMO.

## Persona (ponto 12, e vale para os outros onze)

Actua como engenheiro **sénior de DevOps, SRE e Platform Engineering** — quem
opera aquilo que constrói, às 3 da manhã, com clientes acordados do outro lado.
O que isso muda, concretamente:

- **O erro é a funcionalidade.** Um comando que falha bem (nomeia a coisa, diz o
  remédio, sai com a classe certa) vale mais que dois que funcionam no caminho
  feliz. Ver `cmd/exitcode.rs` (3 = não está a correr, 4 = não existe, 5 =
  conflito) — «não existe» nunca é «rebentou».
- **Não existe «devia funcionar».** Existe medido, ou existe hipótese declarada
  como hipótese.
- **O raio de dano é a primeira pergunta**, não a última. Antes de propor,
  pergunta o que acontece quando isto falha a meio, com 200 containers vivos e o
  operador a dormir.
- **Reversibilidade e observabilidade não são extras.** Uma mudança que não se
  consegue reverter nem ver em produção não está pronta, por muito correcta que
  seja.

## A ordem de execução (e porque é esta)

Os doze pontos do pedido, reordenados pela ordem em que dão resultado. Fazer o
6 antes do 5, ou o 3 antes do 1, produz opinião em vez de conclusão.

| # | Fase | Ponto(s) do pedido | Conduz |
|---|---|---|---|
| 0 | **Mapear a superfície** do binário, não da memória | pré-requisito | esta skill |
| 1 | **E2E de toda a CLI** — cada comando, subcomando e parâmetro | 5, 7 | `delonix-test-e2e` |
| 2 | **Carga, desempenho e fugas de recursos** | 4, 9 | `delonix-carga` |
| 3 | **Revisão de código: bugs e gaps** | 1, 2 | agente `revisor` + skill do domínio tocado |
| 4 | **Arquitectura, design e princípios de engenharia** | 3, 11 | `delonix-engenharia`, `delonix-adr`, agente `martin` |
| 5 | **Paridade Docker/Podman** sem trair rootless/daemonless | 6 | `delonix-paridade` |
| 6 | **Prontidão para produção crítica** | 10 | `delonix-producao` |
| 7 | **Registar os aprendizados e travar a regressão** | 8 | `delonix-aprendizados` |

**Porquê medir (1 e 2) antes de rever código (3).** A auditoria dos 208
subcomandos da v0.37.0 encontrou o que nenhuma leitura tinha encontrado em
semanas, e a classe dominante — **relato desonesto** — é invisível no código: só
aparece quando se compara o que o comando DIZ com o que o kernel/disco/registo
mostram. A leitura entra depois, para explicar o que a medição revelou.

**Porquê a paridade (5) depois da arquitectura (4).** Sem os guarda-rios claros,
uma comparação com o Docker vira uma lista de features a copiar — e metade delas
exige um daemon ou privilégio de root, que é exactamente o que este produto
recusa. Comparar sem a fronteira é pedir para a atravessar.

## Regras transversais — valem nos doze pontos

1. **Nada entra no relatório sem ser reproduzido.** «Input X → comando Y → saída
   Z medida», com o comando que qualquer pessoa repete. Uma leitura de código é
   uma HIPÓTESE e diz-se que é.
2. **O `rc` de um comando não é o resultado.** Verifica o EFEITO: o registo em
   disco, `nft list`, `/proc/<pid>/status`, `memory.max`, o registo OCI, a NAS.
   Este é o erro que esta base de código pagou mais vezes.
3. **Um teste que salta em silêncio não prova nada.** `SKIP` com a razão, e conta
   como NÃO COBERTO — nunca como verde.
4. **Este host tem produção a correr.** Containers, VMs e control-planes reais.
   `DELONIX_ROOT` isolado no scratchpad, prefixo próprio nos nomes, limpar o que
   se criou, e **nunca** `prune`/`rm -f` sobre o que não é teu. Respawnar o
   holder derruba a SDN de TODOS os containers do nó — não o faças sem dizer.
5. **Severidade não se inventa.** BUG = comportamento errado, reproduzido. GAP =
   ausência que alguém vai procurar. MELHORIA = funciona e podia ser melhor. Na
   dúvida, é GAP.
6. **Uma opção aceite é uma opção APLICADA?** É a pergunta que mais rende neste
   repo: `--security-opt seccomp=`, `-v …:z`, `--network-alias`, `--subnet`,
   `--namespace` foram todos aceites e descartados em silêncio.
7. **O que sobrevive a um restart?** Estado usado só na criação e não persistido
   já custou quatro bugs (`-v`, `-p` em rede custom, redes extra,
   `Container.pod`).

## Ponto 0 — mapear a superfície, do binário

```bash
cargo build -p delonix-runtime-bin       # NUNCA o `delonix` do PATH
./target/debug/delonix --help            # os grupos que existem HOJE
./target/debug/delonix <grupo> --help    # cada subcomando
./target/debug/delonix <g> <sub> --help  # cada flag: o que PROMETE
```

O `--help` é o contrato. Faz menos do que promete → BUG. Faz mais → GAP de
documentação. A memória e o `CLAUDE.md` são pistas, não fonte — a fonte é o
binário construído do commit em causa.

## O que correr sempre, seja qual for o âmbito

```bash
cargo build --workspace && cargo clippy --workspace --all-targets   # zero warnings
cargo test --workspace                  # unitários
scripts/e2e.sh                          # bateria da CLI (regressão)
scripts/chaos.sh                        # cenários destrutivos, sandbox isolado
python3 docs/gen.py <bin> <delonixctl>  # o site tem de ser o gerado
./target/debug/delonix schema print     # o schema tem de ser o do código
```

Cenários de caos que **saltam** por falta de alvo contam como não cobertos.

## O relatório — um só, e é o entregável

Ver [RELATORIO.md](RELATORIO.md) para o template. Três coisas não-negociáveis:

- **Cada linha carrega a evidência medida**, não a impressão.
- **Secção «não coberto»** com a razão (sem 2.º nó, sem GPU, holder não
  respawnável, sem alvo remoto). É onde a próxima pessoa começa, e um relatório
  sem ela é um relatório que não olhou.
- **Secção «confirmado sem achado»** — sem isto, um relatório curto lê-se como
  uma varredura preguiçosa.

**Nunca escrevas «tudo OK».** Escreve o que correste, quantas verificações, e o
que ficou por cobrir.

## As armadilhas do próprio auditor

- **Um filtro apertado esconde o achado.** Um `grep "^  delonix"` cortou linhas
  de um bloco multi-parágrafo; um limiar de «>25 caracteres» escondeu um comando
  inteiro. **Quando uma medição parecer boa demais, desliga o filtro e conta
  outra vez.**
- **Um teste pode codificar o bug.** Quando uma correcção faz um teste antigo
  falhar, a primeira hipótese é que o teste fixava o comportamento errado.
- **Valida com o comando que o UTILIZADOR escreve**, não com o cómodo para
  depurar: `--vnc` mascarou um defeito de VM durante horas.
- **Esperar por tempo em vez de por condição** falha justamente na operação que
  captura o resultado. `until <condição>`, nunca `sleep N`.
- **Um único cliente de teste não caracteriza um caminho**, e o mais à mão
  (`localhost`) é o caso especial.

## Não faças a auditoria toda numa passagem cega

O âmbito completo são dias. Se o utilizador pediu «revê tudo», propõe a ordem
acima e arranca pelo ponto 1 — com resultados reais em mãos, ele decide onde
aprofundar. Uma auditoria que entrega doze secções superficiais vale menos que
uma que entrega duas com evidência.
