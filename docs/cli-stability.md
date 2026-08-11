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
* **Os códigos de saída** — ver a secção «Códigos de saída» abaixo.
* **A saída JSON de `inspect`** — campos podem ser ACRESCENTADOS, nunca removidos
  nem com o tipo mudado.
* **Os atalhos de topo** (`ps`, `run`, `exec`, `logs`, `rm`, `images`), que são
  literalmente o mesmo comando por reescrita de argv.

## Códigos de saída

> **Alteração de contrato na v0.49.0.** Até à v0.48.0 **toda** a falha do motor
> era `1` — «não existe» e «rebentou» eram o mesmo número. Continua a valer que
> `0` é sucesso e não-zero é falha, portanto um `if delonix …; then` não muda de
> comportamento; o que muda é para quem testa `[ $? -eq 1 ]` **à espera de uma
> falha específica**. Ver a nota de migração no fim desta secção.

| código | significado |
|---|---|
| `0` | sucesso |
| `1` | falha sem classe própria (o default de sempre) |
| `2` | uso inválido (o do `clap`) — e, em `stack plan --detailed-exitcode`, «há alterações» |
| `3` | o recurso existe mas **não está a correr** |
| `4` | **não existe** esse recurso |
| `5` | **conflito** — o nome já está tomado |

`3` e `4` não são números inventados: são os códigos de estado do LSB que o
`systemctl` ainda fala (`3` = o programa não está a correr, `4` = não há tal
unidade). `5` não tem convenção por trás — é o número livre seguinte, abaixo da
gama que a shell usa (`126`/`127`, e `128+N` para sinais).

O que isto resolve é concreto: um reconciliador — o `Makefile`, o passo de CI, o
ciclo em bash que conduz esta CLI — não conseguia separar «cria, porque falta»
de «pára, porque falhou» sem ler a MENSAGEM de erro. E a mensagem é traduzida
(`--l18n=pt`), portanto um script que faz `grep 'no such'` funciona na máquina
onde foi escrito e deixa de classificar num nó com outra locale.

```bash
delonix container inspect web >/dev/null 2>&1
case $? in
  0) ;;                       # está lá
  4) delonix container run -d --name web nginx ;;   # falta — cria
  3) delonix container start web ;;                 # existe, parado — arranca
  *) exit 1 ;;                # qualquer outra coisa: pára, não adivinhes
esac
```

**O código de um `run`/`exec` continua a ser o do WORKLOAD, não o do motor.**
`run` em primeiro plano devolve o código do processo do container, `exec` o do
comando, e `healthcheck` sai `1` quando não saudável — as três promessas de
sempre, inalteradas. A consequência a reter: um container que saia `4` sai `4`,
logo **o `$?` de um `run`/`exec` nunca se lê como uma das classes acima** — esse
número foi escolhido pelo workload, não pelo motor.

**O que fica de fora**, e é honesto dizê-lo: uma pré-condição do host por
satisfazer (uma sessão sem delegação de cgroup, por exemplo) não tem código
próprio — o motor avisa e continua, portanto não há falha para classificar. E
os grupos ainda não estáveis (ver a lista mais abaixo) podem responder `1` onde
um comando estável já responde `4`: o `workload describe` de um nome que não
existe é o caso conhecido.

### Migrar

* `if delonix …; then` / `|| exit 1` / `set -e` — **nada muda**.
* `[ $? -eq 1 ]` a testar «este comando falhou» — passa a ser `[ $? -ne 0 ]`.
* `[ $? -eq 1 ]` a testar «não existe» — passa a ser `[ $? -eq 4 ]`, que é o
  teste que antes não havia maneira de escrever.

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

## O schema dos manifestos — estável, e é o que mais importa

> Esta secção **substitui** a linha que dizia que o schema «é aditivo na prática,
> mas não é uma promessa». Era o compromisso ao contrário: a CLI estava mais
> protegida que o formato declarativo, quando é o formato que as pessoas põem em
> git e revêem em PR. Um `delonix-manifest.yaml` só se versiona se se souber que
> não parte sozinho.

