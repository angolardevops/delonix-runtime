# Promessa de estabilidade da CLI

> Aplica-se a partir da **v0.42.3**. Escrita para o `0.x` — cada quebra listada
> abaixo tinha de esperar por um major. **A v1.0.0 é esse major**: fechou a
> única quebra pendente (os atalhos de topo), e esta lista passa a valer como a
> promessa de semver real de resto em diante — um breaking change deixa de
> caber num `1.x`.

Um motor sem contrato não se automatiza. Quem escreve um `Makefile`, um passo de
CI ou um script de deploy precisa de saber o que pode partir num upgrade — e a
resposta «é 0.x, tudo pode partir» é verdadeira e inútil: garante que ninguém
depende de nada, o que é o mesmo que ninguém adoptar.

Isto listava o que se compromete e o que não se compromete dentro do `0.x`,
que é o que falta a maior parte dos projectos nessa fase. Desde a v1.0.0 é
mais do que isso: é o contrato de semver do projecto.

## Estável — não quebra sem um major

**Os verbos de ciclo de vida de container**, com os nomes e a semântica que
Docker e Podman lhes dão:

```
container run   ps   stop   start   restart   kill   rm   exec   logs
                wait   inspect   port   rename   pause   unpause
image     pull  list  remove    build (delonix build)
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

> **Quebra de contrato na v1.0.0.** Até à v0.69.0 os atalhos de topo (`ps`,
> `run`, `exec`, `logs`, `rm`, `images`) estavam aqui, como reescrita de argv
> para `container <verbo>`/`image list`. Saíram — corte limpo, sem alias: a
> grafia antiga falha com `unrecognized subcommand`, nunca em silêncio, a
> mesma regra que a reorganização da v0.30.0 já seguia. Os grupos continuam:
> `delonix container ps`, `delonix container run`, `delonix container exec`,
> `delonix container logs`, `delonix container rm`, `delonix image list`.
> **Os códigos de saída não mudam nesta versão** — ver a secção abaixo.

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
| `69` | **capacidade que este host não tem** — uma ferramenta por instalar, um backend indisponível |
| `74` | **o sistema de ficheiros disse que não** — disco cheio, caminho impossível |
| `77` | **permissão negada** — o remédio é uma permissão, e depois repetir |
| `124` | **o prazo esgotou-se** — `stack wait --timeout`, e o que vier a ter prazo |

`3` e `4` não são números inventados: são os códigos de estado do LSB que o
`systemctl` ainda fala (`3` = o programa não está a correr, `4` = não há tal
unidade). `5` não tem convenção por trás — é o número livre seguinte, abaixo da
gama que a shell usa (`126`/`127`, e `128+N` para sinais).

**`74` e `77` saíram do balde do `1`, e não de nenhuma classe publicada.** Até
à v0.66.1 uma falha de I/O respondia `1`, que a linha de cima descreve como
«falha sem classe própria» — dar-lhe classe é o que esta tabela existe para
fazer, e um script com `*) exit 1` continua a cair no mesmo ramo. O `77` é
recortado do `74` pelo `kind` do erro, porque é assim que ele chega: medido
contra um state root sem bit de escrita, `volumes create` e `secret create`
vêm ambos como permissão negada. É a falha mais accionável que existe — corrige
a permissão e repete — e respondia o mesmo número que um disco cheio.

**`69` e `124` também não.** `69` é o `EX_UNAVAILABLE` do `sysexits.h`; `124` é
o que o `timeout(1)` devolve quando o prazo passa, e está portanto já nos dedos
de quem embrulha um comando num. Entraram porque tinham **produtores reais mal
classificados**, não para completar uma tabela: um `stack wait` que esgotava o
tempo respondia `1` — o mesmo número de um apply rebentado, no comando cuja
função inteira é ser lido por CI — e um `wg`/`virt-customize`/`ngrok` em falta
respondia `1` também, indistinguível de um erro de escrita numa flag. As duas
chamadas seguintes de um reconciliador são opostas: esperar mais, ou parar e
instalar alguma coisa.

```bash
delonix stack wait -f delonix-manifest.yaml --timeout 120
case $? in
  0)   ;;                     # tudo de pé
  124) exit 0 ;;              # ainda a subir — o pipeline seguinte volta a tentar
  69)  echo "falta uma ferramenta neste nó" >&2; exit 1 ;;
  *)   exit 1 ;;              # qualquer outra coisa: pára
