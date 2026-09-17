<!-- translated-from: README.md sha256:29776abb02b6075313674773e1fe104e088721c2ffd8ce4d80684b8fcc3ec301 -->
# Delonix Runtime — Manuel du contributeur

Ce manuel s’adresse aux personnes qui veulent **modifier le moteur** : vous avez cloné le dépôt
aujourd’hui et vous voulez envoyer une première pull request sans casser votre hôte ni le moteur. Si vous voulez seulement
*utiliser* Delonix, commencez plutôt par le [README](../../../README.rst) et le
[site de documentation utilisateur](https://angolardevops.github.io/delonix-runtime/).

<!-- dev-docs:begin crate-count -->
Le workspace compte **22 crates** et livre **4 binaires** (`delonix`, `delonix-cri`, `delonix-mcp`, `delonix-mgmt`).
<!-- dev-docs:end crate-count -->

## Ce qu’est le moteur — et ce qu’il n’est pas

Delonix Runtime est une abstraction d’exécution pour **un nœud** : il exécute des **containers et des microVM**
et gère le réseau et le stockage dont ils ont besoin. Il est déclaratif (ses propres Kinds, regroupés par
`apiVersion` — voir `delonix api-resources`), et il ne parle aux providers (le noyau Linux, libvirt,
Cloud Hypervisor, Proxmox VE, le CRI de Kubernetes) qu’à travers des ports, jamais à travers
des branches `if provider == …` dispersées dans le code.

Trois principes façonnent presque tous les commentaires de revue que vous recevrez :

- **Cloud native** — plan / apply / dérive, API-first (la CLI, l’API de nœud, le CRI et le serveur
  MCP exposent les mêmes opérations), observable au moyen de standards ouverts.
- **Daemonless** — aucun processus résident par défaut. Ce qui doit persister appartient à systemd ou à un
  processus par workload avec un propriétaire clair. Un nouveau daemon exige un ADR.
- **Rootless-first** — le chemin normal s’exécute sans root ; le privilège est un opt-in explicite.

Et une frontière imposée par un gate (contrôle CI) : **le moteur ne connaît aucun consommateur.** Il ne sait pas
qui l’appelle, et il n’a aucune notion de locataire, de compte, d’offre ou de facturation. Une exigence venant d’un
consommateur entre sous la forme d’une capacité générique du moteur, ou elle n’entre pas. Le texte canonique est la
section *«Identidade e fronteira do motor»* en tête de [`AGENTS.md`](../../../AGENTS.md).

## Deux moitiés : faits générés et récit

Les pages de ce manuel mélangent deux types de contenu :

- **Faits** — quels crates existent, leur couche, qui dépend de qui, les binaires, la
  toolchain épinglée, les jobs de CI. Ils vivent entre les marqueurs `<!-- dev-docs:begin <key> -->` et
  `<!-- dev-docs:end <key> -->` et sont **générés** par `python3 scripts/dev_docs.py`
  à partir de `Cargo.toml`, `scripts/arch_fitness.py`, `rust-toolchain.toml` et
  `.github/workflows/ci.yml`. Ne les modifiez jamais à la main — la CI exécute `dev_docs.py --check` et échoue.
  Si un fait est faux, corrigez la source ou le générateur.
- **Récit** — pourquoi les choses sont ainsi, comment les flux fonctionnent, comment contribuer. Il est
  écrit à la main et relu après chaque release. Voir [Publier la documentation](publishing-docs.md).

## Parcours de lecture

| Si vous voulez… | Lisez, dans l’ordre |
|---|---|
| **Partir de zéro, sans personne à qui demander** | [Commencer ici](start-here.md) (gardez [Glossaire](glossary.md) ouvert) → le parcours ci-dessous qui correspond à votre modification |
| **Envoyer une première PR** (un correctif de CLI, un correctif de doc, une petite fonctionnalité) | [Commencer ici](start-here.md) → [Préparer votre environnement](environment.md) → [Cloner, compiler et tester](build-and-test.md) (gardez [Variables d’environnement (`DELONIX_*`)](environment-variables.md) sous la main) → [Flux de contribution](contributing-workflow.md) → [Conventions de code](coding-conventions.md) → [Les crates](crates.md) pour le crate que vous touchez |
| **Comprendre le moteur en profondeur** | [Initiation à Rust pour cette base de code](rust-primer.md) → [Initiation au cloud native](cloud-native-primer.md) → [Standards cloud native, couche par couche](cloud-native-standards.md) → [Architecture](architecture.md) → [Les crates](crates.md) → [Entretien de conception de système — le Delonix Engine](system-design-interview.md) |
| **Travailler sur les VM ou les images de VM** | [Préparer votre environnement](environment.md) → [Cloner, compiler et tester](build-and-test.md) → [Delonixfile et VMfile](delonixfile-and-vmfile.md) → [Construire des microVMs](microvm-setup.md) → la section `delonix-vm` de [Les crates](crates.md) → la section de réglage des VM de [Variables d’environnement (`DELONIX_*`)](environment-variables.md) |
| **Changer la façon dont la documentation est produite** | [Publier la documentation](publishing-docs.md) |
| **Configurer, isoler ou régler une exécution** (racines d’état, logs, échappatoires, providers) | [Isoler l’état du moteur](build-and-test.md#isolating-the-engines-state) → [Variables d’environnement (`DELONIX_*`)](environment-variables.md) |

## Pages

| # | Page | Ce à quoi elle répond |
|---|---|---|
| 00 | [Commencer ici](start-here.md) | Vérification de la configuration au jour 0, votre première contribution de bout en bout, où va une modification, les règles et leurs sources, que faire quand vous êtes bloqué |
| 01 | [Préparer votre environnement](environment.md) | Ce dont le noyau et l’hôte ont besoin, la toolchain épinglée, et les pièges de l’hôte qui ressemblent à des bugs du moteur |
| 02 | [Cloner, compiler et tester](build-and-test.md) | Compiler, exécuter les tests, chaque gate de CI comme commande locale, E2E et chaos avec isolation |
| 03 | [Initiation à Rust](rust-primer.md) | Le Rust que cette base de code utilise réellement |
| 04 | [Initiation au cloud native](cloud-native-primer.md) | Namespaces, cgroups v2, OCI, CRI, CNI, nftables, KVM — et où chacun apparaît dans le moteur |
| 05 | [Architecture](architecture.md) | Couches, le graphe des crates, chemins de contrôle et de données, état sur disque |
| 06 | [Les crates](crates.md) | Un bloc par crate : responsabilité, types principaux, par où commencer la lecture |
| 07 | [System Design Interview](system-design-interview.md) | Le moteur conçu comme une réponse d’entretien, puis comparé à ce qui a été construit |
| 08 | [Delonixfile et VMfile](delonixfile-and-vmfile.md) | Les grammaires des fichiers de build et en quoi elles diffèrent d’un Dockerfile |
| 09 | [Construire des microVM](microvm-setup.md) | KVM, Cloud Hypervisor et firmware, libvirt, images de VM |
| 10 | [Flux de contribution](contributing-workflow.md) | Worktrees, versions, règle de langue, règles d’architecture, ADR, commits et PR |
| 11 | [Publier la documentation](publishing-docs.md) | Comment le site et ce manuel sont générés, contrôlés et publiés |
| 12 | [Conventions de code](coding-conventions.md) | Comment le code de ce dépôt est écrit, et la liste de contrôle qu’appliquent les relecteurs |
| 13 | [Standards cloud native](cloud-native-standards.md) | Les standards cloud native à l’aune desquels une modification est mesurée |
| 14 | [Glossaire](glossary.md) | Les termes du moteur et du cloud native que vous rencontrez ici, avec leur sens dans Delonix et où en lire davantage |
| 15 | [Variables d’environnement](environment-variables.md) | Chaque variable `DELONIX_*` que lit le code : qui la lit, ce qu’elle change, sa valeur par défaut, et lesquelles abaissent une frontière |

Autres références vers lesquelles on vous renverra : [`ARCHITECTURE.md`](../../../ARCHITECTURE.md) (diagrammes C4),
[`docs/adr/`](../../adr/README.md) (décisions d’architecture), [`SECURITY.md`](../../../SECURITY.md)
(signalements privés de vulnérabilités) et [`CONTRIBUTING.md`](../../../CONTRIBUTING.md) (la courte porte d’entrée).
