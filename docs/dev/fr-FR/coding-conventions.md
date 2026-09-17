<!-- translated-from: coding-conventions.md sha256:e8fa0aa20ecb042cbb42224bc761227f36f7876e1b9c0fca26a0a627050e4307 -->
# Conventions de code

Cette page vous indique comment écrire du code qui passe la revue dans ce dépôt, afin que vous
n'ayez pas à deviner les règles ni à inventer les vôtres. Chaque règle ci-dessous porte une
étiquette et une source :

- **Imposé (gate)** : un job de CI échoue si vous l'enfreignez. Le gate (contrôle CI) est nommé,
  vous pouvez donc l'exécuter en local (voir [Construire et tester](build-and-test.md#the-gates-ci-runs)).
- **Décidé (ADR/AGENTS.md)** : un Architecture Decision Record **accepté** dans `docs/adr/` ou une
  section de `AGENTS.md` le tranche. Aucun gate ne le vérifie encore, c'est donc la revue qui le fait.
- **Proposé (ADR, pas encore décidé)** : la seule source écrite est un ADR dont le statut est encore
  **Proposed** (voir la colonne de statut de [`docs/adr/README.md`](../../adr/README.md)). C'est la
  direction dans laquelle évolue le code, et la revue l'applique, mais elle peut encore changer ;
  lorsque l'ADR est accepté ou rejeté, l'étiquette change avec lui.
- **Convention (observée)** : le code le fait de manière cohérente et rien ne l'a mis par écrit. Les
  exemples sont de vraies références `path:symbol`. Copiez-les.

