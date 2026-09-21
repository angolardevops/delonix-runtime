<!-- translated-from: releases-and-stability.md sha256:38c64cb15968ed86136ea555f0d3b7515baa85a8a5e7439fcf65700807c8df37 -->
# Releases et stabilité

**À lire avant :** [Flux de contribution](contributing-workflow.md#version-alignment) (le gate de version) et [Publier la documentation](publishing-docs.md) (ce qu'une release régénère).

Cette page traite de l'autre moitié d'une release : ce que promet le `version` dans
`Cargo.toml`, ce qui se passe quand un tag `v*` est poussé, et ce que le moteur garantit de ne pas
casser sans une version majeure. [Publier la documentation](publishing-docs.md) couvre déjà le
côté documentation d'une release (ce qui est régénéré, ce que vérifie la CI) ; cette page
couvre le numéro de version lui-même et le contrat CLI/manifeste qu'il soutient.

## Le gate de version

`scripts/version_gate.py` (job de CI `version`, `fetch-depth: 0` car il a besoin de tous les
tags) autorise le `version` du workspace, dans le `Cargo.toml` racine, à être exactement l'une
de deux choses :

1. **Égal au tag le plus récent que ce commit contient.** Travail ordinaire entre les releases.
   Deux builds avec le même numéro de version se distinguent par `delonix --version`, qui affiche
   la distance depuis ce tag : `commit: <hash> (+N commits since vX.Y.Z)`.
2. **Supérieur au tag contenu le plus récent, seulement dans le commit de release** — et alors
   `docs/releases/v<version>.md` doit exister, car le workflow de release le publie comme corps
   de la GitHub Release.

Tout le reste échoue, chacun avec une raison liée à un vrai mode de défaillance :

| Ce que voit le gate | Pourquoi ça échoue |
|---|---|
| Un tag plus récent existe que ce commit ne contient pas | La branche a commencé avant cette release ; la fusionner telle quelle annule ce que la release a déjà publié. Fusionnez d'abord `origin/main`. |
| `Cargo.toml` est inférieur au tag contenu le plus récent | Un numéro de version plus ancien serait publié par-dessus un plus récent. |
| `Cargo.toml` est supérieur au tag contenu le plus récent, sans `docs/releases/v<version>.md` | Un bump sans tag correspondant — envoie le téléchargement de `delonix-cri` (résolu depuis la propre version du binaire en cours d'exécution, voir `vmimage.rs`) vers une release qui n'existera jamais. |

La version n'est pas de la décoration : outre le téléchargement de `delonix-cri`, c'est le
`ServerVersion` de l'API Docker, le `delonix_version` enregistré dans chaque sauvegarde de
ressource, et ce que le workflow de release (ci-dessous) vérifie contre le binaire construit
avant de publier quoi que ce soit.

N'incrémentez pas la version dans une PR de fonctionnalité, et n'utilisez pas de suffixe `-dev`
— la première règle du gate couvre déjà le travail ordinaire, et un suffixe `-dev` enverrait le
téléchargement du CRI vers une release qui n'existe pas.

## Ce que fait un tag poussé

`.github/workflows/release.yml` se déclenche sur `push: tags: ["v*"]` et, en un seul job :

1. Construit `delonix`, `delonix-cri`, `delonix-mcp` et `delonix-mgmt` deux fois — une fois
   générique x86-64, une fois avec `-C target-cpu=x86-64-v3` (AVX2/BMI2/FMA) — spécifiquement sur
   `ubuntu-22.04`, afin que la base glibc (2.35) reste compatible avec RHEL 9 et Debian 12, pas
   seulement avec le dernier Ubuntu. `scripts/install.sh` choisit automatiquement le build `-v3`
   quand le CPU de l'hôte le prend en charge.
   Un job `build-arm64` distinct construit nativement les quatre mêmes binaires sur un runner aarch64 (un par
   composant, sans variante `-v3`), et ils sont publiés sous le nom `<name>-aarch64-linux` avec le même
   `SHA256SUMS`. `install.sh` ne les installe pas encore. Le job ne s'exécute que sur une tag `v*`, donc sa
   première exécution a été la release v4.2.0 elle-même ; la CI exécute la suite de tests nativement sur arm64
   dans le job `test (arm64)` à chaque PR.
2. **Régénère le site utilisateur contre ce build de release exact et échoue si `docs/`
   diffère.** Ce gate existe parce qu'il n'a pas toujours existé : un trou du site est parti en
   direct dans la v0.48.0, cachant une nouvelle commande pendant des heures pendant qu'un job de
   CI parallèle était déjà rouge à ce sujet — les deux workflows ne se regardaient tout simplement
   pas l'un l'autre. Exécuter `docs/gen.py` ici, avant de publier, referme cela.
3. Construit un `SHA256SUMS` avec somme de contrôle, un SBOM SPDX 2.3 (`scripts/sbom.py`, à
   partir de `Cargo.lock`, lui-même haché dans `SHA256SUMS`), et — quand le secret
   `MINISIGN_SECRET_KEY` est configuré — une signature minisign sur `SHA256SUMS`. `SHA256SUMS`
   seul ne prouve que l'intégrité du transfert (il vient de la même URL que le binaire) ; la
   signature prouve que la release vient bien de ce projet, puisque `install.sh` embarque la clé
   publique et refuse d'installer une release non signée sans `--insecure-skip-signature`.
   Échouer à signer quand une clé **est** configurée est une erreur bloquante — cela casserait
   d'un coup la vérification de signature de tous les installateurs.
4. Vérifie que son propre binaire rapporte la version du tag (`delonix --version | grep <tag>`)
   avant de publier quoi que ce soit — la même classe de vérification que `version_gate.py` a
   déjà exécutée plus tôt, contre l'artefact qui est réellement sur le point d'être publié.
5. Attache la provenance de build SLSA (`actions/attest-build-provenance`, l'action propre à
   GitHub, gardée volontairement séparée de minisign : la provenance prouve *où et depuis quel
   commit* quelque chose a été construit, pour qui ne fait pas confiance au projet d'emblée ;
   minisign prouve que la release est bien *de ce projet*, pour qui fait déjà confiance à sa clé
   publique embarquée).
6. Publie la GitHub Release, avec `docs/releases/<tag>.md` comme notes quand ce fichier existe,
   ou `--generate-notes` sinon.
7. Fait un checkout de `main` et régénère `docs/RELEASES.md` (`scripts/gen-releases.sh`) et les
   faits générés du manuel (`scripts/dev_docs.py`) et, séparément, le **site** du manuel
   (`scripts/dev_docs_site.py`) — en commitant ce qui a changé, `[skip ci]`, pour que la
   documentation ne soit jamais en retard de plus d'un commit sur une release. Un échec du
   générateur ici est un avertissement fort, pas une release échouée : la release elle-même
   n'en dépend pas.

La moitié **narrative** du manuel — la prose de ces pages — n'est pas régénérée par la CI. Après
la publication d'une release, une revue menée par un mainteneur lit ce qui a changé depuis le tag
précédent (commits, notes de release) et ne met à jour que les pages que ce changement affecte,
exactement comme décrit dans [Publier la documentation § Ce qui se passe au moment de la
release](publishing-docs.md#what-happens-at-release-time).

## Ce qui est stable, et où vit cette promesse

`docs/cli-stability.md` est le contrat réel, dans le même dépôt, lu aussi bien par `delonix
explain` que par les pages générées — cette section ne fait que vous y orienter, car dupliquer
son contenu ici lui donnerait une seconde copie susceptible de se désynchroniser du code. Elle
s'applique depuis la v0.42.3 et, depuis la v1.0.0, se lit comme la véritable promesse de semver
du projet plutôt que comme une note interne au `0.x`.

**Stable — ne casse pas sans une version majeure :**

- Les verbes de cycle de vie container/image (`container run`, `ps`, `stop`, `exec`, …) et les
  verbes d'image, avec les noms et l'ordre des arguments propres à Docker/Podman, et les options
  courtes/longues précises que `docs/cli-stability.md` liste pour `run`/`exec`.
- Les codes de sortie (`0` succès, `4` non trouvé, `5` conflit, `69` capacité de l'hôte
  manquante, `124` délai dépassé, …) et le numéro de dictionnaire `DX-CDNN` que porte chaque
  échec (ADR-0043) — le numéro identifie *quel* échec et ne change jamais de signification ni
  n'est réutilisé ; `delonix explain DX-4501` en cherche un.
- `-o json` sur chaque commande de listage : des champs peuvent être ajoutés, jamais retirés
  ni retypés (ADR-0005).
- Le **schéma du manifeste** pour les Kinds à spec typée (`Container`, `Pod`, `Volume`,
  `Network`, et les autres que liste `delonix manifest schema`) : un champ n'est jamais retiré,
  retypé ou réaffecté ; un nouveau champ est toujours optionnel avec une valeur par défaut qui
  préserve l'ancien comportement ; un champ renommé garde l'ancienne orthographe comme alias ;
  `apiVersion: delonix.io/v1` continue de se charger même après que les groupes par domaine
  (`compute.delonix.io/v1alpha1`, …) sont devenus canoniques. C'est la promesse qui compte le
  plus en pratique — elle protège ce que les gens mettent dans git et relisent dans une PR, pas
  seulement ce qu'ils tapent à une invite.

**Pas stable — peut changer à n'importe quelle version :** `serve cri`/`serve api`/`serve
docker-api` (l'API de gestion locale en particulier n'a aucun contrat publié et n'est
explicitement pas quelque chose contre quoi automatiser — voir [Les crates §
`delonix-mgmt`](crates.md#delonix-mgmt) et les ADR-0040/0041) ; les surfaces impératives
`cluster`/`vm`/`pod`/`workload`/`net` (leur **schéma** de manifeste, quand il existe, est couvert
ci-dessus — seuls les verbes et les options autour ne le sont pas) ; `compose` ; `backup` ;
`mcp` ; `system`/`dashboard`/`completion`/`init`/`man`/`config`/`explain` ; le format de l'état
sur disque sous `$DELONIX_ROOT` ; et `stack history`/`stack rollback` (ADR-0019 — rien ne lit
cet historique pour décider ce qui existe, donc le perdre ne change rien à ce que fait le
réconciliateur).

## Comment un changement incompatible est fait, quand il le faut

Le précédent, déjà appliqué plus d'une fois (la réorganisation de la CLI en v0.30.0, l'inversion
`image list`→`image ls` en v2.0.0) : **une coupure nette, sans alias de compatibilité.**
L'ancienne orthographe échoue avec `unrecognized subcommand`, bruyamment, dans chaque version à
partir de la coupure — jamais un alias silencieux qui change discrètement de comportement plus
tard. `docs/cli-stability.md § Como uma quebra é feita` consigne une vraie leçon tirée de cette
pratique : un renommage peut laisser un appelant *interne* derrière lui (le serveur CRI
lui-même a continué à invoquer un `delonix netns attach` déjà retiré pendant des mois après la
réorganisation de la v0.30.0, cassant la création de pods rootless) — grepper tout le workspace
pour l'ancienne orthographe, pas seulement la documentation et les tests, fait partie de la
coupure.

Un changement incompatible sur quelque chose que cette page ou `docs/cli-stability.md` marque
comme stable a besoin d'un ADR au préalable ([Flux de contribution § Quand écrire un
ADR](contributing-workflow.md#when-to-write-an-adr)), parce que cela déplace une frontière
structurelle par définition — le même raisonnement qui s'applique à un nouveau backend ou à une
nouvelle frontière de privilège s'applique aussi ici.

---

**Ensuite :** [Publier la documentation](publishing-docs.md) — comment le site et ce manuel sont générés, contrôlés par gate et publiés, et ce que votre PR doit régénérer.
