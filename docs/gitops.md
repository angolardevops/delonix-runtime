# GitOps com o Delonix

O manifesto é a fonte de verdade; o repositório é onde ele vive; o `delonix` é
quem o aplica. Não é preciso Terraform nem Ansible por cima — esta página é o
fluxo completo.

> Aplica-se aos Kinds que **convergem**, que são onze: `Network`,
> `NetworkRoute`, `Volume`, `Image`, `VirtualMachine`, `Container`, `Pod`,
> `NetworkPolicy`, `HTTPRoute`, `Ingress` e `Gateway`. Só o `Secret` continua «garante presente»
> (criado se faltar, nunca actualizado), porque o estado são valores cifrados e
> um plano não os decifra para comparar — e o `plan` marca-o com `!` em vez de o
> esconder, com o obstáculo nomeado. A lista autoritativa não é este parágrafo:
> é `delonix stack plan --fields`, que a imprime do próprio código. Vê também
> [`cli-stability.md`](cli-stability.md).

## Os cinco comandos

```bash
delonix stack plan       # o que mudaria — não muda nada
delonix stack apply      # converge
delonix stack apply --prune   # converge e remove o que saiu do manifesto
delonix stack wait       # bloqueia até estar mesmo de pé
delonix stack destroy    # remove tudo o que esta stack possui
```

`plan` compara três coisas — o manifesto, a máquina, e o último spec que **esta
stack** aplicou. É esse terceiro lado que distingue «tiraste este campo do
ficheiro» (reverte) de «alguém pôs isto à mão com `container update`» (não
mexe).

## Os símbolos

| | Significa |
|---|---|
| `+` | não existe, vai ser criado |
| `+~` | existe, não pertence a stack nenhuma — vai ser **adoptado** |
| `~` | converge **a quente**, sem recriar e sem mudar o PID |
| `-/+` | tem de ser **destruído e recriado** — o `apply` recusa sem `--replace` |
| `-` | pertence a esta stack e saiu do manifesto — só sai com `--prune` |
| `=` | já bate certo |
| `✗` | pertence a **outra** stack — nunca é tocado |
| `!` | este Kind não converge nesta versão |

Se não perceberes porque é que uma alteração tua não aparece:

```bash
delonix stack plan --fields
```

Diz exactamente que campos são comparados por Kind, e quais não são e porquê
(`env` e `command` vêm fundidos com os da imagem; `user` é guardado como uid
resolvido). E, para o Kind que ainda não converge, **porque não** — o `Secret`,
porque o estado são valores cifrados e um plano não os decifra para comparar. Um
obstáculo nomeado é uma decisão; «ainda não converge» seria só silêncio.

## Quem é o dono

A posse vem da label `delonix.io/stack`, carimbada em cada recurso pelo próprio
`apply`. O nome da stack é, por esta ordem: `--name`, o `metadata.name` de um
`kind: Stack`, ou o **directório** do manifesto.

Consequências que interessam:

* Um recurso criado à mão **nunca** é apagado por um `--prune` ou por um
  `destroy` — não tem a label. Aparece no plano como `+~` e é adoptado no
  primeiro apply, que é o que dispensa um comando `import`.
* **Nem tudo é possuível, e isso é deliberado.** Uma `Image` é cache partilhada
  com endereço de conteúdo — o mesmo `alpine:latest` serve todas as stacks do
  host, por isso carimbá-la para uma e removê-la quando essa deixasse de a
  declarar tirava-a debaixo das outras. Uma `NetworkPolicy`, um `HTTPRoute` e um
  `Gateway` não têm registo próprio onde carimbar. Nenhum dos quatro é adoptado
  nem podado; todos convergem.
* Duas stacks a declarar o mesmo nome dão **conflito** (`✗`), não uma corrida.
  O `apply` recusa antes de tocar em nada.

## O PR: `plan` como revisão

```yaml
# .github/workflows/delonix.yml
name: delonix
on:
  pull_request:
  push: { branches: [main] }

jobs:
  plan:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: validar o manifesto
        run: delonix stack validate
      - name: plano
        run: delonix stack plan | tee plan.txt
      - name: comentar o plano no PR
        if: github.event_name == 'pull_request'
        run: gh pr comment ${{ github.event.number }} -F plan.txt
        env: { GH_TOKEN: '${{ github.token }}' }
```

`stack validate` corre primeiro de propósito: resolve as referências cruzadas
(`Container.network`, `.volumes`, alvos de `Ingress`/`Egress`) contra o que o
manifesto declara **mais** o que já existe na máquina. Como o `apply` é
fail-fast sem rollback, uma referência partida tem de parar tudo antes da
primeira criação, não a meio.

## O merge: `apply`

```yaml
  apply:
    if: github.ref == 'refs/heads/main'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: delonix stack apply --prune
      - run: delonix stack wait --timeout 180
```

O `wait` não é decoração: o `apply` devolve assim que **criou** as coisas, o que
não é o mesmo que a stack estar a funcionar. Sem ele, cada pipeline inventa o
seu `sleep`. Bloqueia até cada recurso declarado existir e — onde isso significa
alguma coisa — estar a correr e saudável; falha nomeando exactamente o que não
subiu. Um pré-requisito em falta (o `!` do plano) é **avisado e não esperado**:
um volume que não monta em rootless não começa a montar ao fim de noventa
segundos, e bloquear nisso transformaria um aviso honesto num pendura.

Uma recriação **não passa sozinha**. Se o plano trouxer um `-/+`, o job falha
com o nome do campo que a obriga, e alguém tem de decidir:

```bash
delonix stack apply --replace Container/api
```

