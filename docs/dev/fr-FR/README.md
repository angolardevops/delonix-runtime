<!-- translated-from: README.md sha256:e9205b57d61260f2d44b7c78c60bc5ee9e79f9781fa0948ce1738e55a800ee77 -->
# Delonix Runtime — Manuel du contributeur

Ce manuel s’adresse aux personnes qui veulent **modifier le moteur** : vous avez cloné le dépôt
aujourd’hui et vous voulez envoyer une première pull request sans casser votre hôte ni le moteur.
Après cette page, vous saurez comment le manuel est séquencé et quelles pages lire, dans quel
ordre, pour votre rôle. Si vous voulez seulement *utiliser* Delonix, commencez plutôt par le
[README](../../../README.rst) et le
[site de documentation utilisateur](https://angolardevops.github.io/delonix-runtime/).

<!-- dev-docs:begin crate-count -->
Le workspace compte **23 crates** et livre **4 binaires** (`delonix`, `delonix-cri`, `delonix-mcp`, `delonix-mgmt`).
<!-- dev-docs:end crate-count -->

## Ce qu’est le moteur — et ce qu’il n’est pas

Delonix Runtime est une abstraction d’exécution pour **un nœud** : il exécute des **containers et des microVM**
et gère le réseau et le stockage dont ils ont besoin. Il est déclaratif (ses propres Kinds, regroupés par
`apiVersion` — voir `delonix api-resources`), et il ne parle aux providers (le noyau Linux, libvirt,
Cloud Hypervisor, Proxmox VE, le CRI de Kubernetes) qu’à travers des ports, jamais à travers
des branches `if provider == …` dispersées dans le code.

Trois principes façonnent presque tous les commentaires de revue que vous recevrez :

- **Cloud native** — plan / apply / dérive, API-first (la CLI, l’API de nœud, le CRI et le serveur
  MCP exposent les mêmes opérations), observable au moyen de standards ouverts.
- **Daemonless** — aucun processus résident par défaut. Ce qui doit persister appartient à systemd ou à un
  processus par workload avec un propriétaire clair. Un nouveau daemon exige un ADR.
- **Rootless-first** — le chemin normal s’exécute sans root ; le privilège est un opt-in explicite.

Et une frontière imposée par un gate (contrôle CI) : **le moteur ne connaît aucun consommateur.** Il ne sait pas
qui l’appelle, et il n’a aucune notion de locataire, de compte, d’offre ou de facturation. Une exigence venant d’un
consommateur entre sous la forme d’une capacité générique du moteur, ou elle n’entre pas. Le texte canonique est la
section *«Identidade e fronteira do motor»* en tête de [`AGENTS.md`](../../../AGENTS.md).

## Deux moitiés : faits générés et récit

Les pages de ce manuel mélangent deux types de contenu :

- **Faits** — quels crates existent, leur couche, qui dépend de qui, les binaires, la
  toolchain épinglée, les jobs de CI. Ils vivent entre les marqueurs `<!-- dev-docs:begin <key> -->` et
  `<!-- dev-docs:end <key> -->` et sont **générés** par `python3 scripts/dev_docs.py`
  à partir de `Cargo.toml`, `scripts/arch_fitness.py`, `rust-toolchain.toml` et
  `.github/workflows/ci.yml`. Ne les modifiez jamais à la main — la CI exécute `dev_docs.py --check` et échoue.
  Si un fait est faux, corrigez la source ou le générateur.
- **Récit** — pourquoi les choses sont ainsi, comment les flux fonctionnent, comment contribuer. Il est
  écrit à la main et relu après chaque release. Voir [Publier la documentation](publishing-docs.md).

## Comment ce manuel est organisé

Les pages forment **un seul parcours**, lu de haut en bas. Chaque page s’ouvre sur une ligne
**Avant de lire** qui nomme les pages antérieures qu’elle suppose acquises, et se termine par une
ligne **Suivant** pointant vers la page qui s’appuie dessus. Le numéro affiché à côté d’une page
dans la barre latérale du site est sa position dans cet ordre. Les numéros de section *à
l’intérieur* d’une page (par exemple §3.8 dans l’initiation à Rust, ou 13.4 dans la page des
standards) sont des repères locaux maintenus stables pour les liens ; ce ne sont pas des positions
de page.

Le parcours est regroupé en huit parties :

| Partie | Pages | Ce que vous en tirez |
|---|---|---|
| **Commencer** | [Commencer ici](start-here.md) · [IaaS et cloud native](iaas-and-cloud-native.md) | Un checkout qui fonctionne, un premier chemin de contribution, et le modèle mental de la place d’un moteur de nœud dans un cloud |
| **Fondations** | [Fondations Linux](linux-foundations.md) · [Initiation au cloud native](cloud-native-primer.md) · [Initiation à Rust](rust-primer.md) | Les primitives du noyau en pratique, comment le moteur utilise chacune, et le Rust dans lequel ce code est écrit |
| **Préparer et compiler** | [Préparer votre environnement](environment.md) · [Cloner, compiler et tester](build-and-test.md) | Un hôte capable d’exécuter les chemins réels, et chaque gate de CI comme commande locale |
| **Architecture** | [Structure du projet](project-structure.md) · [Architecture](architecture.md) · [Les crates](crates.md) · [System Design Interview](system-design-interview.md) | Où se trouvent les choses, pourquoi elles sont ainsi découpées, ce que possède chaque crate, et le raisonnement derrière la conception |
| **Images et microVM** | [Delonixfile et VMfile](delonixfile-and-vmfile.md) · [Construire des microVM](microvm-setup.md) | Les deux grammaires de build, et les VM depuis les prérequis de l’hôte jusqu’au démarrage |
| **Opérer et déboguer** | [Diagnostic des problèmes](troubleshooting.md) · [Comment les noms atteignent `/etc/hosts`](service-names-and-hosts.md) | Un index par symptôme de ce qu’un gate ou une exécution réelle affiche, et le mécanisme qui publie les noms de service et les hôtes de route sur la machine de l’opérateur |
| **Contribuer** | [Conventions de code](coding-conventions.md) · [Ajouter un Kind](adding-a-kind.md) · [Flux de contribution](contributing-workflow.md) · [Releases et stabilité](releases-and-stability.md) · [Publier la documentation](publishing-docs.md) | Comment le code doit être écrit, comment un Kind déclaratif s’ajoute, comment un changement est envoyé, ce qu’une release promet de ne pas casser, et comment la documentation suit |
| **Référence** | [Standards cloud native](cloud-native-standards.md) · [Variables d’environnement](environment-variables.md) · [Glossaire](glossary.md) | Des pages où l’on cherche des informations : conformité par standard, chaque nom `DELONIX_*`, chaque terme |

Les concepts sont **enseignés une seule fois** : une primitive du noyau dans
[Fondations Linux](linux-foundations.md), comment le moteur l’utilise dans
[Initiation au cloud native](cloud-native-primer.md), et le standard qu’elle suit avec son état de
conformité dans [Standards cloud native](cloud-native-standards.md). Quand une page mentionne
quelque chose enseigné ailleurs, elle y renvoie au lieu de le répéter.

## Parcours de lecture par rôle

Personne n’est censé lire les vingt-deux pages avant un premier changement. Choisissez la ligne qui
vous décrit et lisez ses pages dans l’ordre indiqué ; gardez le [Glossaire](glossary.md) ouvert.

| Rôle | Lisez, dans cet ordre — et pourquoi |
|---|---|
| **Première PR, sans temps** | 1. [Commencer ici](start-here.md) — la vérification du jour 0 et les huit étapes d’une première PR. 2. [Préparer votre environnement](environment.md#known-host-traps) — seulement *Pièges d’hôte connus*. 3. [Cloner, compiler et tester](build-and-test.md#the-gates-ci-runs) — les gates que vous devez passer. 4. [Les crates](crates.md) — seulement la section du crate que vous touchez. 5. [Flux de contribution](contributing-workflow.md) — comment la PR est jugée. |
| **Ingénieur DevOps** (CI, packaging, installation, releases) | 1. [Commencer ici](start-here.md) — la configuration et les règles. 2. [Préparer votre environnement](environment.md) — ce dont un hôte a besoin et les pièges qui ressemblent à des bugs du moteur. 3. [Cloner, compiler et tester](build-and-test.md) — installer un build, chaque job de CI comme commande locale, E2E et chaos. 4. [Structure du projet](project-structure.md) — ce qui est généré, ce que la CI vérifie, ce que `release.yml` rafraîchit. 5. [Diagnostic des problèmes](troubleshooting.md) — reconnaître l’échec d’un gate à son message. 6. [Releases et stabilité](releases-and-stability.md) — le gate de version, ce que fait une tag poussée, ce qui est stable. 7. [Publier la documentation](publishing-docs.md) — ce qui se passe au moment de la release. 8. [Variables d’environnement](environment-variables.md) — chaque réglage et lesquels abaissent une frontière. |
| **Ingénieur plateforme** (qui construit sur les interfaces du moteur) | 1. [IaaS et cloud native](iaas-and-cloud-native.md) — quelle couche est le moteur et ce qu’il laisse à un control plane. 2. [Initiation au cloud native](cloud-native-primer.md#48-declarative-reconciliation) — les Kinds et le réconciliateur à trois voies. 3. [Architecture](architecture.md) — les interfaces (CLI, CRI, API de gestion, MCP, contrat de nœud) et les couches. 4. [Les crates](crates.md) — `delonix-stack`, `delonix-cri`, `delonix-mgmt`, `delonix-mcp`. 5. [Ajouter un Kind](adding-a-kind.md) — la table et le câblage du réconciliateur qu’exige un nouveau Kind. 6. [System Design Interview](system-design-interview.md) — les choix d’API et leurs compromis. 7. [Standards cloud native](cloud-native-standards.md) — ce qui est conforme, partiel ou absent, avec des dates. |
| **SRE** (opérant des nœuds, diagnostiquant des pannes) | 1. [Fondations Linux](linux-foundations.md) — répondre à « quel namespace, quel cgroup, qui tient ce fd » avec une commande. 2. [Préparer votre environnement](environment.md#diagnosing-the-host) — diagnostiquer un hôte et ses pièges. 3. [Diagnostic des problèmes](troubleshooting.md) — un index par symptôme pour les échecs de gate et de runtime. 4. [Architecture](architecture.md#level-2-containers-executables-and-processes) — quels processus existent en runtime, l’état sur disque, les limitations connues. 5. [System Design Interview](system-design-interview.md#7-failure-modes-and-the-limits-of-one-node) — modes de panne et limites d’un nœud. 6. [Conventions de code](coding-conventions.md#38-exit-codes-and-dx_-codes) — ce que signifie un code de sortie. 7. [Variables d’environnement](environment-variables.md#observability) — logging, OTLP et les échappatoires. 8. [Standards cloud native](cloud-native-standards.md#1311-opentelemetry) — OpenTelemetry et Prometheus. |
| **Développeur cloud** (Kinds, manifestes, images, compatibilité Compose/Docker) | 1. [IaaS et cloud native](iaas-and-cloud-native.md) — les principes tels qu’ils apparaissent dans le code. 2. [Initiation au cloud native](cloud-native-primer.md) — images OCI et réconciliation déclarative. 3. [Cloner, compiler et tester](build-and-test.md) — compiler et exécuter isolé. 4. [Les crates](crates.md#delonix-stack) — `delonix-stack` et `delonix-oci`. 5. [Delonixfile et VMfile](delonixfile-and-vmfile.md) — les grammaires de build. 6. [Conventions de code](coding-conventions.md#36-kinds-api-groups-and-manifest-fields) — règles pour les Kinds et les champs. 7. [Ajouter un Kind](adding-a-kind.md) — câbler un Kind dans le réconciliateur de bout en bout. 8. [Standards cloud native](cloud-native-standards.md#138-the-workload-api-own-kinds-and-the-node-contract) — Kinds propres, API Docker et sous-ensembles de Compose. |
| **Développeur Linux** (namespaces, cgroups, réseau, VM) | 1. [Fondations Linux](linux-foundations.md) — les primitives en pratique. 2. [Initiation au cloud native](cloud-native-primer.md) — où vit chaque primitive dans le code. 3. [Initiation à Rust](rust-primer.md#34-unsafe-ffi-and-linux-syscalls) — `unsafe`, syscalls, `fork`/`clone` dans des processus multithreads. 4. [Préparer votre environnement](environment.md) — pièges d’AppArmor et de délégation de cgroup. 5. [Architecture](architecture.md) — l’infrastructure réseau rootless et les deux flux en séquences. 6. [Les crates](crates.md#delonix-linux) — `delonix-linux`, `delonix-sdn`, `delonix-vm`. 7. [Construire des microVM](microvm-setup.md) — KVM, Cloud Hypervisor, libvirt. 8. [Conventions de code](coding-conventions.md#7-unsafe-syscalls-and-processes) — les règles pour `unsafe` et les processus. |

Deux tâches plus étroites ont leur propre raccourci : **changer la façon dont la documentation est
produite** commence à [Publier la documentation](publishing-docs.md) ; **configurer ou isoler une
exécution** commence à [Isoler l’état du moteur](build-and-test.md#isolating-the-engines-state)
puis [Variables d’environnement](environment-variables.md).

## Pages

| Page | Ce à quoi elle répond |
|---|---|
| [Commencer ici](start-here.md) | Vérification de la configuration au jour 0, votre première contribution de bout en bout, où va une modification, les règles et leurs sources, que faire quand vous êtes bloqué |
| [IaaS et cloud native](iaas-and-cloud-native.md) | De quoi est faite une IaaS, quelle couche est ce moteur, ce qu’il laisse à un control plane, et comment les principes cloud native apparaissent dans ses fichiers |
| [Fondations Linux](linux-foundations.md) | Processus, namespaces, cgroups v2, descripteurs de fichier et signaux — en pratique, avec les commandes pour inspecter chacun |
| [Initiation au cloud native](cloud-native-primer.md) | Comment le moteur utilise namespaces, cgroups, capabilities, OCI, réseau, CRI, KVM et réconciliation — avec fichiers et symboles |
| [Initiation à Rust pour cette base de code](rust-primer.md) | Le Rust que cette base de code utilise réellement |
| [Préparer votre environnement](environment.md) | Ce dont le noyau et l’hôte ont besoin, la toolchain épinglée, et les pièges de l’hôte qui ressemblent à des bugs du moteur |
| [Cloner, compiler et tester](build-and-test.md) | Compiler, installer, exécuter les tests, chaque gate de CI comme commande locale, E2E et chaos avec isolation |
| [Structure du projet](project-structure.md) | Ce qu’est chaque fichier et répertoire de premier niveau, qui le modifie, et ce qui est généré |
| [Architecture](architecture.md) | Couches, le graphe des crates, processus en runtime, chemins de contrôle et de données, état sur disque |
| [Les crates](crates.md) | Un bloc par crate : responsabilité, types principaux, par où commencer la lecture |
| [System Design Interview](system-design-interview.md) | Le moteur conçu comme une réponse d’entretien, puis comparé à ce qui a été construit |
| [Delonixfile et VMfile](delonixfile-and-vmfile.md) | Les grammaires des fichiers de build et en quoi elles diffèrent d’un Dockerfile |
| [Construire des microVM](microvm-setup.md) | KVM, Cloud Hypervisor et firmware, libvirt, images de VM |
| [Diagnostic des problèmes](troubleshooting.md) | Un index par symptôme : messages d’échec de gate, pièges de l’hôte et leurs correctifs, au même endroit |
| [Comment les noms atteignent `/etc/hosts`](service-names-and-hosts.md) | Le bloc délimité unique, les deux façons dont un nom y entre (`hosts: [host]` et `delonix hosts sync`), ce qu’il refuse, et comment le tester sans root |
| [Conventions de code](coding-conventions.md) | Comment le code de ce dépôt est écrit, et la liste de contrôle qu’appliquent les relecteurs |
| [Ajouter un Kind](adding-a-kind.md) | La table, le schéma et le câblage du réconciliateur qu’exige un nouveau Kind déclaratif, illustré avec `Service` |
| [Flux de contribution](contributing-workflow.md) | Worktrees, versions, règle de langue, règles d’architecture, ADR, commits et PR |
| [Releases et stabilité](releases-and-stability.md) | Le gate de version, ce que fait une tag poussée, et ce que la CLI et le schéma de manifeste promettent de ne pas casser |
| [Publier la documentation](publishing-docs.md) | Comment le site et ce manuel sont générés, contrôlés et publiés |
| [Standards cloud native](cloud-native-standards.md) | Chaque standard, ce qu’il exige, comment Delonix l’implémente, et son état de conformité |
| [Variables d’environnement](environment-variables.md) | Chaque variable `DELONIX_*` que lit le code : qui la lit, ce qu’elle change, sa valeur par défaut, et lesquelles abaissent une frontière |
| [Glossaire](glossary.md) | Les termes du moteur et du cloud native que vous rencontrez ici, avec leur sens dans Delonix et où en lire davantage |

Autres références vers lesquelles on vous renverra : [`ARCHITECTURE.md`](../../../ARCHITECTURE.md) (diagrammes C4),
[`docs/adr/`](../../adr/README.md) (décisions d’architecture), [`SECURITY.md`](../../../SECURITY.md)
(signalements privés de vulnérabilités) et [`CONTRIBUTING.md`](../../../CONTRIBUTING.md) (la courte porte d’entrée).

---

**Suivant :** [Commencer ici](start-here.md) — vérifiez votre configuration en trente minutes et parcourez une première contribution de bout en bout.
