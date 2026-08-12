---
name: delonix-paridade
description: Comparar o delonix com Docker e Podman sem trair o rootless/daemonless — o que já existe com outro nome, o que falta mesmo, o que é impossível por desenho (e porquê), e como medir as três ferramentas na mesma máquina no mesmo dia. Usa quando o utilizador pedir uma comparação com Docker/Podman, perguntar «porque é que o Docker tem X e nós não», ou propor uma feature justificada por paridade.
---

# Paridade com Docker e Podman — inferir ausência é o erro clássico

## Antes de escrever uma linha, lê o que já está medido

- `docs/comparacao-medida.md` — **cada linha foi executada**, nas três
  ferramentas, na mesma máquina, no mesmo dia. Inclui aquilo em que o Delonix é
  **pior**. É a fonte.
- `docs/paridade-docker-podman.md` — levantamento por `--help`, e o próprio
  documento começa por dizer que **não mediu nada**.
- `docs/COMPARACAO-DOCKER-PODMAN.md` — os achados de segurança e as falhas
  silenciosas encontradas por esta via.
- `docs/comparacao.html` **é GERADO** da constante `COMPARE` no `docs/gen.py`
  (≈linha 1125). Nunca o edites à mão: uma correcção manual foi revertida por uma
  regeneração e devolveu ao site afirmações de segurança falsas.

## O erro que este documento existe para não repetir

A primeira versão da análise de paridade afirmava que faltavam ao Delonix as
restart policies, o `restart`, os health checks, os limites de recursos e o
registo em registries privados. **Estava errada em quase tudo — todos existem.**
E concluiu que o DNS «não funcionava» porque foi preciso fixar IPs, quando há um
servidor DNS embutido a escutar `:53` em todas as bridges.

**Inferir ausência a partir de um sintoma, sem ir ver.** É o erro, e repete-se
com facilidade porque os nomes diferem. Antes de escrever «falta X»:

```bash
./target/debug/delonix <grupo> --help | grep -i <conceito>
grep -rn "<conceito>" crates/delonix-runtime-bin/src/cmd/
```

## As três categorias — e a diferença entre elas é tudo

1. **Existe com outro nome.** Não é gap. É documentação, ou um alias. Exemplo:
   o firewall L4 por-container faz o trabalho de várias coisas que no Docker
   vivem noutro sítio.
2. **Falta mesmo, e é fazível rootless/daemonless.** É gap real → entra no
   relatório com o custo estimado.
3. **É impossível SEM atravessar a fronteira** (daemon permanente, root no
   init-netns do host, socket global). Não é gap: é **decisão**, e o sítio dela é
   um ADR ou uma limitação documentada — nunca uma feature a copiar. Exemplos
   reais: `macvlan`/`ipvlan` realizados fisicamente precisam de `CAP_NET_ADMIN` na
   init-netns; `exec` interactivo na API Docker precisa de HTTP hijacking; um
   supervisor por-container é o modelo do conmon e muda a filosofia.

**Uma feature justificada só por «o Docker tem» não passa.** Passa a que tem um
utilizador com um problema concreto.

## O que o Delonix tem e eles não — e não se sacrifica

**Reconfigurar portas, volumes, redes, memória e CPU a QUENTE sem mudar o PID**
(`container update`). No Docker, mudar uma porta obriga a recriar. É o argumento
mais forte que este produto tem, e vem de o dataplane não pertencer ao ciclo de
vida do processo. **Uma feature que quebre essa propriedade custa mais do que
traz** — di-lo na análise, com essas palavras.

Na mesma família: rootless sem daemon nenhum, `stack plan`/`apply` convergente
sem ficheiro de estado, e o tecto de capabilities do CRI aplicado no nó.

## Como medir as três na mesma máquina

**Não instales o daemon do Docker neste host.** Medido hoje: o CLI do `docker`
está presente (29.7.2) mas o daemon **não está acessível** (aponta para um socket
de Docker Desktop que não existe); o `podman` está e é 4.9.3, rootless. Este host
tem produção a correr — instalar um daemon aqui é mexer numa máquina que não é
laboratório.

O caminho já usado, e é o correcto: **uma VM criada pelo próprio motor**
(`delonix vm create`), com as três lá dentro. Foi assim que o
`comparacao-medida.md` foi feito, e é por isso que ele vale. Regras da corrida:

- **Mesma máquina, mesmo dia, mesma imagem de teste.** Colunas medidas em sítios
  diferentes não se comparam.
- **Rootless nos três**, ou diz-se claramente que o Docker correu com daemon
  root — senão a comparação é entre modelos de privilégio, não entre ferramentas.
- **Uma célula sem medição diz `não verificado`**, nunca uma afirmação.
- **Primeira invocação vs. quente**: um pull frio contra um cache quente é a
  diferença entre duas ordens de grandeza e não mede o mesmo.

## O que comparar (e o que não vale a pena)

Vale: latência de `run` até ao processo vivo, `exec`, publish de porta e caminho
real do tráfego, tempo de `build` da mesma Dockerfile, tamanho em disco por
container, comportamento a N containers, o que acontece quando se mata o
plano de controlo, e o que cada um faz com uma opção que não suporta.

Esta última é a mais reveladora e quase ninguém a mede: **aceita e ignora, ou
recusa?** Este repo trata aceitar-e-ignorar como a pior falha que existe, e é um
eixo de comparação legítimo.

Não vale: contagem de subcomandos, número de estrelas, ou features de
orquestração de frota — essas são do PaaS, não deste repo (guarda-rio #2).

## Antes de dar por feito

Actualiza a constante `COMPARE` no `gen.py` (não o HTML) e regenera; confirma com
`git diff docs/comparacao.html` que só mudou o que era suposto. Se a corrida
produziu números novos, entram no `comparacao-medida.md` com a data e a máquina.
E diz o que **não** foi verificado — o documento-fonte começa exactamente assim,
e é por isso que se pode confiar nele.
