<!-- translated-from: publishing-docs.md sha256:45aa3ffa5c56ae32f97e6b5e14411d079130649b52c07c87bac2a7ad5ecab6f2 -->
# Publier la documentation

**Avant de lire :** [Flux de contribution](contributing-workflow.md), [Releases et stabilité](releases-and-stability.md) (ce que fait un tag poussé — la section *Ce qui se passe au moment de la release* de cette page en est la moitié documentation) et la [table généré vs écrit à la main](project-structure.md#generated-vs-hand-written) dans Structure du projet.

La documentation de ce dépôt est soit **générée à partir du code** (puis vérifiée par un gate
(contrôle CI)), soit **écrite à la main** (puis relue). Cette page explique ce qui relève de quoi,
comment chaque partie est publiée, et ce que vous devez faire lorsque votre modification touche la
documentation. Après elle, vous savez, pour toute modification, quel générateur exécuter et quels fichiers commiter avec lui.

## Où elle est publiée

Le site est servi par **GitHub Pages** depuis le répertoire `/docs` de `main`, à l'adresse
<https://angolardevops.github.io/delonix-runtime/>. Le réglage de la source Pages lui-même se trouve
dans les paramètres du dépôt, et non dans l'arborescence ; ce que l'arborescence contient, en
revanche, c'est `docs/.nojekyll`, qui désactive Jekyll afin que chaque fichier sous `docs/` soit
servi exactement tel qu'il a été commité.

Une conséquence bonne à connaître : Jekyll étant désactivé, les fichiers Markdown sous `docs/` — y
compris ce manuel dans `docs/dev/` — ne sont **pas** rendus en HTML par Pages ; ils sont servis
comme des fichiers bruts. La vue rendue du manuel est le rendu Markdown propre à GitHub lorsque
l'on parcourt le dépôt (`docs/dev/README.md` sur github.com), qui rend aussi les diagrammes Mermaid.
Les liens entre les pages du manuel sont relatifs, ils fonctionnent donc aux deux endroits.

## Qui est responsable de quoi

| Surface | Ce que c'est | Comment elle reste exacte |
|---|---|---|
| `README.rst` | la page d'accueil du projet | écrite à la main, vérifiée par `docs_cli_gate.py` |
| `docs/*.html`, `docs/comandos/*.html` | le site utilisateur | **générées** par `docs/gen.py` à partir du `--help` du binaire et du texte éditorial présent dans le générateur ; vérifiées par le job CI `docs` et au moment de la release |
| `docs/releases/v<version>.md` | les notes de release, une par tag | écrites à la main dans le commit de release ; publiées comme corps de la GitHub Release |
| `docs/RELEASES.md` | l'annexe des fonctionnalités par release | **générée** par `scripts/gen-releases.sh` à partir de `docs/releases/` ; ne jamais l'éditer à la main |
| `ARCHITECTURE.md` | les diagrammes d'architecture C4 | écrit à la main, tenu au pas du code ; rendu dans le site par `docs/gen.py` |
| `docs/adr/` | les décisions d'architecture | un fichier par décision ; les ADR acceptés sont remplacés par un successeur, jamais réécrits |
| `docs/api/openapi.yaml` | l'encodage REST de l'API de nœud | **généré** à partir de `proto/delonix/node/v1` par `scripts/contract_gate.py --update` |
| `docs/dev/` | ce manuel du contributeur | des **faits** générés (`scripts/dev_docs.py`) et un **récit** écrit à la main |
| `CONTRIBUTING.md` | la courte porte d'entrée des contributeurs | écrit à la main ; renvoie vers `docs/dev/` |

## Le site utilisateur : `docs/gen.py`

Les pages de référence embarquent le `--help` **réel** du binaire `delonix`, capturé lorsque le
générateur s'exécute, de sorte que le site ne documente jamais une option qui n'existe pas. Le
contenu éditorial (introductions, exemples, notes) se trouve dans des dictionnaires à l'intérieur de
`docs/gen.py` lui-même — modifiez-le là, et non dans le HTML.

```bash
cargo build --release -p delonix-runtime-bin
python3 -m pip install markdown     # the generator renders ARCHITECTURE.md
python3 docs/gen.py                 # uses the tree's release binary by default
git diff --stat -- docs/
```

Le générateur accepte aussi le chemin du binaire comme premier argument. Commitez les fichiers
régénérés avec la modification de la CLI qui les a provoqués. Deux vérifications échouent si vous
ne le faites pas :

- le **job `docs`** de la CI régénère le site et échoue lorsque `git diff -- docs/` n'est pas vide ;
  le même job valide chaque `examples/*.yaml` avec `stack apply --dry-run` et `stack validate`, et
  vérifie avec `groff` les pages de manuel générées par `delonix man --dir <dir> --index` ;
- le **workflow de release** refait la régénération contre le build de release avant de publier,
  de sorte qu'un tag dont le site n'est pas à jour n'est pas livré.

## Le gate des citations de commandes : `scripts/docs_cli_gate.py`

Chaque commande `delonix …` citée dans un contexte de code (`<code>`, `<pre>`, blocs de code
Markdown, blocs littéraux reST, commentaires YAML) dans la documentation actuelle doit se résoudre
dans l'arbre de commandes du binaire construit à partir de l'arborescence (lu via
`scripts/cli-tree.sh`). Il s'exécute dans le job CI `cli-surface` :

```bash
cargo build --release -p delonix-runtime-bin
DELONIX_BIN="$PWD/target/release/delonix" python3 scripts/docs_cli_gate.py
python3 scripts/docs_cli_gate.py --list      # every citation and where it is
```

Il existe parce que des commandes ont été supprimées dans des versions majeures consécutives et que
plusieurs pages actuelles continuaient d'enseigner `unrecognized subcommand`. Les archives
historiques datées — `docs/releases/`, `docs/RELEASES.md`, `docs/discovery/` et les rapports d'audit
et de mesure datés — sont exclues volontairement : elles doivent continuer à citer l'orthographe de
la version qu'elles décrivent. Une citation volontairement erronée va dans
`scripts/docs_cli_allow.tsv` avec une justification.

Les fichiers qu'il analyse sont listés dans `TARGETS` en tête du script. Vérifiez cette liste
lorsque vous ajoutez un nouveau répertoire de documentation ; une page en dehors n'est pas vérifiée.

## Les faits du manuel : `scripts/dev_docs.py`

Les faits structurels de `docs/dev/` — les crates et leurs couches, le graphe de dépendances, les
binaires, la toolchain épinglée et les jobs CI — sont générés à partir de `Cargo.toml`,
`scripts/arch_fitness.py`, `rust-toolchain.toml` et `.github/workflows/ci.yml`. Dans une page, ils
se trouvent entre des marqueurs :

```markdown
<!-- dev-docs:begin <key> -->
…generated, do not edit…
<!-- dev-docs:end <key> -->
```

```bash
python3 scripts/dev_docs.py            # rewrite every generated region
python3 scripts/dev_docs.py --check    # exit 1 when docs/dev is stale (CI job `arch`)
```

Règles :

- **N'éditez jamais l'intérieur d'une région.** La régénération suivante l'écrase et `--check`
  échoue en CI. Si un fait est faux, corrigez sa source (`Cargo.toml`, la table `LAYERS` dans
  `arch_fitness.py`, `ci.yml`) ou le générateur.
- Le texte **en dehors** des marqueurs n'est jamais touché ; un récit peut donc entourer une table
  générée.
- Une **nouvelle région** nécessite une fonction `render_*`, une clé dans `regions()` et un
  marqueur dans une page, dans le même commit.
- Aucun nombre qui change à chaque commit (lignes, tests, commits) — ni dans le générateur ni dans
  le récit. Un gate rouge sur chaque PR cesse d'être lu, et un décompte écrit à la main devient faux
  sans bruit.

Si votre PR ajoute ou déplace un crate, modifie une dépendance entre crates, ajoute un binaire,
met à jour la toolchain ou modifie un job CI, exécutez `python3 scripts/dev_docs.py` et commitez le
résultat avec elle.

## Ce qui se passe au moment de la release

Le workflow de release (`.github/workflows/release.yml`) s'exécute sur un tag `v*` poussé. Pour la
documentation, il :

1. régénère le site utilisateur contre le build de release et **échoue** si `docs/` diffère ;
2. publie la GitHub Release avec `docs/releases/<tag>.md` comme notes (ou des notes générées lorsque
   ce fichier n'existe pas) ;
3. fait un checkout de `main`, exécute `scripts/gen-releases.sh` (l'annexe `docs/RELEASES.md`) et
   `scripts/dev_docs.py` (les faits du manuel), puis commite les deux sur `main` avec `[skip ci]`
   lorsqu'ils ont changé.

Le **récit** du manuel n'est pas régénéré par la CI. Une fois une release publiée et validée, une
étape de relecture menée par un mainteneur lit les changements entre le tag précédent et le nouveau
avec les notes de release, met à jour uniquement les pages du manuel touchées par ces changements,
et ouvre une pull request pour cela. Lorsque rien de structurel ou de procédural n'a changé, le
résultat de cette relecture est « rien à mettre à jour », énoncé explicitement.

## Ce que cela implique pour votre pull request

| Votre modification | À faire aussi |
|---|---|
| L'aide de la CLI, une commande, une option | `python3 docs/gen.py` (build de release) et commitez `docs/` ; mettez à jour `scripts/cli_baseline.tsv` si des feuilles ont changé ; corrigez toute citation signalée par `docs_cli_gate.py` |
| Un crate, une dépendance entre crates, un binaire, la toolchain ou un job CI | `python3 scripts/dev_docs.py` et commitez `docs/dev/` |
| Le contrat de nœud dans `proto/` | `python3 scripts/contract_gate.py --update` et commitez `docs/api/openapi.yaml` |
| Une décision structurelle | un ADR dans `docs/adr/` (voir [Flux de contribution](contributing-workflow.md#when-to-write-an-adr)) |
| Une fonctionnalité visible par l'utilisateur | décrivez-la dans la PR afin qu'elle puisse figurer dans les prochaines notes de release |
| La manière dont les contributeurs construisent, testent ou travaillent | la page concernée de ce manuel |

---

**Suivant :** [Normes cloud native, couche par couche](cloud-native-standards.md) — la partie référence : chaque norme cloud native, ce qu'elle exige, et la conformité du moteur avec ses dates.
