<!-- translated-from: adding-a-kind.md sha256:390b9b778fbc57a84fa46c20c5fbddf6e29ec5a6b7b476e1ed05169ecad6a056 -->
# Ajouter un Kind

**À lire avant :** [Conventions de code](coding-conventions.md), [Architecture](architecture.md) et [Les crates](crates.md#delonix-stack) — cette page suppose que vous savez ce que possède `delonix-stack` et pourquoi la planification est pure.

Un Kind est une ressource déclarative que ce moteur connaît — `Container`, `Volume`, `Service`,
et ainsi de suite (`delonix api-resources` les liste tous). En ajouter un touche à plus qu'un
simple bras de `match` : la table qui décrit ce qu'EST le Kind, le code qui l'applique, et le
câblage du réconciliateur qui permet à `stack plan`/`apply` de le traiter comme n'importe quel
autre Kind. Cette page parcourt cela dans l'ordre, avec un Kind réel — **`Service`** (ADR-0032) —
comme exemple travaillé de bout en bout. Ce n'est pas le Kind le plus récent (`IPPool`,
`NetworkGateway`, `NetworkZone` et `RuntimePolicy` sont venus après), mais c'est celui qui
parcourt tous les chemins à la fois : primaire, convergent, supprimable, avec namespace et doté
de son propre registre. Chaque fichier cité ci-dessous est lu depuis l'arbre, pas depuis le
souvenir d'une ancienne disposition.

## La table unique à laquelle un Kind doit répondre

`crates/contexts/delonix-stack/src/kinds.rs` est la source unique de ce qu'est un Kind. Le
commentaire du module lui-même explique pourquoi elle existe : avant cette table, les mêmes faits
sur un Kind vivaient dans six listes séparées (`KINDS`, `CONVERGING_KINDS`, `TEARDOWN_KINDS`,
`kind_honors_namespace`, les `DECLARATIVOS` du test du wait, et les branches de `presence()`), et
elles divergeaient — `Vm`, `FirewallPolicy` et `ShareVolume` ont un jour reçu un adaptateur de
réconciliateur et sont restés HORS de `CONVERGING_KINDS`, donc l'étape de convergence les sautait
silencieusement pendant que leur propre `apply` maintenait la ressource apparemment correcte, mais
par le mauvais chemin.

Ajouter un Kind commence par une ligne `KindFacts` :

```rust
pub const SERVICE: &str = "Service";
```

```rust
KindFacts {
    kind: SERVICE,
    plural: "services",
    short: &["svc"],
    api_version: "networking.delonix.io/v1alpha1",
    domain: Domain::NetConnectivity,
    form: Form::Primary,
    in_stack: true,
    stack_group: "services",
    converges: true,
    teardown: true,
    namespaced: Namespaced::Always,
    presence: Presence::Registry,
},
```

Chaque champ est une décision, pas une formalité :

