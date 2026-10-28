<!-- translated-from: engineering-process.md sha256:b765b1a8f9865fdcb1254f484027564885de6238b1bcaa4a3a234c48e63f1e21 -->
# Processo de engenharia (canónico)

Esta página é a **única fonte de verdade** sobre como o trabalho flui por este repositório: de que
ramo uma mudança parte, como é construída em isolamento, como é validada, como aterra, e como os
seus rastos são limpos. O AGENTS.md, o [CONTRIBUTING.md](../../../CONTRIBUTING.md), o
[fluxo de contribuição](contributing-workflow.md) e as skills de sessão deferem todos para aqui no
que toca ao modelo de ramos e ao ciclo de vida — não o repetem. Quando discordarem desta página,
esta página vence; corrige o outro sítio em vez de bifurcar a regra.

> **Isto é uma decisão da Delonix, não uma afirmação sobre outros projectos.** O modelo de ramos
> abaixo tem forma de GitFlow por decisão do dono (2026-10-23). O Docker/Moby e o Podman são
> projectos PR-first, trunk-based; tomamos emprestada a sua disciplina de *contribuição* e de
> *revisão* (testes antes do merge, validação ao vivo de mudanças que tocam o kernel, commits
> focados ao estilo DCO, revisão por CODEOWNERS) — não a sua topologia de ramos. Não afirmes que
> eles usam este fluxo.

## Os dois ramos de vida longa

- **`main`** é a linha publicada. Só avança por uma release ou por um hotfix. O seu `HEAD` é o que
  um utilizador que instala a partir do código-fonte obtém.
- **`develop`** é a linha de integração. O trabalho do dia-a-dia parte dela e aterra de volta nela.
  Uma release promove `develop` para `main`.

Ambos são obtidos de fresco antes de usar; nunca ramifiques a partir de uma ref local obsoleta, e
nunca a partir do ramo de outra tarefa por conveniência.

## Política de ramos (obrigatória)

Classifica pela **natureza do problema**, não pelo prefixo que alguém sugeriu. O prefixo segue a
classificação; não a conduz.

| Tipo de trabalho            | Prefixo de ramo        | Parte de (remoto fresco)   | Destino normal                |
| --------------------------- | ---------------------- | -------------------------- | ----------------------------- |
| Funcionalidade              | `feat/`                | `origin/develop`           | `develop`                     |
| Manutenção / chore          | `chore/`               | `origin/develop`           | `develop`                     |
| Refactorização              | `refactor/`            | `origin/develop`           | `develop`                     |
| Desempenho                  | `perf/`                | `origin/develop`           | `develop`                     |
| Documentação                | `docs/`                | `origin/develop`           | `develop`                     |
| Teste / CI / build          | `test/` `ci/` `build/` | `origin/develop`           | `develop`                     |
| **Correcção / hotfix**      | `fix/` `bug/` `hotfix/`| **`origin/main`**          | **`develop` e, quando afecta a linha publicada, `main`** |

Os nomes são curtos e descritivos, com o identificador da tarefa quando existir:
`feat/DX-1234-http-route-timeouts`, `fix/compose-secret-lazy-read`.

### A regra do fix/bug/hotfix, e a sua única armadilha real

Um fix ramifica de `main` porque um fix é, por omissão, uma correcção ao que está publicado. A
mesma correcção também tem de chegar a `develop` para que a próxima release não regrida.

A armadilha, que esta política proíbe: **um defeito que existe apenas em `develop`**. Não o
ramifiques de `main` (não reproduz lá), e não faças merge de `develop` para `main` para o fazer
reproduzir. Um defeito só-de-develop corrige-se num ramo cortado de `develop` e aterra apenas em
`develop` — regista essa condição explicitamente quando o classificas. Se esta política alguma vez
bloquear uma correcção coerente, expõe o conflito concreto ao dono para uma decisão; não dobres a
regra em silêncio.

### Levar um fix aos dois destinos

Quando um fix visa a linha publicada, aterra em `main` através dos gates de release e a mesma
mudança chega a `develop`. **Nunca faças merge de `develop` para `main`** para levar um fix isolado
de um lado ao outro. Escolhe merge, back-merge ou `cherry-pick` conforme a convenção de histórico
linear abaixo, regista a equivalência de commits, e aplica a mudança uma vez — não duas. A
autorização para integrar em `develop` **não** autoriza cortar uma release, uma tag, ou publicar
artefactos; esses seguem o processo de release.

## Um worktree por tarefa

Várias sessões escrevem neste clone ao mesmo tempo (medido: o `HEAD` de `main` mexeu quatro vezes
durante uma sessão, oito worktrees vivos, dois a regenerar o mesmo ficheiro com minutos de
diferença). Por isso cada tarefa tem o seu próprio worktree e ramo, e nada é editado directamente
na checkout de `main` ou de `develop`.

