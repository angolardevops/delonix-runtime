# Promessa de estabilidade da CLI

> Aplica-se a partir da **v0.42.3**, e vale dentro do `0.x`.

Um motor sem contrato não se automatiza. Quem escreve um `Makefile`, um passo de
CI ou um script de deploy precisa de saber o que pode partir num upgrade — e a
resposta «é 0.x, tudo pode partir» é verdadeira e inútil: garante que ninguém
depende de nada, o que é o mesmo que ninguém adoptar.

Isto não é semver 1.0. É a lista do que se compromete e do que não se
compromete, que é o que falta a maior parte dos projectos em 0.x.

## Estável — não quebra sem um major

**Os verbos de ciclo de vida de container**, com os nomes e a semântica que
Docker e Podman lhes dão:

```
container run   ps   stop   start   restart   kill   rm   exec   logs
                wait   inspect   port   rename   pause   unpause
image     pull  ls   rm    build (delonix build)
```

Concretamente, garante-se:

* **O nome do comando e a ordem dos argumentos posicionais.**
* **As flags curtas e longas listadas acima e os seus significados** — `-d`,
  `-p`, `-v`, `-e`, `--name`, `--rm`, `-i`, `-t`, `--net`, `--restart`,
  `--memory`, `--cpus`, `--entrypoint`, `-w`, `-u`, `--add-host`, `--wait`,
  `--health-*`.
* **Os códigos de saída**: `run` em primeiro plano devolve o código do próprio
  workload; `exec` o do comando; `healthcheck` sai 1 quando não saudável.
* **A saída JSON de `inspect`** — campos podem ser ACRESCENTADOS, nunca removidos
  nem com o tipo mudado.
* **Os atalhos de topo** (`ps`, `run`, `exec`, `logs`, `rm`, `images`), que são
  literalmente o mesmo comando por reescrita de argv.

## Estável em conteúdo, não em formato

**As tabelas de `ls`/`ps`.** As colunas podem mudar de largura, de ordem ou
ganhar irmãs — são feitas para humanos e medem-se pelo conteúdo real. Um script
que faça `awk '{print $3}'` sobre elas parte, e isso não conta como quebra de
contrato.

**Para automação há `-o json`**, e é ele que é estável: um array JSON por
recurso, campos podem ser ACRESCENTADOS mas não removidos nem com o tipo mudado
(ADR-0005). Verificado a funcionar nos nove comandos de listagem — `container
ps`, `image ls`, `volumes ls`, `network ls`, `vm ls`, `pod ls`, `secret ls`,
`storage ls`, `workload ls`. Também `inspect` e `-q`/`--quiet`.

> Uma versão anterior deste documento dizia que `-o json` estava «por fazer e é a
> lacuna reconhecida aqui». **Estava errado** — existe desde a ADR-0005. É
> exactamente o erro que o `paridade-docker-podman.md` abre por corrigir: inferir
> ausência a partir de um sintoma, sem ir ver.

## NÃO estável — pode mudar em qualquer versão

* **`serve cri`, `serve api`, `serve docker-api`** — superfícies de protocolo
  em construção. O `docker-api` publica a sua cobertura em
  `delonix serve docker-api --matrix`, e é essa tabela que diz o que existe
  hoje, não esta promessa.
* **`cluster`, `vm`, `pod`, `workload`, `storage`, `sharevolume`, `net`** —
  a superfície ainda está a assentar.
* **O schema dos manifestos** (`kind: *`). Campos são aditivos na prática, mas
  não é uma promessa.
* **Tudo o que começa por `net netns`** — plumbing interno exposto por
  conveniência de depuração.
* **O formato dos ficheiros de estado** em `$DELONIX_ROOT`. Lê-se pelo `inspect`,
  nunca do disco.

## Como uma quebra é feita, quando tem de acontecer

Precedente já cumprido pelo projecto: a reorganização da v0.30.0 (`netns` →
`net netns`, `cri` → `serve cri`, …) foi um **corte limpo, sem aliases** — a
forma antiga falha com «unrecognized subcommand», nunca em silêncio.

Isso mantém-se como regra: **falhar alto**. Um alias de compatibilidade que
muda de comportamento é pior que um erro.

E uma lição que custou esta sessão: essa quebra deixou um chamador INTERNO por
actualizar (o `delonix-cri` continuou a invocar `delonix netns attach`), o que
partiu a criação de pod rootless durante meses — ver
[cri-conformance.md](cri-conformance.md). Um corte limpo obriga a fazer o grep
por chamadores em TODO o workspace, não só na documentação.
