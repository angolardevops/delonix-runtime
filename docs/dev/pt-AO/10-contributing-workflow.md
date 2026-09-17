<!-- translated-from: 10-contributing-workflow.md sha256:6090b33867ef5efe832a26a27c06a0b82dbc789dfc29b63341ad78af27536afc -->
# 10. Fluxo de contribuição

Esta página é a parte «como trabalhamos» do manual: onde fazer as mudanças, que regras os gates
impõem e porquê, quando uma mudança precisa primeiro de uma decisão escrita, e como enviá-la. Os
gates em si, e como os correr, estão em [02 — Clonar, construir e testar](02-build-and-test.md).

Para tudo o que não seja trivial — um comando novo, um Kind de manifesto novo, uma mudança na
preparação de namespaces ou de cgroups, um backend novo — abre primeiro uma issue e acorda a
abordagem. Poupa uma reescrita.

## Parte da tag mais recente, não da memória

Antes de escreveres código:

```bash
git fetch --tags origin
git describe --tags --abbrev=0 origin/main          # the newest release
git log --oneline "$(git describe --tags --abbrev=0 origin/main)"..origin/main | wc -l   # how far main is past it
git log --oneline -- <path you will touch>          # what was already decided, fixed or removed there
```

Ler o histórico da área em que mexes não é cerimónia: boa parte desta base de código é um registo
de coisas que foram tentadas, medidas e mudadas. O que já foi decidido ou removido não se refaz por
desconhecimento. O registo por extenso vive em [`AGENTS.md`](../../../AGENTS.md) (organizado por
área) e em [`docs/adr/`](../../adr/README.md).

## Um worktree por tarefa

Várias pessoas e ferramentas trabalham muitas vezes no mesmo clone ao mesmo tempo. Editar numa
checkout partilhada já custou trabalho real aqui: edições absorvidas no commit de outra pessoa, o
`HEAD` a mudar de branch a meio de uma tarefa, e um `cargo test` verde numa árvore suja que não
provava que o `HEAD` compilava. Por isso cada tarefa tem o seu próprio worktree git e a sua branch,
criados a partir de `origin/main`:

```bash
git fetch origin
git worktree add -b <topic>/<task> <workspace>/.worktrees/delonix-runtime/<task> origin/main
cd <workspace>/.worktrees/delonix-runtime/<task>
```

- **Nunca ponhas um worktree em `/tmp`.** Muitos sistemas esvaziam o `/tmp` no arranque; um
  reinício a meio da tarefa leva o trabalho por commitar, e pode deixar objectos escritos a meio no
  `.git` partilhado. Usa um directório persistente fora do repositório (a convenção aqui é um
  directório `.worktrees/` ao lado dos repositórios), para que nenhum `grep -r`, contexto de
  `docker build` ou script de gate o apanhe.
- **Faz commit e push cedo**, a cada passo que passe as suas verificações. Um worktree persistente
  sobrevive a um reinício; uma branch empurrada sobrevive a tudo o resto.
- **Adiciona os ficheiros pelo nome**: `git add <file> <file>`, nunca `git add -A`, `-u` ou `.`.
  Confirma `git branch --show-current` e reconhece cada entrada de `git status --short` como tua
  antes de fazeres commit.
- **Nunca `git checkout -- <path>`** numa árvore que outra pessoa possa estar a usar: reverte para o
  `HEAD` sem stash e destrói o trabalho dela por commitar.
- **Faz rebase, não merge**, quando o teu push for rejeitado por divergência: `git pull --rebase`.
  O histórico é linear.
- **Quando a tarefa estiver feita, remove as duas coisas**, o worktree e a branch — a branch
  sobrevive ao `worktree remove`, e é assim que as branches obsoletas se acumulam:

  ```bash
  git worktree remove <path>
  git branch -D <topic>/<task>
  git worktree list
  ```

## Alinhamento da versão

A `version` do `Cargo.toml` raiz tem peso real: escolhe de que release o `delonix-cri` é
descarregado, é reportada como o `ServerVersion` da API Docker, fica registada em cada backup, e o
workflow de release compara-a com o binário construído. O `scripts/version_gate.py` (job de CI
`version`) permite exactamente dois estados:

1. **Igual à tag mais recente que o commit contém** — todo o trabalho normal. Não subas a versão
   num PR de feature, e não uses um sufixo `-dev` (mandaria o download do `delonix-cri` para uma
   release que não existe).
