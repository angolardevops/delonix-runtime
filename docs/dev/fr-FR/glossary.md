<!-- translated-from: glossary.md sha256:b3639c47dba48ec2f15561c5c42ff3a9e50f62de763f290882cdc887ce8a4991 -->
# Glossaire

Les mots qu’un nouveau venu rencontre dans ce dépôt, avec le sens qu’ils ont **dans Delonix** — qui est
parfois plus restreint que le sens général du cloud native. Chaque entrée renvoie à l’endroit où le terme est
expliqué ou implémenté. Les chemins sont relatifs à la racine du dépôt ; `file.rs::symbol` désigne un
symbole à l’intérieur de ce fichier.

Les termes sont classés par ordre alphabétique. Pour le contexte général (namespaces, cgroups, OCI, CRI, CNI,
KVM), commencez par [Initiation au cloud native](cloud-native-primer.md).

---

**Adapter** — Un crate de `crates/adapters/` qui implémente un port au-dessus d’un mécanisme local : le
noyau (`delonix-linux`), le dataplane réseau (`delonix-sdn`), le magasin OCI (`delonix-oci`), les
hyperviseurs de VM locaux (`delonix-vm`). Un adaptateur peut dépendre des crates de fondation et de contexte, jamais
d’un crate d’interface. Voir : [Les couches](architecture.md#layers-and-the-allowed-direction),
`scripts/arch_fitness.py::LAYERS`.

**ADR (Architecture Decision Record)** — Un fichier Markdown par décision structurelle, dans `docs/adr/`,
nommé `NNNN-title.md`, écrit en anglais, avant le code. Un ADR accepté n’est jamais réécrit ; un
nouvel ADR le remplace. Voir : [`docs/adr/README.md`](../../adr/README.md),
[Quand écrire un ADR](contributing-workflow.md#when-to-write-an-adr).

**Apply / plan / prune** — Les trois verbes de la convergence déclarative. `delonix plan` montre ce qu’un
apply changerait et ne change rien (`--detailed-exitcode` sort avec 2 lorsqu’il y a des changements) ;
`delonix apply` fait converger le manifeste ; `--prune` supprime aussi ce que la stack possède et que le manifeste
ne déclare plus, et ne s’exécute jamais par défaut. Voir : `crates/contexts/delonix-stack/src/reconcile.rs::plan`,
[Réconciliation déclarative](cloud-native-primer.md#48-declarative-reconciliation).

**CAS (content-addressed storage)** — Le magasin de blobs d’images : chaque blob vit sous
`blobs/sha256/<hex>` dans la racine d’état, adressé par son digest, de sorte qu’un contenu identique n’est stocké qu’une fois.
L’intégrité est vérifiée lorsque le contenu **entre** dans le magasin, pas lorsqu’il est lu : un pull compare le
manifeste, la config et chaque couche au digest attendu (`verify_manifest_digest` et les
comparaisons de digest dans `crates/adapters/delonix-oci/src/registry.rs`). `Cas::read` est une simple lecture de fichier
et ne recalcule pas le hash ; `Cas::verify` le recalcule à la demande. Voir :
`crates/adapters/delonix-oci/src/cas.rs::Cas`,
[L’état sur disque](architecture.md#state-on-disk).

**CDI (Container Device Interface)** — Une spécification CNCF qui décrit comment exposer un périphérique (typiquement un
GPU) à un container. Delonix se contente de **consommer** des spécifications déjà générées par un outil du fabricant et les transforme
en ces mêmes montages et nœuds de périphérique que produisent `-v`/`--device` ; il ne découvre jamais les pilotes
lui-même. Voir : `crates/adapters/delonix-linux/src/cdi.rs`.

**cgroup delegation** — Le mécanisme de cgroup v2 qui permet à un utilisateur non privilégié de gérer un sous-arbre de
cgroups. Sans lui, les limites de ressources en rootless (`-m`, `--cpus`) ne peuvent pas être appliquées ; le moteur
le détecte et refuse une limite qu’il ne pourrait pas appliquer plutôt que de l’accepter silencieusement. Un shell
ouvert via SSH n’est souvent pas délégué ; `systemd-run --user --scope -p Delegate=yes` en fournit un qui
l’est. `delonix system info` affiche la réponse sous `cgroup2 delegated`. Voir :
`crates/adapters/delonix-linux/src/lib.rs::cgroup_limits_apply`,
[cgroups v2 et délégation](cloud-native-primer.md#42-cgroups-v2-and-delegation),
[Délégation de cgroup](environment.md#cgroup-delegation-some-limits-are-refused-others-are-not-enforced).

**CNI (Container Network Interface)** — Le standard de plugins que Kubernetes utilise pour donner son réseau à un pod.
Delonix peut exécuter la chaîne de plugins CNI d’un nœud contre un network namespace nommé, et c’est ainsi
qu’un sandbox de pod CRI obtient son réseau. Voir : `crates/adapters/delonix-sdn/src/cni.rs::attach_named_netns`,
[Réseau des containers](cloud-native-primer.md#45-container-networking).

**Contract (node)** — L’API d’un nœud, définie en Protocol Buffers sous `proto/delonix/node/v1/`
(paquet `delonix.node.v1`), avec un document OpenAPI `docs/api/openapi.yaml` généré à partir d’elle.
`scripts/contract_gate.py` surveille le formatage, le lint, la compatibilité avec le dernier tag et
l’OpenAPI généré. C’est un contrat publié ; aucun serveur ne l’implémente encore. Voir :
[Où en est la restructuration](architecture.md#where-the-restructuring-stands),
[ADR-0040](../../adr/0040-engine-restructuring-layers-ports-node-contract.md).

**Control process** — La moitié redémarrable de l’infrastructure réseau rootless : le binaire du moteur lancé avec les
arguments internes `netns control` (qui ne sont pas une commande destinée à l’utilisateur) s’exécute à l’intérieur des namespaces tenus par le pin, écoute sur un socket de contrôle unix `0600` (qui n’accepte que
l’uid du moteur lui-même) et effectue les attachements, les publications, les modifications de pare-feu, le DNS et le DHCP une requête
à la fois. Le tuer ne perturbe pas les workloads en cours d’exécution ; la commande suivante le redémarre. Voir :
`crates/adapters/delonix-sdn/src/infra.rs::start_control`, `control_loop` ; **Holder / pin**.

**CRI (Container Runtime Interface)** — L’API gRPC que le kubelet utilise pour exécuter des pods. Le crate
`delonix-cri` (binaire `delonix-cri`) l’implémente au-dessus du moteur, de sorte qu’un nœud Kubernetes
peut utiliser Delonix à la place d’un autre runtime. Voir : `crates/interfaces/delonix-cri/`,
[Kubernetes](cloud-native-primer.md#46-kubernetes-cri-kubelet-kubeadm-and-kind).

**Daemonless** — Aucun processus résident n’est nécessaire au fonctionnement du moteur : chaque commande de la CLI fait son
travail et se termine. Ce qui doit persister appartient à systemd (unités, timers) ou à un processus par workload
avec un propriétaire clair (le superviseur d’un container, le pin réseau). Un nouveau processus résident exige un ADR.
Voir : *«Identidade e fronteira do motor»* dans [`AGENTS.md`](../../../AGENTS.md),
[Daemonless](cloud-native-primer.md#410-daemonless-in-one-paragraph).

**Delegated cgroup** — Voir **cgroup delegation**.

**`DX_*` exit class** — Chaque erreur du moteur a une chaîne de code stable (`DX_NOT_FOUND`,
`DX_INVALID_ARGUMENT`, …) et correspond à un code de sortie de processus décidé à partir du **type** de l’erreur, jamais
à partir de son message (traduit) : par exemple 4 = ressource introuvable, 5 = conflit. Les scripts et les
réconciliateurs choisissent leur branche d’après le nombre, pas d’après le texte. Voir :
`crates/foundation/delonix-model/src/error.rs::Error::code`,
`crates/foundation/delonix-model/src/exitcode.rs::for_error`,
[Erreurs](rust-primer.md#32-errors-one-error-and-exit-codes-derived-from-its-type).

**Fitness function** — Une vérification automatisée que l’architecture a toujours la forme qui a été
décidée. Ici, il s’agit de `scripts/arch_fitness.py` (job de CI `arch`) : direction des couches, répertoire = couche,
versions des dépendances uniquement à la racine, aucun nom de consommateur dans le code, et les ratchets (cliquets) de dette. Voir :
[Identité et frontières du moteur](architecture.md#engine-identity-and-boundaries),
[Règles d’architecture](contributing-workflow.md#architecture-rules-the-gates-enforce).

**Holder / pin** — La moitié longue durée de l’infrastructure réseau rootless. Le binaire du moteur lancé avec les
arguments internes `netns pin` crée un user namespace, un network namespace et un mount namespace, puis se contente de dormir en les tenant ; son pidfile conserve le
nom historique `holder.pid`, et chaque `nsenter -t <pid>` vers l’infrastructure le vise. Avant la
séparation entre pin et control process, un seul « holder » faisait les deux travaux, ce qui explique pourquoi les deux mots apparaissent dans
le code. Voir : `crates/adapters/delonix-sdn/src/infra.rs::start_pin`, `pin_main`,
`crates/adapters/delonix-sdn/src/pin_userns.rs` ; **Control process**.

**IPAM (IP address management)** — L’allocation des adresses des workloads à l’intérieur du préfixe d’un réseau.
Delonix conserve un fichier de baux par préfixe sous `ipam/` dans la racine d’état, et un collecteur qui ne récupère
un bail qu’après l’avoir vu orphelin deux fois, séparées par une période de grâce. `delonix network ipam ls`
liste les baux. Voir : `crates/adapters/delonix-sdn/src/ipam.rs::allocate`, `reap_orphan_leases` ;
l’arithmétique pure des adresses se trouve dans `crates/foundation/delonix-net-rules/src/lib.rs`.

**Kind** — Le type d’une ressource déclarative dans un manifeste (`kind: Network`, `kind: Pod`,
`kind: VirtualMachine`, …), regroupé par `apiVersion` (`core`, `compute`, `networking`, `gateway`,
`storage`, `artifact`, `infrastructure`). Les faits de chaque Kind — son groupe, s’il est
lié à un namespace, s’il converge, sa forme — vivent dans une seule table. `delonix api-resources` l’affiche ;
`delonix explain <Kind>` documente ses champs. Voir :
`crates/contexts/delonix-stack/src/kinds.rs::FACTS`, `KindFacts`.

**LANG-01** — La règle de langue du code : les identifiants, les commentaires et les messages destinés à l’utilisateur sont écrits
en anglais ; le portugais n’atteint l’opérateur qu’à travers le catalogue de traduction
(`bins/delonix-runtime-bin/data/pt.po`, sélectionné avec `--l18n pt`). `scripts/lang_ratchet.py` compte
le portugais restant dans le code sous forme de ratchet. Voir :
[Langue](contributing-workflow.md#language-english-in-the-code-lang-01).

**Layer (ADR-0040)** — L’un des anneaux architecturaux auxquels appartient chaque crate : fondation, contextes,
adaptateurs, providers, interfaces, binaires. Les dépendances pointent vers l’intérieur, et le répertoire du crate
(`crates/<layer>/`) doit correspondre à sa couche déclarée. À ne pas confondre avec une **couche d’image** (voir
**Overlay / lowerdir**). Voir : [Les couches](architecture.md#layers-and-the-allowed-direction),
[ADR-0040](../../adr/0040-engine-restructuring-layers-ports-node-contract.md).

**Lowering (sugar Kinds)** — La réécriture d’un Kind de commodité en le Kind qui fait réellement le travail
pendant le chargement du manifeste, de sorte que le reste du moteur ne le voit jamais. `Workload` est abaissé en
`Container`/`Pod`/`VirtualMachine`, `Dependency` en `NetworkPolicy`. La colonne `FORM` de
`delonix api-resources` indique ce que devient chaque Kind : `primary`, `sugar → X` (abaissé), `compat → X`
(un schéma étranger conservé mais compilé vers X), `sunset → X` (toujours appliqué en tant que lui-même, successeur
annoncé), `aggregate` (se développe en les documents qu’il contient, comme `Stack`). Voir :
`crates/contexts/delonix-stack/src/kinds.rs::Form`, `bins/delonix-runtime-bin/src/cmd/manifest.rs::load`.

**MCP (Model Context Protocol)** — Un protocole par lequel un client d’IA appelle des outils. `delonix mcp
serve` (crate `delonix-mcp`) est une surface de contrôle **locale**, sans notion de locataire, sur stdio : un processus
au premier plan lancé par le client pour une session, digne de la confiance accordée à l’uid local qui l’exécute — pas une API de
gestion distante. Voir : `crates/interfaces/delonix-mcp/src/lib.rs`,
[ADR-0025](../../adr/0025-mcp-local-ai-control-surface.md).

**microVM** — Une machine virtuelle légère avec un modèle de périphériques minimal, qui démarre vite, utilisée lorsqu’un
workload a besoin de son propre noyau. Dans Delonix, l’hyperviseur de microVM est Cloud Hypervisor ; `libvirt`
(QEMU/KVM) est l’autre backend local. Un `Workload` de `type: microvm` impose le backend
Cloud Hypervisor. Voir : [Construire des microVMs](microvm-setup.md),
[ADR-0006](../../adr/0006-workload-type-microvm.md).

**NoCloud seed** — Une petite ISO contenant les `user-data`, `meta-data`
et `network-config` de cloud-init, attachée à une VM pour que son premier démarrage applique le nom d’hôte, les clés SSH et le réseau.
Delonix en génère une par VM, sauf si l’image est une appliance qui n’exécute pas cloud-init. Voir :
`crates/adapters/delonix-vm/src/cloudinit.rs::generate_seed_iso`,
[Virtualisation](cloud-native-primer.md#47-virtualization-kvm-virtio-cloud-hypervisor-libvirt-cloud-init).

**OCI (Open Container Initiative)** — Les standards pour les images de containers (image spec), pour
leur distribution depuis des registres (distribution spec) et pour leur exécution (runtime spec). Delonix
tire, construit, stocke et pousse lui-même les images OCI. Voir : `crates/adapters/delonix-oci/`,
[Images OCI](cloud-native-primer.md#44-oci-images-content-addressed-storage-and-overlayfs).

**Overlay / lowerdir** — overlayfs empile les couches d’image en lecture seule (les `lowerdir`) sous un répertoire
`upper` inscriptible propre à chaque container. Delonix décompresse chaque couche d’image une seule fois sous `layers/` et tous les
containers de cette image les partagent ; le répertoire du container contient `upper/`, `work/`, `merged/` et
un fichier `overlay-lowers` qui liste les couches. Le montage est effectué à l’intérieur du propre user
namespace du container avec la nouvelle API de montage, un appel `lowerdir+` par couche, de sorte que les images comportant de nombreuses couches n’atteignent pas
la limite de longueur des options du `mount(2)` classique. Voir :
`crates/adapters/delonix-oci/src/overlay.rs::prepare_overlay`,
`crates/adapters/delonix-linux/src/lib.rs::mount_overlay_if_marked`,
[ADR-0037](../../adr/0037-overlay-mount-new-api.md).

**Port (hexagonal)** — Un trait dont un cas d’usage a besoin et qu’un adaptateur ou un provider implémente, de sorte que
le domaine ne nomme jamais un mécanisme concret. Exemples : `VmBackend`, et les ports de calcul
`ImageStore`, `StorageProvider`, `NetworkProvider`, `WorkloadRuntime`. Voir :
`crates/contexts/delonix-compute/src/ports.rs`, `launch.rs`,
[Les traits comme ports](rust-primer.md#33-traits-as-ports-vmbackend-and-the-backend-registry).

**Provider** — Un crate de `crates/providers/` qui implémente un port au-dessus d’**une API de gestion
distante** (aujourd’hui Proxmox VE et TrueNAS), en apportant son propre client HTTP. Un nouveau provider entre sous la forme d’une
implémentation d’un port, enregistrée à la racine de composition — jamais sous la forme de `if provider == …` dans le
code — et exige un ADR. Voir : [Providers](crates.md#providers),
[ADR-0008](../../adr/0008-proxmox-vm-backend.md), [ADR-0009](../../adr/0009-truenas-storage-provisioner.md).

**Ratchet** — Un gate (contrôle CI) sur un compteur de dette qui échoue lorsque le nombre **augmente** et aussi lorsqu’il
**diminue** sans que la ligne de base commitée ait été abaissée dans le même commit, de sorte que le progrès est consigné
et jamais perdu. `scripts/lang_ratchet.py` (le portugais dans le code) et les ratchets de dette de
`scripts/arch_fitness.py` fonctionnent ainsi ; tous deux ont `--list` et `--update`. Voir :
[Règles d’architecture](contributing-workflow.md#architecture-rules-the-gates-enforce).

**Reconcile (3-way)** — La façon dont `plan`/`apply` décident quoi changer sans fichier d’état. Les trois
côtés sont le manifeste (l’état désiré), ce qui est observé sur le nœud (l’état réel), et la dernière spec appliquée,
stockée sur la ressource elle-même dans l’annotation `delonix.io/last-applied`. Le troisième côté distingue
« vous avez retiré ce champ du fichier » (on le rétablit) de « quelqu’un l’a défini à la main » (on n’y touche pas).
Voir : `crates/contexts/delonix-stack/src/reconcile.rs`,
[Le réconciliateur déclaratif](system-design-interview.md#55-the-declarative-reconciler-without-a-state-file).

**Rootless** — L’exécution sous un utilisateur non privilégié, avec des privilèges uniquement à l’intérieur des user namespaces que le
moteur crée. C’est le chemin par défaut dans Delonix (« rootless-first ») ; root est un opt-in explicite.
Voir : [Namespaces Linux et fonctionnement rootless](cloud-native-primer.md#41-linux-namespaces-and-rootless-operation).

**slirp4netns** — Une pile réseau en espace utilisateur qui connecte un network namespace rootless au
réseau de l’hôte sans privilèges, et y redirige des ports de l’hôte. Delonix exécute un seul
`slirp4netns` pour toute l’infrastructure réseau rootless, le NAT et la publication de ports étant assurés par nftables
à l’intérieur du namespace de l’infrastructure. Voir : `crates/adapters/delonix-sdn/src/infra.rs`,
[Réseau](system-design-interview.md#52-networking-pin-control-slirp-nftables).

**Stack** — L’ensemble des ressources que possède un manifeste. La propriété est un label sur chaque ressource,
`delonix.io/stack`, et non un enregistrement séparé : `apply --prune` et `stack destroy` ne touchent que les ressources
qui le portent, et une ressource créée à la main n’est jamais supprimée par eux. `Stack` est aussi un Kind
agrégat qui regroupe des ressources dans un seul document. Voir :
`crates/contexts/delonix-stack/src/reconcile.rs::STACK_LABEL`.

**State root** — Le répertoire qui contient tout l’état du moteur sous forme de fichiers (il n’y a pas de base de données) : `DELONIX_ROOT`
lorsqu’elle est définie, sinon `~/.local/share/delonix` (ou `$XDG_DATA_HOME/delonix`) pour un utilisateur et
`/var/lib/delonix` pour root. Les sockets réseau vivent dans un répertoire d’exécution séparé
(`DELONIX_NET_RUNTIME_DIR`). Définissez toujours les deux lorsque vous testez. Les enregistrements qu’il contient sont lus, écrits et
verrouillés par l’adaptateur `delonix-state`. Voir :
`bins/delonix-runtime-bin/src/cmd/util.rs::state_root`,
`crates/adapters/delonix-state/src/store.rs::Store::default_root`,
[L’état sur disque](architecture.md#state-on-disk),
[Isoler l’état du moteur](build-and-test.md#isolating-the-engines-state).

**subuid / subgid** — Une plage d’identifiants d’utilisateur et de groupe déléguée à votre utilisateur dans `/etc/subuid` et
`/etc/subgid`. Avec elle, un user namespace rootless mappe de nombreux identifiants (écrits via `newuidmap` et
`newgidmap`) ; sans elle, seul votre propre uid est mappé et les images qui utilisent d’autres utilisateurs ne fonctionnent pas. Les fichiers qu’un
container écrit sous un identifiant mappé n’appartiennent pas à votre uid sur l’hôte, ce qui explique pourquoi certaines opérations
entrent de nouveau dans un namespace mappé. Voir : `crates/adapters/delonix-sdn/src/pin_userns.rs`,
[Exigences du noyau](environment.md#kernel-requirements).

**Supervisor** — Le processus que `container run -d` crée par fork pour être le véritable parent du container : il
attend le container, enregistre son véritable statut de sortie (et une raison `OOMKilled`), et applique la
politique `--restart`. Parce qu’il fait un `fork`, il doit être lancé depuis un processus mono-thread ; les serveurs
ré-exécutent d’abord un `delonix` neuf. Voir : `crates/adapters/delonix-linux/src/supervise.rs::run_supervised`,
`crates/adapters/delonix-linux/src/lib.rs::wait_and_record`.

**userns (user namespace)** — Le namespace Linux qui mappe les identifiants d’utilisateur, donnant à un processus des
privilèges root uniquement sur les objets que possède son namespace. C’est le fondement du fonctionnement rootless et, sur
les Ubuntu récents, il peut être bloqué par AppArmor pour les binaires situés hors des chemins attendus. Voir :
[Namespaces Linux](cloud-native-primer.md#41-linux-namespaces-and-rootless-operation),
[AppArmor](environment.md#ubuntu-2310-apparmor-blocks-user-namespaces-for-your-dev-binary),
`namespaces(7)` et `user_namespaces(7)`.

**Verdict map** — Une map nftables d’une clé vers un verdict (`jump`, `accept`, …), utilisée pour qu’un paquet
trouve sa règle en une seule recherche au lieu de parcourir une règle par workload. Delonix utilise `fwmap` (une
adresse de workload → sa chaîne de pare-feu) et `netpair` (une paire de bridges → une exemption qui ouvre une
route entre deux réseaux). Voir : `crates/adapters/delonix-sdn/src/infra.rs::FWMAP`, `NETPAIR_MAP`.

**VmBackend** — Le port que chaque backend de VM implémente (`boot`, `stop`, `destroy`, `is_running`,
`ip`, pause et snapshots, …). Cloud Hypervisor et libvirt sont enregistrés par défaut ; un provider
distant s’enregistre à la racine de composition. L’enregistrement ne fait aucune I/O, et l’auto-détection filtre sur
l’enregistrement avant de construire quoi que ce soit, de sorte qu’un backend distant ne se connecte que lorsqu’il est choisi. Voir :
`crates/adapters/delonix-vm/src/lib.rs::VmBackend`, `register_backend`, `select_backend`,
[Les traits comme ports](rust-primer.md#33-traits-as-ports-vmbackend-and-the-backend-registry).

**Workload** — Deux choses liées. `kind: Workload` est un Kind de commodité avec `spec.type:
container|pod|vm|microvm` qui est abaissé vers le Kind correspondant au chargement
([ADR-0001](../../adr/0001-workload-kind-schema.md)). `delonix workload` est le groupe de commandes du quotidien (day-2)
(`ls`, `describe`, `stop`, `rm`) qui liste les containers et les VM ensemble et agit sur eux
([ADR-0002](../../adr/0002-compute-driver-trait.md)). Voir :
`crates/contexts/delonix-stack/src/kinds.rs::WORKLOAD_LOWERS_TO`,
`bins/delonix-runtime-bin/src/cmd/workload.rs`.

**Worktree** — Un `git worktree` : un second répertoire de travail rattaché au même dépôt, sur sa
propre branch. Chaque tâche a ici le sien, créé à partir de `origin/main` dans un répertoire persistant
en dehors du dépôt (jamais `/tmp`), et supprimé en même temps que sa branch à la fin. Voir :
[Un worktree par tâche](contributing-workflow.md#one-worktree-per-task).
