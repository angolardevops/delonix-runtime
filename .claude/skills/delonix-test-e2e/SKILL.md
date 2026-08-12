---
name: delonix-test-e2e
description: Prova de conceito do delonix — exercita a CLI inteira (todos os grupos, subcomandos e parâmetros) contra o binário real, e devolve um relatório de BUGs, GAPs e melhorias, cada um com a razão e a evidência medida. Usa quando o utilizador pedir «testa tudo», uma varredura antes de uma release, ou um levantamento do que falta para produção.
---

# Varredura E2E — o relatório só vale o que foi medido

Isto NÃO é «correr o `scripts/e2e.sh` e reportar o número». Isso é um gate de
regressão e corre-se na mesma (é barato e apanha o que já se sabia). Isto é a
varredura que **descobre o que ninguém sabia**, e a auditoria dos 208
subcomandos da v0.37.0 mostrou o que ela encontra: a classe dominante não foi
«comando em falta» nem «comando errado» — foi **relato desonesto**.

## Regras não-negociáveis

1. **Nada entra no relatório sem ser reproduzido.** Um achado é «input X →
   comando Y → saída Z medida», com o comando que qualquer pessoa pode repetir.
   Uma leitura de código é uma HIPÓTESE, e diz-se que é.
2. **O `rc` de um comando não é o resultado.** Foi este o erro que esta base de
   código pagou mais vezes. Verifica o EFEITO: o registo em disco, o kernel
   (`nft list`, `/proc/<pid>/status`, `memory.max`), o registo OCI, a NAS.
3. **Um teste que salta em silêncio não prova nada.** Se um caminho precisa de
   um alvo que não existe aqui, escreve-se `SKIP` com a razão — e conta como
   NÃO COBERTO no relatório, nunca como verde.
4. **Não inventes severidade.** BUG = comportamento errado, reproduzido. GAP =
   ausência que alguém vai procurar. MELHORIA = funciona e podia ser melhor. Se
   não sabes distinguir, é GAP.
5. **Não toques em trabalho vivo.** Este host costuma ter containers e VMs a
   sério. Usa `DELONIX_ROOT` isolado num scratchpad, nomes com prefixo próprio,
   e limpa o que criaste. Respawnar o holder derruba a rede de TODOS os
   containers do nó — não o faças sem dizer.

## Como varrer, e por que ordem

**Primeiro o mapa, do binário e não da memória:**

```bash
delonix --help                     # os grupos que existem HOJE
delonix <grupo> --help             # cada subcomando
delonix <grupo> <sub> --help       # cada flag: o que promete
```

O `--help` é o contrato. Um comando que faz menos do que promete é BUG; um que
faz mais é GAP de documentação.

**Depois, por grupo, as quatro perguntas que encontram tudo:**

1. **O caminho feliz faz o que diz?** (e prova-se pelo efeito, não pela saída)
2. **O caminho de erro diz a verdade?** Nome inexistente, argumento inválido,
   pré-condição em falta — o erro nomeia a coisa certa? O exit code distingue
   «não existe» de «rebentou»? (há classes: 3, 4, 5 — ver `cmd/exitcode.rs`)
3. **Uma opção aceite é uma opção APLICADA?** Esta é a que rende mais. Correr o
   comando com a flag e ir ver se o efeito lá está. Este repo já teve
   `--security-opt seccomp=`, `-v …:z`, `--network-alias`, `--subnet` e
   `--namespace` aceites e descartados em silêncio.
4. **O que sobrevive a um restart?** Cria com todas as flags, `stop`, `start`, e
   compara. Estado usado só na criação e não persistido já custou quatro bugs.

**E as combinações que ninguém testa**: dois recursos com o mesmo nome, um lote
misto (`rm a b` onde só `a` existe), remoção com referências vivas, aplicar o
mesmo manifesto duas vezes, `--dry-run` alimentado de volta ao próprio apply.

## As armadilhas do próprio testador

- **Um filtro apertado esconde o achado.** Um `grep "^  delonix"` cortou linhas
  de um bloco multi-parágrafo; um limiar de «mais de 25 caracteres» escondeu um
  comando inteiro. **Quando uma medição parecer boa demais, desliga o filtro e
  conta outra vez.**
- **Um teste pode codificar o bug.** Quando uma correcção faz um teste antigo
  falhar, a primeira hipótese é que o teste fixava o comportamento errado.
- **Valida com o comando que o UTILIZADOR escreve**, não com o que é cómodo para
  depurar. Uma flag de conveniência (`--vnc`) já mascarou um defeito durante
  horas.
- **Esperar por tempo em vez de por condição** falha na operação que captura o
  resultado. `until <condição>`, nunca `sleep N`.

## O relatório

Uma tabela, mais severo primeiro, e **cada linha carrega a evidência**:

| # | Classe | Grupo | O que se mediu | Porque importa |
|---|---|---|---|---|
| 1 | BUG | `container` | `<comando>` → `<saída real>`; esperado `<X>` | consequência concreta |

Depois, três secções curtas:

- **Não coberto** — o que não foi possível exercitar aqui e porquê (sem alvo
  remoto, sem privilégio, sem hardware). Isto é tão importante como os achados:
  é onde a próxima pessoa começa.
- **Confirmado sem achado** — o que foi exercitado e está bem. Sem isto, um
  relatório curto lê-se como uma varredura preguiçosa.
- **Recomendação** — o que fechar antes de produção, por ordem de risco, e o que
  é decisão de desenho e não trabalho.

**Nunca escrevas «tudo OK».** Escreve o que correste, quantas verificações, e o
que ficou por cobrir. Um relatório sem secção de não-coberto é um relatório que
não olhou.

## O que correr sempre, além da varredura

```bash
cargo test --workspace          # unitários
scripts/e2e.sh                  # a bateria da CLI (regressão)
scripts/chaos.sh                # 20 cenários (sandbox isolado, seguro)
python3 docs/gen.py             # o site tem de ser o gerado
delonix schema print            # o schema tem de ser o do código
```

Os cenários de caos que SALTAM por falta de alvo contam como não cobertos.

## No roteiro de auditoria

É o motor dos pontos **5 e 7** (E2E completo, e cada comando/subcomando/
parâmetro) e a principal fonte do **2** (bugs e gaps) — a auditoria dos 208
subcomandos encontrou por esta via o que semanas de leitura não tinham
encontrado. **Corre-se PRIMEIRO** no roteiro (`delonix-auditoria`), antes da
revisão de código: a medição diz onde olhar. O que esta varredura não faz:
carga e fugas (`delonix-carga`), comparação com Docker/Podman (`delonix-paridade`)
e o registo do aprendizado (`delonix-aprendizados`).