2. **Maior, só no commit de release**, juntamente com `docs/releases/v<version>.md`.

Também falha uma branch que **não contém a tag mais recente**: a branch começou antes dessa release,
e fundi-la tal como está desfaria o que a release publicou. Faz rebase sobre `origin/main`.

Entre releases, `delonix --version` distingue as builds pelo commit e pela distância
(`commit: <hash> (+N commits since vX.Y.Z)`), porque duas builds com o mesmo número de versão não
são a mesma build.

## Língua: inglês no código (LANG-01)

Identificadores, comentários e mensagens ao utilizador escrevem-se em **inglês**. O português chega
ao operador só através do catálogo de tradução:

- `bins/delonix-runtime-bin/src/cmd/po.rs` — `po::t("…")` para strings fixas, `po::tf("… {name} …",
  &[("name", value)])` para as interpoladas (placeholders com nome, porque uma tradução pode
  reordená-los). O texto do `--help` da CLI é traduzido em runtime por `po::translate_help`.
- `bins/delonix-runtime-bin/data/pt.po` — as entradas em português, embutidas no binário e
  seleccionadas com `--l18n pt` ou `DELONIX_L18N=pt`.

Uma entrada em falta no catálogo degrada para inglês; uma string portuguesa escrita directamente no
código é um bug. O `scripts/lang_ratchet.py` (job de CI `lang`) conta o português que ainda resta em
identificadores, comentários e mensagens contra `scripts/lang_baseline.json`. É um **ratchet**, não
um tecto: falha quando uma contagem sobe (entrou português novo) **e** quando desce sem a linha de
base ter sido baixada. Quando traduzires alguma coisa, corre
`python3 scripts/lang_ratchet.py --update` e faz commit da nova linha de base **no mesmo commit**
que a tradução. Os testes do crate da CLI verificam também que a ajuda dos comandos tem uma entrada
em português — acrescenta uma quando acrescentares um comando ou uma flag.

## Regras de arquitectura que os gates impõem

O `scripts/arch_fitness.py` (job de CI `arch`) impõe a estrutura decidida no
[ADR-0040](../../adr/0040-engine-restructuring-layers-ports-node-contract.md). A camada de cada
crate e a direcção permitida estão listadas em [05 — Arquitectura](05-architecture.md). O que isso
significa para uma mudança:

- **O motor não conhece nenhum consumidor.** Nenhum nome de produto, plataforma, control plane,
  consola ou agente que use o motor — nem inquilino, conta, plano ou faturação — em `crates/`,
  `bins/`, `proto/` ou nos manifestos, comentários incluídos. Um requisito que venha de um
  consumidor escreve-se como a capacidade genérica que é, no vocabulário do próprio motor, e só
  entra se fizer sentido para qualquer cliente. A história que precisa de nomes externos vive em
  `docs/`, nunca no código.
- **As dependências apontam para dentro.** A fundação só depende da fundação; os contextos não
  dependem de adaptadores; adaptadores e providers não dependem de interfaces; um binário compõe
  **uma** interface.
- **O directório é a camada.** Um crate vive em `crates/<layer>/`, de acordo com a sua entrada na
  tabela `LAYERS`. Um crate novo entra em `LAYERS` e no directório certo no mesmo commit.
- **As versões das dependências vivem só na raiz** `[workspace.dependencies]`; um crate membro
  escreve `{ workspace = true, features = [...] }` e nada mais.
- **As excepções nomeiam a fase que as remove.** Uma excepção sem fase falha, e também falha uma
  excepção que já não se aplica.
- **Ratchets de dívida** — por exemplo `self_exec_sites` (uma biblioteca a voltar a correr o
  próprio binário do motor em vez de chamar uma função), `library_prints` (`println!`/`eprintln!`
  num crate de biblioteca — as bibliotecas emitem `tracing`, as interfaces imprimem), `env_writes`
  (`env::set_var`/`remove_var`) e `shared_error_imports` (um adaptador ou provider a usar o `Error`
  partilhado como seu, em vez de um erro do crate que se converta nele). A lista actual é gerada:

<!-- dev-docs:begin ratchets -->
O `scripts/arch_fitness.py` mantém **4 ratchets de dívida** (linha de base em `scripts/arch_baseline.json`):

- `self_exec_sites`
- `library_prints`
- `env_writes`
- `shared_error_imports`
<!-- dev-docs:end ratchets -->
A mesma semântica do ratchet de língua (`--list`, `--update`).

