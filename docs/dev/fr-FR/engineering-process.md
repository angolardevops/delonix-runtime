<!-- translated-from: engineering-process.md sha256:b765b1a8f9865fdcb1254f484027564885de6238b1bcaa4a3a234c48e63f1e21 -->
# Processus d'ingénierie (canonique)

Cette page est la **source unique de vérité** sur la façon dont le travail circule dans ce dépôt :
de quelle branch part une modification, comment elle est construite en isolation, comment elle est
validée, comment elle atterrit, et comment ses traces sont nettoyées. AGENTS.md,
[CONTRIBUTING.md](../../../CONTRIBUTING.md), le [flux de contribution](contributing-workflow.md) et
les skills de session s'en remettent tous à cette page pour le modèle de branches et le cycle de
vie — ils ne le répètent pas. Lorsqu'ils sont en désaccord avec cette page, c'est cette page qui
l'emporte ; corrigez l'autre endroit plutôt que de bifurquer la règle.

> **C'est une décision Delonix, pas une affirmation sur d'autres projets.** Le modèle de branches
> ci-dessous a la forme de GitFlow par décision du propriétaire (2026-10-23). Docker/Moby et Podman
> sont des projets PR-first, basés sur un trunk ; nous empruntons leur discipline de *contribution*
> et de *revue* (tests avant merge, validation en conditions réelles des modifications touchant le
> kernel, commits focalisés de style DCO, revue CODEOWNERS) — pas leur topologie de branches.
> N'affirmez pas qu'ils utilisent ce flux.

## Les deux branches à longue durée de vie

- **`main`** est la ligne publiée. Elle n'avance jamais que par une release ou un hotfix. Son `HEAD`
  est ce qu'obtient un utilisateur qui installe depuis les sources.
- **`develop`** est la ligne d'intégration. Le travail quotidien en est tiré et y revient. Une
  release promeut `develop` dans `main`.

Les deux sont récupérées fraîchement avant usage ; ne branchez jamais depuis une référence locale
périmée, ni depuis la branch d'une autre tâche par commodité.

## Politique de branches (obligatoire)

Classez par la **nature du problème**, pas par le préfixe que quelqu'un a suggéré. Le préfixe suit la
classification ; il ne la détermine pas.

| Genre de travail            | Préfixe de branch      | Part de (remote frais)     | Destination normale           |
| --------------------------- | ---------------------- | -------------------------- | ----------------------------- |
| Fonctionnalité              | `feat/`                | `origin/develop`           | `develop`                     |
| Corvée / maintenance        | `chore/`               | `origin/develop`           | `develop`                     |
| Refactor                    | `refactor/`            | `origin/develop`           | `develop`                     |
| Performance                 | `perf/`                | `origin/develop`           | `develop`                     |
| Documentation               | `docs/`                | `origin/develop`           | `develop`                     |
| Test / CI / build           | `test/` `ci/` `build/` | `origin/develop`           | `develop`                     |
| **Correctif / hotfix**      | `fix/` `bug/` `hotfix/`| **`origin/main`**          | **`develop` et, lorsqu'il affecte la ligne publiée, `main`** |

Les noms sont courts et descriptifs, avec l'identifiant de tâche quand il en existe un :
`feat/DX-1234-http-route-timeouts`, `fix/compose-secret-lazy-read`.

### La règle fix/bug/hotfix, et son seul vrai piège

Un correctif part de `main` parce qu'un correctif est, par défaut, une correction de ce qui est
publié. La même correction doit aussi atteindre `develop` pour que la prochaine release ne régresse
pas.