Para os Kinds com spec tipado — **`Container`, `Pod`, `Volume`, `Network`** —
garante-se, dentro do `0.x`:

* **Um campo nunca é removido, nem muda de tipo, nem muda de significado.**
* **Um campo novo é sempre opcional** e tem um default que preserva o
  comportamento anterior. Um manifesto escrito hoje continua a fazer o mesmo
  amanhã.
* **Um nome antigo que seja renomeado mantém-se aceite como alias** (é o que já
  acontece com `restart`→`restartPolicy`, `options`→`mountOptions`,
  `wg_ip`→`wgIp`).
* **`apiVersion: delonix.io/v1` só muda com um `v2`**, e um `v2` não sai sem o
  `v1` continuar a ser aceite.

A verdade não é este texto, é o schema: **`delonix schema print`** emite-o a
partir do próprio código (ADR-0007), e o mesmo ficheiro está publicado em
[`schema/v1/delonix.json`](schema/v1/delonix.json). Um teste do repositório
falha se o publicado deixar de ser o gerado, precisamente para esta página não
voltar a poder mentir.

Aponta o editor e escreve manifestos com completação e validação:

```yaml
# yaml-language-server: $schema=https://angolardevops.github.io/delonix-runtime/schema/v1/delonix.json
apiVersion: delonix.io/v1
kind: Container
metadata: { name: web }
spec:
  image: nginx:1.27
```

Para saber o que mudou entre duas versões, não há uma página escrita à mão —
haveria a segunda fonte de verdade que a ADR-0007 aboliu. Há um comando:

```bash
scripts/schema-diff.sh v0.46.0          # dessa tag até à árvore actual
scripts/schema-diff.sh v0.46.0 v0.47.0  # entre duas tags
```

Compara campo a campo (nome e tipo), não o JSON cru, e sai **1** quando há
diferenças — serve directamente como gate de CI. Um campo removido ou com o
tipo mudado é assinalado como **quebra de contrato**, que é o que esta secção
promete não acontecer.

**O que fica de fora, e é honesto dizê-lo:** os restantes Kinds
(`Vm`, `Cluster`, `ShareVolume`, `Image`, `Secret`, `Ingress`,
`FirewallPolicy`, `HTTPRoute`, `Tunnel`, `Workload`, `Stack`) ainda não têm
schema gerado, e por isso continuam sem promessa. O `delonix schema`/`explain`
diz quais são, em vez de os omitir.

> **Três Kinds deixaram de existir** nesta série, fundidos no que já faziam:
> `Egress` → `FirewallPolicy` com `direction: egress`; `Dependency` →
> `FirewallPolicy` (açúcar, reduzido no load); `Storage` → `kind: Volume` com um
> bloco `nfs:`/`cifs:`/`webdav:`. Os nomes antigos continuam a carregar, com
> aviso de depreciação — a regra do «corte limpo» aplica-se a comandos, e um
> manifesto em git merece um degrau em vez de um erro.

## NÃO estável — pode mudar em qualquer versão

* **`serve cri`, `serve api`, `serve docker-api`** — superfícies de protocolo
  em construção. O `docker-api` publica a sua cobertura em
  `delonix serve docker-api --matrix`, e é essa tabela que diz o que existe
  hoje, não esta promessa. A **API de gestão** (`serve api`) é local (socket
  unix, só o próprio uid) e não tem contrato publicado: não construas automação
  sobre ela — para isso existe a CLI, com `-o json`.
* **`cluster`, `vm`, `pod`, `workload`, `storage`, `sharevolume`, `net`** —
  a superfície ainda está a assentar. (O *schema* de `kind: Pod` é estável, ver
  acima; o que não é estável é o grupo de comandos `delonix pod`.)
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