Para além do que o gate consegue ver, três princípios decidem as revisões: **daemonless** (um
processo residente novo precisa de um ADR com a evidência do que units, timers ou socket activation
do systemd não conseguiram fazer), **rootless-first** (o privilégio é um opt-in explícito e
anunciado, nunca um default silencioso), e **nenhuma falha silenciosa** (uma flag que é aceite e
ignorada é pior do que uma flag que não existe — recusa-a em vez disso, com um erro claro).

## Quando escrever um ADR

Escreve um Architecture Decision Record em `docs/adr/` **antes** do código quando uma mudança move
uma fronteira estrutural, por exemplo:

- um backend ou provider novo (um hypervisor, um sistema de armazenamento), ou uma porta nova;
- uma dependência externa nova num crate do motor, ou um daemon ou processo residente novo;
- uma fronteira de privilégio nova — que precisa também, primeiro, de um spike GO/NO-GO;
- uma mudança no contrato de nó, na estabilidade do schema do manifesto, ou na estrutura de camadas.

Uma feature de rotina dentro de uma fronteira existente não precisa de um. O formato e a lista
actual estão em [`docs/adr/README.md`](../../adr/README.md): um ficheiro por decisão,
`NNNN-title.md`, em inglês. **Os ADRs aceites nunca são reescritos** — um ADR novo substitui-os.

## Acrescentar ou mudar um comando da CLI

- **Liga todos os pontos de entrada.** Vários comandos são alcançáveis por mais de um caminho (por
  exemplo `delonix vm pull` e `delonix image vm pull`). Muda-os todos, e depois verifica cada um com
  o binário que construíste — incluindo a completação da shell, porque a declaração `clap` que
  editaste pode não ser a que o caminho do utilizador analisa. O motor de completação pode ser
  sondado directamente; `_CLAP_COMPLETE_INDEX` é a posição da palavra a completar:

  ```bash
  COMPLETE=bash _CLAP_COMPLETE_INDEX=3 ./target/debug/delonix -- delonix image vm ''
  ```
- **Valida contra o binário**, não contra o código-fonte: `./target/debug/delonix <group> <command> --help`
  e uma execução real com os state roots isolados (ver [02](02-build-and-test.md#isolating-the-engines-state)).
- **Actualiza a linha de base da CLI** (`scripts/cli-tree.sh --update`) no mesmo commit quando
  acrescentares ou removeres uma folha, e regenera o site (`python3 docs/gen.py` com uma build de
  release) quando o texto de ajuda mudar.
- **Testa unitariamente cada função pura nova** — parsers, validadores, construtores de argumentos.
  Esta base de código tem um longo registo de bugs reais apanhados exactamente aí.
- **Classifica os erros.** Os códigos de saída carregam uma classe (não existe, conflito, …),
  decidida num só sítio a partir do tipo do erro; devolve a variante `Error` certa em vez de uma
  genérica.

## Commits e pull requests

- Uma mudança lógica por commit; diz **porquê** na mensagem — o diff já mostra o quê.
- Referencia a issue quando houver uma.
- Abre o PR contra `main` e preenche [o modelo](../../../.github/PULL_REQUEST_TEMPLATE.md): o que
  correste e, para código de runtime, namespace, cgroup ou rede, o que correste **ao vivo** num host
  real, com o comando e a sua saída.
- «Compila» e «o comando devolveu 0» não fecham uma mudança. Diz o que foi provado e, com a mesma
  clareza, o que não foi validado e porquê.
- Cada mudança é revista pelos code owners listados em [`.github/CODEOWNERS`](../../../.github/CODEOWNERS).

## Mudanças sensíveis para a segurança

Assinala-o explicitamente no PR quando uma mudança atravessa uma fronteira de privilégio ou de
namespace: o mapeamento do user namespace, o holder de rede ou o seu socket de controlo,
`setns`/`unshare`, o tratamento de capabilities ou de seccomp, ou o tratamento de caminhos
conduzido por input do utilizador ou do manifesto. Estas mudanças têm revisão adicional.

Se encontraste uma **vulnerabilidade** e não um bug — escalada de privilégio, fuga de namespace,
injecção de comandos, path traversal — não abras uma issue nem um PR públicos. Segue o
[`SECURITY.md`](../../../SECURITY.md) (GitHub Private Vulnerability Reporting).
