<!-- translated-from: coding-conventions.md sha256:cdc355999424c8f178844432476c24ee51d98f4628c86cf5b3d42d1a50cac621 -->
# Conventions de code

**Avant de lire :** [Introduction à Rust](rust-primer.md), [Architecture](architecture.md) et [Les crates](crates.md) — les règles ci-dessous font référence aux couches, aux ports et aux crates par leur nom.

Cette page vous indique comment écrire du code qui passe la revue dans ce dépôt, afin que vous n'ayez
pas à deviner les règles ni à inventer les vôtres. Après elle, vous pouvez appliquer la liste de
vérification de la fin à votre propre diff avant qu'un relecteur ne le fasse. Chaque règle ci-dessous
porte une étiquette et une source :

- **Imposé (gate)** : un job de CI échoue si vous l'enfreignez. Le gate est nommé, vous pouvez donc
  l'exécuter en local (voir [Construire et tester](build-and-test.md#the-gates-ci-runs)).
- **Décidé (ADR/AGENTS.md)** : un Architecture Decision Record **accepté** dans `docs/adr/` ou une
  section d'`AGENTS.md` le tranche. Aucun gate ne le vérifie encore, c'est donc la revue qui le fait.
- **Proposé (ADR, pas encore décidé)** : la seule source écrite est un ADR dont le statut est encore
  **Proposed** (voir la colonne de statut de [`docs/adr/README.md`](../adr/README.md)). C'est la
  direction dans laquelle évolue le code, et la revue l'applique, mais elle peut encore changer ;
  lorsque l'ADR est accepté ou rejeté, l'étiquette change avec lui.
- **Convention (observée)** : le code le fait de manière cohérente et rien ne l'a mis par écrit. Les
  exemples sont de vraies références `path:symbol`. Copiez-les.