Isto é deliberado: recriar significa downtime, e num volume significa perder os
dados. Um `apply` distraído não deve poder fazê-lo.

E o valor é **verificado**, em duas etapas: a forma (`<Kind>/<nome>`, um nome nu,
ou `all`) e depois se nomeia mesmo alguma coisa deste manifesto. Um
`--replace Container/wev` com o erro de escrita é **recusado a nomear o valor**,
em vez de se ler como autorizado e deixar a recusa seguinte a falar do recurso —
nunca do engano que a causou. Numa flag que autoriza uma operação destrutiva,
aceitar um valor que não pode casar com nada é dar a ilusão de ter autorizado.

## O gate de deriva

Deriva é o `plan` a dizer alguma coisa com o manifesto inalterado. O exit code
serve-o directamente — mesmo contrato do `terraform plan -detailed-exitcode`:

| exit | significa |
|---|---|
| `0` | nada a fazer |
| `2` | há alterações (= deriva, se o ficheiro não mudou) |
| `1` | o comando falhou |

Desde a v0.49.0 uma falha pode vir CLASSIFICADA em vez de `1` — `4` se o
manifesto nomear um recurso que não existe, por exemplo (a tabela completa está
em [cli-stability.md](cli-stability.md)). Os dois sinais positivos não mudam:
`0` é «nada a fazer», `2` é «há alterações», e nenhum caminho de erro devolve
`2`. Um gate escrito como «`0` passa, `2` falha por deriva, **o resto** falha
por erro» continua correcto; um escrito com `elif [ $? -eq 1 ]` para o erro
passa a precisar de `else`.

```yaml
  drift:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: delonix stack plan --detailed-exitcode
```

```yaml
on:
  schedule: [{ cron: '0 7 * * *' }]
```

Para alimentar outra ferramenta em vez de falhar um job:

```bash
delonix stack plan -o json | jq -r '.[] | select(.changed) | "\(.kind)/\(.name) \(.action)"'
```

Os nomes dos campos do JSON são estáveis e **não mudam com `--l18n`**
(ADR-0005) — é a tabela que é traduzida, nunca as chaves.

## Quando um `apply` morre a meio

Acontece, e é um estado normal: o `apply` é **fail-fast e não tem rollback**.
Não há transacção; o que já foi criado fica criado.

A recuperação não é adivinhação:

```bash
delonix stack plan      # mostra exactamente o que ficou por fazer
delonix stack apply     # continua daí — é idempotente
```

O `plan` é a ferramenta de diagnóstico porque compara com o real, não com um
ficheiro de estado que poderia estar dessincronizado. **Não há `.tfstate`
nenhum**: a posse e o último spec aplicado vivem no próprio recurso
(`delonix.io/stack` e `delonix.io/last-applied`), por isso não existe o modo de
falha «o estado diz uma coisa e a máquina diz outra».

Se a corrida morreu depois de criar mas antes de carimbar, o recurso aparece
como `+~` (por adoptar) e o apply seguinte resolve-o.

## Escrever o manifesto sem decorar nada

```yaml
# yaml-language-server: $schema=https://angolardevops.github.io/delonix-runtime/schema/v1/delonix.json
```

Uma linha, e o editor passa a dar completação, verificação de tipos e a
documentação de cada campo enquanto escreves. O schema é gerado do próprio
código (ADR-0007), por isso não pode divergir dele — apanha um typo num nome de
campo, um `kind` que o motor não conhece, e um `apiVersion` que não é o grupo
daquele Kind.

Num PR, isto vale mais do que parece: um manifesto que o editor sublinha nunca
chega ao `plan`, e o erro aparece a quem o escreveu em vez de aparecer no CI de
outra pessoa.

No terminal:

```bash
delonix explain Container
delonix explain Container.ports
delonix explain Pod.containers.image
delonix stack apply --dry-run   # o manifesto com TODOS os defaults preenchidos
```

## Validado ao vivo, com um container real

O ciclo inteiro, num `DELONIX_ROOT` isolado — e a prova que interessa não é o
comando devolver 0:

| Passo | Resultado |
|---|---|
| `apply` de um manifesto novo | container criado, PID `618350` |
| `plan` outra vez | «sem alterações» |
| mudar `memory: 64M` → `128M` | plano diz `~ update`, com os dois valores |
| `apply` | **PID inalterado (618350)** e o `memory.max` do cgroup REAL passa a `134217728` |
| mudar a `image` | plano diz `-/+`, nomeando `image` |
| `apply` sem `--replace` | **RECUSA**, e o PID continua `618350` — nada foi tocado |
| `container update` por fora | o `plan` seguinte apanha a deriva (`256M → 128M`) |
| `stack wait` | devolve de imediato |
| `stack destroy` | container removido, `ps -a` vazio |

O PID inalterado é o ponto: é o que distingue convergência a quente de um
restart disfarçado, e é a diferença de fundo entre este motor e recriar o
container como o Docker faria.

## O que este fluxo não faz

* **Não reconcilia continuamente.** Converge quando lhe chamas. O repositório
  não é consultado por ninguém no nó — quem corre o `apply` é o CI ou o
  operador, e o que aqui existe é um gate de deriva. Fechar essa metade sem
  trazer um daemon é o que o [ADR-0021](adr/0021-gitops-pull-reconciler.md)
  propõe (`delonix gitops`, por timer e opt-in); enquanto estiver *Proposed*,
  esta linha continua a ser o estado do motor.
* **Não tem rollback transaccional** (ver acima).
* **Não gere frota.** É um runtime de nó: um manifesto, uma máquina. Vários nós
  são várias corridas.