```bash
git fetch origin --prune
# base is origin/develop for feat/chore/..., origin/main for fix/bug/hotfix
git worktree add -b <prefix>/<task> \
    <workspace>/.worktrees/delonix-runtime/<task> origin/<base>
cd <workspace>/.worktrees/delonix-runtime/<task>
export CARGO_TARGET_DIR=<repo>/target   # reuse the build cache safely
```

- **Nunca em `/tmp`** — é limpo no arranque e este host reinicia; um reinício a meio da tarefa leva
  o trabalho por commitar. Usa o persistente `.worktrees/` ao lado dos repositórios (ignorado pelo
  git através do harness do workspace). Faz commit e push do ramo **cedo e a cada passo com valor**.
- **Isola o estado, não só o código.** Um worktree isola os ficheiros; **não** isola o único motor
  Delonix partilhado neste host, nem portas, volumes ou fixtures de teste. Não partilhes ficheiros
  de estado, portas, volumes ou recursos de teste entre tarefas paralelas. As caches de build só
  podem ser partilhadas quando a ferramenta garante que é seguro; nunca corras um binário de outro
  worktree sem verificar a sua identidade (`target/release/delonix --version` + distância de
  commits).
- **Faz stage pelo nome** (`git add <file>`, nunca `-A`/`-u`/`.`), reconhece cada entrada de
  `git status --short` como tua, e **nunca `git checkout -- <path>`** numa árvore partilhada — isso
  destrói o trabalho por commitar de outra sessão sem um stash.
- **Faz rebase, não merge** num push rejeitado (`git pull --rebase`): o histórico é linear, sem
  commits de merge.
- **Limite de WIP: no máximo 3 worktrees/sessões abertos ao mesmo tempo.** Abrir um quarto obriga a
  fechar ou estacionar um, com um handoff escrito.

## O ciclo de vida de um pedido

Cada novo pedido corre este laço. Uma tarefa pequena pode ser um único sprint — não fabriques
cerimónia; mas também não confundas "implementação pronta" com "feito".

1. Recupera as instruções, decisões e checkpoint (o contrato de sessão, o `AGENTS.md` da área, o
   `docs/adr/` para as decisões).
2. Define objectivo, escopo e critérios de aceitação.
3. Classifica o trabalho e escolhe o ramo base a partir da tabela acima.
4. Identifica os especialistas necessários (ver "Delegação").
5. Constrói o grafo de dependências; sequencia as frentes dependentes, paraleliza as independentes.
6. Organiza a execução em sprints curtos dimensionados ao trabalho.
7. Cria o(s) worktree(s).
8. Implementa, revê, valida.
9. Integra o trabalho concluído em `develop`.
10. Limpa os recursos e actualiza o checkpoint.

### O contrato de cada sprint

Um sprint entrega uma **mudança concreta e utilizável** — sem stubs ou caminhos fictícios
apresentados como completos, e nunca um estado que deixe `develop` funcionalmente partido. Nomeia:
o resultado funcional; as tarefas e as suas dependências; os especialistas responsáveis; os
critérios de aceitação; os riscos; os testes; as evidências recolhidas; e a condição de integração.

## Delegação a agentes especialistas

Quando o ambiente oferece agentes especialistas, escolhe-os pelo problema (Rust/sistemas, ciclo de
vida OCI, CRI/CNI/Kubernetes, rede/isolamento, virtualização/providers, imagens cloud, CLI/DX,
segurança, teste funcional, CI/release). Para cada um, declara: objectivo, especialidade, escopo,
ramo e worktree, os ficheiros/contratos que detém, dependências, critérios de aceitação, comandos
de validação, e formato de entrega. As frentes independentes correm em paralelo; as dependentes
correm em sequência a partir de um contrato já integrado. **Dois agentes nunca editam o mesmo
worktree ao mesmo tempo.** Se o ambiente não oferecer agentes, di-lo e corre as responsabilidades
sequencialmente — não simules uma delegação que não aconteceu.

O agente principal detém a arquitectura, os contratos comuns e a integração. Quando uma frente
depende de outra: estabiliza e valida a primeira, integra-a em `develop`, corta a frente seguinte a
partir do `develop` actualizado, e revalida o contrato consumido. Não arranques implementações
paralelas sobre pressupostos incompatíveis acerca de um schema, trait ou API. Controla a
concorrência de build/teste para evitar saturar CPU, RAM e I/O.

## Qualidade antes da integração

Antes de uma tarefa ser considerada validada:

- Confirma os requisitos e as fontes relevantes; revê o diff.
- Corre os gates que o repositório exige — `make ci` (ou o subconjunto que a mudança toca, com o
  `rc=0` citado). Zero avisos do clippy é imposto, não meramente aconselhado.
- Corre testes proporcionais ao risco; uma mudança que depende do kernel, de um provider ou de um
  protocolo é validada **ao vivo**, e o PR diz o que foi testado (comando, esperado vs. real).