Si une question ne trouve pas de réponse ici, et que le code autour de votre modification n'y répond
pas non plus, la réponse honnête est **« non décidé — suivez le code environnant »**. La liste des
questions ouvertes connues se trouve dans [Non décidé](#undecided). Ne transformez pas une préférence
personnelle en règle dans une PR.

Pages connexes : les couches et les raisons qui les justifient dans [Architecture](architecture.md),
ce que contient chaque crate dans [Crates](crates.md), les idiomes Rust dans
[Introduction à Rust](rust-primer.md), et le flux de travail (worktree, version, ADR, PR) dans
[Flux de contribution](contributing-workflow.md).

---

## 1. L'outillage qui impose le style

| Outil | Ce qu'il vérifie | Étiquette et source |
|---|---|---|
| **rustfmt** | Le formatage. **Aucun fichier de configuration** : il n'y a ni `rustfmt.toml` ni `.rustfmt.toml` à la racine, ce sont donc les valeurs par défaut qui s'appliquent. | **Imposé (gate)** : le job de CI `fmt` exécute `cargo fmt --all --check` (`.github/workflows/ci.yml`). `CONTRIBUTING.md` § Style : « `cargo fmt` defaults, no custom config ». |
| **clippy** | Chaque lint clippy **et chaque avertissement de rustc** fait échouer le build, tests compris (`--all-targets`). Pas de `clippy.toml`, ce sont donc les lints par défaut qui s'appliquent. | **Imposé (gate)** : le job de CI `clippy` exécute `cargo clippy --workspace --all-targets --locked -- -D warnings`. |
| **`[workspace.lints]`** | Le `Cargo.toml` racine déclare exactement un lint de workspace : `[workspace.lints.clippy] undocumented_unsafe_blocks = "deny"`. Chaque manifeste membre contient `[lints] workspace = true`. | **Imposé (gate)** : clippy. Il a été introduit par la phase P0 de l'ADR-0040 (« `[workspace.lints]` (`undocumented_unsafe_blocks = deny`) »), un ADR encore **Proposed** ; le gate s'applique quoi qu'il en soit. |
| **cargo-deny** | Les avis RUSTSEC et les crates retirés (yanked), uniquement des licences permissives (ni GPL ni AGPL), crates.io comme seul registre, aucune source git. La section `[bans]` de `deny.toml` (versions dupliquées et jokers, réglée sur avertissement) n'est **pas** évaluée en CI. | **Imposé (gate)** : le job de CI `deny` exécute `check advisories licenses sources` avec `deny.toml`. **Convention (observée)** : chaque avis ignoré dans `deny.toml` porte un commentaire donnant sa raison — aucun gate ne vérifie que ce commentaire existe. |
| **`scripts/lang_ratchet.py`** | Le portugais dans les identifiants, les commentaires et les chaînes visibles par l'utilisateur (LANG-01, voir [§2](#2-language-of-the-code)). | **Imposé (gate)** : job de CI `lang`, base de référence dans `scripts/lang_baseline.json`. |
| **`scripts/arch_fitness.py`** | La direction des couches, répertoire du crate = couche, les versions uniquement à la racine, les noms de consommateurs, et les ratchets de dette listés dans [Architecture](architecture.md#layers-and-the-allowed-direction) (voir [§4](#4-structure-where-code-goes) et [§5](#5-internal-api-and-boundaries)). | **Imposé (gate)** : job de CI `arch`, base de référence dans `scripts/arch_baseline.json`. |
| **`scripts/contract_gate.py`** | Le contrat de nœud sous `proto/delonix/node/v1` (voir [§5.4](#54-the-node-contract)). | **Imposé (gate)** : job de CI `contract`. |

Gardez en tête un mécanisme pour les **ratchets (cliquets)** (`lang_ratchet.py` et les nombres
de `arch_fitness.py`). Un ratchet échoue lorsque son nombre **augmente**, et il échoue **aussi**
lorsque le nombre diminue sans que la base de référence soit abaissée dans le même commit
(`arch_fitness.py` affiche « debt was paid; lower the baseline in the same commit (--update) »). Si
vous remboursez de la dette, exécutez `--update` et commitez la nouvelle base de référence avec la
correction. Une vérification `<=` laisserait la dette apparaître au vert pour toujours (docstrings de
module des deux scripts).

Une conséquence que vous rencontrerez : **Imposé (gate)**. Comme `-D warnings` couvre les lints
propres à rustc, les lints de nommage de rustc (`non_snake_case`, `non_camel_case_types`,
`non_upper_case_globals`) sont imposés eux aussi. Les fonctions et variables en `snake_case`, les
types en `UpperCamelCase` et les constantes en `SCREAMING_SNAKE_CASE` ne sont pas un choix de style ici.

`#[allow(clippy::…)]` existe dans l'arborescence, par exemple `#[allow(clippy::too_many_arguments)]`
dans `crates/contexts/delonix-compute/src/run.rs` et `crates/contexts/delonix-compute/src/pod.rs`.
**Non décidé** : aucune règle ne dit quand un `allow` est acceptable. Si vous en ajoutez un, mettez un
commentaire `// why` à côté, comme pour toute autre exception (voir [§10](#10-comments-and-documentation)).

---

## 2. Langue du code

- **Les identifiants, les commentaires et les messages sont écrits en anglais.** **Imposé (gate)** :
  `scripts/lang_ratchet.py`. **Décidé** : AGENTS.md § « Língua do código: inglês (LANG-01) ». Le
  ratchet analyse chaque fichier `.rs`, `.py`, `.ts`, `.go` et `.yml`/`.yaml` en dehors de `target/`,
  `third_party/` et assimilés. Il compte trois choses :
  - **identifiants** : tout nom déclaré par `fn`, `struct`, `enum`, `trait`, `const`, `static`,
    `type`, `mod`, `union` ou `let` dont les segments `snake_case`/`camelCase` contiennent un mot de
    `scripts/lang_pt_lexicon.txt` ;
  - **commentaires** : les lignes `//`, `///` et `//!` contenant un mot du lexique ;
  - **texte utilisateur** : les littéraux de 8 caractères ou plus à l'intérieur de `format!`,
    `println!`, `eprintln!`, `panic!`, `anyhow!`, `bail!`, `expect` ou `unimplemented!`.
- **Le portugais n'atteint l'opérateur qu'à travers le catalogue.** **Décidé** : AGENTS.md §
  « i18n (fonte EN + catálogo pt.po embutido) » et `CONTRIBUTING.md` § « If you add or change a CLI
  command ». Le texte anglais va dans le code source. Le portugais va dans
  `bins/delonix-runtime-bin/data/pt.po`, que `bins/delonix-runtime-bin/src/cmd/po.rs` embarque avec
  `include_str!`.
  - `po::t("…")` pour une chaîne fixe.
  - `po::tf("… {name} …", &[("name", value)])` pour un texte interpolé. Utilisez des espaces
    réservés **nommés** : une traduction peut les réordonner, et `format!` exige un littéral connu à
    la compilation, donc le modèle est traduit d'abord et les valeurs sont substituées ensuite
    (`po.rs`, commentaire de documentation de `tf`).

    ```rust
    // bins/delonix-runtime-bin/src/cmd/config.rs — refuse_unknown_key
    Err(Error::Invalid(super::po::tf(
        "'{key}' is not a config key — known: {known}",
        &[("key", key), ("known", &KNOWN_KEYS.join(", "))],
    )))
    ```
  - Le texte de `--help` est lui aussi en anglais dans le code source, traduit à l'exécution par
    `po::translate_help`. **Imposé (gate)** : `help_i18n_tests` dans `bins/delonix-runtime-bin/src/main.rs`.
    `todo_o_help_de_comando_tem_traducao_pt` est strict pour l'aide des commandes, et
    `o_help_dos_argumentos_so_pode_encolher` est un ratchet sur `ARG_HELP_PENDING` pour l'aide des
    flags. Une nouvelle commande ou un nouveau flag nécessite une entrée dans `pt.po`.
  - Une entrée manquante retombe sur l'anglais, de sorte que l'interface n'est jamais vide (`po::t`).
    Une chaîne portugaise écrite directement dans le code est un bug, pas un raccourci.
  - Ne réutilisez pas un `msgid` dont le portugais dépend du genre grammatical du sujet.
    « created » peut se dire *criada* pour un réseau et *criado* pour un volume : utilisez donc des
    clés distinctes. **Décidé** : AGENTS.md § « v0.32.2 — 380+ strings PT hardcoded ».
- **Pièges du lexique.** **Décidé** : AGENTS.md § LANG-01.
  - **Ne nommez rien `num`.** Il figure volontairement dans le lexique, car il détecte du vrai
    portugais (« num apply falhado »). Un identifiant `num` compte donc comme dette portugaise et fait
    échouer le gate. Utilisez `count` ou `number`.
  - Ajouter un mot au lexique **augmente** le compte et fait échouer le gate. Abaissez la base de
    référence dans le même commit. Les homographes (`data`, `base`, `no`, `nas`…) sont exclus, sauf si une mesure montre qu'ils détectent
    du vrai portugais ; `num` est l'exception consignée (AGENTS.md § LANG-01).

---

## 3. Nommage

### 3.1 Crates et répertoires

- **Le répertoire est la couche.** Un crate se trouve dans `crates/foundation/`, `crates/contexts/`,
  `crates/adapters/`, `crates/providers/` ou `crates/interfaces/`, ou bien c'est un binaire dans
  `bins/`. Le répertoire doit correspondre à l'entrée du crate dans la table `LAYERS`. **Imposé
  (gate)** : `scripts/arch_fitness.py` `misplaced()` / `LAYER_DIR`. Un nouveau crate entre dans
  `LAYERS` **et** dans le bon répertoire dans le même commit.
- **Convention de nommage cible pour les crates nouveaux ou restructurés.** **Proposé (ADR, pas encore
  décidé)** : ADR-0040 D2.1.

  | Rôle | Nom |
  |---|---|
  | Fondation pure partagée | `delonix-model` |
  | Contexte délimité (bounded context) | `delonix-<context>`, nommé d'après le groupe d'API publié (D2.2) : `delonix-compute`, `delonix-stack` |
  | Adaptateur technologique | `delonix-<technology>` : `delonix-linux`, `delonix-sdn`, `delonix-oci` |
  | Provider enfichable | `delonix-provider-<technology>` |
  | Bibliothèque d'interface | `delonix-<protocol>` : `delonix-cri`, `delonix-mcp` |

  Un crate n'existe que s'il est un contexte délimité, s'il isole une dépendance lourde ou
  privilégiée, ou s'il est un binaire installé séparément. **Pas de suffixes `-core`, `-common`,
  `-utils` ni `-types`** (l'ADR-0040 D2.1 consigne comment un crate `-core` est devenu « le puits de
  tout »). Certains crates portent encore d'anciens noms : `delonix-runtime-core`, `delonix-proxmox`,
  `delonix-truenas`, `delonix-security-runtime`. L'ADR-0040 renomme chacun d'eux dans la phase qui le
  restructure, « jamais deux fois ». **Ne renommez pas un crate en dehors de sa phase.**
- **Le chemin de chaque crate est écrit une seule fois**, dans `[workspace.dependencies]` du
  `Cargo.toml` racine. Les membres dépendent les uns des autres avec `{ workspace = true }`.
  **Décidé** : AGENTS.md § « A direcção das dependências é um portão (ADR-0040, fase P0) », et le
  commentaire en tête de `[workspace.dependencies]`.

### 3.2 Modules et fichiers

- **Groupes de commandes de la CLI** : un module par groupe dans
  `bins/delonix-runtime-bin/src/cmd/<group>.rs`, avec un `pub enum <Group>Cmd` (sous-commandes clap),
  un répartiteur `pub fn run(action: <Group>Cmd) -> Result<()>`, et une fonction `cmd_<verb>` par
  sous-commande. **Convention (observée)** : `cmd/volume.rs:VolumeCmd` + `run` +
  `cmd_create`/`cmd_ls`/`cmd_describe` ; `cmd/secret.rs:SecretCmd` + `run` ;
  `cmd/container.rs:cmd_run`/`cmd_start`/`cmd_stop`. AGENTS.md § « CLI (`delonix`) » énonce la
  partie un-module-par-groupe.
- **Les adaptateurs d'un port de calcul** vont dans un fichier nommé d'après la préoccupation du port,
  pas d'après la technologie : `crates/adapters/delonix-linux/src/workload.rs`, `.../run_host.rs`,
  `crates/adapters/delonix-sdn/src/run_network.rs`, `.../vm_network.rs`,
  `crates/adapters/delonix-oci/src/run_images.rs`. **Convention (observée)**.

### 3.3 Types, traits, fonctions, constantes

- **Les ports sont nommés d'après la capacité, pas d'après la technologie.** **Proposé (ADR, pas
  encore décidé)** : l'ADR-0040 D3 liste les ports : `WorkloadRuntime`, `SandboxProvider`, `VmProvider`, `NetworkProvider`, `StorageProvider`,
  `ImageRegistry`, `ImageStore`, … Ceux qui existent aujourd'hui sont dans
  `crates/contexts/delonix-compute/src/ports.rs` (`ImageStore`, `StorageProvider`, `DeviceResolver`,
  `RunHost`, `VmNetwork`, `NetworkProvider`) et `.../launch.rs` (`WorkloadRuntime`). L'ancien
  `VmBackend` de `crates/adapters/delonix-vm/src/lib.rs` doit devenir `VmProvider` en P4.
- **Les implémentations hôte d'un port s'appellent `Host<Thing>`.** **Convention (observée)** :
  `delonix-linux/src/workload.rs:HostWorkload` (implémente `WorkloadRuntime`),
  `delonix-sdn/src/run_network.rs:HostNetwork` (implémente `NetworkProvider`),
  `delonix-oci/src/run_images.rs:HostImages`, `delonix-volume/src/lib.rs:HostVolumes`,
  `delonix-linux/src/cdi.rs:HostDevices`.
- **Les fonctions de décision pures** sont des verbes ou des questions : `resolve_*`, `parse_*`,
  `valid_*`, `is_*`, `*_plan`. **Convention (observée)** : `cmd/vm.rs:resolve_vm_defaults`,
  `delonix-oci/src/registry.rs:parse_content_range`, `cmd/stack.rs:is_pending`,
  `delonix-net-rules/src/lib.rs:bridge_name`.
- **Les constantes** sont en `SCREAMING_SNAKE_CASE` (**Imposé (gate)**, lint rustc sous clippy
  `-D warnings`). **Les noms de Kind sont eux aussi des constantes, jamais des littéraux de chaîne
  répétés** (**Décidé** : AGENTS.md § « Os Kinds ganham grupos e nomes definitivos » ; les constantes
  se trouvent dans `crates/contexts/delonix-stack/src/kinds.rs` : `pub const VM: &str = "VirtualMachine";`).

### 3.4 Noms des tests

- **Le ratchet compte les noms de tests.** `lang_ratchet.py` reconnaît chaque déclaration `fn` et
  n'ignore pas `#[cfg(test)]`, donc un nom de test en portugais augmente `identifiers` et fait échouer
  le gate. **Imposé (gate)**.
- De nombreux tests existants ont des noms en forme de phrase portugaise, par exemple
  `delonix-model/src/exitcode.rs:nao_existe_e_rebentou_deixam_de_ser_o_mesmo_numero`. C'est de la
  dette comptabilisée, pas un style à copier. Les nouveaux tests sont des **phrases en anglais qui
  énoncent le comportement démontré**, comme les tests plus récents du même fichier :
  `a_missing_capability_is_not_a_wrong_argument` et `the_text_class_and_the_number_cannot_diverge`.
  **Décidé** : LANG-01 (AGENTS.md) ; la forme « phrase » est une **Convention (observée)**.
- Si vous traduisez un nom de test existant, le compte diminue : exécutez donc
  `python3 scripts/lang_ratchet.py --update` dans le même commit.

### 3.5 Commandes et flags de la CLI

- **Commandes groupées, `delonix <group> <verb>`.** Pas de raccourcis plats au niveau supérieur.
  **Décidé** : AGENTS.md § « Reorganização da raiz da CLI (v0.30.0) » ; `docs/cli-stability.md` (les
  raccourcis de niveau supérieur ont été supprimés en v1.0.0).
- **Les verbes suivent Docker/Podman/kubectl** lorsqu'un tel verbe existe. **Décidé** : AGENTS.md §
  les sprints « Reestruturação da CLI (semântica Docker/Podman/kubectl) » ; `docs/cli-stability.md` §
  « Estável ».
  - Les verbes de liste utilisent `ls` (`network ls`, `volume ls`, `image ls`…). `image list` a été
    ramené à `ls` en v2.0.0 (`docs/cli-stability.md`).
  - `create` ne fait que créer, et refuse un nom existant avec le code de sortie 5 sauf avec
    `--force`. L'upsert est un verbe distinct (`secret set`). `apply` est un « ensure present »
    idempotent. **Décidé** : AGENTS.md § « Sprint 1: `secret create` vs `secret set` ».
  - `describe` est destiné aux humains (style kubectl), `inspect` produit du JSON pour les scripts.
    **Décidé** : AGENTS.md § « Output: `ls` estilo docker, `describe` estilo kubectl ».
  - L'ordre et les noms des flags copient Docker lorsque Docker possède le concept :
    `network connect <NETWORK> <CONTAINER>`, `-p [hostIp:]hostPort:containerPort`,
    `volume create --driver … --opt k=v`. **Décidé** : AGENTS.md § Sprints 5 et 6.
- **Les changements incompatibles sont des coupures nettes, sans alias.** L'ancienne forme doit
  échouer avec `unrecognized subcommand`, et ne jamais faire silencieusement autre chose. Avant de
  couper, recherchez (grep) les appelants internes dans **tout** le workspace. **Décidé** :
  `docs/cli-stability.md` § « Como uma quebra é feita ». Les groupes listés comme stables dans ce
  fichier ne peuvent être cassés que dans une release majeure.
- **Une commande accessible par plusieurs chemins doit être câblée sur tous** (par exemple
  `vm pull` / `image vm pull` / `image --vm pull`). **Décidé** : `CONTRIBUTING.md` ; voir
  [Flux de contribution](contributing-workflow.md#adding-or-changing-a-cli-command).
- **Les modifications de feuilles mettent à jour la base de référence de la CLI**
  (`scripts/cli-tree.sh --update`) dans le même commit. **Imposé (gate)** : voir
  [Flux de contribution](contributing-workflow.md#adding-or-changing-a-cli-command).

### 3.6 Kinds, groupes d'API et champs de manifeste

- **Les Kinds sont des noms en `UpperCamelCase`** dans l'un des groupes publiés
  `core`, `compute`, `networking`, `gateway`, `storage`, `artifact`, `infrastructure`
  (`<group>.delonix.io/v1alpha1`). **Décidé** : AGENTS.md § « Identidade e fronteira do
  motor » et § « Os Kinds ganham grupos » (l'ADR-0020, qui a introduit les groupes, est encore
  **Proposed**). Chaque Kind est **une ligne** dans
  `crates/contexts/delonix-stack/src/kinds.rs` (`KindFacts` : `kind`, `plural`, `short`,
  `api_version`, `domain`, `form`, `in_stack`, `converges`, …). `delonix api-resources` affiche cette
  table. Ajouter un Kind touche aussi des tables que rien ne dérive de `kinds.rs` (`hot_fields`,
  `NAMESPACE_SOURCES`, `TYPED_KINDS`, le schéma généré). Les tests échouent tant que chacune n'est pas
  faite. **Imposé (gate)** : AGENTS.md § « `kind: Service` » liste quel test a détecté chaque table.
- **Un Kind renommé garde son ancien nom comme alias silencieux, insensible à la casse.** Une
  **fusion** avertit, car sa signification a changé. **Décidé** : AGENTS.md § « Os Kinds ganham grupos
  e nomes definitivos » (« Alias silencioso, não depreciação ») ; implémenté dans
  `cmd/manifest.rs:KIND_ALIASES`. L'ADR-0020 est encore **Proposed**.
- **Les champs de manifeste sont en `camelCase`.** Si un champ avait auparavant une graphie en
  `snake_case`, cette graphie reste acceptée comme `alias` `serde`. **Convention (observée)**, champ
  par champ plutôt qu'avec `rename_all` :

  ```rust
  // bins/delonix-runtime-bin/src/cmd/vm.rs — VmSpec
  /// Canonical `cpuAffinity`; `cpu_affinity` stays accepted (back-compat).
  #[serde(rename = "cpuAffinity", alias = "cpu_affinity")]
  cpu_affinity: Option<String>,
  ```

  Autres exemples : `delonix-compute/src/pod.rs:PodSpec.restart_policy` (`rename = "restartPolicy"`),
  `cmd/service.rs:ServiceSelector.match_labels` (`rename = "matchLabels"`). Le schéma publié
  (`docs/schema/v1/delonix.json`) est **généré** à partir de ces structs, et un test vérifie qu'il leur
  correspond (ADR-0007). Le schéma des manifestes est déclaré **stable** (`docs/cli-stability.md` § « O
  schema dos manifestos »).
- **Les enregistrements internes** (le JSON sous la racine d'état) gardent les noms de champs Rust en
  `snake_case`. Voir `crates/foundation/delonix-runtime-core/src/lib.rs` (`net_mode`, `namespace`).
  **Convention (observée)**.

### 3.7 Variables d'environnement

- **Préfixe `DELONIX_`**, en majuscules : `DELONIX_ROOT`, `DELONIX_NET_RUNTIME_DIR`, `DELONIX_L18N`,
  `DELONIX_LOG_FORMAT`, `DELONIX_CRI_CAP_CEILING`. **Convention (observée)** dans `crates/` et
  `bins/`.
- **Pour la télémétrie, lisez les variables standard `OTEL_*`**, et non un nouvel alias `DELONIX_*`.
  **Proposé (ADR, pas encore décidé)** : ADR-0040 D6. C'est la cible, pas le code d'aujourd'hui :
  `crates/adapters/delonix-telemetry/src/telemetry.rs` lit `DELONIX_OTLP_ENDPOINT` pour l'exportateur
  OTLP, et de l'ensemble standard uniquement `OTEL_SERVICE_NAME`. N'ajoutez pas de nouvelle variable
  de télémétrie `DELONIX_*`, et ne supprimez pas `DELONIX_OTLP_ENDPOINT` en dehors de la phase qui la
  migre.
- **Une échappatoire qui affaiblit une valeur par défaut de sécurité est bruyante et explicite.** Elle
  est désactivée sauf si elle vaut `1`, et elle avertit : `DELONIX_ENABLE_IPV6=1`,
  `DELONIX_ALLOW_LINK_LOCAL=1`. **Décidé** : AGENTS.md § « Bloco 0 do plano 33 (v0.37.1) ».
- Un flag l'emporte sur la variable d'environnement, qui l'emporte sur la valeur par défaut
  (`serve cri --cap-ceiling` contre `DELONIX_CRI_CAP_CEILING`). **Décidé** : AGENTS.md § « Tecto de
  capabilities no CRI ».

### 3.8 Codes de sortie et codes `DX_*`

- **Les codes de sortie sont dérivés du type d'erreur en un seul endroit** : la fonction exhaustive
  `crates/foundation/delonix-model/src/exitcode.rs:for_error`. Les classes sont 1 générique,
  2 usage, 3 `NOT_RUNNING`, 4 `NOT_FOUND`, 5 `CONFLICT`, 69 `UNAVAILABLE`, 74 `IO`,
  77 `NO_PERMISSION`, 124 `TIMEOUT`. Chaque erreur a aussi une identité textuelle stable,
  `Error::code()`, qui renvoie une chaîne `DX_*`. **Imposé (gate)** : le match n'a pas de branche
  `_ =>`, donc une nouvelle variante arrête le build jusqu'à ce que quelqu'un la classe. Le test
  `the_text_class_and_the_number_cannot_diverge` maintient les deux en phase. **Décidé** : AGENTS.md §
  « Códigos de saída com classe (v0.49.0) » ; `docs/cli-stability.md` § « Códigos de saída ».
- **Vous ne choisissez pas un nombre, vous renvoyez la bonne variante.** « Ça n'existe pas » est
  `Error::NotFound`, « ça existe déjà » est `Error::Conflict`, « il manque un outil sur cet hôte » est
  `Error::Unavailable`. Un nouveau nombre exige un vrai producteur. **Décidé** : documentation de module
  de `exitcode.rs` (« every extra number is a promise »).

---

## 4. Structure : où va le code

### 4.1 Les couches et la direction

La direction autorisée est écrite en un seul endroit, `ALLOWED` dans `scripts/arch_fitness.py`.
**Imposé (gate)** :

| Couche | Peut dépendre de |
|---|---|
| foundation | foundation |
| context | foundation, context |
| adapter / provider | foundation, context |
| interface | foundation, context, adapter, provider |
| bin | tout |

Les dépendances de développement et de build ne comptent pas. Une exception déclarée doit nommer la
phase de l'ADR-0040 qui la supprime, et une exception qui ne s'applique plus échoue aussi
(`EXCEPTIONS`). La table des couches générée et les exceptions actuelles se trouvent dans
[Architecture](architecture.md#layers-and-the-allowed-direction).

D'autres règles structurelles, chacune **Imposé (gate)** par `scripts/arch_fitness.py` :

- **La fondation et les contextes restent exempts de dépendances lourdes.** `tokio`, `axum`, `hyper`,
  `tonic`, `reqwest`, `clap`, `ratatui`, `serde_yaml`, `rmcp`, OpenTelemetry et `prometheus-client` y
  sont refusés (`HEAVY`).
- **Un binaire compose exactement une interface.** Imposé dans `rule_failures()` (introduit par
  l'ADR-0040 D2.4, encore **Proposed** ; également écrit dans AGENTS.md § « A direcção das dependências
  é um portão »).
- **Les versions des dépendances ne vivent que dans le `[workspace.dependencies]` racine.** Un membre
  écrit `{ workspace = true, features = [...] }` et rien d'autre. `default-features = false` reste à la
  racine, car un membre ne peut pas désactiver ce que la racine active (`inline_versions()`).
- **Les bibliothèques n'affichent rien.** Le ratchet `library_prints` compte les
  `println!`/`eprintln!`/`print!` en dehors de `bins/`. Émettez plutôt du `tracing` (par exemple
  `tracing::warn!` dans `crates/adapters/delonix-sdn/src/lib.rs`) et laissez l'interface présenter la
  sortie. **Décidé** : AGENTS.md § « A direcção das dependências é um portão (ADR-0040, fase P0) »
  (« Uma biblioteca não escreve para o terminal; emite `tracing` »).
- **Les bibliothèques ne réexécutent pas le binaire du moteur lui-même.** Le ratchet `self_exec_sites`
  compte `current_exe()`, `cli_bin()` et `delonix_bin()` en dehors de `bins/`. Appelez plutôt une
  fonction ou un cas d'utilisation. `Command::new("ip")`, `nft`, `qemu-img` et `ssh` ne sont **pas**
  comptés, car exécuter ces outils est exactement la raison d'être d'un adaptateur (commentaire
  au-dessus de `SELF_EXEC`).
- **N'écrivez pas dans l'environnement du processus.** Le ratchet `env_writes` compte
  `env::set_var`/`remove_var` partout, tests compris. Les tests s'exécutent sur des threads parallèles
  et une écriture entre en concurrence avec chaque lecteur. Passez plutôt les valeurs en paramètre
  (commentaire au-dessus de `ENV_WRITES`).

### 4.2 « Ma modification est X → elle va dans Y »

Cette table utilise les crates tels qu'ils existent aujourd'hui. Consultez [Les crates](crates.md) pour le
contenu de chaque crate avant d'y ajouter quoi que ce soit. Lorsque la colonne source cite l'ADR-0040
ou l'ADR-0026, l'étiquette est **Proposé (ADR, pas encore décidé)** : les deux ADR sont encore
Proposed, même si les crates qu'ils décrivent existent déjà.

| Votre modification | Crate (couche) | Source |
|---|---|---|
| Une règle pure sur les CIDR, les noms de bridge ou l'arithmétique IPAM que les deux côtés doivent calculer à l'identique | `delonix-net-rules` (foundation) | 06 ; AGENTS.md § Arquitetura |
| Une nouvelle classe d'erreur, un code de sortie ou un code `DX_*` ; les noms générés | `delonix-model` (foundation) | ADR-0040 D1 |
| Un type d'enregistrement persisté (`Container`, `Vm`, …) | `delonix-runtime-core` (foundation) — le reliquat du découpage de l'ADR-0040 P3. N'y ajoutez que ce qui relève des enregistrements, jamais des helpers généraux | ADR-0040 D2.1 « no `-core` » |
| Une règle pure du modèle des secrets (`Secret`, noms et clés valides, analyse de fichiers env) | `delonix-model` (foundation) : `secret.rs` | ADR-0040 P3 (la PR qui a déplacé les stores) |
| Un store, le verrou de fichier, `write_atomic*`/`write_private_temp`, le store de secrets chiffré ou le coffre d'identifiants | `delonix-state` (adapter) | ADR-0040 D2.3 |
| Les faits des Kinds, le planificateur/diff, les conditions, les révisions | `delonix-stack` (context) | AGENTS.md § Arquitetura |
| La spécification d'exécution, sa validation pure, un port dont le cas d'utilisation run a besoin | `delonix-compute` (context) : `run_opts.rs`, `preflight.rs`, `ports.rs` | ADR-0040 D2.2 |
| Politique de sécurité, admission, score, masquage (redaction) | `delonix-security-runtime` (context) | ADR-0026 |
| Namespaces, cgroups, montages, capabilities, seccomp, périphériques | `delonix-linux` (adapter) | ADR-0040 D2.3 |
| holder de netns, nftables, slirp, DNS, DHCP, overlay, WireGuard, CNI | `delonix-sdn` (adapter) | ADR-0040 D2.3 |
| Client de registre, CAS, layers, overlay, build d'images | `delonix-oci` (adapter) | ADR-0040 D2.3 |
| SBOM / CVE | `delonix-scanner` (adapter) | ADR-0040 D2.3 |
| Tracing, OpenTelemetry, mise en place du registre Prometheus | `delonix-telemetry` (adapter) | ADR-0040 D2.3 |
| Un backend de VM local (Cloud Hypervisor, libvirt) | `delonix-vm` (adapter) | ADR-0008 |
| Un provider distant ou enfichable (API d'hyperviseur, API de NAS) | un crate provider dans `crates/providers/`. **Écrivez d'abord un ADR** | ADR-0008, ADR-0009 ; [Flux de contribution](contributing-workflow.md#when-to-write-an-adr) |
| Un RPC CRI | `delonix-cri` (interface) | AGENTS.md |
| L'API de gestion locale, `/metrics` | `delonix-mgmt` (interface) | ADR-0010 |
| Un outil MCP | `delonix-mcp` (interface) | ADR-0025 |
| Une commande CLI, sa présentation et ses traductions | `bins/delonix-runtime-bin/src/cmd/<group>.rs` + `data/pt.po` | AGENTS.md § CLI |
| Quel adaptateur soutient quel port (composition) | la racine de composition du binaire. Aucune logique métier à cet endroit | ADR-0040 D1 « Binaries » |

### 4.3 Cœur pur, E/S aux bords

- **Les décisions sont des fonctions pures sur des données que vous avez déjà lues.** Elles ne prennent
  aucun store, n'exécutent aucune commande et n'ont besoin d'aucun privilège, de sorte qu'un test peut
  les appeler avec de simples valeurs. **Proposé (ADR, pas encore décidé)** : ADR-0040 D1 (le
  `domain/` d'un contexte n'a « no I/O, no `tokio`, `libc`, `nix`, `std::fs` »). **Convention
  (observée)** : `delonix-stack/src/reconcile.rs` (« decides it WITHOUT touching the machine »),
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
  exemple, `resolve_image_ref` prend le store d'images au lieu d'ouvrir le vrai, afin que le test
  puisse passer un répertoire temporaire. **Décidé** : AGENTS.md § « O manifesto de VM resolvia a
  imagem de outra maneira que a CLI ».
- **Une règle, un propriétaire.** Lorsque deux sites d'appel ont besoin de la même dérivation, extrayez
  une fonction et appelez-la depuis les deux. Une seconde copie dérive. Exemples : `delonix_net_rules::bridge_name`, réexporté par `delonix-sdn` (le nom du bridge
  avait deux formules et affichait un périphérique qui n'existait pas), `infra::dhcp_lease_ip`,
  `effective_entrypoints`. **Décidé** : AGENTS.md § « `delonix network` », § « Isolamento de
  namespace », § « Reverse-proxy L7 ».

---

## 5. API interne et frontières

### 5.1 Ports et adaptateurs

- **Un nouveau backend implémente un port. Ce n'est jamais un `if provider == …` ailleurs.**
  **Décidé** : AGENTS.md § « Identidade e fronteira do motor » (« Um provider novo entra como
  implementação de uma porta, nunca como um `if provider == …` »). L'ADR-0040 D3, règle 3 (« No string
  matching on provider names outside the composition root »), le reformule et est encore **Proposed**.
  D3 prévoit un test de conformité (fitness test) pour cela, mais
  **il n'existe pas encore dans `arch_fitness.py`**, c'est donc la revue qui l'impose pour l'instant.
- **La connaissance propre à un backend vit dans le backend.** Par exemple,
  `VmBackend::ip_is_predicted()` indique si l'IP d'une VM a été prédite, au lieu que le site d'appel
  vérifie `backend.contains("cloud-hypervisor")`. **Décidé** : ADR-0008, cité dans le commentaire de
  documentation de `crates/adapters/delonix-vm/src/lib.rs`.
- **Un adaptateur ne dépend pas d'un autre adaptateur.** Ce dont il a besoin d'une autre préoccupation
  lui arrive sous forme de hook ou de port, câblé par la racine de composition. **Imposé (gate)** :
  `ALLOWED` (adapter → foundation, context). **Convention (observée)** : le commentaire de
  documentation de `delonix-linux/src/workload.rs:HostWorkload` explique ses hooks
  `addresses`/`attach_slirp` de cette manière.

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

- **Un trait a besoin d'un vrai consommateur au moment où il arrive.** N'écrivez pas d'échafaudage qui
  attend son premier appelant. Chaque méthode doit avoir un appelant. **Décidé** : AGENTS.md §
  « `delonix workload` » (ADR-0002). Une fonction publique sans appelant est aussi un danger connu :
  plusieurs d'entre elles se sont révélées cacher un bug latent (`mount_live`, `set_net_rate`,
  `update_limits`, `publish_port_allow`), et certaines ont été supprimées plutôt que câblées
  (AGENTS.md § « Endurecimento do ingress/egress »).

### 5.2 Erreurs par crate (ADR-0040 P3)

- **Un adaptateur ou un provider définit son propre `Error` et le convertit dans la classe partagée**
  (`delonix_model::Error`), qui porte le code `DX_*`. **Proposé (ADR, pas encore décidé)** :
  ADR-0040 P3.
  **Imposé (gate)** : le ratchet `shared_error_imports` de `arch_fitness.py` (`SHARED_ERROR`,
  limité à `crates/adapters/` et `crates/providers/`) compte les imports
  `use delonix_runtime_core::{…Error/Result…}` ou `use delonix_model::{…}` qui font du type partagé le
  type de résultat propre du crate. Le type partagé peut toujours être nommé à l'intérieur d'une impl
  `From`. L'implémentation de référence est `crates/adapters/delonix-scanner/src/error.rs` :

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

  Trois choses dans ce fichier constituent le modèle : la conversion **décide** de la classe ; `code()`
  interroge la conversion au lieu de tenir une seconde table ; et un test
  (`the_code_is_the_code_of_the_class_it_converts_into`) maintient les deux en phase. Le message
  converti est aussi conservé identique, octet pour octet, à ce que la CLI affichait auparavant
  (`the_converted_message_is_the_one_printed_before`).
- **Demandez sa classe à l'erreur ; ne faites pas de `match` sur une variante de l'erreur partagée.**
  Hors de la fondation, écrivez `e.is_not_found()` ou `e.class()`, ou faites un `match` sur `e.root()`
  lorsque vous avez besoin du contenu — jamais `Err(Error::NotFound(_))` ni `matches!(…, Error::NotFound(_))`.
  L'erreur propre d'un crate voyage dans la classe partagée avec son code : un `match` sur une
  variante cesse de la reconnaître sans que le compilateur ne dise rien. **Décidé** : ADR-0043 D4 (Accepted).
  **Imposé (gate)** : le ratchet `raw_error_variant_matches` de `arch_fitness.py`
  (`RAW_VARIANT_MATCH`, qui ignore `crates/foundation/delonix-model/`), dont la base de référence est 0 —
  tout nouveau `match` de ce type fait échouer la CI. Les méthodes se trouvent dans
  `crates/foundation/delonix-model/src/codes.rs`.

### 5.3 Visibilité

- **Privé par défaut.** À l'intérieur du crate de la CLI, utilisez `pub(crate)` pour ce qu'un autre
  module `cmd` doit appeler. **Convention (observée)** : `cmd/container.rs:cmd_run`, `cmd_start` et
  `cmd_stop` sont `pub(crate)` afin que `pod`, `compose` et `stack` puissent leur déléguer ;
  `cmd/firewall.rs:update_locked` ; `cmd/manifest.rs:KIND_ALIASES`.
- **`pub` dans un crate bibliothèque est une promesse faite aux autres crates.** Supprimer un élément
  public est un changement incompatible pour les utilisateurs de la bibliothèque, même avec zéro
  appelant dans ce workspace. Le lint `dead_code` de rustc ne voit pas les éléments `pub` inutilisés :
  lorsque vous en supprimez un, comptez donc à la main les éléments publics devenus orphelins.
  **Décidé** : AGENTS.md § « `delonix_sdn::Net` foi APAGADO — e é breaking para quem usa a biblioteca ».

### 5.4 Le contrat de nœud

`proto/delonix/node/v1` est la source de vérité des deux encodages, gRPC et HTTP/JSON.
`docs/api/openapi.yaml` en est généré. **Ne modifiez jamais le fichier OpenAPI à la main.**
**Imposé (gate)** : `scripts/contract_gate.py` exécute `buf format`, `buf lint`, `buf breaking`
contre le dernier tag qui contient `proto/`, vérifie le mappage HTTP de chaque RPC, et vérifie que le
fichier OpenAPI est identique au fichier généré. **Décidé** : AGENTS.md § « O contrato de nó é um
portão » (qui cite l'ADR-0040 P1 ; l'ADR-0040 D4 est encore **Proposed**).

- **Un message de requête par RPC**, nommé `<Rpc>Request`. **Imposé (gate)** : `buf lint`
  `RPC_REQUEST_STANDARD_NAME` (voir `buf.yaml`). Une requête partagée laisse un champ destiné à une
  méthode apparaître dans cinq.
- **Identité explicite dans la requête** : des champs `name` et `namespace`, jamais un message de
  métadonnées générique avec des champs que le moteur ignorerait. **Décidé** : AGENTS.md.
  **Convention (observée)** : `compute.proto:GetContainerRequest { string name = 1; string namespace = 2; }`.
- **Les images sont adressées par requête (query), pas dans le chemin**, car dans un chemin comme
  `alpine:3.20` le `:` serait lu comme un verbe personnalisé. **Décidé** : AGENTS.md. **Convention
  (observée)** : `infra.proto` `GetImage` → `get: "/v1/images:get"`, avec
  `GetImageRequest { string reference = 1; }`.
- **Les réponses** renvoient la ressource, ou une `Operation` pour les mutations de longue durée. Les
  règles de nommage des réponses de `buf lint` sont désactivées volontairement (commentaire de
  `buf.yaml`).
- **Chaque RPC a un mappage HTTP, sauf les flux bidirectionnels** (`Exec`, `Console`), qui ne doivent
  pas en avoir. **Imposé (gate)** : `contract_gate.py`, vérification 4.

### 5.5 Le moteur ne connaît aucun consommateur

Le moteur ne sait pas qui l'utilise. Aucun nom de plateforme, de control plane, de console ou d'agent,
et aucune notion de tenant, de compte, de plan ou de facturation, ne peut apparaître dans `crates/`,
`bins/`, `proto/`, le `Cargo.toml` racine ou le `Makefile`, **commentaires compris**.

- **Noms de consommateurs.** **Imposé (gate)** : `scripts/arch_fitness.py` `consumer_mentions()`
  applique l'expression régulière `CONSUMER_NAMES`, une liste fixe de noms, à ces chemins. Un nom qui
  n'est pas dans la liste n'est pas détecté.
- **Concepts de tenant, de compte, de plan et de facturation.** **Décidé** : AGENTS.md § « Identidade
  e fronteira do motor ». Aucun gate ne les détecte ; la revue le vérifie.

Si un consommateur a besoin de quelque chose, écrivez-le comme une
capacité générique dans le vocabulaire propre du moteur (Kinds et ressources), et ne l'ajoutez que si
elle a du sens pour n'importe quel client. Le moteur valide son propre contrat et ne fait jamais
confiance à un appelant pour refuser ce qu'il ne prend pas en charge.

---

## 6. Gestion des erreurs et messages

- **Pas d'échec silencieux.** Si une option est acceptée puis ignorée, c'est pire qu'une fonctionnalité
  manquante, car l'utilisateur croit qu'elle a pris effet. Refusez-la avec une erreur claire, et nommez
  le flag. **Décidé** : AGENTS.md § « Falhas silenciosas corrigidas (fail-closed) » ; l'audit de la
  v0.37.0 (§ « Auditoria sistemática dos 208 subcomandos ») appelle cette classe « relato desonesto »
  (compte rendu malhonnête). Les motifs que cette section liste sont :
  - **Ne détruisez rien avant de savoir que l'objet est à vous de détruire, et supprimez la
    comptabilité en dernier.** Si l'enregistrement est supprimé d'abord et que la suppression des
    données échoue ensuite, les données deviennent orphelines et un `create` ultérieur les remet à
    quelqu'un d'autre.
  - **Une mesure illisible est *inconnue*, jamais zéro.** Un `read_dir` qui échoue n'est pas un
    répertoire vide. C'est pourquoi `Usage { bytes, unreadable }` existe.
  - **Surveillez les motifs qui transforment des échecs en succès** : `let _ =` sur un résultat qui
    compte (entropie, lecture de socket), `as u64` sur un `f64` (il sature), `capture()` lu par son
    `Result` au lieu de sa sortie. AGENTS.md § « A classe «X não é Y» » les répertorie.
- **Inconnu ou non mesurable n'est pas une supposition.** Lorsque le moteur ne peut pas lire une
  valeur, il signale qu'il ne sait pas, ou il refuse. Il ne choisit pas la réponse la plus probable.
  Exemple : `system prune --auto` refuse si l'occupation du disque ne peut pas être lue, et le
  collecteur IPAM échoue fermé (fail closed) lorsqu'un store est illisible (une liste vide se lirait
  comme « rien n'est vivant »). **Décidé** : AGENTS.md § CLI (`system prune`), § « O IPAM vaza ».
- **Forme du message : le fait d'abord, puis quoi faire**, avec la commande exacte lorsqu'il y en a
  une. **Décidé** : AGENTS.md § « `-p 80:80` respondia com o JSON cru do slirp » (« facto primeiro,
  depois os comandos prontos a copiar »). **Convention (observée)** :
  `delonix-model/src/error.rs:Error::VmNotFound` → `"no such VM: {0} (see \`delonix vm ls\`)"` ;
  `cmd/config.rs:refuse_unknown_key` nomme les clés valides. Nommez l'**outil manquant et son
  paquet** au lieu de transmettre un `ENOENT` brut, qui se lit comme un fichier manquant (AGENTS.md §
  « A bateria mede o `--help` de tudo », achado 1).
- **Une mesure partielle n'est pas un succès.** `--wait` doit observer ce qu'il affirme, et `✓ … is up`
  n'est affiché qu'après vérification. **Décidé** : AGENTS.md § « O `--wait` de uma VM CH ».
- **N'analysez jamais un message d'erreur pour décider quoi faire.** Les messages sont traduits
  (`--l18n=pt`/`DELONIX_L18N`), donc un `grep 'no such'` classe sur une machine et cesse
  silencieusement de classer sur une autre. Utilisez la classe de sortie ou le code `DX_*`.
  **Décidé** : documentation de module de `delonix-model/src/exitcode.rs`.
- **Renvoyez la variante qui correspond à la classe** (`NotFound`, `Conflict`, `NotRunning`,
  `Unavailable`, `Timeout`), et non `Invalid` pour tout. **Décidé** : AGENTS.md § « Códigos de saída
  com classe » : `util::find` qui renvoyait `Invalid` pour « introuvable » rendait la ressource la plus
  utilisée impossible à classer.

---

## 7. `unsafe`, appels système et processus

- **Chaque bloc `unsafe` a un commentaire `// SAFETY:` juste au-dessus.** **Imposé (gate)** :
  `undocumented_unsafe_blocks = "deny"` dans `[workspace.lints.clippy]`.

  ```rust
  // crates/adapters/delonix-linux/src/lib.rs — apply_filter_logged
  // SAFETY: `fprog` points to a valid BPF program; NO_NEW_PRIVS is already set.
  let rc = unsafe {
      libc::syscall(libc::SYS_seccomp, SET_MODE_FILTER, FLAG_LOG, &fprog as *const _)
  };
  ```

  Le commentaire doit énoncer l'invariant qui rend l'appel correct. « same » n'est acceptable que
  juste après un appel identique et justifié (comme dans `delonix-linux/src/lib.rs` juste après le
  premier `_exit(126)`).
- **Pas de `clone()`/`fork()` bruts dans un processus multithread** (les serveurs tokio, le shim de
  l'API Docker). `clone` n'exécute pas les handlers `pthread_atfork`, donc l'enfant peut se bloquer
  (deadlock) sur le verrou de malloc. Réexécutez plutôt une spécification typée, transmise par un
  fichier `0600`/`O_EXCL` plutôt que par argv. **Décidé** : AGENTS.md § « Auditoria de segurança #3 »,
  point 5. Contexte : [Initiation à Rust pour cette base de code](rust-primer.md#why-forkclone-in-a-multi-threaded-process-is-dangerous).
- **Un hook `pre_exec` ne doit pas se bloquer sur quelque chose que le parent fait après le retour de
  `spawn`.** `Command::spawn` ne revient qu'après `exec`, donc les deux processus s'attendent
  mutuellement pour toujours. Utilisez un `fork` brut pour les poignées de main (handshakes).
  **Décidé** : AGENTS.md § « A classe «X não é Y» » (l'entrée `reexec_mapped_hold`).
- **Fichiers temporaires : utilisez `delonix_state::write_private_temp`.** Il ouvre avec un nom
  unique, `O_EXCL` et le mode `0600`, et ne suit donc jamais un lien symbolique piégé. N'utilisez pas
  un nom fixe ou dérivé du pid dans `/tmp`. **Décidé** : AGENTS.md § « Auditoria de segurança #3 »,
  passagem 2 ; commentaire de documentation dans `crates/adapters/delonix-state/src/store.rs`. **Convention (observée)** :
  `delonix-sdn/src/bpf.rs`, `delonix-linux/src/run_host.rs`.
- **Fichiers qui doivent être privés ou atomiques : utilisez `write_atomic_mode(path, bytes, Some(0o600))`.**
  Il fixe le mode à la création et publie par un renommage atomique. N'écrivez jamais le fichier pour
  ensuite faire un `chmod`, car un autre utilisateur peut l'ouvrir entre-temps. **Décidé** :
  commentaire de documentation de `delonix-state/src/store.rs:write_atomic_mode` ; AGENTS.md (TOCTOU du kubeconfig).
- **Avant de signaler un pid lu dans un fichier, vérifiez qu'il s'agit toujours du même processus.**
  Utilisez `delonix_runtime_core::safe_to_signal(pid, starttime)`, qui compare l'heure de démarrage
  afin qu'un pid recyclé ne soit pas tué. **Décidé** : AGENTS.md § « A classe «X não é Y» » (les
  entrées sur les pid).
- **L'argv d'un processus ne prouve pas qu'il est à nous.** D'autres racines d'état du même
  utilisateur, et d'autres outils, s'exécutent avec le même argv. Vérifiez un jeton que nous seuls
  choisissons : un chemin dérivé de notre racine, ou une variable d'environnement fixée au spawn.
  **Décidé** : AGENTS.md § « A classe «X não é Y» » (l'entrée « o argv de um processo »).
- **Passez `--` avant les arguments positionnels issus d'une entrée** dans l'argv des outils externes
  (`ssh`, `scp`, `virsh`, `mount`, `qemu-img`), et validez toute valeur qui aboutit dans un shell
  distant contre une liste blanche de caractères. `shell_quote` n'assainit pas le contenu. **Décidé** :
  AGENTS.md § « Auditoria de segurança (skill `delonix-runtime-sec`) » et § « #2 ».
- **Les chemins construits à partir de noms issus de l'entrée utilisateur ou d'un manifeste sont
  confinés.** Utilisez une vérification de nom `valid_*` à la frontière du moteur
  (`delonix_vm::valid_vm_name`), et une jointure sûre qui refuse les composants `..`/absolus et les
  liens symboliques (`safe_join`, `safe_bind_target`). **Décidé** : mêmes sections d'AGENTS.md.

---

## 8. État et concurrence

- **La séquence lire–modifier–écrire passe par `update`, jamais par `load` → mutation → `save`.**
  `Store::update` et `JsonStore::update` (`crates/adapters/delonix-state/src/store.rs`)
  prennent un `flock`, **relisent sous le verrou**, appliquent votre closure et écrivent de manière
  atomique. Une closure qui renvoie `false` annule l'écriture. La CLI, le serveur CRI et les
  rafraîchissements en arrière-plan touchent tous les mêmes enregistrements en parallèle, et sans le
  verrou une écriture est silencieusement perdue. **Décidé** : commentaires de documentation des deux
  fonctions ; AGENTS.md § « Revisão ampla de código/arquitectura (2026-07-27) », bug 5 et l'élément
  `JsonStore`. **Convention (observée)** : une mutation qui peut elle-même échouer est enveloppée
  ainsi :

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

- **Persistez chaque étape dès que le dataplane la confirme.** Si une modification en plusieurs étapes
  échoue à mi-chemin, l'enregistrement doit toujours correspondre à ce que le noyau a réellement.
  **Décidé** : AGENTS.md § « Reconfiguração a quente » (« Persistência »).
- **Les nouveaux champs des enregistrements persistés prennent `#[serde(default)]`** (ou
  `default = "fn"`), afin que les enregistrements écrits par des versions plus anciennes se chargent
  toujours. La valeur par défaut doit décrire ce qu'étaient réellement les anciens enregistrements, pas
  une supposition. **Décidé** : AGENTS.md (par exemple `Vm.namespace`, `VmImage.cloud_init`).
  **Convention (observée)** : `delonix-runtime-core/src/lib.rs`, le commentaire de documentation de
  `Vm.namespace` (« the default is a statement of fact and not a guess »).
- **Tout ce qui est nécessaire pour reconstruire une ressource doit être persisté, et pas seulement
  utilisé à la création.** Lorsque vous touchez un chemin `start`/`restart`, comparez champ par champ
  ce que la création utilise avec ce que l'enregistrement stocke. **Décidé** : AGENTS.md § « BUG GRAVE
  corrigido… `-v` nunca era persistido » (listé là comme le troisième bug de la même famille).
- **Les fichiers de verrou ne sont jamais supprimés.** En supprimer un ouvre une fenêtre où deux
  processus verrouillent des inodes différents. **Décidé** : commentaire de documentation de
  `store.rs:lock_path` (`delonix-state`).
- **`SecretStore::update` est le seul `update` dont le verrou est au mieux (best-effort).** Son `FileLock::acquire`
  renvoie `Option` et continue sans verrou si le fichier de verrou ne peut pas être ouvert, contrairement à `Store` et
  `JsonStore`. **Non décidé** : s'il devrait refuser comme les autres ; suivez le code environnant
  et signalez-le dans la PR si vous y touchez.

---

## 9. Tests

- **Où ils vont.** Les tests unitaires vont dans `#[cfg(test)] mod tests` en bas du fichier. Les tests
  d'intégration vont dans `crates/<layer>/<crate>/tests/` et n'utilisent que l'API publique. Les tests
  en direct contre des providers sont optionnels (opt-in). **Convention (observée)** ; détails dans
  [03 § 3.9](rust-primer.md#39-tests).
- **Le pur d'abord.** Placez la décision dans une fonction pure et testez-la comme des données. Tout ce
  qui a besoin de vrais namespaces, de cgroups ou d'un holder réseau est validé en direct ou avec
  `scripts/e2e.sh`. **Décidé** : `CONTRIBUTING.md` (« Write a unit test for any new pure function ») ;
  AGENTS.md § « IaC nativo » (`reconcile.rs` est pur pour pouvoir être testé comme des données).
- **Noms** : des phrases en anglais qui énoncent le comportement (voir [§3.4](#34-test-names)).
- **Les tests ne touchent jamais l'état réel de l'hôte.** Donnez aux stores une racine temporaire.
  N'appelez pas de code qui résout la vraie racine d'état. Ne faites pas de `set_var` (le ratchet
  `env_writes`, [§4.1](#41-the-layers-and-the-direction)).
  **Décidé** : AGENTS.md § « IaC nativo », la note sur la fusion de `ShareVolume` (« Nota de método: um
  teste que chamasse `apply_share` … escreveria no estado REAL da máquina »). **Convention
  (observée)** : les tests de `delonix-state/src/store.rs` utilisent un helper `tmp_dir(tag)`.
  Pour les exécutions manuelles et E2E, isolez **à la fois** `DELONIX_ROOT` et
  `DELONIX_NET_RUNTIME_DIR`. N'en isoler qu'un est pire que n'en isoler aucun (AGENTS.md §
  « Meia-isolação é pior que nenhuma » ; [Cloner, compiler et tester](build-and-test.md#isolating-the-engines-state)).
- **Un test de régression doit échouer lorsque la correction est annulée.** Annulez la correction,
  constatez l'échec du test, puis rétablissez la correction. Un test qui passe dans les deux cas ne
  prouve rien, et AGENTS.md en consigne plusieurs (une vérification de code de sortie que `1` ne
  permettait pas de distinguer ; un scénario de chaos resté au vert malgré une annulation).
  **Décidé** : AGENTS.md, « verificado pela regra do repo » partout, par exemple § « IaC nativo »
  (`stack_converge`) et § « A bateria mede o `--help` ».
- **Testez le chemin qu'utilise la production.** Si la production passe des chemins relatifs, le test
  utilise des chemins relatifs. Un test peut encoder le bug. **Décidé** : AGENTS.md § « Auditoria
  sistemática dos 208 subcomandos » (`default_project_name`).
- **Les bugs de concurrence ont droit à une vraie course.** Utilisez des threads plus une attente
  explicite (sleep) à l'intérieur de la fenêtre critique. **Convention (observée)** :
  `delonix-state/src/store.rs:jsonstore_update_concorrente_nao_perde_escritas`.
- **Préférez les propriétés au timing échantillonné.** Lorsqu'une course ne peut être
  qu'échantillonnée, générez de la charge et répétez. **Décidé** : AGENTS.md § « Um `exec` logo a
  seguir ao `run -d` corria no HOST ».
- **Les régions générées ne contiennent aucun décompte volatil** (lignes, tests, commits). **Décidé** :
  docstring de `scripts/dev_docs.py` (« Deliberately NOT generated: line counts, test counts, commit
  counts »). Une **mesure écrite à la main** cite la valeur mesurée avec la date de la mesure, jamais un
  total courant. **Décidé** : AGENTS.md § « A bateria mede o `--help` de tudo e EXECUTA um quarto »
  (« Cita-se a fracção medida e a data, nunca o total »). La question de savoir si des décomptes peuvent
  apparaître dans la prose ou dans les commentaires du code est **non décidée** — suivez le texte
  environnant.

---

## 10. Commentaires et documentation

- **Les commentaires expliquent le *pourquoi*, pas le *quoi*.** Écrivez-en un pour une contrainte
  cachée, un contournement d'un bug précis, ou un invariant que le code ne rend pas évident. Les
  commentaires qui reformulent le code sont supprimés en revue. **Décidé** : `CONTRIBUTING.md` § Style.
- **Lorsqu'une décision a été mesurée, dites ce qui a été mesuré.** « Measured: … » vaut mieux que
  « should ». La documentation de module de `exitcode.rs` et de `reconcile.rs` sert de modèle.
  **Convention (observée)**.
- **Les commentaires de documentation (`///`, `//!`) sur les éléments publics et en tête de module**
  disent ce que l'élément promet et pourquoi il existe. **Convention (observée)** : chaque port de
  `delonix-compute/src/ports.rs`, `delonix-state/src/store.rs:write_private_temp`, `exitcode.rs`. **Non décidé** : aucun
  lint `missing_docs` n'est activé.
- **Les commentaires sont en anglais et ne nomment aucun consommateur.** Le ratchet de langue et le gate
  des consommateurs analysent tous deux les commentaires. **Imposé (gate)**.
- **N'ajoutez pas d'abstractions, de flags de configuration ni de gestion d'erreurs pour des cas qui ne
  peuvent pas se produire.** **Décidé** : `CONTRIBUTING.md` § Style.
- **Lorsque vous déplacez une frontière, mettez à jour ses traces dans la même modification** :
  - un ADR **avant** le code, pour un nouveau provider, un port, un daemon, une dépendance externe dans
    un crate du moteur, une frontière de privilège, ou une modification du contrat ou des couches. Les
    ADR acceptés ne sont jamais réécrits ; un nouvel ADR les remplace (**Décidé** : `docs/adr/README.md` ;
    [Flux de contribution](contributing-workflow.md#when-to-write-an-adr)) ;
  - la section de `AGENTS.md` qui décrit la zone. Une section obsolète à cet endroit induit en erreur la
    personne suivante, et **les conflits dans `AGENTS.md` se résolvent en gardant les deux côtés**
    (**Décidé** : AGENTS.md § « Método: um worktree por sessão ») ;
  - les artefacts générés : `docs/schema/v1/delonix.json`, `docs/api/openapi.yaml`, la base de
    référence de la CLI, `docs/gen.py`, et les régions générées de `docs/dev/` via
    `python3 scripts/dev_docs.py` (**Imposé (gate)** ; voir
    [Publier la documentation](publishing-docs.md)).

---

## Non décidé

Rien dans le dépôt ne tranche ces points. Suivez le code environnant et mentionnez votre choix dans
votre PR :

- **Quand `#[allow(clippy::…)]` est acceptable.** Il est utilisé (`too_many_arguments`) sans politique
  écrite.
- **Le type `Result` des ports de calcul actuels.** Les ports de
  `delonix-compute/src/ports.rs` renvoient `delonix_runtime_core::Result`, donc les adaptateurs qui les
  implémentent (`HostWorkload`, `HostNetwork`, …) importent le type de résultat partagé, et ces imports
  comptent dans `shared_error_imports`. P3 décide des erreurs par crate. Aucun document ne dit comment
  la signature d'un port change, et ajouter un tel import dans un nouveau fichier fait échouer le
  ratchet. Si votre modification en a besoin, soulevez la question dans la PR. Ne contournez pas
  l'expression régulière.
- **Les noms de crates pendant la transition.** L'ADR-0040 D2 (encore Proposed) fixe les noms cibles, mais un crate entièrement nouveau
  qui arrive avant que son contexte existe (par exemple un second provider avant les renommages
  `delonix-provider-*`) n'a aucune règle écrite. Demandez d'abord dans l'issue.
- **Les commentaires de documentation obligatoires.** Il n'y a pas de lint `missing_docs`, seulement
  l'habitude observée.
- **Le test de conformité sur la correspondance des noms de provider** (ADR-0040 D3, règle 3) est
  proposé mais pas encore implémenté.

---

## Liste de vérification de revue

Avant d'ouvrir la PR, parcourez la liste :

1. `cargo fmt --all --check` et `cargo clippy --workspace --all-targets --locked -- -D warnings`
   sont propres. → [§1](#1-the-tooling-that-enforces-style)
2. `python3 scripts/lang_ratchet.py` et `python3 scripts/arch_fitness.py` passent, et toute base de
   référence que vous avez abaissée figure dans ce commit. → [§1](#1-the-tooling-that-enforces-style)
3. Les nouveaux identifiants, commentaires, messages **et noms de tests** sont en anglais. Le texte
   utilisateur passe par `po::t`/`po::tf`, avec des entrées dans `pt.po`. Pas de `num`. →
   [§2](#2-language-of-the-code), [§3.4](#34-test-names)
4. Le code est dans le bon crate et la bonne couche, les versions ne sont qu'à la racine, la
   bibliothèque n'a pas de `println!` et ne réexécute pas son propre binaire. → [§4](#4-structure-where-code-goes)
5. Les décisions sont des fonctions pures avec des tests unitaires, et les E/S restent aux bords. →
   [§4.3](#43-pure-core-io-at-the-edges)
6. Les nouveaux backends implémentent un port, sans correspondance sur les noms de provider. Les
   adaptateurs ne dépendent pas d'adaptateurs. → [§5.1](#51-ports-and-adapters)
7. Les échecs d'adaptateur/provider utilisent un `Error` de crate qui se convertit en
   `delonix_model::Error`. → [§5.2](#52-errors-per-crate-adr-0040-p3)
8. Si vous avez touché `proto/` : `scripts/contract_gate.py` passe, avec une requête par RPC, une
   identité explicite et un OpenAPI régénéré. → [§5.4](#54-the-node-contract)
9. Aucun nom de consommateur nulle part dans `crates/`, `bins/` ou `proto/`, commentaires compris. →
   [§5.5](#55-the-engine-knows-no-consumer)
10. Rien n'est accepté puis ignoré. Les erreurs énoncent le fait puis la correction, et utilisent la
    variante de la bonne classe. → [§6](#6-error-handling-and-messages), [§3.8](#38-exit-codes-and-dx_-codes)
11. Chaque bloc `unsafe` a un commentaire `// SAFETY:`. Il n'y a pas de fork brut dans un processus
    multithread, les fichiers temporaires utilisent `write_private_temp`, et les secrets utilisent
    `write_atomic_mode`. → [§7](#7-unsafe-syscalls-and-processes)
12. Les mutations d'enregistrements passent par `update`, et les nouveaux champs d'enregistrement ont
    `#[serde(default)]`. → [§8](#8-state-and-concurrency)
13. Les tests utilisent des racines isolées, le test de régression échoue lorsque la correction est
    annulée, et un nombre mesuré porte sa date. → [§9](#9-tests)
14. Modifications de la CLI : tous les points d'entrée sont câblés, les verbes sont alignés sur Docker,
    les coupures n'ont pas d'alias, et la base de référence de la CLI est mise à jour. →
    [§3.5](#35-cli-commands-and-flags)
15. Kinds et champs de manifeste : une ligne dans `kinds.rs`, des champs en `camelCase` avec les
    anciennes graphies en alias, et le schéma régénéré. → [§3.6](#36-kinds-api-groups-and-manifest-fields)
16. L'ADR, `AGENTS.md` et la documentation générée sont mis à jour si une frontière a bougé. →
    [§10](#10-comments-and-documentation)