esac
```

### A identidade textual: `DX_*`

O número serve quem lê `$?`. Quem lê **texto** — um cliente HTTP, um consumidor
de `-o json`, um pipeline de logs — tem o código `DX_*`, que é a mesma
classificação noutra grafia:

| `DX_*` | código | quando |
|---|---|---|
| `DX_NOT_FOUND` | `4` | não existe esse recurso |
| `DX_NOT_RUNNING` | `3` | existe, mas não está a correr |
| `DX_CONFLICT` | `5` | o nome já está tomado |
| `DX_UNAVAILABLE` | `69` | capacidade que este host não tem |
| `DX_TIMEOUT` | `124` | o prazo esgotou-se |
| `DX_INVALID_ARGUMENT` · `DX_REGISTRY` · `DX_SYSCALL_FAILED` · `DX_INVALID_STATE` · `DX_IO` | `1` | falhas sem número próprio |

**Estes nomes são contrato**: um código pode ser ACRESCENTADO; um existente
nunca muda de grafia nem de significado.

A relação é **assimétrica de propósito**. Um `DX_*` mapeia sempre para UM
número — se mapeasse para dois, o `$?` e o texto contradiziam-se para a mesma
falha. Mas o `1` carrega VÁRIOS códigos, porque é o balde «sem classe própria» e
o texto pode dar-se ao luxo de ser mais fino: cada NÚMERO é uma promessa que tem
de valer o resto do `0.x`, enquanto `DX_REGISTRY` ao lado de
`DX_INVALID_ARGUMENT` não custa nada. Há teste a exigir as duas metades desta
regra, incluindo que o balde continue a ser um balde.

Hoje o código sai na **API de gestão** (`delonix serve api`), como campo
acrescentado ao corpo de erro — `{"error": "...", "code": "DX_NOT_FOUND"}`. O
campo `error` não foi tocado: a regra do ADR-0005 (acrescentar sim, remover ou
mudar de tipo não) vale aqui como vale no `-o json`.

**O que continua sem código próprio, e é honesto dizê-lo:** *permissão negada* e
*falha temporária* foram considerados e ficaram de fora — não há hoje uma
variante de erro que os produza (as falhas de permissão chegam embrulhadas no
`errno` de uma syscall, e o retry que existe acontece dentro do motor e nunca
chega a quem chama). Publicar um número que nada constrói é um número que nunca
pode voltar.

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
  `v1` continuar a ser aceite. Desde a v0.64.0 cada Kind tem também o grupo do
  seu domínio (`compute.delonix.io/v1alpha1`, `networking.…`, …) — o
  `delonix api-resources` diz qual é o de cada um. As duas grafias são aceites;
  a de grupo é a canónica, e o Kind só aceita **o grupo dele** ou o legado.

A verdade não é este texto, é o schema: **`delonix manifest schema`** emite-o a
partir do próprio código (ADR-0007), e o mesmo ficheiro está publicado em
[`schema/v1/delonix.json`](schema/v1/delonix.json). Um teste do repositório
falha se o publicado deixar de ser o gerado, precisamente para esta página não
voltar a poder mentir.

O que o schema RECUSA, e vale saber porque é o que aparece sublinhado no editor:
um campo desconhecido (um typo), **um `kind` que o motor não conhece** — um typo
no nome, ou um Kind removido — e **um `apiVersion` que não é o grupo daquele
Kind**. Até à v0.65.x o `kind` era uma string livre e qualquer nome passava, por
isso um `kind: Contaner` validava limpo e um `Egress` já removido também; e o
`apiVersion` era fixo no legado, por isso a grafia de grupo — a canónica — era
sublinhada como se fosse erro. Um validador que discorda do motor é pior do que
nenhum, porque o visto verde é o que as pessoas seguem.

Aponta o editor e escreve manifestos com completação e validação:

```yaml
# yaml-language-server: $schema=https://angolardevops.github.io/delonix-runtime/schema/v1/delonix.json
apiVersion: compute.delonix.io/v1alpha1
kind: Pod
metadata: { name: web }
spec:
  containers:
    - name: web
      image: nginx:1.27
```

Há também uma extensão de VS Code que traz isto já ligado, mais um template por
Kind: [`angolardevops/delonix-vscode`](https://github.com/angolardevops/delonix-vscode).
Aponta para o MESMO ficheiro publicado acima — buscado ao vivo, não embutido —
por isso o editor e o `stack apply` não podem discordar.

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
* **`mcp`** — o servidor Model Context Protocol (ADR-0025), superfície nova. O
  transporte `stdio` é o suportado (um processo filho do cliente de IA por
  sessão, nunca um daemon); um transporte HTTP local, se vier a existir, seria
  loopback-only com um token local, como o `serve api` já é local-only e sem
  contrato publicado — não construas automação sobre ele ainda.
* **`backup`** — o grupo não estava declarado de nenhum dos lados, e essa
  omissão é ela própria um defeito: quem quisesse saber se podia depender de
  `delonix backup vm x` não tinha resposta. Fica NÃO estável enquanto os
  arquivos ganham verbos. O **formato do arquivo** é outra coisa e tem a sua
  própria guarda: o `backup.json` traz um número de versão e um leitor que não o
  conheça RECUSA em vez de adivinhar uma disposição que nunca viu.

  **Mudou na v0.67.0:** `delonix backup <kind> <nome>` passou a
  `delonix backup create <kind> <nome>`, e o `delonix restore` de raiz passou a
  `delonix backup restore <arquivo>` — sem o `<kind>` posicional, que o arquivo
  já regista (continua a poder afirmar-se com `--kind` para que uma discordância
  seja recusada). O agendamento saiu de flags do backup para
  `delonix backup schedule`, com a mesma semântica: instala o temporizador **e**
  tira o primeiro arquivo. Corte limpo, sem aliases — a forma antiga falha com
  «unrecognized subcommand», nunca em silêncio.

  **`delonix system backup` não foi tocado.** É outro âmbito — o state root
  inteiro de um nó — e não uma segunda porta para este grupo.
* **Tudo o que começa por `net netns`** — plumbing interno exposto por
  conveniência de depuração.
* **O formato dos ficheiros de estado** em `$DELONIX_ROOT`. Lê-se pelo `inspect`,
  nunca do disco.
* **`stack history`**, **`stack rollback`** e o conteúdo de `$DELONIX_ROOT/stacks/` (ADR-0019). É um
  registo do que foi aplicado, e o desenho promete explicitamente que **nada o
  lê para decidir o que existe**: apagar essa pasta não muda o que o `plan`, o
  `apply`, o `prune` ou o `destroy` fazem — só perde o histórico, e há gate na
  bateria E2E a exigi-lo. Automação que dependa da presença de uma revisão está
  a apostar contra essa promessa.

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
