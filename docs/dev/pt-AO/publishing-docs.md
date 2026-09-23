<!-- translated-from: publishing-docs.md sha256:45aa3ffa5c56ae32f97e6b5e14411d079130649b52c07c87bac2a7ad5ecab6f2 -->
# Publicar a documentação

**Antes de leres:** [Fluxo de contribuição](contributing-workflow.md), [Releases e estabilidade](releases-and-stability.md) (o que publicar uma release faz — a secção *O que acontece na altura da release* desta página é a sua metade de documentação) e a [tabela gerado vs escrito à mão](project-structure.md#generated-vs-hand-written) em Estrutura do projecto.

A documentação deste repositório ou é **gerada a partir do código** (e depois verificada por um
gate) ou é **escrita à mão** (e depois revista). Esta página explica qual é qual, como cada uma é
publicada, e o que tens de fazer quando a tua mudança afecta a documentação. Depois dela sabes,
para qualquer mudança, que gerador correr e que ficheiros fazer commit com ele.

## Onde é publicada

O site é servido pelo **GitHub Pages** a partir do directório `/docs` da `main`, em
<https://angolardevops.github.io/delonix-runtime/>. A configuração da origem do Pages vive nas
definições do repositório, não na árvore; o que a árvore contém é o `docs/.nojekyll`, que desliga o
Jekyll para que cada ficheiro em `docs/` seja servido exactamente como foi commitado.

Uma consequência que convém conhecer: como o Jekyll está desligado, os ficheiros Markdown em
`docs/` — incluindo este manual em `docs/dev/` — **não** são renderizados para HTML pelo Pages;
são servidos como ficheiros simples. A vista renderizada do manual é a renderização de Markdown do
próprio GitHub ao navegar no repositório (`docs/dev/README.md` no github.com), que também renderiza
os diagramas Mermaid. Os links entre páginas do manual são relativos, por isso funcionam nos dois
sítios.

## Quem é dono de quê

| Superfície | O que é | Como se mantém verdadeira |
|---|---|---|
| `README.rst` | a página de entrada do projecto | escrita à mão, verificada por `docs_cli_gate.py` |
| `docs/*.html`, `docs/comandos/*.html` | o site do utilizador | **gerado** por `docs/gen.py` a partir do `--help` do binário mais o texto editorial do gerador; verificado pelo job de CI `docs` e na altura da release |
| `docs/releases/v<version>.md` | notas de release, uma por tag | escritas à mão no commit de release; publicadas como corpo da GitHub Release |
| `docs/RELEASES.md` | o apêndice de funcionalidades por release | **gerado** por `scripts/gen-releases.sh` a partir de `docs/releases/`; nunca editar à mão |
| `ARCHITECTURE.md` | diagramas de arquitectura C4 | escrito à mão, mantido a par do código; renderizado no site por `docs/gen.py` |
| `docs/adr/` | decisões de arquitectura | um ficheiro por decisão; os ADRs aceites são substituídos, nunca reescritos |
| `docs/api/openapi.yaml` | a codificação REST da API de nó | **gerado** a partir de `proto/delonix/node/v1` por `scripts/contract_gate.py --update` |
| `docs/dev/` | este manual do contribuidor | **factos** gerados (`scripts/dev_docs.py`) mais **narrativa** escrita à mão |
| `CONTRIBUTING.md` | a porta de entrada curta para contribuidores | escrito à mão; aponta para `docs/dev/` |

## O site do utilizador: `docs/gen.py`

As páginas de referência embutem o `--help` **real** do binário `delonix`, capturado quando o
gerador corre, para que o site nunca documente uma flag que não existe. O conteúdo editorial
(introduções, exemplos, notas) vive em dicionários dentro do próprio `docs/gen.py` — edita-o aí,
não no HTML.

```bash
cargo build --release -p delonix-runtime-bin
python3 -m pip install markdown     # the generator renders ARCHITECTURE.md
python3 docs/gen.py                 # uses the tree's release binary by default
git diff --stat -- docs/
```

O gerador também aceita o caminho do binário como primeiro argumento. Faz commit dos ficheiros
regenerados juntamente com a mudança da CLI que os causou. Duas verificações falham se não o
fizeres:

- o **job `docs`** da CI regenera o site e falha quando `git diff -- docs/` não está vazio; o mesmo
  job valida cada `examples/*.yaml` com `stack apply --dry-run` e `stack validate`, e verifica com
  `groff` as páginas man geradas por `delonix man --dir <dir> --index`;
- o **workflow de release** repete a regeneração contra a build de release antes de publicar, para
  que uma tag cujo site esteja desactualizado não saia.

## O gate de citação de comandos: `scripts/docs_cli_gate.py`

Cada comando `delonix …` citado num contexto de código (`<code>`, `<pre>`, fences de código
Markdown, blocos literais reST, comentários YAML) na documentação actual tem de se resolver na
árvore de comandos do binário construído a partir da árvore (lida através de `scripts/cli-tree.sh`).
Corre no job de CI `cli-surface`:

```bash
cargo build --release -p delonix-runtime-bin
DELONIX_BIN="$PWD/target/release/delonix" python3 scripts/docs_cli_gate.py
python3 scripts/docs_cli_gate.py --list      # every citation and where it is
```

Existe porque houve comandos removidos em majors consecutivas e várias páginas actuais continuaram
a ensinar `unrecognized subcommand`. Os registos históricos datados — `docs/releases/`,
`docs/RELEASES.md`, `docs/discovery/` e os relatórios de auditoria e de medição datados — são
excluídos de propósito: têm de continuar a citar a grafia da versão que descrevem. Uma citação que
está errada de propósito vai para `scripts/docs_cli_allow.tsv` com uma razão.

Os ficheiros que ele varre estão listados em `TARGETS`, no topo do script. Verifica essa lista
quando acrescentares um directório de documentação novo; uma página fora dela não é verificada.

## Os factos do manual: `scripts/dev_docs.py`

Os factos estruturais em `docs/dev/` — os crates e as suas camadas, o grafo de dependências, os
binários, a toolchain fixada e os jobs de CI — são gerados a partir de `Cargo.toml`,
`scripts/arch_fitness.py`, `rust-toolchain.toml` e `.github/workflows/ci.yml`. Numa página ficam
entre marcadores:

```markdown
<!-- dev-docs:begin <key> -->
…generated, do not edit…
<!-- dev-docs:end <key> -->
```

```bash
python3 scripts/dev_docs.py            # rewrite every generated region
python3 scripts/dev_docs.py --check    # exit 1 when docs/dev is stale (CI job `arch`)
```

Regras:

- **Nunca edites dentro de uma região.** A regeneração seguinte sobrescreve-a e o `--check` falha na
  CI. Se um facto estiver errado, corrige a sua origem (`Cargo.toml`, a tabela `LAYERS` em
  `arch_fitness.py`, `ci.yml`) ou o gerador.
- O texto **fora** dos marcadores nunca é tocado, por isso a narrativa pode rodear uma tabela
  gerada.
- Uma **região nova** precisa de uma função `render_*`, de uma chave em `regions()` e de um
  marcador numa página, no mesmo commit.
- Nada de números que mudem a cada commit (linhas, testes, commits) — nem no gerador nem na
  narrativa. Um gate que está vermelho em todos os PRs deixa de ser lido, e uma contagem escrita à
  mão torna-se falsa em silêncio.

Se o teu PR acrescentar ou mover um crate, mudar uma dependência entre crates, acrescentar um
binário, subir a toolchain ou mudar um job de CI, corre `python3 scripts/dev_docs.py` e faz commit
do resultado com ele.

## O que acontece na altura da release

O workflow de release (`.github/workflows/release.yml`) corre quando alguém o pede
(`gh workflow run release.yml -f tag=v4.4.0`) — uma tag empurrada sozinha não faz nada. Para
a documentação, ele:

1. regenera o site do utilizador contra a build de release e **falha** se `docs/` diferir;
2. publica a GitHub Release com `docs/releases/<tag>.md` como notas (ou notas geradas quando esse
   ficheiro não existe);
3. faz checkout da `main`, corre `scripts/gen-releases.sh` (o apêndice `docs/RELEASES.md`) e
   `scripts/dev_docs.py` (os factos do manual), e faz commit dos dois na `main` com `[skip ci]`
   quando mudaram.

A **narrativa** do manual não é regenerada pela CI. Depois de uma release ter sido publicada e
validada, um passo de revisão corrido por um maintainer lê as mudanças entre a tag anterior e a
nova, juntamente com as notas de release, actualiza só as páginas do manual que essas mudanças
afectam, e abre um pull request para isso. Quando nada de estrutural ou de procedimento mudou, o
resultado dessa revisão é «nada a actualizar», dito explicitamente.

## O que isto significa para o teu pull request

| A tua mudança | Faz também |
|---|---|
| Ajuda da CLI, um comando, uma flag | `python3 docs/gen.py` (build de release) e commit de `docs/`; actualiza `scripts/cli_baseline.tsv` se as folhas mudaram; corrige qualquer citação que `docs_cli_gate.py` reporte |
| Um crate, uma dependência entre crates, um binário, a toolchain ou um job de CI | `python3 scripts/dev_docs.py` e commit de `docs/dev/` |
| O contrato de nó em `proto/` | `python3 scripts/contract_gate.py --update` e commit de `docs/api/openapi.yaml` |
| Uma decisão estrutural | um ADR em `docs/adr/` (ver [Fluxo de contribuição](contributing-workflow.md#when-to-write-an-adr)) |
| Uma funcionalidade visível ao utilizador | descreve-a no PR para que possa entrar nas próximas notas de release |
| Como os contribuidores constroem, testam ou trabalham | a página relevante deste manual |

---

**Seguinte:** [Normativos cloud native, camada a camada](cloud-native-standards.md) — a parte de referência: cada normativo cloud native, o que exige, e a conformidade do motor com datas.
