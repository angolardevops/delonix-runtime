<!-- translated-from: rust-primer.md sha256:1816db1c294bfdfdb88795ccb366f48135a04bbd7d7d37815302e6d006a262c8 -->
# Initiation à Rust pour cette base de code

**Avant de lire :** [Initiation au cloud native](cloud-native-primer.md), dont les exemples utilisent le vocabulaire, et des bases de Rust ([The Rust Programming Language](https://doc.rust-lang.org/book/), chapitres 1 à 10).

Ce n’est pas un tutoriel Rust. C’est le sous-ensemble de Rust dont vous avez besoin pour *lire ce dépôt*,
chaque idée étant rattachée à un fichier que vous pouvez ouvrir. Si un concept est nouveau pour vous, les liens « En savoir plus »
mènent à la source officielle ; revenez ici pour voir comment le moteur l’utilise. Ensuite, vous pourrez
ouvrir n’importe quel crate du workspace et suivre ses types d’erreur, ses traits, ses blocs `unsafe`
et ses tests sans buter sur le langage.

Les chemins sont relatifs à la racine du dépôt. Les symboles sont nommés pour que vous puissiez les chercher avec `grep` —
les numéros de ligne sont volontairement omis, car ils bougent à chaque PR.

La toolchain épinglée se trouve dans `rust-toolchain.toml` (un `channel` fixe, plus `rustfmt` et
`clippy`). `rustup` la respecte automatiquement, vous compilez donc avec le même compilateur que la CI.

---

## 3.1 Le workspace Cargo

Le dépôt est un seul **workspace** Cargo : un `Cargo.toml` racine avec `[workspace] members = [...]`
et un `Cargo.toml` par crate. Trois conventions comptent ici :

1. **Chaque chemin et chaque version sont écrits une seule fois, à la racine.** La racine a une
   table `[workspace.dependencies]` qui liste à la fois les crates propres au moteur (par `path`) et chaque
   crate tiers (par `version`). Un membre n’écrit jamais de version ; il écrit :

   ```toml
   # crates/contexts/delonix-node/Cargo.toml
   [dependencies]
   delonix-model = { workspace = true }
   serde = { workspace = true }
   serde_json = { workspace = true }
   libc = { workspace = true }
   ```

   Un membre peut ajouter `features = [...]`, et c’est tout. `default-features = false` vit à
   la racine, car un membre ne peut pas désactiver des valeurs par défaut que le workspace a activées.
   `scripts/arch_fitness.py` (fonction `inline_versions`) fait échouer le build si un membre écrit
   sa propre version.

2. **Le répertoire est la couche.** Les crates vivent dans `crates/foundation/`, `crates/contexts/`,
   `crates/adapters/`, `crates/providers/`, `crates/interfaces/` et les binaires dans `bins/`
   (ADR-0040). `scripts/arch_fitness.py` refuse un crate dont le répertoire ne correspond pas à sa
   couche déclarée, ainsi qu’une dépendance qui va à l’encontre de la direction autorisée. Les
   règles de couche sont enseignées plus loin dans le parcours, dans
   [Architecture — Les couches et la direction autorisée](architecture.md#layers-and-the-allowed-direction).

3. **Les lints sont hérités.** La racine déclare `[workspace.lints.clippy]` avec
   `undocumented_unsafe_blocks = "deny"`, et chaque membre y adhère avec `[lints] workspace = true`.
   Chaque bloc `unsafe` porte donc un commentaire `// SAFETY:` (voir §3.4).

La racine définit aussi `[workspace.package]` (la `version`, l’`edition` et la `license` partagées), que
les membres consomment sous la forme `version.workspace = true`. La version n’est pas décorative : voir
[Alignement de version](contributing-workflow.md#version-alignment) pour la règle et
[Les gates exécutés par la CI](build-and-test.md#the-gates-ci-runs) pour le gate de version.

**En savoir plus :** Cargo Book —
[Workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html),
[`[workspace.dependencies]`](https://doc.rust-lang.org/cargo/reference/workspaces.html#the-dependencies-table),
[`[lints]`](https://doc.rust-lang.org/cargo/reference/manifest.html#the-lints-section),
[`rust-toolchain.toml`](https://rust-lang.github.io/rustup/overrides.html#the-toolchain-file).

---

## 3.2 Erreurs : une seule `Error`, et des codes de sortie dérivés de son type

Presque chaque fonction faillible du moteur renvoie `delonix_model::Result<T>`, un alias sur
l’enum partagé défini dans `crates/foundation/delonix-model/src/error.rs`. L’enum vit dans le crate
de fondation pur `delonix-model`, dont peut dépendre n’importe quel autre crate du moteur. Certains
adapters définissent leur propre erreur et la convertissent en celle-ci (§5.2 des
[Conventions de code](coding-conventions.md)). Il est construit avec
[`thiserror`](https://docs.rs/thiserror) : `#[derive(Error)]` génère `Display` à partir de
l’attribut `#[error("...")]`, et `#[from]` génère des impls `From` afin que `?` convertisse automatiquement une erreur
de plus bas niveau :

```rust
// crates/foundation/delonix-model/src/error.rs
#[error("I/O error: {0}")]
Io(#[from] std::io::Error),
```

Les variantes portent sur **ce que l’appelant doit faire ensuite**, pas sur le sous-système qui a échoué :
`NotFound`, `NotRunning`, `Conflict`, `Unavailable` (un outil ou une fonctionnalité du noyau manquant),
`Timeout`, `Invalid`, `Registry`, `Runtime { context, message }` pour un appel système échoué, etc.
Les commentaires de documentation de chaque variante expliquent pourquoi elle existe ; lisez-les avant d’ajouter une variante.

Ces variantes deviennent des **codes de sortie de processus** en un seul endroit :
`crates/foundation/delonix-model/src/exitcode.rs`, fonction `for_error`. La CLI réexporte le
module (`pub use delonix_model::exitcode;` dans `bins/delonix-runtime-bin/src/cmd/mod.rs`) et
`bins/delonix-runtime-bin/src/main.rs` appelle `std::process::exit(cmd::exitcode::for_error(&e))`.

```rust
// crates/foundation/delonix-model/src/exitcode.rs
pub fn for_error(e: &Error) -> i32 {
    match e {
        Error::NotFound(_) | Error::VmNotFound(_) => NOT_FOUND,
        Error::NotRunning(_) => NOT_RUNNING,
        // ...
```

Deux choses à remarquer :

- Le `match` est **exhaustif, sans bras `_ =>`**. Ajouter une variante à `Error` arrête le build
  dans `for_error` (et dans `Error::code` dans `error.rs`) jusqu’à ce que quelqu’un décide de sa classe. C’est un
  usage délibéré du compilateur comme liste de contrôle.
- Les messages sont traduits pour l’opérateur, les scripts doivent donc se brancher sur le code de sortie (ou le
  code machine stable de `Error::code`), jamais sur le texte du message.

**En savoir plus :** The Rust Book —
[Recoverable errors with `Result`](https://doc.rust-lang.org/book/ch09-02-recoverable-errors-with-result.html),
[l’opérateur `?`](https://doc.rust-lang.org/book/ch09-02-recoverable-errors-with-result.html#a-shortcut-for-propagating-errors-the--operator) ;
Rust by Example — [Defining an error type](https://doc.rust-lang.org/rust-by-example/error/multiple_error_types/define_error_type.html) ;
[`thiserror` sur docs.rs](https://docs.rs/thiserror).

---

## 3.3 Les traits comme ports : `VmBackend` et le registre de backends

Le moteur parle aux providers à travers des **traits** (« ports »), et un provider est une implémentation
de l’un d’eux. L’exemple le plus clair est `VmBackend` dans `crates/adapters/delonix-vm/src/lib.rs` :

```rust
pub trait VmBackend {
    fn id(&self) -> &'static str;
    fn available(&self) -> bool;
    fn boot(&self, vmdir: &Path, cfg: &VmConfig, overlay: &str,
            on: &dyn Fn(CreateStage)) -> Result<Boot>;
    fn is_running(&self, vm: &Vm) -> bool;
    fn ip(&self, vm: &Vm) -> Option<String>;
    // ... more methods, many with default bodies
}
```

Implémentations : `CloudHypervisorBackend` et `LibvirtBackend` dans le même fichier, et
`ProxmoxBackend` dans `crates/providers/delonix-proxmox/src/lib.rs`. Les méthodes ayant un **corps
par défaut** dans le trait (par exemple `auto_selectable`) permettent à un nouveau backend d’hériter d’un comportement raisonnable
et de ne redéfinir que ce qui diffère.

Les backends sont choisis à l’exécution, ils sont donc manipulés comme **objets trait**, `Box<dyn VmBackend>`.
Ils sont créés au moyen d’un registre de fabriques :

```rust
// crates/adapters/delonix-vm/src/lib.rs
pub type BackendFactory = Box<dyn Fn() -> Result<Box<dyn VmBackend>> + Send + Sync>;

static BACKENDS: std::sync::OnceLock<std::sync::RwLock<Vec<BackendRegistration>>> =
    std::sync::OnceLock::new();
```

- La fabrique est une **closure** (`dyn Fn`), car un backend distant a besoin d’une configuration
  capturée (endpoint, nœud, identifiant) qu’un simple pointeur `fn()` ne peut pas transporter.
- `Send + Sync` est requis parce que la table est un `static` à l’échelle du processus ; la contrainte porte sur
  la closure, pas sur le trait `VmBackend`.
- `OnceLock` initialise la table paresseusement (`builtin_backends()` y place les deux backends locaux),
  et `RwLock` permet à `register_backend` d’en ajouter un troisième au démarrage. Le binaire le fait dans
  `bins/delonix-runtime-bin/src/cmd/vmbackends.rs` (`register_configured`).

La décision derrière cette forme est l’[ADR-0008](../../adr/0008-proxmox-vm-backend.md). Le même
motif « trait + implémentations + un seul endroit qui choisit » apparaît ailleurs (par exemple le
port `VmNetwork`, conservé dans un `OnceLock<Box<dyn VmNetwork>>` près du début du même fichier).

**En savoir plus :** The Rust Book —
[Traits](https://doc.rust-lang.org/book/ch10-02-traits.html),
[Trait objects](https://doc.rust-lang.org/book/ch18-02-trait-objects.html),
[Closures](https://doc.rust-lang.org/book/ch13-01-closures.html),
[`Send` et `Sync`](https://doc.rust-lang.org/book/ch16-04-extensible-concurrency-sync-and-send.html) ;
documentation std — [`OnceLock`](https://doc.rust-lang.org/std/sync/struct.OnceLock.html).

---

## 3.4 `unsafe`, FFI et appels système Linux

Un moteur de containers, ce sont surtout des appels système. Ce dépôt atteint le noyau à travers trois crates :

| Crate | Utilisé pour | Exemple dans ce dépôt |
|---|---|---|
| [`nix`](https://docs.rs/nix) | Enveloppes plus ou moins sûres : `clone`, `setns`, `unshare`, `pivot_root`, `fork`, `mount`, signaux | `use nix::sched::{clone, setns, unshare, CloneFlags};` dans `crates/adapters/delonix-linux/src/lib.rs` |
| [`libc`](https://docs.rs/libc) | Appels bruts que `nix` n’enveloppe pas, ou lorsque la structure exacte compte | `libc::getsockopt(.., SO_PEERCRED, ..)` dans `crates/contexts/delonix-node/src/peer_cred.rs` (`peer_uid`) ; `libc::flock` dans `crates/adapters/delonix-state/src/store.rs` |
| [`rustix`](https://docs.rs/rustix) | La nouvelle API de montage (`fsopen`/`fsconfig`/`fsmount`/`move_mount`) | `fsopen_overlay` dans `crates/adapters/delonix-linux/src/lib.rs` |

**Chaque bloc `unsafe` indique pourquoi il est correct**, juste à côté (le lint du workspace impose la
présence du commentaire ; les relecteurs imposent sa véracité) :

```rust
// crates/contexts/delonix-node/src/peer_cred.rs
// SAFETY: getsockopt on SO_PEERCRED with a correctly-sized ucred buffer.
let r = unsafe { libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, ...) };
```

### Où naît le container

`fn spawn` dans `crates/adapters/delonix-linux/src/lib.rs` se termine par :

```rust
// SAFETY: single-threaded; the child mounts the container and does `exec`.
let cloned = unsafe { clone(cb, &mut stack, flags, Some(Signal::SIGCHLD as i32)) };
```

L’enfant exécute `container_init`, qui appelle `setup_rootfs` (bind mounts, puis `pivot_root`),
`drop_capabilities`, installe seccomp, et enfin `execvp`. `setns` est la façon dont `exec` entre dans les
namespaces d’un container existant. Les maps uid/gid d’un user namespace sont écrites depuis le
parent (`write_userns_maps`, qui utilise les utilitaires `newuidmap`/`newgidmap` lorsqu’ils sont disponibles).

### Pourquoi `fork`/`clone` dans un processus multi-thread est dangereux

Après `fork` ou `clone`, seul le thread appelant existe dans l’enfant, mais l’enfant hérite de
chaque verrou *dans l’état où il était* — y compris des verrous détenus par des threads qui n’existent plus. Si un autre thread
détenait le verrou de l’allocateur à cet instant, la première allocation de l’enfant bloque pour toujours. `fork`
atténue une partie de ce problème avec les gestionnaires `pthread_atfork` ; un `clone` brut ne les exécute pas.

Ce dépôt a deux règles concrètes à cause de cela, toutes deux expliquées dans des commentaires que vous devriez lire :

- **`serve docker-api` n’appelle jamais `spawn` dans le processus.** Le commentaire de documentation au-dessus de l’utilitaire de ré-exécution
  dans `bins/delonix-runtime-bin/src/cmd/dockerapi.rs` explique que le serveur est un runtime tokio
  multi-thread ; il ré-exécute donc le binaire (`__apirun <spec.json>`) pour obtenir un processus neuf
  mono-thread dans lequel la précondition de `clone` est de nouveau vraie.
- **Après un `fork` brut, uniquement du travail async-signal-safe.** `reexec_mapped` et `reexec_mapped_hold`
  dans `crates/adapters/delonix-linux/src/lib.rs` précalculent chaque allocation *avant* le fork.
  Le commentaire sur `reexec_mapped_hold` consigne aussi pourquoi ils utilisent un `fork` brut au lieu de
  `std::process::Command` + `pre_exec` : `Command::spawn` attend que l’enfant atteigne `exec`, et
  un hook `pre_exec` qui bloque en attendant le parent provoque un interblocage.

### La nouvelle API de montage, et pourquoi elle existe ici

`mount_overlay_if_marked` monte la racine overlay d’un container via `rustix::mount::fsopen` et
un appel `fsconfig_set_string(&fs, "lowerdir+", lower)` par couche. Le `mount(2)` classique regroupe
toutes les options dans une unique chaîne `data` de la taille d’une page, que le noyau tronque silencieusement pour
les images comportant de nombreuses couches. La mesure et la décision se trouvent dans
l’[ADR-0037](../../adr/0037-overlay-mount-new-api.md). C’est un bon exemple de l’habitude du dépôt :
le commentaire `///` au-dessus d’un appel système non évident explique la défaillance qui l’a motivé.

**En savoir plus :** The Rust Book — [Unsafe Rust](https://doc.rust-lang.org/book/ch20-01-unsafe-rust.html) ;
le [Rustonomicon](https://doc.rust-lang.org/nomicon/) (en particulier
[FFI](https://doc.rust-lang.org/nomicon/ffi.html)) ; pages de manuel
[`clone(2)`](https://man7.org/linux/man-pages/man2/clone.2.html),
[`setns(2)`](https://man7.org/linux/man-pages/man2/setns.2.html),
[`pivot_root(2)`](https://man7.org/linux/man-pages/man2/pivot_root.2.html),
[`fork(2)`](https://man7.org/linux/man-pages/man2/fork.2.html),
[`signal-safety(7)`](https://man7.org/linux/man-pages/man7/signal-safety.7.html),
[`fsopen(2)`](https://man7.org/linux/man-pages/man2/fsopen.2.html).

---

## 3.5 Sérialisation : serde, manifestes YAML, schéma généré

L’état sur disque est en JSON ; les manifestes sont en YAML. Les deux passent par
des derives [`serde`](https://serde.rs/).

**Enregistrements rétrocompatibles.** Un enregistrement écrit par un binaire plus ancien doit toujours se charger. La règle
est la suivante : un nouveau champ reçoit `#[serde(default)]`, et lorsque « absent » doit signifier autre chose que la
valeur par défaut du type, il devient une `Option` :

```rust
// bins/delonix-runtime-bin/src/cmd/vmimage.rs  (VmImage)
#[serde(default)]
pub cloud_init: Option<bool>,
```

Ici, `None` (chaque enregistrement écrit avant que le champ n’existe) est lu comme « oui » ; un simple `bool`
aurait valu `false` par défaut et aurait silencieusement changé le comportement des anciennes images. Vous verrez le
même raisonnement dans `crates/contexts/delonix-compute/src/record.rs` : `Mount::propagation` est une
`Option` (`#[serde(default, skip_serializing_if = "Option::is_none")]`), et `Mount::optional` est un
simple `bool` avec `#[serde(default)]`, parce que `false` est ce que signifiait tout enregistrement plus
ancien.

**Manifestes.** `bins/delonix-runtime-bin/src/cmd/manifest.rs` analyse le YAML multi-document avec
`serde_yaml::Deserializer::from_str(text)` vers `ManifestDoc`, dont le `spec` reste une
`serde_yaml::Value` brute jusqu’à ce que le Kind propriétaire le désérialise dans sa spec typée.

**Schéma généré.** Les types de spec dérivent `schemars::JsonSchema` (par exemple `PodSpec` dans
`crates/contexts/delonix-compute/src/pod.rs`), et `bins/delonix-runtime-bin/src/cmd/schema.rs`
génère à partir d’eux le JSON Schema publié ([ADR-0007](../../adr/0007-generated-manifest-schema.md)).
Notez le commentaire à cet endroit : schemars respecte `#[serde(rename)]` mais pas `#[serde(alias)]`.

**En savoir plus :** [serde.rs](https://serde.rs/) —
[attributs de champ](https://serde.rs/field-attrs.html) (`default`, `skip_serializing_if`, `alias`) ;
[`serde_yaml`](https://docs.rs/serde_yaml) ; [`schemars`](https://docs.rs/schemars).

---

## 3.6 La CLI : clap derive et sortie traduite

Le binaire `delonix` est `bins/delonix-runtime-bin`. Son niveau supérieur est un derive
[`clap`](https://docs.rs/clap) dans `src/main.rs` : `#[derive(Parser)] struct Cli` contenant une
option globale `--l18n` et `#[command(subcommand)] cmd: Cmd`, où `enum Cmd` est un
`#[derive(Subcommand)]`. Chaque groupe a son propre module et son propre enum dans `src/cmd/` — par exemple
`pub enum ContainerCmd` dans `src/cmd/container.rs`.

Les commentaires de documentation (`///`) sur les variantes et les champs deviennent le texte de `--help`. Vous verrez
`#[allow(clippy::large_enum_variant)]` sur les enums de commandes, avec un commentaire expliquant pourquoi : ils sont
analysés une fois par invocation, mettre les variantes en boîte n’apporterait donc rien.

**Les chaînes sources sont en anglais ; le portugais vient d’un catalogue.** `src/cmd/po.rs` embarque
`data/pt.po` avec `include_str!` et expose :

- `po::t("already exists, nothing to do")` — une chaîne fixe ;
- `po::tf("port {port} is taken by '{owner}'", &[("port", &hp), ("owner", &ow)])` — un modèle
  avec des espaces réservés **nommés**, car une traduction peut les réordonner ;
- `po::translate_help`, qui réécrit le texte d’aide de clap après que `po::peek_lang` a décidé de la
  langue *avant* l’analyse.

Une nouvelle chaîne destinée à l’utilisateur est en anglais dans le code, plus une entrée dans `data/pt.po`. La politique
de langue (LANG-01) et son gate sont traités dans [Flux de contribution](contributing-workflow.md).

**En savoir plus :** [tutoriel clap derive](https://docs.rs/clap/latest/clap/_derive/_tutorial/index.html) ;
std — [`include_str!`](https://doc.rust-lang.org/std/macro.include_str.html).

---

## 3.7 Async et gRPC — et pourquoi la plus grande partie du moteur est synchrone

Le cœur du moteur est **synchrone** : la CLI démarre, fait son travail avec des appels système bloquants et
des E/S de fichiers, puis se termine. Il n’y a pas de daemon ni de runtime async ambiant. Le code async n’apparaît que dans
les *interfaces* qui servent un protocole :

- **Serveur CRI** — `crates/interfaces/delonix-cri`. Les stubs gRPC sont générés au moment du build :
  `build.rs` appelle `tonic_build::configure()...compile_protos(&["proto/api.proto"], &["proto"])`.
  Cela nécessite que **`protoc`** soit installé (la CI installe `protobuf-compiler`) ; un `protoc` manquant est
  l’échec le plus courant du premier build. `serve_blocking` dans `src/lib.rs` crée son propre
  runtime `tokio::runtime::Builder::new_multi_thread()`, et les gestionnaires déportent le travail bloquant du moteur
  hors des workers async au moyen de l’utilitaire `blocking` dans `src/runtime_svc.rs` — sinon un
  `clone` ou un appel shell à `delonix` bloquerait les workers tokio.
- **Reverse proxy L7** — `bins/delonix-runtime-bin/src/cmd/ingress_proxy.rs` utilise
  [`hyper`](https://docs.rs/hyper) directement (`hyper::service::service_fn`) sur son propre runtime
  tokio.
- **Serveur MCP** — `crates/interfaces/delonix-mcp` utilise [`rmcp`](https://docs.rs/rmcp) sur
  stdio (`rmcp::transport::io::stdio`).
- **Télémétrie** — `crates/adapters/delonix-telemetry/src/telemetry.rs` exporte des spans OpenTelemetry
  depuis un thread dédié avec un client HTTP *bloquant*, précisément pour que la CLI synchrone n’ait pas
  besoin d’un runtime.

Le contrat de nœud sous `proto/delonix/node/v1` est une API protobuf distincte avec son propre gate
(`scripts/contract_gate.py`, `buf`) ; voir [Compiler et tester](build-and-test.md).

**En savoir plus :** [Asynchronous Programming in Rust](https://rust-lang.github.io/async-book/) ;
[tutoriel Tokio](https://tokio.rs/tokio/tutorial) ;
[`tonic`](https://docs.rs/tonic) et [`tonic-build`](https://docs.rs/tonic-build) ;
[installation de Protocol Buffers](https://protobuf.dev/installation/).

---

## 3.8 Concurrence et état partagé

Il n’y a pas de base de données. L’état est constitué de fichiers JSON sous la racine d’état, et **plusieurs processus** (deux
invocations de la CLI, le serveur CRI, un superviseur) peuvent toucher le même enregistrement en même temps. Le motif pour
chaque lecture-modification-écriture est `update` avec une closure, sous un `flock` exclusif :

```rust
// crates/adapters/delonix-state/src/store.rs  (Store::update)
let id = self.load(id_or_name)?.id;
let _lock = FileLock::acquire(&self.lock_path(&id))?;
// Re-read UNDER the lock ...
let mut c = self.load(&id)?;
if !f(&mut c) { return Ok(c); }
self.save(&c)?;
```

`JsonStore<T>::update` dans le même fichier est la version générique pour les autres types d’enregistrement. Tous deux
**refusent** de s’exécuter lorsque le verrou ne peut pas être pris (`Error::Lock`) au lieu de continuer sans verrou.
(`SecretStore::update` dans `secret.rs` est l’exception : son verrou est au mieux (best-effort).) Règles qui
en découlent :

- Ne faites jamais `load` → modification → `save` à la main pour un enregistrement qu’un autre processus peut écrire ; vous
  perdrez des mises à jour. Utilisez `update`.
- Relisez à l’intérieur du verrou. La valeur que vous avez chargée plus tôt est peut-être déjà obsolète.
- `FileLock` libère le verrou dans `Drop` — le motif RAII.

À l’intérieur d’un même processus, l’état partagé utilise les types standard : un
`Arc<std::sync::RwLock<Arc<Vec<Route>>>>` contient la table de routes remplaçable à chaud du proxy
(`SharedRoutes` dans `cmd/ingress_proxy.rs`), et le tableau de bord partage son échantillon lent au moyen d’un
`Arc<Mutex<...>>` (`cmd/dash.rs`).

Une idée apparentée que vous rencontrerez dans `crates/foundation/delonix-model/src/typestate.rs` : le
motif **typestate**, où les états du cycle de vie sont des types et où les transitions illégales ne compilent pas
(ses tests de documentation incluent des exemples `compile_fail`).

**En savoir plus :** The Rust Book —
[Shared-state concurrency](https://doc.rust-lang.org/book/ch16-03-shared-state.html),
[`Drop`](https://doc.rust-lang.org/book/ch15-03-drop.html) ;
[`flock(2)`](https://man7.org/linux/man-pages/man2/flock.2.html) ;
std — [`Arc`](https://doc.rust-lang.org/std/sync/struct.Arc.html),
[`RwLock`](https://doc.rust-lang.org/std/sync/struct.RwLock.html).

---

## 3.9 Tests

- **Les tests unitaires vivent à côté du code**, dans un `#[cfg(test)] mod tests { ... }` en bas du
  fichier. Beaucoup de fonctions pures existent précisément pour qu’une décision puisse être testée comme une donnée (par exemple
  `reconcile::plan` dans `crates/contexts/delonix-stack/src/reconcile.rs`).
- **Les tests d’intégration** vivent dans `crates/<layer>/<crate>/tests/*.rs` et n’utilisent que l’API
  publique du crate. Exemple : `crates/interfaces/delonix-cri/tests/grpc_status.rs` démarre le serveur CRI
  sur un socket Unix et lui parle avec le *client gRPC généré* (c’est pourquoi `build.rs` définit
  `build_client(true)`). Les tests sous `crates/providers/*/tests/live.rs` ont besoin d’un vrai provider et
  sont à activer explicitement.
- **Les tests de propriété** utilisent [`proptest`](https://docs.rs/proptest) : voir
  `crates/adapters/delonix-sdn/tests/ip_invariants.rs` (`proptest! { ... }`).
- **Les tests de documentation** s’exécutent aussi — y compris les blocs `compile_fail` comme ceux de `typestate.rs`.
- **Les benchmarks** utilisent [`criterion`](https://docs.rs/criterion) (`crates/adapters/delonix-oci/benches/`).
- **Noms.** Les noms de tests décrivent le comportement prouvé. Beaucoup de tests existants ont des
  noms en portugais ; les nouveaux identifiants sont en anglais (LANG-01, imposé comme ratchet (cliquet) par `scripts/lang_ratchet.py`).
- **Les tests ne doivent pas toucher l’état réel de l’hôte.** Prenez un répertoire temporaire et passez-le
  (les stores prennent un chemin racine) plutôt que d’appeler du code qui résout la racine d’état réelle.
  Tout ce qui a besoin de vrais namespaces, de vrais cgroups ou d’un holder réseau relève de la validation
  réelle/E2E décrite dans [Compiler et tester](build-and-test.md).

**En savoir plus :** The Rust Book — [Writing tests](https://doc.rust-lang.org/book/ch11-01-writing-tests.html),
[Test organization](https://doc.rust-lang.org/book/ch11-03-test-organization.html) ;
rustdoc — [Documentation tests](https://doc.rust-lang.org/rustdoc/write-documentation/documentation-tests.html).

---

## 3.10 Outillage imposé par la CI

| Outil | Commande CI (depuis `.github/workflows/ci.yml`) | Ce qu’il détecte |
|---|---|---|
| rustfmt | `cargo fmt --all --check` | dérive de formatage |
| clippy | `cargo clippy --workspace --all-targets --locked -- -D warnings` | n’importe quel avertissement, y compris `undocumented_unsafe_blocks` |
| tests | `cargo test --workspace --locked --no-fail-fast` | régressions |
| cargo-deny | `EmbarkStudios/cargo-deny-action` avec `deny.toml` | avis RUSTSEC, licences, sources des crates |

Le dépôt a aussi des gates (contrôles CI) en Python (`scripts/arch_fitness.py`, `scripts/lang_ratchet.py` et d’autres).
La liste complète et la façon d’exécuter chacun localement se trouvent dans [Compiler et tester](build-and-test.md).

**En savoir plus :** [Clippy](https://doc.rust-lang.org/clippy/),
[rustfmt](https://rust-lang.github.io/rustfmt/),
[cargo-deny](https://embarkstudios.github.io/cargo-deny/).

---

**Suivant :** [Préparer votre environnement](environment.md) — un hôte capable de compiler l’arborescence et d’exécuter les chemins réels, et les pièges de l’hôte qui ressemblent à des bugs du moteur.