Le piège, que cette politique interdit : **un défaut qui n'existe que sur `develop`**. Ne le
branchez pas depuis `main` (il ne s'y reproduira pas), et ne fusionnez pas `develop` dans `main`
pour le faire se reproduire. Un défaut propre à `develop` est corrigé sur une branch tirée de
`develop` et n'atterrit que sur `develop` — consignez cette condition explicitement lorsque vous le
classez. Si cette politique bloque un jour un correctif cohérent, remontez le conflit concret au
propriétaire pour décision ; ne contournez pas la règle en silence.

### Porter un correctif vers les deux destinations

Lorsqu'un correctif vise la ligne publiée, il atterrit sur `main` à travers les gates de release et
la même modification atteint `develop`. **Ne fusionnez jamais `develop` dans `main`** pour y porter
un correctif isolé. Choisissez le merge, le back-merge ou le `cherry-pick` selon la convention
d'historique linéaire ci-dessous, consignez l'équivalence des commits, et appliquez la modification
une seule fois — pas deux. L'autorisation d'intégrer dans `develop` n'autorise **pas** à couper une
release, un tag ou à publier des artefacts ; ceux-ci suivent le processus de release.

## Un worktree par tâche

Plusieurs sessions écrivent dans ce clone à la fois (mesuré : le `HEAD` de `main` a bougé quatre
fois pendant une session, huit worktrees vivants, deux régénérant le même fichier à quelques minutes
d'intervalle). Chaque tâche reçoit donc son propre worktree et sa propre branch, et rien n'est édité
directement dans le checkout `main` ou `develop`.

```bash
git fetch origin --prune
# base is origin/develop for feat/chore/..., origin/main for fix/bug/hotfix
git worktree add -b <prefix>/<task> \
    <workspace>/.worktrees/delonix-runtime/<task> origin/<base>
cd <workspace>/.worktrees/delonix-runtime/<task>
export CARGO_TARGET_DIR=<repo>/target   # reuse the build cache safely
```

- **Jamais dans `/tmp`** — il est vidé au démarrage et cet hôte redémarre ; un redémarrage en cours
  de tâche emporte le travail non commité. Utilisez le `.worktrees/` persistant à côté des dépôts
  (ignoré par le harness de workspace). Commitez et poussez la branch **tôt et à chaque étape qui a
  de la valeur**.
- **Isolez l'état, pas seulement le code.** Un worktree isole les fichiers ; il **n'isole pas** le
  moteur Delonix unique et partagé de cet hôte, ni les ports, volumes ou fixtures de test. Ne
  partagez pas de fichiers d'état, de ports, de volumes ni de ressources de test entre des tâches
  parallèles. Les caches de build ne peuvent être partagés que lorsque l'outil garantit que c'est
  sûr ; n'exécutez jamais un binaire d'un autre worktree sans vérifier son identité
  (`target/release/delonix --version` + distance de commit).
- **Indexez par nom** (`git add <file>`, jamais `-A`/`-u`/`.`), reconnaissez chaque entrée de
  `git status --short` comme la vôtre, et **jamais de `git checkout -- <path>`** dans une
  arborescence partagée — cela détruit le travail non commité d'une autre session sans stash.
- **Faites un rebase, pas un merge** sur un push refusé (`git pull --rebase`) : l'historique est
  linéaire, pas de commits de merge.
- **Plafond de WIP : au plus 3 worktrees/sessions ouverts à la fois.** En ouvrir un quatrième force à
  en fermer ou à en garer un, avec une passation écrite.

## Le cycle de vie d'un prompt

Chaque nouvelle requête exécute cette boucle. Une petite tâche peut être un seul sprint — ne
fabriquez pas de cérémonie ; mais ne confondez pas non plus « implémentation prête » avec « fini ».

1. Récupérez les instructions, les décisions et le point de contrôle (le contrat de session,
   `AGENTS.md` pour le domaine, `docs/adr/` pour les décisions).
2. Définissez l'objectif, le périmètre et les critères d'acceptation.
3. Classez le travail et choisissez la branch de base dans la table ci-dessus.
4. Identifiez les spécialistes nécessaires (voir « Délégation »).
5. Construisez le graphe de dépendances ; séquencez les fronts dépendants, parallélisez les
   indépendants.
6. Organisez l'exécution en sprints courts dimensionnés au travail.
7. Créez le(s) worktree(s).
8. Implémentez, relisez, validez.
9. Intégrez le travail terminé dans `develop`.
10. Nettoyez les ressources et mettez à jour le point de contrôle.

### Le contrat de chaque sprint

Un sprint livre une **modification concrète et utilisable** — pas de stubs ni de chemins fictifs
présentés comme complets, et jamais un état qui laisse `develop` fonctionnellement cassé. Il
nomme : le résultat fonctionnel ; les tâches et leurs dépendances ; les spécialistes responsables ;
les critères d'acceptation ; les risques ; les tests ; les preuves collectées ; et la condition
d'intégration.

## Délégation aux agents spécialistes

Lorsque l'environnement offre des agents spécialistes, choisissez-les selon le problème
(Rust/systèmes, cycle de vie OCI, CRI/CNI/Kubernetes, réseau/isolation, virtualisation/providers,
images cloud, CLI/DX, sécurité, tests fonctionnels, CI/release). Pour chacun, énoncez : l'objectif,
la spécialité, le périmètre, la branch et le worktree, les fichiers/contrats qu'il possède, les
dépendances, les critères d'acceptation, les commandes de validation, et le format de livraison. Les
fronts indépendants tournent en parallèle ; les fronts dépendants tournent en séquence à partir d'un
contrat déjà intégré. **Deux agents n'éditent jamais le même worktree en même temps.** Si
l'environnement n'offre aucun agent, dites-le et exécutez les responsabilités séquentiellement — ne
simulez pas une délégation qui n'a pas eu lieu.

L'agent principal possède l'architecture, les contrats communs et l'intégration. Lorsqu'un front
dépend d'un autre : stabilisez et validez le premier, intégrez-le dans `develop`, coupez le front
suivant depuis le `develop` mis à jour, et re-validez le contrat consommé. Ne lancez pas
d'implémentations parallèles sur des hypothèses incompatibles à propos d'un schéma, d'un trait ou
d'une API. Contrôlez la concurrence de build/test pour éviter de saturer le CPU, la RAM et les E/S.

## Qualité avant intégration

Avant qu'une tâche soit considérée comme validée :

- Confirmez les exigences et les sources pertinentes ; relisez le diff.
- Exécutez les gates que le dépôt exige — `make ci` (ou le sous-ensemble que la modification touche,
  avec le `rc=0` cité). Zéro avertissement clippy est imposé, pas indicatif.
- Exécutez des tests proportionnels au risque ; une modification qui dépend du kernel, d'un provider
  ou d'un protocole est validée **en conditions réelles**, et la PR dit ce qui a été testé (commande,
  attendu vs. réel).
- Mettez à jour la documentation et les contrats que la modification affecte
  (`scripts/dev_docs.py --check`, `scripts/dev_docs_site.py --check`).
- Confirmez l'absence de secrets ou d'artefacts accidentels ; vérifiez la migration et la
  compatibilité là où elles s'appliquent.
- Pour les modifications pertinentes, un **agent relecteur différent de l'implémenteur** traque les
  défauts, les régressions, les lacunes de test et les violations de contrat — approuver la
  description de l'auteur n'est pas une revue.

Ne modifiez pas les assertions, ne réduisez pas l'isolation et n'ignorez pas les échecs pour obtenir
du vert. Ne déclarez pas l'intégration validée quand les tests matériels manquent.

## Intégration dans `develop` (obligatoire)

Le travail terminé et validé est intégré dans `develop` dans la **même séquence de travail**,
lorsque les permissions et les gates le permettent. L'intégration se fait par **pull request,
squash-mergée, avec la suite CI verte** — en accord avec l'historique linéaire et les Conventional
Commits déjà en usage. Avant d'intégrer :

1. `git fetch origin --prune` et vérifiez si `develop` a bougé.
2. Résolvez les conflits sur la branch de travail (`git pull --rebase origin develop`).
3. Re-exécutez les tests affectés.
4. Confirmez le candidat exact à intégrer.
5. Obtenez la revue requise.
6. Ouvrez/complétez la PR contre `develop` ; laissez la CI finir avant de merger.

Respectez les PR, les checks et les protections. Ne contournez jamais un contrôle par un push direct
ou un force-push. Si une protection bloque l'intégration, laissez la PR et les preuves prêtes et
expliquez le blocage exact — ne marquez pas le travail comme intégré.

La CI exécute la suite de gates au push sur `main` et `develop` et sur les pull requests visant l'un
ou l'autre (voir [`.github/workflows/ci.yml`](../../../.github/workflows/ci.yml)). Une branch poussée
sans PR ne prouve pas, à elle seule, « CI verte ».

## Nettoyage après intégration

Après que l'intégration est **confirmée** — pas supposée — vérifiez : la branch de destination
localement et à distance ; que la modification est effectivement incluse ; les checks requis ; aucun
changement local restant ; aucun commit non préservé ; et aucun agent/processus n'utilisant encore
le worktree. Ensuite :

1. `git worktree remove <path>` (jamais `--force` pour cacher des changements en attente).
2. `git branch -d <branch>` (jamais `-D`).
3. Supprimez la branch distante si elle appartient à la tâche et n'est plus nécessaire.
4. `git worktree prune` pour les références périmées.
5. Mettez à jour le registre de tâche / le point de contrôle.

Un squash ou un cherry-pick signifie que **l'identité du commit change** — prouvez l'inclusion par la
PR, le diff et `git cherry develop <branch>`, pas par la seule ascendance. Ne supprimez jamais la
branch ou le worktree de quelqu'un d'autre. Une ressource bloquée reste identifiée avec sa raison et
son prochain pas — pas de branches abandonnées sans état.

## Définition de terminé, et les états en chemin

Une tâche est **CONCLUÍDA / DONE** uniquement lorsque : les critères d'acceptation sont remplis ; la
revue est terminée ; les tests requis passent ; la documentation et les contrats sont à jour ;
l'intégration dans `develop` est confirmée ; toute intégration supplémentaire dans `main` est
résolue lorsqu'elle est dans le périmètre autorisé ; le worktree et la branch temporaires sont
retirés en sécurité ; et le point de contrôle est mis à jour.

Utilisez les états intermédiaires explicites dans le contrat de session :

`EM IMPLEMENTAÇÃO` → `EM REVISÃO` → `BLOQUEADO NA VALIDAÇÃO` →
`VALIDADO, A AGUARDAR INTEGRAÇÃO` → `INTEGRADO, LIMPEZA PENDENTE` → `CONCLUÍDO`.

## Le harness qui impose ceci

`scripts/gitflow.py` transforme cette page en contrôles exécutables. Il valide le dépôt et le remote
avant toute mutation, supporte `--dry-run`, est idempotent, préserve les données, et propage les
codes de sortie.

```bash
python3 scripts/gitflow.py classify "fix a crash in compose config"   # -> prefix + base
python3 scripts/gitflow.py start  <prefix>/<task>  [--dry-run]          # fetch, base, worktree, contract
python3 scripts/gitflow.py preflight                                    # remote, base freshness, stale worktree refs, WIP cap
python3 scripts/gitflow.py gates                                        # run the gates, collect evidence
python3 scripts/gitflow.py integrate  [--dry-run]                       # rebase, re-gate, push, open PR to develop
python3 scripts/gitflow.py done                                         # prove inclusion, checks, clean tree
python3 scripts/gitflow.py cleanup  [--dry-run]                         # remove worktree + branches safely
```

`make gitflow ARGS="<subcommand> ..."` est la même chose à travers le Makefile. Les hooks locaux
assistent la personne au clavier ; les checks CI et (quand le propriétaire les active) les
protections de branch sont ce qui tient les règles importantes. Les fichiers d'instructions ne
remplacent pas un contrôle exécutable.