| Champ | Question à laquelle il répond | Se tromper |
|---|---|---|
| `kind` | Le nom, comme `const` — jamais un littéral de chaîne isolé. Un renommage touchant chaque site d'appel a été mesuré à **106 occurrences dans dix fichiers** avant que cette table n'existe ; un `const` transforme une faute de frappe en erreur de compilation plutôt qu'en bras de `match` silencieusement inatteignable (un motif `&'static str` mal orthographié dégénère en un *binding* attrape-tout, ce que le commentaire même de `SECRET`/`NETWORK`/etc. dans la table nomme explicitement). | Un second Kind ne répondant à personne, ou un renommage qui manque un site sans aucune erreur de build. |
| `plural` | Le mot qu'accepte `delonix get <plural>`. Champ propre, pas `kind` + `"s"` — `Dependency`→`dependencies` et `Ingress`→`ingresses` ne suivent pas cette règle. | `delonix get services` (correct) contre un `delonix get servicess` deviné. |
| `short` | Les abréviations acceptées. Volontairement rares — un test impose l'unicité globale, donc une abréviation qui entre en collision avec un autre Kind fait échouer le build plutôt que de l'occulter silencieusement. | Deux Kinds répondant aux mêmes trois lettres. |
| `api_version` | Le `apiVersion` qu'écrit un manifeste, réparti par domaine (`compute.delonix.io/…`, `networking.delonix.io/…`, …) depuis l'ADR-0020. Une colonne, pas une constante partagée, précisément pour que les Kinds puissent migrer vers le schéma réparti un par un. | Un Kind coincé sur la chaîne héritée `delonix.io/v1`, ou dans le mauvais groupe. |
| `domain` | Le domaine d'action, affiché dans la colonne `DOMAIN` de `stack ls`/`plan --fields`. Les trois de réseau sont séparées volontairement : `NetConnectivity` répond « un chemin existe-t-il », `NetPolicy` répond « le trafic y est-il permis » — les fusionner cacherait que `NetworkRoute` ouvre un chemin tandis que `FirewallPolicy` décide de laisser le trafic le traverser. | Un domaine qui répond à une question différente de celle sur laquelle le Kind agit réellement. |
| `form` | Ce que devient un document de ce Kind : `Primary` (son propre apply, survit au load), `Sugar(cible)` (réécrit vers un autre Kind au moment du load, disparaît), `Aggregate` (se développe dans les documents qu'il contient, comme `Stack`), `Compat(cible)` (un schéma étranger — `Ingress` est `networking.k8s.io/v1` — compilé sur le mécanisme d'un autre Kind, et contrairement à `Sugar` il *survit* au load), ou `Sunset(cible)` (toujours primaire, survit au load, mais un successeur est annoncé ; utilisé quand réécrire changerait silencieusement ce que le moteur *fait* — `Container` ne peut pas être abaissé en `Pod` à un seul membre car un Pod construit toujours un netns partagé, ce qui est une forme d'exécution différente). | Choisir `Sugar` pour quelque chose qui doit garder son propre apply, ou l'inverse. |
| `in_stack` | Si `stack apply` s'en occupe du tout. **Les lignes avec `in_stack: true` doivent rester un préfixe contigu de la table** — `destroy` dérive son ordre de démontage en inversant l'ordre du stack, donc une ligne placée après un Kind hors stack change l'ordre d'apply sans que personne n'édite un « ordre » nulle part. Un test (`os_kinds_do_stack_sao_um_prefixo_contiguo`) impose ceci. | Un Kind appliqué hors de l'ordre de dépendance, ou un Kind de procédure distante comme `KubernetesCluster` (SSH contre des hôtes qui existent déjà, pas une ressource locale) câblé par erreur dans le cycle. |
| `converges` | Si un champ *modifié* est réellement appliqué, par opposition à « garantir présent » seulement. `false` est légitime — l'état de `Secret` ce sont des valeurs chiffrées qu'un plan ne déchiffrera pas pour comparer — mais il faut une raison (voir `not_converged_reason` ci-dessous) ; une excuse générique fait échouer un test. | Un Kind qui rapporte `!` sur chaque plan avec une raison qui se lit comme « personne n'y est arrivé » alors que la vérité est une propriété de la ressource. |
| `teardown` | Si `destroy_one` peut le retirer, pour que `--prune` et `destroy` puissent le promettre. Un test (`so_um_kind_convergente_tem_teardown`) refuse un Kind avec `teardown: true` et `converges: false` — promettre de purger quelque chose que le plan ne peut même pas représenter comme modifié. | `--prune` promettant un retrait et `destroy_one` refusant à mi-course, après que des Kinds antérieurs dans l'ordre de démontage aient déjà disparu. |
| `namespaced` | `Never`, `Always`, ou `PerDocument`. Pas un `bool` — `Volume` a réellement trois réponses : aucune pour un volume simple, réelle pour un avec un bloc `share:` ; le modéliser comme `true` avertirait « namespace sans effet » sur chaque volume ordinaire, et comme `false` avertirait la même chose, à tort, sur un partage dont le namespace décide dans quel répertoire vivent ses données. | Un avertissement de namespace qui contredit ce que fait réellement le propre `apply` du Kind avec le champ. |
| `presence` | Comment `stack ls`/`wait` apprennent si la ressource est là : `Registry` (un store répond oui/non), `Derived` (calculé à partir d'autre chose — un Pod est ses membres étiquetés), `Declarative` (rien à relire ; la ressource est une directive appliquée à une cible, et `presence()` répond `-`, ce qui n'est *pas* « absent »), ou `NotObservable` (n'atteint jamais `presence()` — ne survit pas au load, ou n'est pas du tout une ressource locale). | `NetworkRoute` n'avait autrefois aucune branche dans `presence()` et tombait dans `_ => ("?", "unsupported kind")` — affiché par `ls`/`describe`, et lu par `wait` comme éternellement en attente. |

La ligne de `Service` se lit, en une phrase : appliqué par le stack, juste après les Kinds de
calcul qu'il sélectionne ; a un chemin réel (`NetConnectivity`) ; primaire ; converge sans
recréer ; peut être démonté ; toujours namespaced ; et un vrai registre est derrière.

## Le type de spec et le schéma

Un Kind avec une spec de manifeste typée (la plupart) a besoin d'une struct
`#[derive(Deserialize, Serialize, JsonSchema)]` — `ServiceSpec` pour cet exemple, dans
`bins/delonix-runtime-bin/src/cmd/service.rs` — et d'une branche dans `TYPED_KINDS`, dans
`bins/delonix-runtime-bin/src/cmd/schema.rs`. Cette constante alimente `delonix manifest schema` et
`delonix explain <Kind>.<champ>`, tous deux générés depuis la même struct (ADR-0007), afin que le
schéma publié ne puisse jamais diverger de ce que le code accepte réellement. Laisser un Kind de
côté est un état réel et autorisé — `Storage` et `ShareVolume` n'ont pas de schéma exprès, car ils
sont réécrits vers `Volume` au load et une seconde struct ne serait qu'une copie entretenue à la
main des champs que possède déjà `Volume` — mais cela doit être *dit* : `untyped_hint(kind)`
donne la redirection spécifique, et `todo_kind_conhecido_tem_schema_ou_dica` (dans `schema.rs`)
fait échouer le build si un Kind que la table connaît n'est ni dans `TYPED_KINDS` ni doté d'un
indice. Le message générique (« no typed schema for X ») se lit comme un bug du manifeste ;
l'indice dit que c'est une propriété du Kind.

