<!-- translated-from: troubleshooting.md sha256:47d1c5984d20cc95f303cbc9d9579447fa3fec9218d0e0c4f6e052daf1781f74 -->
# Dépannage

**À lire avant :** [Préparer votre environnement](environment.md) (pièges de l'hôte) et [Cloner, compiler et tester](build-and-test.md#the-gates-ci-runs) (les gates, et l'isolation de l'état du moteur).

Cette page est un index par **symptôme** : le texte littéral qu'un gate ou une exécution réelle
affiche, ce qu'il signifie vraiment, et le correctif. Elle ne réenseigne pas ce que [Préparer votre
environnement](environment.md) et [Cloner, compiler et tester](build-and-test.md) couvrent déjà en
profondeur — elle pointe plutôt vers la bonne section, pour qu'un dépôt tout juste cloné puisse
passer de « je suis bloqué » à « je sais quelle section lire » sans avoir à lire ces deux pages en
entier d'abord. Si un symptôme ne figure pas ici, [Commencer ici § Quand vous êtes
bloqué](start-here.md#when-you-are-stuck) est l'étape suivante — cette page est un raccourci pour
le cas précis où *l'outil lui-même vous a déjà dit ce qui ne va pas*, dans un texte que vous ne
reconnaissiez pas encore.

## Index rapide

| Vous avez vu | C'est | Corrigé dans |
|---|---|---|
| `FAIL <name>: N (baseline M) — new debt entered` | `scripts/arch_fitness.py` | [Un ratchet de dette a bougé](#a-debt-ratchet-moved-arch_fitnesspy) |
| `FALHA <kind>: N > M — entrou português novo.` | `scripts/lang_ratchet.py` | [Le ratchet de langue](#new-portuguese-entered-lang_ratchetpy) |
| `<nom> (<couche>) → <nom> (<couche>): forbidden direction` | `scripts/arch_fitness.py` | [Une dépendance va contre le sens des couches](#a-dependency-goes-against-the-layer-direction) |
| `<fichier>:<n>: names a consumer (…) — the engine knows none` | `scripts/arch_fitness.py` | [Un nom de consommateur s'est glissé dans le moteur](#a-consumer-name-leaked-into-the-engine) |
| `FAIL <tag> is published but this commit does not contain it` | `scripts/version_gate.py` | [La branche est antérieure au dernier tag](#the-version-gate-refuses-your-branch) |
| `FAIL Cargo.toml says X, above Y, and docs/releases/vX.md does not exist` | `scripts/version_gate.py` | [Un bump de version sans commit de release](#the-version-gate-refuses-your-branch) |
| `FAIL  buf format` / `buf lint` / `buf breaking against …` / `has no google.api.http mapping` / `openapi.yaml is not the generated one` | `scripts/contract_gate.py` | [Le gate du contrat du nœud](#the-node-contract-gate) |
| `unshare()` échoue, `EPERM` | AppArmor + user namespaces | [Préparer votre environnement § AppArmor](environment.md#ubuntu-2310-apparmor-blocks-user-namespaces-for-your-dev-binary) |
| `-m`/`--cpus`/`--cpu-weight` refusés, code de sortie `69` | délégation de cgroup | [Préparer votre environnement § Délégation de cgroup](environment.md#cgroup-delegation-some-limits-are-refused-others-are-not-enforced) |
| `path must be shorter than SUN_LEN` | `DELONIX_NET_RUNTIME_DIR` trop long | [Cloner, compiler et tester § Isoler l'état du moteur](build-and-test.md#isolating-the-engines-state) |
| Un gate échoue contre un diff que vous n'avez pas écrit, ou un build se termine étrangement vite | un cache de build partagé ou obsolète | [Un cache de build partagé ou obsolète](#a-shared-or-stale-build-cache) |
| Le binaire répond avec une ancienne version ou une commande qui n'existe pas | un `delonix` obsolète dans le `PATH` | [Préparer votre environnement § Un PATH obsolète](environment.md#a-stale-delonix-on-your-path) |

## Gates de CI dont le message donne la raison

Chaque gate nomme ce qu'il faut corriger — le tableau ci-dessous n'existe que pour que vous
reconnaissiez la *forme* du message avant de le lire en détail. Exécutez la commande locale de
[Cloner, compiler et tester § Les gates que fait tourner la CI](build-and-test.md#the-gates-ci-runs)
pour reproduire n'importe lequel de ces cas sans attendre la CI.

### Du portugais nouveau est entré (`lang_ratchet.py`)

```
FALHA identifiers: 1051 > 1050 — entrou português novo.
       `python3 scripts/lang_ratchet.py --list --only identifiers` mostra onde.
```

(Les messages du gate lui-même sont en portugais — la LANG-01 concerne le *code*, pas ce script ;
voir [Flux de contribution § Langue](contributing-workflow.md#language-english-in-the-code-lang-01).)
Le compte d'identifiants, de commentaires ou de messages utilisateur en portugais a augmenté.
`--list --only <kind>` nomme les nouvelles lignes. Si vous avez *traduit* quelque chose et que le
compte a au contraire **baissé** sans que vous n'ayez abaissé la ligne de base, le message est
l'image inversée (« traduziste, mas não baixaste a linha de base ») — exécutez `python3
scripts/lang_ratchet.py --update` et commitez `scripts/lang_baseline.json` dans le même commit que
la traduction.

### Un ratchet de dette a bougé (`arch_fitness.py`)

```
FAIL  self_exec_sites: 6 (baseline 5) — new debt entered
```

L'un des cinq compteurs suivis (`self_exec_sites`, `library_prints`, `env_writes`,
`shared_error_imports`, `raw_error_variant_matches`) a augmenté. `python3 scripts/arch_fitness.py
--list` affiche chaque site que compte chacun d'eux, avec une courte explication de ce que
signifie le compte, juste à côté — voir [Flux de contribution § Les règles d'architecture que les
gates imposent](contributing-workflow.md#architecture-rules-the-gates-enforce) pour savoir ce
qu'est chacun. Comme pour le ratchet de langue, la même forme de message apparaît quand le compte
**baisse** sans que la ligne de base ne bouge avec lui dans le même commit (« debt was paid; lower
the baseline … `--update` »).

### Une dépendance va contre le sens des couches

```
FAIL  delonix-oci (adapters) → delonix-cri (interfaces): forbidden direction
```

Un crate a importé un autre crate d'une couche dont il n'a pas le droit de dépendre — voir
[Architecture § Les couches et le sens autorisé](architecture.md#layers-and-the-allowed-direction)
pour le tableau de qui peut dépendre de qui. Soit la dépendance est erronée (le cas le plus
fréquent — un adapter n'a rien à faire à dépendre d'une interface), soit le changement a
réellement besoin d'une exception déclarée et phasée dans `EXCEPTIONS`, à l'intérieur de
`scripts/arch_fitness.py`, que le gate lui-même refuse d'accepter sans une phase qui la retire et
une raison.

### Un nom de consommateur s'est glissé dans le moteur

```
FAIL  crates/adapters/delonix-oci/src/registry.rs:42: names a consumer ('SomeControlPlaneName') — the engine knows none
```

La frontière canonique du moteur — *« le moteur ne connaît aucun consommateur »*, la section en
tête de l'[`AGENTS.md`](../../../AGENTS.md) — est imposée par un grep, pas seulement par la revue.
Ceci se déclenche sur le nom de toute plateforme, control plane, console ou agent utilisant le
moteur, dans le code **ou les commentaires**, n'importe où sous `crates/`, `bins/` ou `proto/`.
Généralisez l'exigence vers la capacité générique qu'elle est réellement, dans le vocabulaire
propre au moteur ; l'historique qui a besoin du nom externe appartient à `docs/`, jamais au code.

### Le gate de version refuse votre branche

```
FAIL  v1.4.2 is published but this commit does not contain it (newest contained: v1.4.0) —
      merge origin/main first; merging a branch that predates a release undoes what it shipped
```

Votre branche a commencé avant une release qui a depuis été publiée. `git fetch --tags origin &&
git merge origin/main` (ce dépôt fusionne, il ne rebase pas les branches de fonctionnalité sur les
releases — voir [Flux de contribution § Un worktree par
tâche](contributing-workflow.md#one-worktree-per-task) pour comprendre pourquoi un historique
linéaire reste néanmoins attendu de vos propres commits).

```
FAIL  Cargo.toml says 1.5.0, above 1.4.2, and docs/releases/v1.5.0.md does not exist —
      a bump belongs only to the release commit
```

Vous avez incrémenté `version` dans `Cargo.toml`. Ne le faites pas — cela ne se fait que dans le
commit de release, avec le fichier de notes de release. Annulez le bump ; voir [Releases et
stabilité § Le gate de version](releases-and-stability.md#the-version-gate).

### Le gate du contrat du nœud

`scripts/contract_gate.py` enveloppe cinq vérifications indépendantes, et chacune affiche sa
propre ligne `FAIL` — l'exécuter en local est le moyen le plus rapide de voir laquelle des cinq
vous concerne :

```
FAIL  buf format — run `buf format -w proto`
FAIL  buf lint
FAIL  buf breaking against v1.4.0
<suit la sortie stdout/stderr propre à buf, nommant le champ ou le RPC devenu incompatible>
FAIL  node.proto: SomeRpc has no google.api.http mapping
FAIL  docs/api/openapi.yaml is not the generated one — run `python3 scripts/contract_gate.py --update` and commit it
```

Il a besoin de `protoc`, `buf` (figé à la v1.73.0) et `protoc-gen-openapi` (figé à la v0.7.1) dans
le `PATH`, ainsi que des tags git — un outil manquant échoue avec le plus familier « command not
found », mais un tag manquant fait afficher `ok` par la vérification `buf breaking`, en disant
explicitement qu'il n'y a pas encore de ligne de base à comparer, plutôt que de la sauter en
silence. Un changement réellement incompatible sur `proto/delonix/node/v1` a besoin d'un ADR au
préalable, comme tout changement à un contrat de nœud stable — voir [Flux de contribution § Quand
écrire un ADR](contributing-workflow.md#when-to-write-an-adr). Si ce qui a vraiment changé est la
sortie du générateur (un nouveau champ, un nouveau RPC), `python3 scripts/contract_gate.py
--update` régénère `docs/api/openapi.yaml` ; commitez-le dans le même commit que le changement
du `.proto`.

## Un cache de build partagé ou obsolète

[Cloner, compiler et tester § Compiler](build-and-test.md#build) nomme déjà le compromis :
pointer plusieurs worktrees vers un seul `CARGO_TARGET_DIR` partagé économise de l'espace disque,
mais deux builds s'exécutant dessus en même temps s'attendent l'un l'autre et **peuvent invalider
les artefacts l'un de l'autre**. Le symptôme est spécifique et facile à confondre avec un véritable
échec : un gate (en particulier `test` ou `clippy`) échoue contre du code qui semble sans lien
avec votre changement, ou un build se termine étrangement vite et le binaire produit se comporte
ensuite comme une version plus ancienne — y compris un hook local de pre-commit ou pre-push qui
réutilise un répertoire de target partagé entre plusieurs sessions et se lie avec les fichiers
objets qui se trouvent là, issus d'un build différent et concurrent.

Ce n'est pas un bug du gate : il compile réellement la mauvaise chose. Écartez un cache obsolète
avant de déboguer l'« échec » lui-même :

```bash
cargo clean -p delonix-runtime-bin   # ou le crate que l'échec désigne
cargo build -p delonix-runtime-bin   # recompilez proprement, puis relancez le gate qui a échoué
```

Si vous travaillez régulièrement depuis plusieurs worktrees à la fois, donner à chacun son propre
`CARGO_TARGET_DIR` (ne pas exporter le partagé, ou exporter un chemin local au worktree) supprime
entièrement cette classe de symptôme, au prix d'un premier build plus lent dans chacun — le même
compromis que [Cloner, compiler et tester](build-and-test.md#build) énonce déjà.

---

**Ensuite :** [Comment les noms atteignent `/etc/hosts`](service-names-and-hosts.md) — le bloc qui publie les noms de service et les hôtes de route, et pourquoi il refuse. Puis [Conventions de code](coding-conventions.md) — comment le code de ce dépôt doit être écrit, chaque règle étant associée au gate ou à la décision qui la justifie.
