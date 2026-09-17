<!-- translated-from: contributing-workflow.md sha256:7a27cf21c3440859aaa70b55740a2566cfff13b0f365c965900c98be188a10db -->
# Flux de contribution

**Avant de lire :** [Cloner, construire et tester](build-and-test.md#the-gates-ci-runs) (les gates) et [Conventions de code](coding-conventions.md) (ce que les relecteurs vérifient).

Cette page est la partie « comment nous travaillons » du manuel : où apporter les modifications,
quelles règles les gates (contrôles CI) imposent et pourquoi, quand une modification exige d'abord
une décision écrite, et comment l'envoyer. Les gates eux-mêmes, et la manière de les exécuter, sont
décrits dans [Cloner, construire et tester](build-and-test.md). Après elle, vous pouvez faire passer une modification d'une issue à une pull request fusionnée sans casser un gate ni le travail d'une autre session.

Pour tout ce qui n'est pas trivial — une nouvelle commande, un nouveau Kind de manifeste, une
modification de la mise en place des namespaces ou des cgroups, un nouveau backend — ouvrez d'abord
une issue et mettez-vous d'accord sur l'approche. Cela évite une réécriture.

## Partez du dernier tag, pas de votre mémoire

Avant d'écrire du code :

```bash
git fetch --tags origin
git describe --tags --abbrev=0 origin/main          # the newest release
git log --oneline "$(git describe --tags --abbrev=0 origin/main)"..origin/main | wc -l   # how far main is past it
git log --oneline -- <path you will touch>          # what was already decided, fixed or removed there
```

Lire l'historique de la zone que vous touchez n'est pas une cérémonie : une grande partie de ce code
est la trace de choses qui ont été essayées, mesurées et modifiées. Ce qui a déjà été décidé ou
supprimé n'est pas refait par méconnaissance. L'historique détaillé se trouve dans
[`AGENTS.md`](../../../AGENTS.md) (organisé par domaine) et dans [`docs/adr/`](../../adr/README.md).

## Un worktree par tâche

Plusieurs personnes et outils travaillent souvent sur le même clone en même temps. Éditer dans un
checkout partagé a déjà coûté du vrai travail ici : des modifications absorbées dans le commit de
quelqu'un d'autre, `HEAD` qui change de branch au milieu d'une tâche, et un `cargo test` vert sur une
arborescence sale qui ne prouvait pas que `HEAD` compilait. Chaque tâche reçoit donc son propre
worktree git et sa propre branch, créés à partir de `origin/main` :

```bash
git fetch origin
git worktree add -b <topic>/<task> <workspace>/.worktrees/delonix-runtime/<task> origin/main
cd <workspace>/.worktrees/delonix-runtime/<task>
```

- **Ne placez jamais un worktree dans `/tmp`.** De nombreux systèmes vident `/tmp` au démarrage ; un
  redémarrage en cours de tâche emporte le travail non commité, et peut laisser des objets à moitié
  écrits dans le `.git` partagé. Utilisez un répertoire persistant en dehors du dépôt (la convention
  ici est un répertoire `.worktrees/` à côté des dépôts), afin qu'aucun `grep -r`, contexte de
  `docker build` ou script de gate ne le prenne en compte.
- **Commitez et poussez tôt**, à chaque étape qui passe ses vérifications. Un worktree persistant
  survit à un redémarrage ; une branch poussée survit à tout le reste.
- **Indexez les fichiers par nom** : `git add <file> <file>`, jamais `git add -A`, `-u` ou `.`.
  Vérifiez `git branch --show-current` et reconnaissez chaque entrée de `git status --short` comme
  la vôtre avant de commiter.
- **Jamais de `git checkout -- <path>`** dans une arborescence que quelqu'un d'autre peut utiliser :
  cela revient à `HEAD` sans stash et détruit son travail non commité.
- **Faites un rebase, pas un merge**, lorsque votre push est refusé pour divergence :
  `git pull --rebase`. L'historique est linéaire.
- **Lorsque la tâche est terminée, supprimez les deux**, le worktree et la branch — la branch
  survit à `worktree remove`, et c'est ainsi que les branches obsolètes s'accumulent :

  ```bash
  git worktree remove <path>
  git branch -D <topic>/<task>
  git worktree list
  ```

## Alignement de version

La `version` du `Cargo.toml` racine est porteuse de sens : elle détermine depuis quelle release
`delonix-cri` est téléchargé, elle est renvoyée comme `ServerVersion` de l'API Docker, elle est
enregistrée dans chaque sauvegarde, et le workflow de release la compare au binaire construit.
`scripts/version_gate.py` (job CI `version`) n'autorise exactement que deux états :

1. **Égale au tag le plus récent que contient le commit** — tout le travail ordinaire. Ne modifiez
   pas la version dans une PR de fonctionnalité, et n'utilisez pas de suffixe `-dev` (il enverrait le
   téléchargement de `delonix-cri` vers une release qui n'existe pas).
2. **Supérieure, uniquement dans le commit de release**, accompagnée de
   `docs/releases/v<version>.md`.

Il fait aussi échouer une branch qui **ne contient pas le tag le plus récent** : la branch a démarré
avant cette release, et la fusionner telle quelle déferait ce que la release a publié. Faites un
rebase sur `origin/main`.

Entre deux releases, `delonix --version` distingue les builds par commit et par distance
(`commit: <hash> (+N commits since vX.Y.Z)`), car deux builds portant le même numéro de version ne
sont pas le même build.

## Langue : l'anglais dans le code (LANG-01)

Les identifiants, les commentaires et les messages destinés à l'utilisateur sont écrits en
**anglais**. Le portugais n'atteint l'opérateur qu'à travers le catalogue de traduction :

- `bins/delonix-runtime-bin/src/cmd/po.rs` — `po::t("…")` pour les chaînes fixes, `po::tf("… {name} …",
  &[("name", value)])` pour les chaînes interpolées (espaces réservés nommés, car une traduction peut
  les réordonner). Le texte `--help` de la CLI est traduit à l'exécution par `po::translate_help`.
- `bins/delonix-runtime-bin/data/pt.po` — les entrées portugaises, embarquées dans le binaire et
  sélectionnées avec `--l18n pt` ou `DELONIX_L18N=pt`.

Une entrée manquante dans le catalogue se replie sur l'anglais ; une chaîne portugaise écrite
directement dans le code est un bug. `scripts/lang_ratchet.py` (job CI `lang`) compte le portugais
qui subsiste dans les identifiants, les commentaires et les messages par rapport à
`scripts/lang_baseline.json`. C'est un **ratchet (cliquet)**, pas un plafond : il échoue lorsqu'un
décompte augmente (du nouveau portugais est entré) **et** lorsqu'il diminue sans que la ligne de base
ait été abaissée. Lorsque vous traduisez quelque chose, exécutez
`python3 scripts/lang_ratchet.py --update` et commitez la nouvelle ligne de base **dans le même
commit** que la traduction. Des tests du crate de la CLI vérifient aussi que l'aide des commandes a
une entrée portugaise — ajoutez-en une lorsque vous ajoutez une commande ou une option.

## Les règles d'architecture imposées par les gates

`scripts/arch_fitness.py` (job CI `arch`) impose la structure décidée dans
[ADR-0040](../../adr/0040-engine-restructuring-layers-ports-node-contract.md). La couche de chaque
crate et la direction autorisée sont listées dans [Architecture](architecture.md). Ce que
cela implique pour une modification :

- **Le moteur ne connaît aucun consommateur.** Aucun nom de produit, de plateforme, de control plane,
  de console ou d'agent qui utilise le moteur — ni tenant, compte, plan ou facturation — dans
  `crates/`, `bins/`, `proto/` ou les manifestes, commentaires compris. Une exigence qui vient d'un
  consommateur s'écrit comme la capacité générique qu'elle est, dans le vocabulaire propre du moteur,
  et n'entre que si elle a du sens pour n'importe quel client. L'historique qui a besoin de noms
  externes vit dans `docs/`, jamais dans le code.
- **Les dépendances pointent vers l'intérieur.** La fondation ne dépend que de la fondation ; les
  contextes ne dépendent pas des adaptateurs ; les adaptateurs et les providers ne dépendent pas des
  interfaces ; un binaire compose **une seule** interface.
- **Le répertoire est la couche.** Un crate vit sous `crates/<layer>/`, conformément à son entrée
  dans la table `LAYERS`. Un nouveau crate entre dans `LAYERS` et dans le bon répertoire dans le même
  commit.
- **Les versions des dépendances ne vivent qu'à la racine**, dans `[workspace.dependencies]` ; un
  crate membre écrit `{ workspace = true, features = [...] }` et rien d'autre.
- **Les exceptions nomment la phase qui les supprime.** Une exception sans phase échoue, tout comme
  une exception qui ne s'applique plus.
- **Ratchets de dette** — par exemple `self_exec_sites` (une bibliothèque qui ré-exécute le binaire
  du moteur au lieu d'appeler une fonction), `library_prints` (`println!`/`eprintln!` dans un crate
  de bibliothèque — les bibliothèques émettent du `tracing`, les interfaces affichent), `env_writes`
  (`env::set_var`/`remove_var`) et `shared_error_imports` (un adaptateur ou un provider qui utilise
  l'`Error` partagée comme la sienne au lieu d'une erreur de crate qui se convertit en elle). La
  liste actuelle est générée :

<!-- dev-docs:begin ratchets -->
`scripts/arch_fitness.py` maintient **5 cliquets de dette** (référence dans `scripts/arch_baseline.json`) :

- `self_exec_sites`
- `library_prints`
- `env_writes`
- `shared_error_imports`
- `raw_error_variant_matches`
<!-- dev-docs:end ratchets -->
Même sémantique que le ratchet de langue (`--list`, `--update`).

Au-delà de ce que le gate peut voir, trois principes décident des revues : **daemonless** (un
nouveau processus résident exige un ADR qui démontre ce que les units systemd, les timers ou la
socket activation n'ont pas pu faire), **rootless-first** (le privilège est un opt-in explicite et
annoncé, jamais un défaut silencieux), et **aucun échec silencieux** (une option acceptée puis
ignorée est pire qu'une option qui n'existe pas — refusez-la plutôt avec une erreur claire).

## Quand écrire un ADR

Écrivez un Architecture Decision Record dans `docs/adr/` **avant** le code lorsqu'une modification
déplace une frontière structurelle, par exemple :

- un nouveau backend ou provider (un hyperviseur, un système de stockage), ou un nouveau port ;
- une nouvelle dépendance externe dans un crate du moteur, ou un nouveau daemon ou processus
  résident ;
- une nouvelle frontière de privilège — qui exige aussi d'abord un spike GO/NO-GO ;
- une modification du contrat de nœud, de la stabilité du schéma de manifeste ou de la structure en
  couches.

Une fonctionnalité de routine à l'intérieur d'une frontière existante n'en a pas besoin. Le format
et la liste actuelle se trouvent dans [`docs/adr/README.md`](../../adr/README.md) : un fichier par
décision, `NNNN-title.md`, en anglais. **Les ADR acceptés ne sont jamais réécrits** — un nouvel ADR
les remplace.

## Ajouter ou modifier une commande de la CLI

- **Câblez chaque point d'entrée.** Plusieurs commandes sont accessibles par plus d'un chemin (par
  exemple `delonix vm pull` et `delonix image vm pull`). Modifiez-les tous, puis vérifiez chacun avec
  le binaire que vous avez construit — y compris la complétion du shell, car la déclaration `clap`
  que vous avez éditée n'est peut-être pas celle que le chemin de l'utilisateur analyse. Le moteur
  de complétion peut être sondé directement ; `_CLAP_COMPLETE_INDEX` est la position du mot en cours
  de complétion :

  ```bash
  COMPLETE=bash _CLAP_COMPLETE_INDEX=3 ./target/debug/delonix -- delonix image vm ''
  ```
- **Validez contre le binaire**, pas contre la source : `./target/debug/delonix <group> <command> --help`
  et une exécution réelle avec les racines d'état isolées (voir [Cloner, compiler et tester](build-and-test.md#isolating-the-engines-state)).
- **Mettez à jour la ligne de base de la CLI** (`scripts/cli-tree.sh --update`) dans le même commit
  lorsque vous ajoutez ou supprimez une feuille, et régénérez le site (`python3 docs/gen.py` avec un
  build de release) lorsque le texte d'aide change.
- **Testez unitairement chaque nouvelle fonction pure** — parseurs, validateurs, constructeurs
  d'arguments. Ce code a un long historique de vrais bugs attrapés exactement là.
- **Classez les erreurs.** Les codes de sortie portent une classe (introuvable, conflit, …) décidée
  en un seul endroit à partir du type d'erreur ; renvoyez la bonne variante d'`Error` plutôt qu'une
  variante générique.

## Commits et pull requests

- Une modification logique par commit ; dites **pourquoi** dans le message — le diff montre déjà
  quoi.
- Référencez l'issue lorsqu'il y en a une.
- Ouvrez la PR contre `main` et remplissez [le modèle](../../../.github/PULL_REQUEST_TEMPLATE.md) :
  ce que vous avez exécuté et, pour le code du runtime, des namespaces, des cgroups ou du réseau, ce
  que vous avez exécuté **en conditions réelles** sur un vrai hôte, avec la commande et sa sortie.
- « Ça compile » et « la commande a renvoyé 0 » ne clôturent pas une modification. Dites ce qui a été
  prouvé et, tout aussi explicitement, ce qui n'a pas été validé et pourquoi.
- Chaque modification est relue par les code owners listés dans
  [`.github/CODEOWNERS`](../../../.github/CODEOWNERS).

## Modifications sensibles pour la sécurité

Signalez-le explicitement dans la PR lorsqu'une modification franchit une frontière de privilège ou
de namespace : le mappage du user namespace, le holder réseau ou sa socket de contrôle,
`setns`/`unshare`, la gestion des capabilities ou de seccomp, ou la gestion de chemins pilotée par
une entrée utilisateur ou de manifeste. Celles-ci font l'objet d'une revue supplémentaire.

Si vous avez trouvé une **vulnérabilité** plutôt qu'un bug — élévation de privilège, évasion de
namespace, injection de commandes, traversée de chemin — n'ouvrez pas d'issue ni de PR publique.
Suivez [`SECURITY.md`](../../../SECURITY.md) (GitHub Private Vulnerability Reporting).

---

**Suivant :** [Releases et stabilité](releases-and-stability.md) — ce que fait un tag poussé, et ce que la CLI et le schéma de manifeste promettent de ne pas casser.