## Câbler le Kind dans le réconciliateur

Le plan du réconciliateur (`crates/contexts/delonix-stack/src/reconcile.rs::plan`) est pur — il
n'ouvre jamais un store ni n'exécute une commande. Tout ce qui touche la machine vit dans
`bins/delonix-runtime-bin/src/cmd/stack.rs`, ré-exporté comme `cmd::kinds`/`cmd::reconcile` depuis
`delonix-stack` (`pub use delonix_stack::kinds;` dans `cmd/mod.rs`, donc rien de ce qui appelait
déjà `cmd::kinds::…` n'a eu à changer quand la table est passée dans son propre crate). Quatre
fonctions dans `stack.rs` ont besoin d'une branche pour un nouveau Kind qui converge :

1. **`desired_of`** — un `reconcile::Desired` par document, avec `fields` indexé par le nom de
   champ **du manifeste** (`matchLabels`, `port`), jamais par le nom d'enregistrement interne,
   car le diff est lu par qui a écrit le YAML. `Service::desired` construit cela à partir de
   `ServiceSpec`.
2. **`actual_of`** — chaque instance du Kind existant sur la machine, pour que `--prune` ait
   quelque chose à comparer. `Service::actual` lit `delonix_sdn::infra::service_list()` et
   remplit les *mêmes* noms de champs que ceux utilisés par `desired`, sinon le diff compare des
   choux et des carottes.
3. **`converge_and_stamp`** — applique un `Action::Update` à chaud. `Service` réutilise ici son
   propre `apply_one` (`converge_doc`), car cette fonction écrase déjà entièrement l'entrée du
   registre — la même forme qu'utilisent `FirewallPolicy` et `NetworkAccessRule`, et le même
   raisonnement : un chemin séparé par champ serait une seconde façon d'écrire le même
   enregistrement, et deux façons finissent par diverger. Un Kind sans chemin de mise à jour à
   chaud du tout (`Pod` n'a aucun champ chaud) renvoie l'erreur explicite « no live update path »
   plutôt que de tomber en silence.
4. **`stamp_all`** — enregistre la possession (étiquette `delonix.io/stack`) et la carte de
   champs appliquée (annotation `delonix.io/last-applied`) après un apply réussi, ce qui est ce
   qui transforme le prochain plan en diff à trois voies plutôt qu'à deux. `Service::stamp`
   écrit cela directement sur la propre entrée de registre du service — contrairement à
   `NetworkAccessRule`, dont la règle vit sur un *container* qu'il ne possède pas, un `Service` a
   un enregistrement entièrement à lui.

Et `destroy_one` a besoin d'une branche appelant `remove_for_replace` quand `teardown: true` —
`Service::remove_for_replace` appelle simplement `delonix_sdn::infra::service_remove`.

**Rien de tout cela n'est inventé par Kind.** `run_layers` (aussi dans `stack.rs`) est l'endroit
où un document est réellement *créé* la première fois, une couche par Kind `in_stack`, dans
l'ordre de la table (`layers.run(k::SERVICE, "🧭", || super::service::apply(docs))?`) — cette
fonction `apply(docs)` est la même que possède déjà le groupe impératif de la CLI, réutilisée
plutôt que dupliquée.

## Champs qui convergent sans recréer

Si `converges: true`, ajoutez une entrée à `hot_fields` dans `reconcile.rs` nommant exactement
quels champs du manifeste peuvent changer *à chaud*. Cette table est une promesse que l'exécuteur
doit tenir — lister un champ que l'étape de convergence ne peut pas réellement appliquer
transforme un `Replace` propre en un `Update` qui échoue à mi-course, ce que le commentaire du
module appelle « strictement pire que de déclarer le replace d'emblée ». `Service` liste
`["matchLabels", "port"]`, car `service::apply_one` écrase déjà entièrement l'enregistrement sans
qu'aucun redémarrage ne soit nécessaire — rien dans un `Service` n'est froid. Un champ laissé hors
de `hot_fields` apparaît quand même dans le plan ; il force simplement `Action::Replace`
(refusé sans `--replace <Kind>/<nom>`) plutôt que `Action::Update`.

Trois tests dans `cmd/stack.rs` maintiennent cette table honnête vis-à-vis de la table de
`kinds.rs` et entre eux :

- **`as_tres_listas_de_kinds_convergentes_concordam`** — chaque Kind marqué `converges: true`
  doit apparaître dans la sortie `--fields` (`compared_fields_table`), et vice-versa ; et un
  Kind convergent ne peut jamais porter l'excuse générique de `not_converged_reason`.
- **`todo_kind_nao_convergente_tem_razao_especifica`** — l'inverse : chaque Kind qui ne
  converge PAS a besoin de sa propre phrase concrète dans `not_converged_reason`, jamais le
  `NOT_CONVERGED_GENERIC` partagé.
- **`todo_kind_convergente_tem_teardown_ou_razao`** — un Kind convergent est soit retirable
  (`kinds::has_teardown`), soit possède une entrée dans `no_teardown_reason` ; jamais les deux,
  jamais aucun.

## Présence, pour `ls`/`describe`/`wait`

Ajoutez une branche à `presence(kind, doc, containers)` dans `stack.rs` qui indique si une
instance existe et, le cas échéant, sa chaîne de statut. La branche de `Service` vérifie
`delonix_sdn::infra::service_get(namespace, nom)` et, si trouvée, compte combien de containers
vivants son sélecteur atteint en ce moment (en lisant le *même* index que lit le résolveur DNS
lui-même, pour que le plan ne puisse jamais être en désaccord avec ce que reçoit réellement un
client demandant le nom). Deux tests protègent spécifiquement cette colonne :

- **`todo_o_kind_declarativo_de_kinds_tem_braco_no_presence`** — chaque Kind dont `presence` est
  `Declarative` répond bien `-` depuis `presence()` (pas le repli `"?"`/« unsupported kind » dans
  lequel tombait `NetworkRoute` avant de gagner son propre registre).
- **`um_kind_declarativo_nao_fica_pendente_para_sempre`** — `is_pending("-", kind, statut)` vaut
  `false` pour chaque Kind déclaratif. `stack wait` lisait autrefois `present == "yes"` comme
  seul signe de disponibilité, donc un manifeste avec *n'importe quel* Kind déclaratif épuisait
  tout le `--timeout` en attendant un marqueur que ce Kind ne peut jamais produire.

Si le Kind est namespaced (`namespaced != Namespaced::Never`), il lui faut aussi une entrée dans
`NAMESPACE_SOURCES` (`bins/delonix-runtime-bin/src/cmd/complete.rs`) — soit `NsSource::Store(fn)`
lisant le propre store du Kind, soit `NsSource::Via("AutreKind — raison")` quand le namespace
voyage vers l'enregistrement d'un autre Kind (le namespace d'un `Pod` se trouve sur ses
`Container`s membres, par exemple). `every_namespaced_kind_declares_a_source` échoue sinon —
l'effet pratique d'oublier cela est que l'autocomplétion du shell ne peut jamais proposer un
locataire dont la seule ressource serait le nouveau Kind.

## La checklist

Pour un Kind qui se comporte comme `Service` (primaire, converge, a un démontage, namespaced) :

1. `kinds.rs` — un `pub const` nommé, et une ligne `KindFacts`.
2. Une struct de spec avec `JsonSchema`, et une branche dans `TYPED_KINDS` (`schema.rs`) — ou
   une entrée dans `untyped_hint` expliquant pourquoi pas.
3. `stack.rs` — des branches dans `desired_of`, `actual_of`, `converge_and_stamp`, `stamp_all`,
   `destroy_one`, `presence`, et une couche dans `run_layers` appelant le propre `apply(docs)` du
   Kind.
4. `reconcile.rs` — une entrée dans `hot_fields` nommant les champs qui convergent à chaud.
5. Si le Kind ne converge pas ou ne peut pas être démonté : une phrase spécifique dans
   `not_converged_reason` / `no_teardown_reason` (`stack.rs`).
6. Si namespaced : une entrée dans `NAMESPACE_SOURCES` (`complete.rs`).
7. `cargo test -p delonix-runtime-bin -p delonix-stack` — les tests nommés ci-dessus sont ce qui
   attrape une étape sautée, pas un relecteur lisant le diff à l'œil.

Un Kind qui est `Sugar`/`Aggregate` (réécrit ou développé au load, comme `Workload` ou `Stack`)
saute la plupart de tout cela : `um_kind_que_baixa_para_outro_nao_pertence_ao_ciclo_do_stack`
exige `in_stack: false` et `converges: false` pour ces formes, et le travail vit alors dans
`manifest::load`, là où la réécriture se produit.

---

**Ensuite :** [Flux de contribution](contributing-workflow.md) — worktrees, alignement de version, la règle de langue, quand écrire un ADR, et comment envoyer le changement.