- Actualiza a documentação e os contratos que a mudança afecta
  (`scripts/dev_docs.py --check`, `scripts/dev_docs_site.py --check`).
- Confirma que não há segredos nem artefactos acidentais; verifica a migração e a compatibilidade
  onde se aplicam.
- Para mudanças relevantes, um **agente revisor diferente do implementador** caça defeitos,
  regressões, lacunas de teste e violações de contrato — aprovar a descrição do autor não é revisão.

Não alteres asserções, não reduzas o isolamento nem ignores falhas para obter um verde. Não
declares a integração validada quando os testes materiais estão em falta.

## Integração em `develop` (obrigatória)

O trabalho concluído e validado é integrado em `develop` na **mesma sequência de trabalho**, quando
as permissões e os gates o permitirem. A integração é por **pull request, squash-merged, com a
suíte de CI verde** — a condizer com o histórico linear e os Conventional Commits já em uso. Antes
de integrar:

1. `git fetch origin --prune` e verifica se `develop` mexeu.
2. Resolve os conflitos no ramo de trabalho (`git pull --rebase origin develop`).
3. Volta a correr os testes afectados.
4. Confirma o candidato exacto a integrar.
5. Obtém a revisão exigida.
6. Abre/completa o PR contra `develop`; deixa o CI terminar antes do merge.

Respeita os PRs, os checks e as protecções. Nunca contornes um controlo com um push directo ou um
force-push. Se uma protecção bloquear a integração, deixa o PR e a evidência prontos e explica o
bloqueador exacto — não marques o trabalho como integrado.

O CI corre a suíte de gates no push para `main` e `develop` e nos pull requests que visam qualquer
um (ver [`.github/workflows/ci.yml`](../../../.github/workflows/ci.yml)). Um ramo empurrado sem PR
não prova, por si só, "CI verde".

## Limpeza após a integração

Depois de a integração ser **confirmada** — não assumida — verifica: o ramo de destino local e
remotamente; que a mudança está efectivamente incluída; os checks exigidos; que não ficaram
mudanças locais; que não há commits não preservados; e que nenhum agente/processo continua a usar o
worktree. Depois:

1. `git worktree remove <path>` (nunca `--force` para esconder mudanças pendentes).
2. `git branch -d <branch>` (nunca `-D`).
3. Apaga o ramo remoto se pertence à tarefa e já não é necessário.
4. `git worktree prune` para referências obsoletas.
5. Actualiza o registo da tarefa / checkpoint.

Um squash ou cherry-pick significa que **a identidade do commit muda** — prova a inclusão pelo PR,
pelo diff e por `git cherry develop <branch>`, não só pela ascendência. Nunca removas o ramo ou
worktree de outra pessoa. Um recurso bloqueado fica identificado com a sua razão e o próximo passo —
sem ramos abandonados sem estado.

## Definição de pronto, e os estados pelo caminho

Uma tarefa está **CONCLUÍDA / DONE** só quando: os critérios de aceitação estão cumpridos; a revisão
está terminada; os testes exigidos passam; a documentação e os contratos estão actualizados; a
integração em `develop` está confirmada; qualquer integração adicional em `main` está resolvida
quando está no escopo autorizado; o worktree e o ramo temporários estão removidos em segurança; e o
checkpoint está actualizado.

Usa os estados intermédios explícitos no contrato de sessão:

`EM IMPLEMENTAÇÃO` → `EM REVISÃO` → `BLOQUEADO NA VALIDAÇÃO` →
`VALIDADO, A AGUARDAR INTEGRAÇÃO` → `INTEGRADO, LIMPEZA PENDENTE` → `CONCLUÍDO`.

## O harness que impõe isto

O `scripts/gitflow.py` transforma esta página em controlos executáveis. Valida o repositório e o
remoto antes de qualquer mutação, suporta `--dry-run`, é idempotente, preserva dados, e propaga os
códigos de saída.

```bash
python3 scripts/gitflow.py classify "fix a crash in compose config"   # -> prefix + base
python3 scripts/gitflow.py start  <prefix>/<task>  [--dry-run]          # fetch, base, worktree, contract
python3 scripts/gitflow.py preflight                                    # remote, base freshness, stale worktree refs, WIP cap
python3 scripts/gitflow.py gates                                        # run the gates, collect evidence
python3 scripts/gitflow.py integrate  [--dry-run]                       # rebase, re-gate, push, open PR to develop
python3 scripts/gitflow.py done                                         # prove inclusion, checks, clean tree
python3 scripts/gitflow.py cleanup  [--dry-run]                         # remove worktree + branches safely
```

`make gitflow ARGS="<subcommand> ..."` é a mesma coisa através do Makefile. Os hooks locais ajudam
a pessoa ao teclado; os checks de CI e (quando o dono as activa) as protecções de ramo são o que
sustenta as regras importantes. Os ficheiros de instruções não substituem um controlo executável.