Si une question ne trouve pas de réponse ici, et que le code autour de votre modification n'y répond
pas non plus, la réponse honnête est **« non décidé — suivez le code environnant »**. La liste des
questions ouvertes connues se trouve dans [Non décidé](#undecided). Ne transformez pas une préférence
personnelle en règle dans une PR.

Pages connexes : les couches et les raisons qui les justifient dans [Architecture](architecture.md),
ce que contient chaque crate dans [Crates](crates.md), les idiomes Rust dans
[Introduction à Rust](rust-primer.md), et le flux de travail (worktree, version, ADR, PR) dans
[Flux de contribution](contributing-workflow.md).

---

## 1. L'outillage qui impose le style

| Outil | Ce qu'il vérifie | Étiquette et source |
|---|---|---|
| **rustfmt** | Le formatage. **Aucun fichier de configuration** : il n'y a ni `rustfmt.toml` ni `.rustfmt.toml` à la racine, ce sont donc les valeurs par défaut qui s'appliquent. | **Imposé (gate)** : le job de CI `fmt` exécute `cargo fmt --all --check` (`.github/workflows/ci.yml`). `CONTRIBUTING.md` § Style : « `cargo fmt` defaults, no custom config ». |
| **clippy** | Chaque lint clippy **et chaque avertissement de rustc** fait échouer le build, tests compris (`--all-targets`). Pas de `clippy.toml`, ce sont donc les lints par défaut qui s'appliquent. | **Imposé (gate)** : le job de CI `clippy` exécute `cargo clippy --workspace --all-targets --locked -- -D warnings`. |
| **`[workspace.lints]`** | Le `Cargo.toml` racine déclare exactement un lint de workspace : `[workspace.lints.clippy] undocumented_unsafe_blocks = "deny"`. Chaque manifeste membre contient `[lints] workspace = true`. | **Imposé (gate)** : clippy. Il a été introduit par la phase P0 de l'ADR-0040 (« `[workspace.lints]` (`undocumented_unsafe_blocks = deny`) »), un ADR encore **Proposed** ; le gate s'applique quoi qu'il en soit. |
| **cargo-deny** | Les avis RUSTSEC et les crates retirés (yanked), uniquement des licences permissives (ni GPL ni AGPL), crates.io comme seul registre, aucune source git. La section `[bans]` de `deny.toml` (versions dupliquées et jokers, réglée sur avertissement) n'est **pas** évaluée en CI. | **Imposé (gate)** : le job de CI `deny` exécute `check advisories licenses sources` avec `deny.toml`. **Convention (observée)** : chaque avis ignoré dans `deny.toml` porte un commentaire donnant sa raison — aucun gate ne vérifie que ce commentaire existe. |
| **`scripts/lang_ratchet.py`** | Le portugais dans les identifiants, les commentaires et les chaînes visibles par l'utilisateur (LANG-01, voir [§2](#2-language-of-the-code)). | **Imposé (gate)** : le job de CI `lang`, base de référence dans `scripts/lang_baseline.json`. |
| **`scripts/arch_fitness.py`** | La direction des couches, répertoire de crate = couche, versions uniquement à la racine, noms de consommateurs, et les cliquets de dette listés dans [Architecture](architecture.md#layers-and-the-allowed-direction) (voir [§4](#4-structure-where-code-goes) et [§5](#5-internal-api-and-boundaries)). | **Imposé (gate)** : le job de CI `arch`, base de référence dans `scripts/arch_baseline.json`. |
| **`scripts/contract_gate.py`** | Le contrat de nœud sous `proto/delonix/node/v1` (voir [§5.4](#54-the-node-contract)). | **Imposé (gate)** : le job de CI `contract`. |

Gardez en tête un mécanisme pour les **cliquets** (`lang_ratchet.py` et les nombres d'`arch_fitness.py`).
Un cliquet échoue quand son nombre **monte**, et il échoue **aussi** quand le nombre baisse sans que
la base de référence soit abaissée dans le même commit (`arch_fitness.py` affiche « debt was paid;
lower the baseline in the same commit (--update) »). Si vous remboursez de la dette, exécutez
`--update` et committez la nouvelle base de référence avec le correctif. Un contrôle `<=` laisserait
la dette apparaître comme verte pour toujours (docstrings de module des deux scripts).

Une conséquence que vous rencontrerez : **Imposé (gate)**. Comme `-D warnings` couvre les propres
lints de rustc, les lints de nommage de rustc (`non_snake_case`, `non_camel_case_types`,
`non_upper_case_globals`) sont imposés eux aussi. Les fonctions et variables en `snake_case`, les
types en `UpperCamelCase` et les constantes en `SCREAMING_SNAKE_CASE` ne sont pas un choix de style ici.

`#[allow(clippy::…)]` existe dans l'arbre, par exemple `#[allow(clippy::too_many_arguments)]` sur
`crates/contexts/delonix-compute/src/run.rs` et `crates/contexts/delonix-compute/src/pod.rs`.
**Non décidé** : aucune règle ne dit quand un `allow` est acceptable. Si vous en ajoutez un, placez un
commentaire `// why` à côté, comme vous le feriez pour toute autre exception (voir [§10](#10-comments-and-documentation)).

---

## 2. La langue du code

- **Les identifiants, les commentaires et les messages sont écrits en anglais.** **Imposé (gate)** :
  `scripts/lang_ratchet.py`. **Décidé** : AGENTS.md § « Língua do código: inglês (LANG-01) ». Le
  cliquet parcourt chaque fichier `.rs`, `.py`, `.ts`, `.go` et `.yml`/`.yaml` hors de `target/`,
  `third_party/` et similaires. Il compte trois choses :
  - **identifiants** : tout nom déclaré par `fn`, `struct`, `enum`, `trait`, `const`, `static`,
    `type`, `mod`, `union` ou `let` dont les segments `snake_case`/`camelCase` contiennent un mot de
    `scripts/lang_pt_lexicon.txt` ;
  - **commentaires** : les lignes `//`, `///` et `//!` contenant un mot du lexique ;
  - **texte utilisateur** : les littéraux de 8 caractères ou plus à l'intérieur de `format!`,
    `println!`, `eprintln!`, `panic!`, `anyhow!`, `bail!`, `expect` ou `unimplemented!`.
- **Le portugais n'atteint l'opérateur qu'à travers le catalogue.** **Décidé** : AGENTS.md §
  « i18n (fonte EN + catálogo pt.po embutido) » et `CONTRIBUTING.md` § « If you add or change a CLI
  command ». Le texte anglais va dans la source. Le portugais va dans
  `bins/delonix-runtime-bin/data/pt.po`, que `bins/delonix-runtime-bin/src/cmd/po.rs` intègre avec
  `include_str!`.
  - `po::t("…")` pour une chaîne fixe.
  - `po::tf("… {name} …", &[("name", value)])` pour un texte interpolé. Utilisez des espaces réservés
    **nommés** : une traduction peut les réordonner, et `format!` a besoin d'un littéral connu à la
    compilation, donc le gabarit est traduit d'abord et les valeurs sont substituées ensuite (`po.rs`,
    commentaire de documentation de `tf`).

    ```rust
    // bins/delonix-runtime-bin/src/cmd/config.rs — refuse_unknown_key
    Err(Error::Invalid(super::po::tf(
        "'{key}' is not a config key — known: {known}",
        &[("key", key), ("known", &KNOWN_KEYS.join(", "))],
    )))
    ```
  - Le texte de `--help` est lui aussi en anglais dans la source, traduit à l'exécution par
    `po::translate_help`. **Imposé (gate)** : `help_i18n_tests` dans
    `bins/delonix-runtime-bin/src/main.rs`. `todo_o_help_de_comando_tem_traducao_pt` est strict pour
    l'aide des commandes, et `o_help_dos_argumentos_so_pode_encolher` est un cliquet sur
    `ARG_HELP_PENDING` pour l'aide des flags. Une nouvelle commande ou un nouveau flag nécessite une
    entrée `pt.po`.
  - Une entrée manquante retombe sur l'anglais, de sorte que l'UI ne devient jamais vide (`po::t`).
    Une chaîne portugaise écrite directement dans le code est un bug, pas un raccourci.
  - Ne réutilisez pas un `msgid` dont le portugais dépend du genre grammatical du sujet. « created »
    peut être *criada* pour un réseau et *criado* pour un volume, utilisez donc des clés distinctes.
    **Décidé** : AGENTS.md § « v0.32.2 — 380+ strings PT hardcoded ».
- **Pièges du lexique.** **Décidé** : AGENTS.md § LANG-01.
  - **Ne nommez rien `num`.** Il est dans le lexique à dessein, car il capture du vrai portugais
    (« num apply falhado »). Un identifiant `num` compte donc comme dette portugaise et fait échouer
    le gate. Utilisez `count` ou `number`.
  - Ajouter un mot au lexique **augmente** le compte et fait échouer le gate. Abaissez la base de
    référence dans le même commit. Les homographes (`data`, `base`, `no`, `nas`…) sont exclus à moins
    qu'une mesure ne montre qu'ils capturent du vrai portugais ; `num` est l'exception consignée
    (AGENTS.md § LANG-01).

---

## 3. Nommage

### 3.1 Crates et répertoires

- **Le répertoire est la couche.** Un crate vit dans `crates/foundation/`, `crates/contexts/`,
  `crates/adapters/`, `crates/providers/` ou `crates/interfaces/`, ou c'est un binaire dans `bins/`.
  Le répertoire doit correspondre à l'entrée du crate dans la table `LAYERS`. **Imposé (gate)** :
  `scripts/arch_fitness.py` `misplaced()` / `LAYER_DIR`. Un nouveau crate entre dans `LAYERS` **et**
  dans le bon répertoire dans le même commit.
- **Convention de nommage cible pour les crates nouveaux ou restructurés.** **Proposé (ADR, pas
  encore décidé)** : ADR-0040 D2.1.

  | Rôle | Nom |
  |---|---|
  | Fondation pure partagée | `delonix-model` |
  | Contexte borné | `delonix-<context>`, nommé d'après le groupe d'API publié (D2.2) : `delonix-compute`, `delonix-stack` |
  | Adaptateur de technologie | `delonix-<technology>` : `delonix-linux`, `delonix-sdn`, `delonix-oci` |
  | Provider enfichable | `delonix-provider-<technology>` |
  | Bibliothèque d'interface | `delonix-<protocol>` : `delonix-cri`, `delonix-mcp` |

  Un crate n'existe que s'il est un contexte borné, isole une dépendance lourde ou privilégiée, ou est
  un binaire installé séparément. **Pas de suffixes `-core`, `-common`, `-utils` ou `-types`**
  (ADR-0040 D2.1 consigne comment un crate `-core` est devenu « le puits de tout » ; ce crate,
  `delonix-runtime-core`, a été supprimé dans #406). Certains crates portent encore des noms plus
  anciens : `delonix-proxmox`, `delonix-truenas`, `delonix-security-runtime`. ADR-0040 renomme chacun
  dans la phase qui le restructure, « jamais deux fois ». **Ne renommez pas un crate hors de sa phase.**
- **Le chemin de chaque crate est écrit une seule fois**, dans `[workspace.dependencies]` du
  `Cargo.toml` racine. Les membres dépendent les uns des autres avec `{ workspace = true }`.
  **Décidé** : AGENTS.md § « A direcção das dependências é um portão (ADR-0040, fase P0) », et le
  commentaire en tête de `[workspace.dependencies]`.

### 3.2 Modules et fichiers

- **Groupes de commandes CLI** : un module par groupe dans
  `bins/delonix-runtime-bin/src/cmd/<group>.rs`, avec un `pub enum <Group>Cmd` (sous-commandes clap),
  un répartiteur `pub fn run(action: <Group>Cmd) -> Result<()>`, et une fonction `cmd_<verb>` par
  sous-commande. **Convention (observée)** : `cmd/volume.rs:VolumeCmd` + `run` +
  `cmd_create`/`cmd_ls`/`cmd_describe` ; `cmd/secret.rs:SecretCmd` + `run` ;
  `cmd/container.rs:cmd_run`/`cmd_start`/`cmd_stop`. AGENTS.md § « CLI (`delonix`) » énonce la partie
  un-module-par-groupe.
- **Les adaptateurs d'un port de compute** vont dans un fichier nommé d'après la préoccupation du
  port, pas d'après la technologie : `crates/adapters/delonix-linux/src/workload.rs`,
  `.../run_host.rs`, `crates/adapters/delonix-sdn/src/run_network.rs`, `.../vm_network.rs`,
  `crates/adapters/delonix-oci/src/run_images.rs`. **Convention (observée)**.

### 3.3 Types, traits, fonctions, constantes

- **Les ports sont nommés d'après la capacité, pas la technologie.** **Proposé (ADR, pas encore
  décidé)** : ADR-0040 D3 liste les ports : `WorkloadRuntime`, `SandboxProvider`, `VmProvider`,
  `NetworkProvider`, `StorageProvider`, `ImageRegistry`, `ImageStore`, … Ceux qui existent aujourd'hui
  sont dans `crates/contexts/delonix-compute/src/ports.rs` (`ImageStore`, `StorageProvider`,
  `DeviceResolver`, `RunHost`, `VmNetwork`, `NetworkProvider`) et `.../launch.rs` (`WorkloadRuntime`).
  L'ancien `VmBackend` (`crates/contexts/delonix-compute/src/vm_backend.rs`) coexiste avec le
  `VmProvider` d'ADR-0044 (`vm_provider.rs`) pendant que P4 fait migrer les backends dessus.
- **Les implémentations hôte d'un port s'appellent `Host<Thing>`.** **Convention (observée)** :
  `delonix-linux/src/workload.rs:HostWorkload` (implémente `WorkloadRuntime`),
  `delonix-sdn/src/run_network.rs:HostNetwork` (implémente `NetworkProvider`),
  `delonix-oci/src/run_images.rs:HostImages`, `delonix-volume/src/lib.rs:HostVolumes`,
  `delonix-linux/src/cdi.rs:HostDevices`.
- **Les fonctions de décision pures** sont des verbes ou des questions : `resolve_*`, `parse_*`,
  `valid_*`, `is_*`, `*_plan`. **Convention (observée)** : `cmd/vm.rs:resolve_vm_defaults`,
  `delonix-oci/src/registry.rs:parse_content_range`, `cmd/stack.rs:is_pending`,
  `delonix-net-rules/src/lib.rs:bridge_name`.
- **Les constantes** sont en `SCREAMING_SNAKE_CASE` (**Imposé (gate)**, lint rustc sous clippy
  `-D warnings`). **Les noms de Kinds sont des constantes eux aussi, jamais des littéraux de chaîne
  répétés** (**Décidé** : AGENTS.md § « Os Kinds ganham grupos e nomes definitivos » ; les constantes
  sont dans `crates/contexts/delonix-stack/src/kinds.rs` : `pub const VM: &str = "VirtualMachine";`).

### 3.4 Noms de tests

- **Le cliquet compte les noms de tests.** `lang_ratchet.py` capture chaque déclaration `fn` et ne
  saute pas `#[cfg(test)]`, donc un nom de test en portugais augmente `identifiers` et fait échouer le
  gate. **Imposé (gate)**.
- Beaucoup de tests existants ont des noms en phrases portugaises, par exemple
  `delonix-model/src/exitcode.rs:nao_existe_e_rebentou_deixam_de_ser_o_mesmo_numero`. C'est de la dette
  comptée, pas un style à copier. Les nouveaux tests sont des **phrases anglaises qui énoncent le
  comportement prouvé**, comme les tests plus récents dans le même fichier :
  `a_missing_capability_is_not_a_wrong_argument` et `the_text_class_and_the_number_cannot_diverge`.
  **Décidé** : LANG-01 (AGENTS.md) ; la forme en « phrase » est une **Convention (observée)**.
- Si vous traduisez un nom de test existant, le compte baisse, exécutez donc
  `python3 scripts/lang_ratchet.py --update` dans le même commit.

### 3.5 Commandes et flags de la CLI

- **Commandes groupées, `delonix <group> <verb>`.** Aucun raccourci de premier niveau à plat.
  **Décidé** : AGENTS.md § « Reorganização da raiz da CLI (v0.30.0) » ; `docs/cli-stability.md` (les
  raccourcis de premier niveau ont été retirés en v1.0.0).
- **Les verbes suivent Docker/Podman/kubectl** lorsqu'un tel verbe existe. **Décidé** : AGENTS.md §
  les sprints « Reestruturação da CLI (semântica Docker/Podman/kubectl) » ; `docs/cli-stability.md` §
  « Estável ».
  - Les verbes de listage utilisent `ls` (`network ls`, `volume ls`, `image ls`…). `image list` a été
    ramené à `ls` en v2.0.0 (`docs/cli-stability.md`).
  - `create` ne fait que créer, et refuse un nom existant avec le code de sortie 5 sauf `--force`.
    L'upsert est un verbe distinct (`secret set`). `apply` est idempotent « assure la présence ».
    **Décidé** : AGENTS.md § « Sprint 1: `secret create` vs `secret set` ».
  - `describe` est pour les humains (style kubectl), `inspect` est du JSON pour les scripts. **Décidé** :
    AGENTS.md § « Output: `ls` estilo docker, `describe` estilo kubectl ».
  - L'ordre et les noms des flags copient Docker là où Docker a le concept :
    `network connect <NETWORK> <CONTAINER>`, `-p [hostIp:]hostPort:containerPort`,
    `volume create --driver … --opt k=v`. **Décidé** : AGENTS.md § Sprints 5 et 6.
- **Les changements cassants sont des coupures nettes, sans alias.** L'ancienne orthographe doit
  échouer avec `unrecognized subcommand`, jamais faire silencieusement autre chose. Avant de couper,
  cherchez (grep) les appelants internes dans **tout** le workspace. **Décidé** :
  `docs/cli-stability.md` § « Como uma quebra é feita ». Les groupes listés comme stables dans ce
  fichier ne peuvent casser que dans une release majeure.
- **Une commande accessible par plusieurs chemins doit être câblée sur tous** — mais préférez UN chemin
  à plusieurs lorsque la tâche est le cycle de vie d'une seule ressource. `vm pull` / `image vm pull` /
  `image --vm pull` étaient autrefois trois tels chemins, maintenus synchrones à la main ; ils ont été
  repliés en un seul groupe imbriqué, `vm image <verb>` (le Sprint 9 de restructuration de la CLI
  d'AGENTS.md). **Décidé** : `CONTRIBUTING.md` ; voir
  [Flux de contribution](contributing-workflow.md#adding-or-changing-a-cli-command).
- **Les changements de feuille mettent à jour la base de référence de la CLI**
  (`scripts/cli-tree.sh --update`) dans le même commit. **Imposé (gate)** : voir
  [Flux de contribution](contributing-workflow.md#adding-or-changing-a-cli-command).

### 3.6 Kinds, groupes d'API et champs de manifeste

- **Les Kinds sont des noms en `UpperCamelCase`** dans l'un des groupes publiés `core`, `compute`,
  `networking`, `gateway`, `storage`, `artifact`, `infrastructure` (`<group>.delonix.io/v1alpha1`).
  **Décidé** : AGENTS.md § « Identidade e fronteira do motor » et § « Os Kinds ganham grupos »
  (l'ADR-0020, qui a introduit les groupes, est encore **Proposed**). Chaque Kind est **une ligne**
  dans `crates/contexts/delonix-stack/src/kinds.rs` (`KindFacts` : `kind`, `plural`, `short`,
  `api_version`, `domain`, `form`, `in_stack`, `converges`, …). `delonix api-resources` imprime cette
  table. Ajouter un Kind touche aussi des tables que rien ne dérive de `kinds.rs` (`hot_fields`,
  `NAMESPACE_SOURCES`, `TYPED_KINDS`, le schéma généré). Les tests échouent jusqu'à ce que chacune
  soit faite. **Imposé (gate)** : AGENTS.md § « `kind: Service` » liste quel test a attrapé chaque
  table.
- **Un Kind renommé garde son ancien nom comme alias silencieux, insensible à la casse.** Une **fusion**
  avertit, car son sens a changé. **Décidé** : AGENTS.md § « Os Kinds ganham grupos e nomes definitivos »
  (« Alias silencioso, não depreciação ») ; implémenté dans `cmd/manifest.rs:KIND_ALIASES`. L'ADR-0020
  est encore **Proposed**.
- **Les champs de manifeste sont en `camelCase`.** Si un champ avait auparavant une orthographe
  `snake_case`, cette orthographe reste acceptée comme `alias` serde. **Convention (observée)**, par
  champ plutôt que `rename_all` :

  ```rust
  // bins/delonix-runtime-bin/src/cmd/vm.rs — VmSpec
  /// Canonical `cpuAffinity`; `cpu_affinity` stays accepted (back-compat).
  #[serde(rename = "cpuAffinity", alias = "cpu_affinity")]
  cpu_affinity: Option<String>,
  ```

  Autres exemples : `delonix-compute/src/pod.rs:PodSpec.restart_policy` (`rename = "restartPolicy"`),
  `cmd/service.rs:ServiceSelector.match_labels` (`rename = "matchLabels"`). Le schéma publié
  (`docs/schema/v1/delonix.json`) est **généré** à partir de ces structs et est testé pour leur
  correspondre (ADR-0007). Le schéma de manifeste est déclaré **stable** (`docs/cli-stability.md` §
  « O schema dos manifestos »).
- **Les enregistrements internes** (le JSON sous la racine d'état) gardent les noms de champs
  `snake_case` de Rust. Voir `crates/contexts/delonix-compute/src/record.rs` (`net_mode`, `namespace`).
  **Convention (observée)**.

### 3.7 Variables d'environnement

- **Préfixe `DELONIX_`**, en majuscules : `DELONIX_ROOT`, `DELONIX_NET_RUNTIME_DIR`, `DELONIX_L18N`,
  `DELONIX_LOG_FORMAT`, `DELONIX_CRI_CAP_CEILING`. **Convention (observée)** à travers `crates/` et
  `bins/`.
- **Pour la télémétrie, lisez les variables standard `OTEL_*`**, pas un nouvel alias `DELONIX_*`.
  **Proposé (ADR, pas encore décidé)** : ADR-0040 D6. C'est la cible, pas le code d'aujourd'hui :
  `crates/adapters/delonix-telemetry/src/telemetry.rs` lit `DELONIX_OTLP_ENDPOINT` pour l'exporteur
  OTLP, et de l'ensemble standard seulement `OTEL_SERVICE_NAME`. N'ajoutez pas de nouvelle variable de
  télémétrie `DELONIX_*`, et ne retirez pas `DELONIX_OTLP_ENDPOINT` hors de la phase qui la migre.
- **Une échappatoire qui affaiblit une valeur de sécurité par défaut est bruyante et explicite.** Elle
  est désactivée à moins d'être réglée à `1`, et elle avertit : `DELONIX_ENABLE_IPV6=1`,
  `DELONIX_ALLOW_LINK_LOCAL=1`. **Décidé** : AGENTS.md § « Bloco 0 do plano 33 (v0.37.1) ».
- Un flag l'emporte sur la variable d'environnement, qui l'emporte sur la valeur par défaut
  (`serve cri --cap-ceiling` vs `DELONIX_CRI_CAP_CEILING`). **Décidé** : AGENTS.md § « Tecto de
  capabilities no CRI ».

### 3.8 Codes de sortie et codes `DX_*`

- **Les codes de sortie sont dérivés du type d'erreur en un seul endroit** : l'exhaustif
  `crates/foundation/delonix-model/src/exitcode.rs:for_error`. Les classes sont 1 générique,
  2 usage, 3 `NOT_RUNNING`, 4 `NOT_FOUND`, 5 `CONFLICT`, 69 `UNAVAILABLE`, 74 `IO`,
  77 `NO_PERMISSION`, 124 `TIMEOUT`. Chaque erreur a aussi une identité textuelle stable,
  `Error::code()`, qui renvoie une chaîne `DX_*`. **Imposé (gate)** : le match n'a pas de bras `_ =>`,
  donc une nouvelle variante arrête le build jusqu'à ce que quelqu'un la classe. Le test
  `the_text_class_and_the_number_cannot_diverge` maintient les deux synchronisés. **Décidé** :
  AGENTS.md § « Códigos de saída com classe (v0.49.0) » ; `docs/cli-stability.md` § « Códigos de
  saída ».
- **Vous ne choisissez pas un numéro, vous renvoyez la bonne variante.** « Ça n'existe pas » est
  `Error::NotFound`, « ça existe déjà » est `Error::Conflict`, « cet hôte manque d'un outil » est
  `Error::Unavailable`. Un nouveau numéro a besoin d'un vrai producteur. **Décidé** : docs de module de
  `exitcode.rs` (« every extra number is a promise »).

---

## 4. Structure : où va le code

### 4.1 Les couches et la direction

La direction autorisée est écrite en un seul endroit, `ALLOWED` dans `scripts/arch_fitness.py`.
**Imposé (gate)** :

| Couche | Peut dépendre de |
|---|---|
| foundation | foundation |
| context | foundation, context |
| adapter / provider | foundation, context |
| interface | foundation, context, adapter, provider |
| bin | tout |

Les dépendances de dev et de build ne comptent pas. Une exception déclarée doit nommer la phase
ADR-0040 qui la supprime, et une exception qui ne s'applique plus échoue aussi (`EXCEPTIONS`). La table
des couches générée et les exceptions actuelles sont dans
[Architecture](architecture.md#layers-and-the-allowed-direction).

D'autres règles structurelles, chacune **Imposée (gate)** par `scripts/arch_fitness.py` :

- **La fondation et les contextes restent purs de dépendances lourdes.** `tokio`, `axum`, `hyper`,
  `tonic`, `reqwest`, `clap`, `ratatui`, `serde_yaml`, `rmcp`, OpenTelemetry et `prometheus-client` y
  sont refusés (`HEAVY`).
- **Un binaire compose exactement une interface.** Imposé dans `rule_failures()` (introduit par
  ADR-0040 D2.4, encore **Proposed** ; aussi écrit dans AGENTS.md § « A direcção das dependências é um
  portão »).
- **Les versions de dépendances ne vivent que dans `[workspace.dependencies]` à la racine.** Un membre
  écrit `{ workspace = true, features = [...] }` et rien d'autre. `default-features = false` reste à la
  racine, car un membre ne peut pas désactiver ce que la racine active (`inline_versions()`).
- **Les bibliothèques n'impriment pas.** Le cliquet `library_prints` compte les
  `println!`/`eprintln!`/`print!` hors de `bins/`. Émettez du `tracing` à la place (par exemple
  `tracing::warn!` dans `crates/adapters/delonix-sdn/src/lib.rs`) et laissez l'interface présenter la
  sortie. **Décidé** : AGENTS.md § « A direcção das dependências é um portão (ADR-0040, fase P0) »
  (« Uma biblioteca não escreve para o terminal; emite `tracing` »).
- **Les bibliothèques ne relancent pas le propre binaire du moteur.** Le cliquet `self_exec_sites`
  compte `current_exe()`, `cli_bin()` et `delonix_bin()` hors de `bins/`. Appelez une fonction ou un
  cas d'usage à la place. `Command::new("ip")`, `nft`, `qemu-img` et `ssh` ne sont **pas** comptés, car
  exécuter ces outils est exactement ce à quoi sert un adaptateur (commentaire au-dessus de
  `SELF_EXEC`).
- **N'écrivez pas l'environnement du processus.** Le cliquet `env_writes` compte
  `env::set_var`/`remove_var` partout, tests compris. Les tests s'exécutent sur des threads parallèles
  et une écriture est en concurrence avec tous les lecteurs. Passez plutôt les valeurs en paramètre
  (commentaire au-dessus de `ENV_WRITES`).

### 4.2 « Ma modification est X → elle va dans Y »

Cette table utilise les crates tels qu'ils existent aujourd'hui. Vérifiez [Les crates](crates.md) pour
le contenu de chaque crate avant d'y ajouter. Là où la colonne source cite ADR-0040 ou ADR-0026,
l'étiquette est **Proposé (ADR, pas encore décidé)** : les deux ADR sont encore Proposed, même si les
crates qu'ils décrivent existent déjà.

| Votre modification | Crate (couche) | Source |
|---|---|---|
| Une règle pure sur les CIDR, les noms de pont ou l'arithmétique IPAM que les deux côtés doivent calculer de façon identique | `delonix-net-rules` (foundation) | 06 ; AGENTS.md § Arquitetura |
| Une nouvelle classe d'erreur, un code de sortie ou un code `DX_*` ; des noms générés | `delonix-model` (foundation) | ADR-0040 D1 |
| Un type d'enregistrement de charge de travail persisté (`Container`, `Vm`, `Mount`, …) | `delonix-compute` (context) : `record.rs` | ADR-0040 D2.2 ; #406 |
| Un enregistrement de données brutes sans mécanisme (`Status`, `ContainerFw`/`FwRule`, `typestate`) | `delonix-model` (foundation) : `records.rs`, `typestate.rs` | ADR-0040 P3 (#405) |
| Une question posée à l'hôte ou à un processus (`now_unix`, vivacité de pid, user namespace, génération d'id), le journal d'événements, la règle de dispatch du serveur, `SO_PEERCRED` | `delonix-node` (context) | ADR-0040 D2.2 ; #406 |
| Une règle pure du modèle de secret (`Secret`, noms et clés valides, parsing de fichier env) | `delonix-model` (foundation) : `secret.rs` | ADR-0040 P3 (la PR qui a déplacé les stores) |
| Un store, le verrou de fichier, `write_atomic*`/`write_private_temp`, le store de secrets chiffré ou le coffre de credentials | `delonix-state` (adapter) | ADR-0040 D2.3 |
| Les faits de Kind, le planificateur/diff, les conditions, les révisions | `delonix-stack` (context) | AGENTS.md § Arquitetura |
| La spécification de run, sa validation pure, un port dont le cas d'usage run a besoin | `delonix-compute` (context) : `run_opts.rs`, `preflight.rs`, `ports.rs` | ADR-0040 D2.2 |
| Politique de sécurité, admission, score, rédaction | `delonix-security-runtime` (context) | ADR-0026 |
| Namespaces, cgroups, montages, capabilities, seccomp, devices | `delonix-linux` (adapter) | ADR-0040 D2.3 |
| Holder de netns, nftables, slirp, DNS, DHCP, overlay, WireGuard, CNI | `delonix-sdn` (adapter) | ADR-0040 D2.3 |
| Client de registre, CAS, couches, overlay, construction d'image | `delonix-oci` (adapter) | ADR-0040 D2.3 |
| SBOM / CVE | `delonix-scanner` (adapter) | ADR-0040 D2.3 |
| Tracing, OpenTelemetry, configuration du registre Prometheus | `delonix-telemetry` (adapter) | ADR-0040 D2.3 |
| Ce qu'une opération de VM décide (create, stop, start, status, remove, verbes day-2) | `delonix-compute` (context) : `vm.rs` (`VmEngine`), atteignant les backends, le disque et le seed uniquement via `VmBackends`/`LocalDiskImages`/`SeedBuilder` ; pas de `Command::new` dans un contexte (`context_spawns`, base 0) | ADR-0044 P4b.3 (#596, #597) |
| Un backend de VM local (Cloud Hypervisor, libvirt), et les implémentations de ce nœud pour les ports de VM | `delonix-vm` (adapter) : `lib.rs`, `local_ports.rs` | ADR-0008 ; ADR-0044 P4b.3 |
| Un provider distant ou enfichable (API d'hyperviseur, API NAS) | un crate de provider dans `crates/providers/`. **Écrivez d'abord un ADR** | ADR-0008, ADR-0009 ; [Flux de contribution](contributing-workflow.md#when-to-write-an-adr) |
| Un RPC CRI | `delonix-cri` (interface) | AGENTS.md |
| L'API de gestion locale, `/metrics` | `delonix-mgmt` (interface) | ADR-0010 |
| Un outil MCP | `delonix-mcp` (interface) | ADR-0025 |
| Une commande CLI, sa présentation et ses traductions | `bins/delonix-runtime-bin/src/cmd/<group>.rs` + `data/pt.po` | AGENTS.md § CLI |
| Quel adaptateur soutient quel port (composition) | la racine de composition du binaire. Aucune logique métier ici | ADR-0040 D1 « Binaries » |

### 4.3 Cœur pur, I/O aux bords

- **Les décisions sont des fonctions pures sur des données que vous avez déjà lues.** Elles ne prennent
  aucun store, n'exécutent aucune commande et n'ont besoin d'aucun privilège, de sorte qu'un test peut
  les appeler avec des valeurs simples. **Proposé (ADR, pas encore décidé)** : ADR-0040 D1 (le
  `domain/` d'un contexte n'a « pas d'I/O, pas de `tokio`, `libc`, `nix`, `std::fs` »). **Convention
  (observée)** : `delonix-stack/src/reconcile.rs` (« decides it WITHOUT touching the machine »),
  `cmd/vm.rs:resolve_vm_defaults`, `delonix-sdn/src/infra.rs:vmtap_line`,
  `delonix-oci/src/registry.rs:parse_content_range`.

  ```rust
  // bins/delonix-runtime-bin/src/cmd/stack.rs — is_pending
  fn is_pending(present: &str, kind: &str, status: &str) -> bool {
      match present {
          // Declarative: nothing to observe, so nothing to wait for.
          "-" => false,
          "yes" => !ready_status(kind, status),
          // "no" (absent) and "?" (unknown/unreadable) both keep waiting.
          _ => true,
      }
  }
  ```

- **Si une fonction pure a besoin de quelque chose de l'extérieur, prenez-le en paramètre.** Par
  exemple, `resolve_image_ref` prend le store d'images au lieu d'ouvrir le vrai, de sorte que le test
  peut passer un répertoire temporaire. **Décidé** : AGENTS.md § « O manifesto de VM resolvia a imagem
  de outra maneira que a CLI ».
- **Une règle, un propriétaire.** Quand deux sites d'appel ont besoin de la même dérivation, extrayez
  une fonction et appelez-la depuis les deux. Une seconde copie dérive. Exemples :
  `delonix_net_rules::bridge_name`, réexporté par `delonix-sdn` (le nom de pont avait deux formules et
  imprimait un device qui n'existait pas), `infra::dhcp_lease_ip`, `effective_entrypoints`. **Décidé** :
  AGENTS.md § « `delonix network` », § « Isolamento de namespace », § « Reverse-proxy L7 ».

---

## 5. API interne et frontières

### 5.1 Ports et adaptateurs

- **Un nouveau backend implémente un port. Ce n'est jamais un `if provider == …` ailleurs.**
  **Décidé** : AGENTS.md § « Identidade e fronteira do motor » (« Um provider novo entra como
  implementação de uma porta, nunca como um `if provider == …` »). ADR-0040 D3 règle 3 (« No string
  matching on provider names outside the composition root ») le reformule et est encore **Proposed**.
  D3 prévoit un test de fitness pour cela, mais **il n'existe pas encore dans `arch_fitness.py`**, donc
  c'est la revue qui l'impose pour l'instant.
- **La connaissance spécifique à un backend vit sur le backend.** Par exemple,
  `VmBackend::ip_is_predicted()` répond si l'IP d'une VM a été prédite, au lieu que le site d'appel
  vérifie `backend.contains("cloud-hypervisor")`. **Décidé** : ADR-0008, cité dans le commentaire de
  documentation de `crates/contexts/delonix-compute/src/vm_backend.rs`.
- **Un adaptateur ne dépend pas d'un autre adaptateur.** Ce dont il a besoin d'une autre préoccupation
  arrive comme un hook ou un port, câblé par la racine de composition. **Imposé (gate)** : `ALLOWED`
  (adapter → foundation, context). **Convention (observée)** : le commentaire de documentation de
  `delonix-linux/src/workload.rs:HostWorkload` explique ainsi ses hooks `addresses`/`attach_slirp`.

  ```rust
  // crates/contexts/delonix-compute/src/ports.rs — NetworkProvider (excerpt)
  pub trait NetworkProvider {
      /// Refuses a network that does not exist.
      fn check_network(&self, name: &str) -> Result<()>;
      /// Undoes an attach; best effort, used on the way out of a failure.
      fn detach(&self, id: &str, ip: &str);
      /// Publishes one `-p` specification on a container's address.
      fn publish(&self, ip: &str, spec: &str) -> Result<()>;
  }
  ```

- **Un trait a besoin d'un vrai consommateur au moment où il atterrit.** N'écrivez pas d'échafaudage
  qui attend son premier appelant. Chaque méthode doit avoir un appelant. **Décidé** : AGENTS.md §
  « `delonix workload` » (ADR-0002). Une fonction publique sans appelants est aussi un danger connu :
  plusieurs d'entre elles se sont avérées cacher un bug latent (`mount_live`, `set_net_rate`,
  `update_limits`, `publish_port_allow`), et certaines ont été supprimées plutôt que câblées (AGENTS.md
  § « Endurecimento do ingress/egress »).

### 5.2 Erreurs par crate (ADR-0040 P3)

- **Un adaptateur ou un provider définit son propre `Error` et le convertit en la classe partagée**
  (`delonix_model::Error`), qui porte le code `DX_*`. **Proposé (ADR, pas encore décidé)** : ADR-0040 P3.
  **Imposé (gate)** : le cliquet `shared_error_imports` dans `arch_fitness.py` (`SHARED_ERROR`, limité
  à `crates/adapters/` et `crates/providers/`) compte les imports `use delonix_model::{…Error/Result…}`
  qui font du type partagé le propre type de résultat du crate. Le type partagé peut encore être nommé
  à l'intérieur d'une impl `From`. L'implémentation de référence est
  `crates/adapters/delonix-scanner/src/error.rs` :

  ```rust
  impl From<Error> for Dx {
      fn from(e: Error) -> Self {
          match e {
              e @ (Error::EmptySbom | Error::OsvShape | /* … */ Error::NoModule) => Dx::Invalid(e.to_string()),
              Error::ModuleScan(io) => Dx::Runtime { context: "module scan", message: io.to_string() },
              Error::Engine(e) => e,
          }
      }
  }
  ```

  Trois choses dans ce fichier sont le motif : la conversion **décide** la classe ; `code()` interroge
  la conversion au lieu de tenir une seconde table ; et un test
  (`the_code_is_the_code_of_the_class_it_converts_into`) maintient les deux synchronisés. Le message
  converti est aussi gardé octet pour octet identique à ce que la CLI imprimait avant
  (`the_converted_message_is_the_one_printed_before`).
- **Demandez à l'erreur sa classe ; ne faites pas de match sur une variante de l'erreur partagée.** Hors
  de la fondation, écrivez `e.is_not_found()` ou `e.class()`, ou faites un match sur `e.root()` lorsque
  vous avez besoin de la charge utile — jamais `Err(Error::NotFound(_))` ou
  `matches!(…, Error::NotFound(_))`. La propre erreur d'un crate voyage à l'intérieur de la classe
  partagée avec son code, donc un match de variante cesse de l'attraper sans un mot du compilateur.
  **Décidé** : ADR-0043 D4 (Accepté). **Imposé (gate)** : le cliquet `raw_error_variant_matches` dans
  `arch_fitness.py` (`RAW_VARIANT_MATCH`, sautant `crates/foundation/delonix-model/`), dont la base de
  référence est 0 — tout nouveau match brut fait échouer la CI. Les méthodes vivent dans
  `crates/foundation/delonix-model/src/codes.rs`.

### 5.3 Visibilité

- **Privé par défaut.** À l'intérieur du crate CLI, utilisez `pub(crate)` pour quelque chose qu'un
  autre module `cmd` doit appeler. **Convention (observée)** : `cmd/container.rs:cmd_run`, `cmd_start`
  et `cmd_stop` sont `pub(crate)` pour que `pod`, `compose` et `stack` puissent y déléguer ;
  `cmd/firewall.rs:update_locked` ; `cmd/manifest.rs:KIND_ALIASES`.
- **`pub` dans un crate de bibliothèque est une promesse aux autres crates.** Retirer un élément public
  est un changement cassant pour les utilisateurs de la bibliothèque, même avec zéro appelant dans ce
  workspace. Le lint `dead_code` de rustc ne voit pas les éléments `pub` inutilisés, donc quand vous en
  supprimez un, comptez à la main les éléments publics orphelins. **Décidé** : AGENTS.md §
  « `delonix_sdn::Net` foi APAGADO — e é breaking para quem usa a biblioteca ».

### 5.4 Le contrat de nœud

`proto/delonix/node/v1` est la source de vérité pour les encodages gRPC et HTTP/JSON.
`docs/api/openapi.yaml` en est généré. **N'éditez jamais le fichier OpenAPI à la main.**
**Imposé (gate)** : `scripts/contract_gate.py` exécute `buf format`, `buf lint`, `buf breaking` contre
le dernier tag qui contient `proto/`, vérifie le mapping HTTP de chaque RPC, et vérifie que le fichier
OpenAPI est égal au fichier généré. **Décidé** : AGENTS.md § « O contrato de nó é um portão » (qui cite
ADR-0040 P1 ; ADR-0040 D4 est encore **Proposed**).

- **Un message de requête par RPC**, nommé `<Rpc>Request`. **Imposé (gate)** : `buf lint`
  `RPC_REQUEST_STANDARD_NAME` (voir `buf.yaml`). Une requête partagée laisse un champ destiné à une
  méthode apparaître dans cinq.
- **Identité explicite dans la requête** : champs `name` et `namespace`, jamais un message de métadonnées
  générique avec des champs que le moteur ignorerait. **Décidé** : AGENTS.md. **Convention (observée)** :
  `compute.proto:GetContainerRequest { string name = 1; string namespace = 2; }`.
- **Les images sont adressées par requête, pas dans le chemin**, car dans un chemin comme `alpine:3.20`
  le `:` serait lu comme un verbe personnalisé. **Décidé** : AGENTS.md. **Convention (observée)** :
  `infra.proto` `GetImage` → `get: "/v1/images:get"`, avec `GetImageRequest { string reference = 1; }`.
- **Les réponses** renvoient la ressource, ou une `Operation` pour les mutations de longue durée. Les
  règles de nommage des réponses de `buf lint` sont désactivées à dessein (commentaire de `buf.yaml`).
- **Chaque RPC a un mapping HTTP sauf les flux bidirectionnels** (`Exec`, `Console`), qui ne doivent pas
  en avoir. **Imposé (gate)** : `contract_gate.py` vérification 4.

### 5.5 Le moteur ne connaît aucun consommateur

Le moteur ne sait pas qui l'utilise. Aucun nom de plateforme, de plan de contrôle, de console ou
d'agent, et aucune notion de tenant, de compte, de plan ou de facturation, ne peut apparaître dans
`crates/`, `bins/`, `proto/`, le `Cargo.toml` racine ou le `Makefile`, **commentaires compris**.

- **Noms de consommateurs.** **Imposé (gate)** : `scripts/arch_fitness.py` `consumer_mentions()`
  capture l'expression régulière `CONSUMER_NAMES`, une liste fixe de noms, dans ces chemins. Un nom qui
  n'est pas sur la liste n'est pas attrapé.
- **Concepts de tenant, de compte, de plan et de facturation.** **Décidé** : AGENTS.md § « Identidade e
  fronteira do motor ». Aucun gate ne les capture ; la revue le vérifie.

Si un consommateur a besoin de quelque chose, écrivez-le comme une capacité générique dans le propre
vocabulaire du moteur (Kinds et ressources), et ne l'ajoutez que si cela a du sens pour n'importe quel
client. Le moteur valide son propre contrat et ne fait jamais confiance à un appelant pour refuser ce
qu'il ne supporte pas.

---

## 6. Gestion des erreurs et messages

- **Pas d'échec silencieux.** Si une option est acceptée puis ignorée, c'est pire qu'une fonctionnalité
  manquante, car l'utilisateur croit qu'elle a pris effet. Refusez-la avec une erreur claire, et nommez
  le flag. **Décidé** : AGENTS.md § « Falhas silenciosas corrigidas (fail-closed) » ; l'audit v0.37.0
  (§ « Auditoria sistemática dos 208 subcomandos ») appelle cette classe « relato desonesto » (compte
  rendu malhonnête). Les motifs que cette section liste sont :
  - **Ne détruisez rien avant de savoir que l'objet est à vous à détruire, et supprimez la comptabilité
    en dernier.** Si l'enregistrement est retiré en premier et que la suppression des données échoue
    ensuite, les données sont orphelines et un `create` ultérieur les remet à quelqu'un d'autre.
  - **Une mesure illisible est *inconnue*, jamais zéro.** Un `read_dir` qui échoue n'est pas un
    répertoire vide. C'est pourquoi `Usage { bytes, unreadable }` existe.
  - **Surveillez les motifs qui transforment les échecs en succès** : `let _ =` sur un résultat qui
    compte (entropie, une lecture de socket), `as u64` sur un `f64` (il sature), `capture()` lu par son
    `Result` au lieu de sa sortie. AGENTS.md § « A classe «X não é Y» » les catalogue.
- **Inconnu ou non mesurable n'est pas une supposition.** Quand le moteur ne peut pas lire une valeur,
  il rapporte qu'il ne sait pas, ou il refuse. Il ne choisit pas la réponse la plus probable. Exemple :
  `system prune --auto` refuse si l'usage du disque ne peut être lu, et le faucheur IPAM échoue fermé
  quand un store est illisible (une liste vide se lirait comme « rien n'est vivant »). **Décidé** :
  AGENTS.md § CLI (`system prune`), § « O IPAM vaza ».
- **Forme du message : le fait d'abord, puis quoi faire**, avec la commande exacte quand il y en a une.
  **Décidé** : AGENTS.md § « `-p 80:80` respondia com o JSON cru do slirp » (« facto primeiro, depois os
  comandos prontos a copiar »). **Convention (observée)** :
  `delonix-model/src/error.rs:Error::VmNotFound` → `"no such VM: {0} (see \`delonix vm ls\`)"` ;
  `cmd/config.rs:refuse_unknown_key` nomme les clés valides. Nommez l'**outil manquant et son paquet**
  au lieu de transmettre un `ENOENT` brut, qui se lit comme un fichier manquant (AGENTS.md § « A bateria
  mede o `--help` de tudo », achado 1).
- **Une mesure partielle n'est pas un succès.** `--wait` doit observer ce qu'il prétend, et `✓ … is up`
  n'est imprimé qu'après vérification. **Décidé** : AGENTS.md § « O `--wait` de uma VM CH ».
- **Ne parsez jamais un message d'erreur pour décider quoi faire.** Les messages sont traduits
  (`--l18n=pt`/`DELONIX_L18N`), donc un `grep 'no such'` classe sur une machine et cesse silencieusement
  de classer sur une autre. Utilisez la classe de sortie ou le code `DX_*`. **Décidé** : docs de module
  de `delonix-model/src/exitcode.rs`.
- **Renvoyez la variante qui correspond à la classe** (`NotFound`, `Conflict`, `NotRunning`,
  `Unavailable`, `Timeout`), pas `Invalid` pour tout. **Décidé** : AGENTS.md § « Códigos de saída com
  classe » : `util::find` renvoyant `Invalid` pour « not found » rendait la ressource la plus utilisée
  inclassable.

---

## 7. `unsafe`, appels système et processus

- **Chaque bloc `unsafe` a un commentaire `// SAFETY:` juste au-dessus.** **Imposé (gate)** :
  `undocumented_unsafe_blocks = "deny"` dans `[workspace.lints.clippy]`.

  ```rust
  // crates/adapters/delonix-linux/src/lib.rs — apply_filter_logged
  // SAFETY: `fprog` points to a valid BPF program; NO_NEW_PRIVS is already set.
  let rc = unsafe {
      libc::syscall(libc::SYS_seccomp, SET_MODE_FILTER, FLAG_LOG, &fprog as *const _)
  };
  ```

  Le commentaire doit énoncer l'invariant qui rend l'appel sain. « same » n'est acceptable que
  directement après un appel identique et justifié (comme dans `delonix-linux/src/lib.rs` juste après
  le premier `_exit(126)`).
- **Pas de `clone()`/`fork()` bruts dans un processus multithread** (les serveurs tokio, le shim de
  l'API Docker). `clone` n'exécute pas les handlers `pthread_atfork`, donc l'enfant peut se bloquer sur
  le verrou de malloc. Ré-exécutez plutôt une spec typée, transmise par un fichier `0600`/`O_EXCL`
  plutôt que par argv. **Décidé** : AGENTS.md § « Auditoria de segurança #3 », point 5. Contexte :
  [Introduction à Rust pour ce codebase](rust-primer.md#why-forkclone-in-a-multi-threaded-process-is-dangerous).
- **Un hook `pre_exec` ne doit pas se bloquer sur quelque chose que le parent fait après le retour de
  `spawn`.** `Command::spawn` ne retourne qu'après `exec`, donc les deux processus s'attendent l'un
  l'autre pour toujours. Utilisez un `fork` brut pour les poignées de main. **Décidé** : AGENTS.md § « A
  classe «X não é Y» » (l'entrée `reexec_mapped_hold`).
- **Fichiers temporaires : utilisez `delonix_state::write_private_temp`.** Il ouvre avec un nom unique,
  `O_EXCL` et le mode `0600`, de sorte qu'il ne suit jamais un lien symbolique planté. N'utilisez pas de
  nom fixe ou dérivé du pid dans `/tmp`. **Décidé** : AGENTS.md § « Auditoria de segurança #3 »,
  passage 2 ; commentaire de documentation dans `crates/adapters/delonix-state/src/store.rs`.
  **Convention (observée)** : `delonix-sdn/src/bpf.rs`, `delonix-linux/src/run_host.rs`.
- **Fichiers qui doivent être privés ou atomiques : utilisez
  `write_atomic_mode(path, bytes, Some(0o600))`.** Il fixe le mode à la création et publie avec un
  renommage atomique. N'écrivez jamais le fichier puis ne le `chmod`ez, car un autre utilisateur peut
  l'ouvrir entre-temps. **Décidé** : commentaire de documentation de
  `delonix-state/src/store.rs:write_atomic_mode` ; AGENTS.md (TOCTOU de kubeconfig).
- **Avant de signaler un pid lu dans un fichier, vérifiez que c'est toujours le même processus.**
  Utilisez `delonix_node::safe_to_signal(pid, starttime)`, qui compare l'heure de démarrage pour qu'un
  pid recyclé ne soit pas tué. **Décidé** : AGENTS.md § « A classe «X não é Y» » (les entrées pid).
- **L'argv d'un processus ne prouve pas qu'il est à nous.** D'autres racines d'état du même
  utilisateur, et d'autres outils, s'exécutent avec le même argv. Vérifiez un jeton que nous seuls
  choisissons : un chemin dérivé de notre racine, ou une variable d'environnement que nous avons fixée
  au spawn. **Décidé** : AGENTS.md § « A classe «X não é Y» » (l'entrée « o argv de um processo »).
- **Passez `--` avant les arguments positionnels qui viennent de l'entrée** dans l'argv des outils
  externes (`ssh`, `scp`, `virsh`, `mount`, `qemu-img`), et validez toute valeur qui finit dans un shell
  distant contre une liste blanche de caractères. `shell_quote` ne nettoie pas le contenu. **Décidé** :
  AGENTS.md § « Auditoria de segurança (skill `delonix-runtime-sec`) » et § « #2 ».
- **Les chemins construits à partir de noms issus de l'entrée utilisateur ou de manifeste sont
  confinés.** Utilisez un contrôle de nom `valid_*` à la frontière du moteur
  (`delonix_vm::valid_vm_name`), et une jonction sûre qui refuse les composants `..`/absolus et les
  liens symboliques (`safe_join`, `safe_bind_target`). **Décidé** : mêmes sections d'AGENTS.md.

---

## 8. État et concurrence

- **Lecture–modification–écriture passe par `update`, jamais `load` → muter → `save`.**
  `Store::update` et `JsonStore::update` (`crates/adapters/delonix-state/src/store.rs`) prennent un
  `flock`, **relisent sous le verrou**, appliquent votre closure et écrivent de façon atomique. Une
  closure qui renvoie `false` annule l'écriture. La CLI, le serveur CRI et les rafraîchissements en
  arrière-plan touchent tous les mêmes enregistrements en concurrence, et sans le verrou une écriture
  est silencieusement perdue. **Décidé** : commentaires de documentation des deux fonctions ; AGENTS.md
  § « Revisão ampla de código/arquitectura (2026-07-27) », bugs 5 et l'élément `JsonStore`. **Convention
  (observée)** : une mutation qui peut elle-même échouer est enveloppée ainsi :

  ```rust
  // bins/delonix-runtime-bin/src/cmd/firewall.rs — update_locked
  let c = store.update(id_or_name, |c| match f(c) {
      Ok(commit) => commit,
      Err(e) => {
          err = Some(e);
          false
      }
  })?;
  ```

- **Persistez chaque étape dès que le dataplane la confirme.** Si un changement multi-étapes échoue à
  mi-chemin, l'enregistrement doit encore correspondre à ce que le noyau a réellement. **Décidé** :
  AGENTS.md § « Reconfiguração a quente » (« Persistência »).
- **Les nouveaux champs sur les enregistrements persistés prennent `#[serde(default)]`** (ou
  `default = "fn"`), de sorte que les enregistrements écrits par des versions plus anciennes se chargent
  encore. La valeur par défaut doit décrire ce qu'étaient réellement les anciens enregistrements, pas une
  supposition. **Décidé** : AGENTS.md (par exemple `Vm.namespace`, `VmImage.cloud_init`). **Convention
  (observée)** : `delonix-compute/src/record.rs`, le commentaire de documentation sur `Vm.namespace`
  (« the default is a statement of fact and not a guess »).
- **Tout ce qui est nécessaire pour reconstruire une ressource doit être persisté, pas seulement utilisé
  à la création.** Quand vous touchez à un chemin `start`/`restart`, comparez champ par champ ce que la
  création utilise avec ce que l'enregistrement stocke. **Décidé** : AGENTS.md § « BUG GRAVE corrigido…
  `-v` nunca era persistido » (listé là comme le troisième bug de la même famille).
- **Les fichiers de verrou ne sont jamais supprimés.** En supprimer un ouvre une fenêtre où deux
  processus verrouillent des inodes différents. **Décidé** : commentaire de documentation de
  `store.rs:lock_path` (`delonix-state`).
- **`SecretStore::update` est le seul `update` dont le verrou est au mieux (best-effort).** Son
  `FileLock::acquire` renvoie `Option` et poursuit sans verrou si le fichier de verrou ne peut être
  ouvert, contrairement à `Store` et `JsonStore`. **Non décidé** : s'il devrait refuser comme les
  autres ; suivez le code environnant et dites-le dans la PR si vous y touchez.

---

## 9. Tests

- **Où ils vont.** Les tests unitaires vont dans `#[cfg(test)] mod tests` au bas du fichier. Les tests
  d'intégration vont dans `crates/<layer>/<crate>/tests/` et n'utilisent que l'API publique. Les tests
  de provider en direct sont opt-in. **Convention (observée)** ; détails dans
  [03 § 3.9](rust-primer.md#39-tests).
- **Le pur d'abord.** Mettez la décision dans une fonction pure et testez-la comme des données. Tout ce
  qui a besoin de vrais namespaces, cgroups ou d'un holder de réseau est validé en direct ou avec
  `scripts/e2e.sh`. **Décidé** : `CONTRIBUTING.md` (« Write a unit test for any new pure function ») ;
  AGENTS.md § « IaC nativo » (`reconcile.rs` est pur pour pouvoir être testé comme des données).
- **Noms** : des phrases anglaises qui énoncent le comportement (voir [§3.4](#34-test-names)).
- **Les tests ne touchent jamais l'état réel de l'hôte.** Donnez aux stores une racine temporaire.
  N'appelez pas de code qui résout la vraie racine d'état. Ne faites pas de `set_var` (le cliquet
  `env_writes`, [§4.1](#41-the-layers-and-the-direction)). **Décidé** : AGENTS.md § « IaC nativo », la
  note de fusion de `ShareVolume` (« Nota de método: um teste que chamasse `apply_share` … escreveria no
  estado REAL da máquina »). Pour les exécutions manuelles et E2E, isolez **à la fois** `DELONIX_ROOT`
  et `DELONIX_NET_RUNTIME_DIR`. N'en isoler qu'un est pire que rien (AGENTS.md § « Meia-isolação é pior
  que nenhuma » ; [Cloner, construire et tester](build-and-test.md#isolating-the-engines-state)).
- **Un test supprime son répertoire temporaire à chaque sortie.** Un assert échoué, un `return`
  anticipé et un contrôle sauté sont aussi des sorties, donc la suppression vit dans une garde avec
  `Drop`, pas dans une ligne à la fin : tenez le répertoire dans un `tempfile::TempDir` (déclaré une
  fois dans le `Cargo.toml` racine). Une racine qu'un binaire de test entier partage via `DELONIX_ROOT`
  utilise le bail de `delonix-sdn/tests/network_alloc_race.rs`. Un dossier nommé d'après le pid sous
  `temp_dir()` avec un `remove_dir_all` à la fin est l'ancien idiome : ne le copiez pas. Depuis le
  2026-09-28 aucun test ne le fait. Un arbre extrait d'une couche d'image peut garder des répertoires
  `0555`, rendez-les donc inscriptibles avant de le supprimer. `tempdir()` crée le répertoire avec le
  mode `0700` : si un autre uid doit le lire (un user namespace, un subuid mappé, un hyperviseur),
  mettez `0755` dessus. Un socket Unix a besoin d'un chemin court (`sun_path` fait 108 octets) :
  utilisez `tempfile::tempdir_in("/tmp")` (`/tmp/.tmpXXXXXX/x.sock` fait 22 octets), pas un chemin
  littéral `/tmp` avec le pid, qu'un assert échoué laisse derrière lui. **Imposé (gate)** :
  `scripts/tmp_roots_gate.py` fait échouer le job `test` sur tout ce que les tests laissent dans leur
  `TMPDIR` (la base de référence est vide depuis #572) et sur tout ce qui est nouveau dans `/tmp`, jugé
  contre un listage pris juste avant `cargo test` (`--before`). Les deux recensements s'exécutent aussi
  quand un test échoue.
- **Un test de régression doit échouer avec le correctif annulé.** Annulez le correctif, regardez le
  test échouer, puis restaurez le correctif. Un test qui passe dans les deux cas ne prouve rien, et
  AGENTS.md en consigne plusieurs (un contrôle de code de sortie que `1` ne pouvait distinguer ; un
  scénario de chaos resté vert avec une annulation). **Décidé** : AGENTS.md, « verificado pela regra do
  repo » partout, par exemple § « IaC nativo » (`stack_converge`) et § « A bateria mede o `--help` ».
- **Testez le chemin que la production utilise.** Si la production passe des chemins relatifs, le test
  utilise des chemins relatifs. Un test peut encoder le bug. **Décidé** : AGENTS.md § « Auditoria
  sistemática dos 208 subcomandos » (`default_project_name`).
- **Les bugs de concurrence obtiennent une vraie course.** Utilisez des threads plus un sleep explicite
  à l'intérieur de la fenêtre critique. **Convention (observée)** :
  `delonix-state/src/store.rs:jsonstore_update_concorrente_nao_perde_escritas`.
- **Préférez les propriétés au timing échantillonné.** Quand une course ne peut qu'être échantillonnée,
  générez de la charge et répétez. **Décidé** : AGENTS.md § « Um `exec` logo a seguir ao `run -d` corria
  no HOST ».
- **Les régions générées ne portent aucun compte volatil** (lignes, tests, commits). **Décidé** :
  docstring de `scripts/dev_docs.py` (« Deliberately NOT generated: line counts, test counts, commit
  counts »). Une **mesure écrite à la main** cite la valeur mesurée avec la date à laquelle elle a été
  mesurée, jamais un total courant. **Décidé** : AGENTS.md § « A bateria mede o `--help` de tudo e
  EXECUTA um quarto » (« Cita-se a fracção medida e a data, nunca o total »). Que des comptes puissent
  figurer dans la prose ou les commentaires de code est **non décidé** — suivez le texte environnant.

---

## 10. Commentaires et documentation

- **Les commentaires expliquent *pourquoi*, pas *quoi*.** Écrivez-en un pour une contrainte cachée, un
  contournement d'un bug précis, ou un invariant que le code ne rend pas évident. Les commentaires qui
  redisent le code sont retirés en revue. **Décidé** : `CONTRIBUTING.md` § Style.
- **Quand une décision a été mesurée, dites ce qui a été mesuré.** « Measured: … » vaut mieux que
  « should ». Les docs de module dans `exitcode.rs` et `reconcile.rs` sont le modèle. **Convention
  (observée)**.
- **Les commentaires de documentation (`///`, `//!`) sur les éléments publics et les en-têtes de module**
  disent ce que l'élément promet et pourquoi il existe. **Convention (observée)** : chaque port dans
  `delonix-compute/src/ports.rs`, `delonix-state/src/store.rs:write_private_temp`, `exitcode.rs`. **Non
  décidé** : aucun lint `missing_docs` n'est activé.
- **Les commentaires sont en anglais et ne nomment aucun consommateur.** Le cliquet de langue et le gate
  de consommateur parcourent tous deux les commentaires. **Imposé (gate)**.
- **N'ajoutez pas d'abstractions, de flags de configuration ou de gestion d'erreur pour des cas qui ne
  peuvent pas se produire.** **Décidé** : `CONTRIBUTING.md` § Style.
- **Quand vous déplacez une frontière, mettez à jour ses enregistrements dans le même changement** :
  - un ADR **avant** le code, pour un nouveau provider, port, daemon, dépendance externe dans un crate du
    moteur, frontière de privilège, ou changement au contrat ou aux couches. Les ADR acceptés ne sont
    jamais réécrits ; un nouveau les remplace (**Décidé** : `docs/adr/README.md` ;
    [Flux de contribution](contributing-workflow.md#when-to-write-an-adr)) ;
  - la section d'`AGENTS.md` qui décrit la zone. Une section obsolète là-bas induit en erreur la personne
    suivante, et **les conflits dans `AGENTS.md` sont résolus en gardant les deux côtés** (**Décidé** :
    AGENTS.md § « Método: um worktree por sessão ») ;
  - les artefacts générés : `docs/schema/v1/delonix.json`, `docs/api/openapi.yaml`, la base de référence
    de la CLI, `docs/gen.py`, et les régions générées de `docs/dev/` via `python3 scripts/dev_docs.py`
    (**Imposé (gate)** ; voir [Publier la documentation](publishing-docs.md)).

---

## Non décidé

Rien dans le dépôt ne tranche ceci. Suivez le code environnant et mentionnez le choix dans votre PR :

- **Quand `#[allow(clippy::…)]` est acceptable.** Il est utilisé (`too_many_arguments`) sans politique
  écrite.
- **Le type `Result` des ports de compute actuels.** Les ports dans `delonix-compute/src/ports.rs`
  renvoient `delonix_model::Result`, donc les adaptateurs qui les implémentent (`HostWorkload`,
  `HostNetwork`, …) importent le type de résultat partagé, et ces imports comptent dans
  `shared_error_imports`. P3 décide des erreurs par crate. Aucun document ne dit comment la signature
  d'un port change, et ajouter un tel import dans un nouveau fichier fait échouer le cliquet. Si votre
  changement en a besoin, soulevez-le dans la PR. Ne contournez pas la regex.
- **Noms de crates pendant la transition.** ADR-0040 D2 (encore Proposed) fixe les noms cibles, mais un
  tout nouveau crate qui atterrit avant que son contexte n'existe (par exemple un second provider avant
  les renommages `delonix-provider-*`) n'a aucune règle écrite. Demandez d'abord dans l'issue.
- **Commentaires de documentation requis.** Il n'y a pas de lint `missing_docs`, seulement l'habitude
  observée.
- **Le test de fitness pour le matching de noms de provider** (ADR-0040 D3 règle 3) est proposé mais pas
  encore implémenté.

---

## Liste de vérification de revue

Avant d'ouvrir la PR, parcourez la liste :

1. `cargo fmt --all --check` et `cargo clippy --workspace --all-targets --locked -- -D warnings` sont
   propres. → [§1](#1-the-tooling-that-enforces-style)
2. `python3 scripts/lang_ratchet.py` et `python3 scripts/arch_fitness.py` passent, et toute base de
   référence que vous avez abaissée est dans ce commit. → [§1](#1-the-tooling-that-enforces-style)
3. Les nouveaux identifiants, commentaires, messages **et noms de tests** sont en anglais. Le texte
   utilisateur passe par `po::t`/`po::tf`, avec des entrées `pt.po`. Pas de `num`. →
   [§2](#2-language-of-the-code), [§3.4](#34-test-names)
4. Le code est dans le bon crate et la bonne couche, les versions ne sont qu'à la racine, la
   bibliothèque n'a pas de `println!` et ne relance pas son propre binaire. →
   [§4](#4-structure-where-code-goes)
5. Les décisions sont des fonctions pures avec des tests unitaires, et l'I/O reste aux bords. →
   [§4.3](#43-pure-core-io-at-the-edges)
6. Les nouveaux backends implémentent un port, sans matching de noms de provider. Les adaptateurs ne
   dépendent pas d'adaptateurs. → [§5.1](#51-ports-and-adapters)
7. Les échecs d'adaptateur/provider utilisent un `Error` de crate qui se convertit en
   `delonix_model::Error`. → [§5.2](#52-errors-per-crate-adr-0040-p3)
8. Si vous avez touché `proto/` : `scripts/contract_gate.py` passe, avec une requête par RPC, une
   identité explicite, et un OpenAPI régénéré. → [§5.4](#54-the-node-contract)
9. Aucun nom de consommateur nulle part dans `crates/`, `bins/` ou `proto/`, commentaires compris. →
   [§5.5](#55-the-engine-knows-no-consumer)
10. Rien n'est accepté puis ignoré. Les erreurs énoncent le fait puis le correctif, et utilisent la
    variante de la bonne classe. → [§6](#6-error-handling-and-messages), [§3.8](#38-exit-codes-and-dx_-codes)
11. Chaque bloc `unsafe` a un commentaire `// SAFETY:`. Il n'y a pas de fork brut dans un processus
    multithread, les fichiers temporaires utilisent `write_private_temp`, et les secrets utilisent
    `write_atomic_mode`. → [§7](#7-unsafe-syscalls-and-processes)
12. Les mutations d'enregistrement passent par `update`, et les nouveaux champs d'enregistrement ont
    `#[serde(default)]`. → [§8](#8-state-and-concurrency)
13. Les tests utilisent des racines isolées, le test de régression échoue avec le correctif annulé, et
    un nombre mesuré porte sa date. → [§9](#9-tests)
14. Changements CLI : tous les points d'entrée sont câblés, les verbes sont alignés sur Docker, les
    coupures n'ont pas d'alias, et la base de référence de la CLI est mise à jour. →
    [§3.5](#35-cli-commands-and-flags)
15. Kinds et champs de manifeste : une ligne dans `kinds.rs`, des champs `camelCase` avec les anciennes
    orthographes comme alias, et le schéma régénéré. → [§3.6](#36-kinds-api-groups-and-manifest-fields)
16. L'ADR, `AGENTS.md` et les docs générées sont mis à jour si une frontière a bougé. →
    [§10](#10-comments-and-documentation)

---

**Suivant :** [Ajouter un Kind](adding-a-kind.md) — la table, le schéma et le câblage du réconciliateur
dont un nouveau Kind déclaratif a besoin, traités à travers un cas réel.
