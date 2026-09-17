<!-- translated-from: 04-cloud-native-primer.md sha256:53c436f2a27992274b150554fcfa81d0e618778b7dbc82ee9fcb12e48908762d -->
# 4. Initiation au cloud native

Le moteur est une couche fine et soigneuse au-dessus de fonctionnalités du noyau Linux et d’une poignée de
spécifications ouvertes. Cette page vous donne juste assez de chaque concept pour lire le code, et indique
**où il vit** dans ce dépôt. Pour approfondir, suivez les liens officiels — ils valent mieux
que n’importe quel résumé ici.

Chaque section comporte trois parties : le concept, **Dans Delonix** (fichiers et symboles que vous pouvez chercher avec `grep`),
et **En savoir plus**. Les chemins sont relatifs à la racine du dépôt.

Pour vous orienter dans l’écosystème plus large, le [CNCF Landscape](https://landscape.cncf.io/) et le
[CNCF Glossary](https://glossary.cncf.io/) sont de bonnes cartes. Les comparaisons avec runc, crun, containerd
ou Podman n’apparaissent que là où elles aident à expliquer un choix de conception.

---

## 4.1 Namespaces Linux et fonctionnement rootless

Un **namespace** donne à un processus sa propre vue d’un type de ressource globale. Un container est,
au fond, un processus démarré dans un ensemble neuf de namespaces : **mount** (sa propre arborescence de fichiers),
**PID** (sa propre numérotation des processus, lui-même étant le PID 1), **network** (ses propres interfaces et
routes), **IPC**, **UTS** (nom d’hôte), **cgroup** (sa propre vue de l’arbre des cgroups) et **user**.

Le **user namespace** est ce qui rend possibles les containers rootless. À l’intérieur, un processus peut être
l’uid 0 avec toutes les capabilities *sur les ressources appartenant à ce namespace*, tout en étant sur l’hôte un
utilisateur ordinaire. La correspondance entre uid intérieurs et extérieurs est écrite dans
`/proc/<pid>/uid_map` et `gid_map`. Un utilisateur non privilégié ne peut mapper que son propre uid ; mapper une
*plage* exige les utilitaires setuid `newuidmap`/`newgidmap`, qui vérifient `/etc/subuid` et
`/etc/subgid`. Certaines distributions restreignent en outre les user namespaces non privilégiés via
AppArmor — voir [1. Environnement](01-environment.md) pour les conséquences pratiques.

**Dans Delonix**

- `fn spawn` dans `crates/adapters/delonix-linux/src/lib.rs` construit les `CloneFlags`
  (`CLONE_NEWNS`, `CLONE_NEWPID`, `CLONE_NEWNET`, `CLONE_NEWUSER`, …) et appelle `nix::sched::clone`.
  Le partage IPC/UTS entre membres d’un pod est géré par `setns` dans `container_init`.
- `write_userns_maps` dans le même fichier écrit les maps depuis le parent : une map à uid unique
  (`0 <euid> 1`) en rootless, ou une plage de subuid via `newuidmap`/`newgidmap` lorsque
  `have_subid_helpers()` indique qu’ils sont disponibles. `USERNS_UID_BASE`/`USERNS_RANGE` définissent la plage
  utilisée lors d’une exécution en root.
- `setup_rootfs` monte la racine du container et appelle `pivot_root` ; `container_init` est le
  code qui s’exécute à l’intérieur des nouveaux namespaces avant `execvp`.
- Les opérations rootless sur des fichiers appartenant à des subuid mappés ré-exécutent le binaire dans un user
  namespace mappé : `reexec_mapped`, `reexec_mapped_hold`, `remove_tree_mapped`.

**En savoir plus :** [`namespaces(7)`](https://man7.org/linux/man-pages/man7/namespaces.7.html),
[`user_namespaces(7)`](https://man7.org/linux/man-pages/man7/user_namespaces.7.html),
[`newuidmap(1)`](https://man7.org/linux/man-pages/man1/newuidmap.1.html),
[`subuid(5)`](https://man7.org/linux/man-pages/man5/subuid.5.html),
[`pivot_root(2)`](https://man7.org/linux/man-pages/man2/pivot_root.2.html).

---

## 4.2 cgroups v2 et délégation

Les **control groups** limitent et comptabilisent les ressources (mémoire, CPU, PID, E/S) d’un ensemble de
processus. cgroup v2 est un arbre unique monté sur `/sys/fs/cgroup` ; un répertoire est un groupe, et
des fichiers comme `memory.max`, `cpu.max`, `pids.max` et `memory.events` constituent son interface. Un
contrôleur n’est disponible pour un enfant que si le parent le liste dans `cgroup.subtree_control`.

Un utilisateur non privilégié ne peut gérer un sous-arbre que s’il lui a été **délégué**. Sur les hôtes
systemd, `user@<uid>.service` délègue généralement certains contrôleurs, et
`systemd-run --user --scope -p Delegate=yes` crée à la demande un scope délégué. Un processus dans un
scope de session SSH se trouve généralement *en dehors* du sous-arbre délégué, et la règle « pas de processus internes »
empêche de l’y déplacer — les limites peuvent donc silencieusement ne pas s’y appliquer. Le moteur ne prend en charge que v2.

**Dans Delonix**

- Le mode root place les containers sous `delonix_runtime_core::DELONIX_SLICE`
  (`/sys/fs/cgroup/delonix.slice`).
- Le mode rootless trouve le cgroup du service de l’utilisateur et crée des feuilles sous
  `<user@uid.service>/dlx-containers` — voir `user_service_base` et `try_delegated_base` dans
  `crates/adapters/delonix-linux/src/lib.rs`. `cgroup_limits_apply` répond à « les limites s’appliqueront-elles
  sur cet hôte ? » sans démarrer de container.
- La décision de conception concernant le niveau intermédiaire est
  l’[ADR-0015](../../adr/0015-intermediate-cgroup-level.md) ; la façon dont le CRI suit la hiérarchie de cgroups
  du kubelet est l’[ADR-0038](../../adr/0038-cri-follows-kubelet-resource-model.md), le parent fourni par le kubelet
  étant validé par `KubeCgroupParent::parse` dans
  `crates/foundation/delonix-runtime-core/src/lib.rs`.

**En savoir plus :** [Linux kernel — Control Group v2](https://docs.kernel.org/admin-guide/cgroup-v2.html),
[systemd — Control Group APIs and Delegation](https://systemd.io/CGROUP_DELEGATION/),
[`cgroups(7)`](https://man7.org/linux/man-pages/man7/cgroups.7.html).

---

## 4.3 Capabilities, seccomp, AppArmor, chemins masqués

Le pouvoir de root est découpé en **capabilities** (`CAP_NET_ADMIN`, `CAP_SYS_ADMIN`, …). Un container
conserve un petit ensemble par défaut et abandonne le reste. **seccomp** installe un filtre BPF qui autorise ou
refuse des appels système, réduisant la surface d’attaque du noyau. **AppArmor** (et SELinux sur d’autres
distributions) sont des Linux Security Modules qui confinent un processus selon un profil. Enfin, les runtimes
**masquent** les chemins sensibles de `/proc` et `/sys` (en montant quelque chose de vide par-dessus) et en rendent d’autres
accessibles en lecture seule, car ces fichiers divulguent des informations sur l’hôte ou permettent de le contrôler.

Un point subtil que le code documente : `clone3` passe ses options à travers un pointeur qu’un filtre seccomp
ne peut pas inspecter ; un filtre qui bloque `clone(CLONE_NEWUSER)` doit donc aussi faire
échouer `clone3` avec `ENOSYS` pour forcer la libc à revenir au `clone` filtrable.

**Dans Delonix**

- Capabilities : `KEPT_CAPS` et `resolve_cap_keep` dans
  `crates/adapters/delonix-linux/src/capabilities.rs` ; `drop_capabilities` dans `lib.rs`.
- seccomp : `apply_seccomp` dans `crates/adapters/delonix-linux/src/lib.rs` (construit avec le
  crate [`seccompiler`](https://docs.rs/seccompiler), y compris le pré-filtre `clone3` → `ENOSYS`) ;
  les profils JSON personnalisés sont analysés et compilés dans `seccomp_profile.rs` (`parse`,
  `compile`).
- AppArmor : `apply_apparmor` dans `lib.rs`.
- Chemins masqués et en lecture seule : `DEFAULT_MASKED_PATHS`, `DEFAULT_READONLY_PATHS`,
  `apply_masked_paths`, `apply_readonly_paths`, `mask_proc_paths` dans `lib.rs`.
- Les décisions de sécurité au niveau du nœud (politique, admission, événements, score) forment un crate pur distinct :
  `evaluate` dans `crates/contexts/delonix-security-runtime/src/admission.rs`
  ([ADR-0026](../../adr/0026-security-runtime-decision-crate.md)).

**En savoir plus :** [`capabilities(7)`](https://man7.org/linux/man-pages/man7/capabilities.7.html),
[kernel — Seccomp BPF](https://docs.kernel.org/userspace-api/seccomp_filter.html),
[documentation AppArmor](https://gitlab.com/apparmor/apparmor/-/wikis/Documentation),
[OCI runtime spec — Linux config](https://github.com/opencontainers/runtime-spec/blob/main/config-linux.md)
(les champs `maskedPaths`/`readonlyPaths`/`seccomp` que consomment les autres runtimes).

---

## 4.4 Images OCI, stockage adressé par contenu et overlayfs

L’**Open Container Initiative** publie trois spécifications :

- l’**image spec** — une image est un *manifeste* (JSON) qui pointe vers une *config* et une liste ordonnée
  de *couches* (archives tar), et éventuellement un *index* qui pointe vers un manifeste par plateforme ;
- la **distribution spec** — l’API HTTP que servent les registres (`/v2/<name>/manifests/<ref>`,
  `/v2/<name>/blobs/<digest>`, authentification par jeton) ;
- la **runtime spec** — la façon dont on indique à un runtime comme runc ou crun d’exécuter un bundle de système de fichiers.

Tout est **adressé par contenu** : un blob est nommé par le condensé SHA-256 de ses octets, de sorte qu’un
client vérifie ce qu’il a téléchargé en le hachant. Récupérer par condensé (`name@sha256:…`) n’est une
garantie que si le *manifeste* est vérifié par rapport à ce condensé, en plus de chaque blob par rapport au
manifeste.

À l’exécution, les couches sont empilées avec **overlayfs** : des `lowerdir` en lecture seule, un `upperdir` inscriptible
où les modifications sont copiées vers le haut, et un `workdir`. De nombreux containers peuvent partager les mêmes couches inférieures.

**Dans Delonix**

- Client de registre (distribution spec) : `crates/adapters/delonix-oci/src/registry.rs` —
  les fonctions `pull_from_registry*`, les types de média `ACCEPT_MANIFEST`, et
  `verify_manifest_digest`. Les types viennent du crate [`oci-spec`](https://docs.rs/oci-spec).
- Store de blobs adressé par contenu : `Cas` dans `crates/adapters/delonix-oci/src/cas.rs`.
- Écriture d’une archive au format OCI image layout : `write_oci_archive` dans `save.rs`.
- Préparation de l’overlay : `ImageStore::prepare_overlay` dans `overlay.rs` écrit un marqueur `overlay-lowers`
  (`LOWERS_FILE`) ; le montage lui-même a lieu à l’intérieur des namespaces user et mount du container
  dans `mount_overlay_if_marked` (`crates/adapters/delonix-linux/src/lib.rs`), en utilisant
  la nouvelle API de montage — voir l’[ADR-0016](../../adr/0016-filesystem-under-the-state-root.md) et
  l’[ADR-0037](../../adr/0037-overlay-mount-new-api.md).
- Le moteur exécute lui-même les containers au lieu de confier un bundle de runtime OCI à runc/crun.

**En savoir plus :** [OCI image spec](https://github.com/opencontainers/image-spec),
[OCI distribution spec](https://github.com/opencontainers/distribution-spec),
[OCI runtime spec](https://github.com/opencontainers/runtime-spec),
[kernel — Overlay Filesystem](https://docs.kernel.org/filesystems/overlayfs.html).

---

## 4.5 Réseau des containers

Briques de base du réseau Linux :

- un **network namespace** a ses propres interfaces, routes et pare-feu ;
- une **paire veth** est un câble virtuel avec une extrémité dans chaque namespace ;
- un **bridge** est un commutateur virtuel qui relie de nombreuses extrémités veth ;
- **nftables** est le moteur de filtrage de paquets et de NAT du noyau ; le **DNAT** réécrit une destination (c’est ainsi qu’un
  port publié atteint un container), et **conntrack** suit les flux pour que le trafic de réponse d’une
  connexion autorisée passe (`ct state established,related`) ;
- **slirp4netns** donne à un network namespace non privilégié une connectivité sortante en émulant une
  pile TCP/IP en espace utilisateur, et y redirige des ports de l’hôte ;
- **VXLAN** transporte des trames L2 sur UDP entre hôtes, et **WireGuard** chiffre un tunnel.

**CNI** (Container Network Interface) est une spécification selon laquelle un runtime exécute des binaires de plugins
(`bridge`, `host-local`, `portmap`, …) avec des commandes `ADD`/`DEL` et une configuration JSON issue de
`/etc/cni/net.d`. Les runtimes Kubernetes l’utilisent pour le réseau des pods.

**Dans Delonix**

- Le réseau rootless ne peut pas créer d’interfaces sur l’hôte, le moteur conserve donc un network namespace
  **holder** de longue durée : un processus minimal *pin* possède les namespaces, et un processus
  *control* redémarrable sert un socket Unix. Voir `start_pin`, `start_control` et `ensure_up` dans
  `crates/adapters/delonix-sdn/src/infra.rs`.
- Attacher un workload : `attach_container` (IPAM + commande de contrôle) et `do_attach` (veth dans le
  bridge, à l’intérieur du holder). Les noms de bridge viennent de `bridge_name` dans le crate sans dépendance
  `crates/foundation/delonix-net-rules/src/lib.rs`.
- Sortie et redirection de ports : `slirp_attach` et `slirp_add_hostfwd` dans
  `crates/adapters/delonix-sdn/src/lib.rs` (qui lancent `slirp4netns`) ; publication à l’intérieur du holder dans
  `publish_port`/`do_publish` (`infra.rs`).
- Pare-feu : `table ip dlxing` avec les chaînes de base `fwguard`, `fwdeny`, `fwcont` et la verdict map `fwmap`
  (`FWMAP`), générées dans `infra.rs` (`do_firewall`, `apply_firewall_all`,
  `ns_set_join` pour les ensembles d’isolation des namespaces).
- DNS interne (`<name>.<namespace>.delonix.internal`) : `dns_server_main`, `handle_dns`,
  `dns_resolve_for`, `dns_resolve_multi_for` dans `infra.rs`.
- Réseaux overlay : `set_vxlan` (`infra.rs`) et les utilitaires WireGuard dans
  `crates/adapters/delonix-sdn/src/wg.rs`, orchestrés par `realize_overlay` dans
  `bins/delonix-runtime-bin/src/cmd/network.rs`.
- CNI : `crates/adapters/delonix-sdn/src/cni.rs` — `add`, `del`, `readiness`,
  `attach_named_netns`. L’usage rootless est à activer explicitement (`enabled_conf` vérifie `DELONIX_CNI=1`) ; le chemin CRI
  en root utilise la chaîne CNI du nœud (`root_cni_readiness` dans
  `crates/interfaces/delonix-cri/src/runtime_svc.rs`).
- Décisions de topologie : [ADR-0013](../../adr/0013-network-topology.md),
  [ADR-0014](../../adr/0014-runtime-dir-per-root.md).

**En savoir plus :** [`network_namespaces(7)`](https://man7.org/linux/man-pages/man7/network_namespaces.7.html),
[`veth(4)`](https://man7.org/linux/man-pages/man4/veth.4.html),
[wiki nftables](https://wiki.nftables.org/),
[slirp4netns](https://github.com/rootless-containers/slirp4netns),
[kernel — VXLAN](https://docs.kernel.org/networking/vxlan.html),
[WireGuard](https://www.wireguard.com/),
[CNI](https://www.cni.dev/) et sa [spécification](https://www.cni.dev/docs/spec/).

---

## 4.6 Kubernetes : CRI, kubelet, kubeadm et kind

Le **kubelet** est l’agent de nœud de Kubernetes. Il n’exécute pas lui-même les containers ; il parle à un
runtime de containers via la **Container Runtime Interface**, une API gRPC (`RuntimeService`,
`ImageService`) servie sur un socket Unix local. Le kubelet crée un *pod sandbox*
(`RunPodSandbox`) puis des containers à l’intérieur. Il a aussi un réglage de **driver de cgroups**
(`systemd` ou `cgroupfs`) qui doit correspondre à la façon dont le runtime gère les cgroups, faute de quoi les cgroups des pods et
ceux des containers divergent.

**kubeadm** amorce un cluster sur des machines existantes (`kubeadm init`, `kubeadm join`).
**kind** exécute des nœuds Kubernetes sous forme de containers construits à partir de l’image `kindest/node`.

**Dans Delonix**

- `crates/interfaces/delonix-cri` est un serveur CRI `runtime.v1`. Le protobuf est
  `proto/api.proto` dans ce crate, compilé par `build.rs` avec `tonic-build`.
  Le binaire est `src/bin/delonix-cri.rs`.
- Le driver de cgroups signalé au kubelet : `engine_cgroup_driver` dans `runtime_svc.rs` ; son commentaire de
  documentation consigne pourquoi la réponse est celle-ci et ce qui devrait changer pour l’autre.
- Un aller-retour sur du vrai gRPC est testé dans `crates/interfaces/delonix-cri/tests/grpc_status.rs`.
- Commandes d’amorçage de cluster : kubeadm via SSH dans `bins/delonix-runtime-bin/src/cmd/cluster.rs`
  (avec `kubeadm_config.rs`, `etcd.rs`, `lb.rs`), et clusters locaux à la manière de kind dans `kindmode.rs`.

**En savoir plus :** [Kubernetes — Container Runtime Interface](https://kubernetes.io/docs/concepts/architecture/cri/),
[dépôt cri-api](https://github.com/kubernetes/cri-api),
[Configuring a cgroup driver](https://kubernetes.io/docs/tasks/administer-cluster/kubeadm/configure-cgroup-driver/),
[kubeadm](https://kubernetes.io/docs/reference/setup-tools/kubeadm/),
[kind](https://kind.sigs.k8s.io/).

---

## 4.7 Virtualisation : KVM, virtio, Cloud Hypervisor, libvirt, cloud-init

**KVM** est l’hyperviseur du noyau, exposé sous la forme de `/dev/kvm` ; un **VMM** en espace utilisateur (QEMU, Cloud
Hypervisor) l’utilise pour exécuter des invités. **virtio** est la famille de périphériques paravirtuels (disque, réseau,
partage de système de fichiers 9p) que les invités utilisent pour des E/S efficaces. **Cloud Hypervisor** est un VMM en Rust orienté
vers les workloads cloud, qui peut s’exécuter sans privilège avec un accès à `/dev/kvm` ; il démarre les invités via un
firmware (un build UEFI d’EDK2 ou `rust-hypervisor-firmware`) ou directement depuis une image de noyau.
**libvirt** gère des domaines QEMU/KVM décrits en XML, via `virsh` et `libvirtd`.

Les images cloud sont génériques ; la configuration propre à chaque instance (nom d’hôte, clés SSH, utilisateurs, réseau) vient
de **cloud-init**, qui lit une source de données. La source de données **NoCloud** est un petit ISO étiqueté
`cidata` contenant `user-data`, `meta-data` et éventuellement `network-config`.

**Dans Delonix**

- Le port est `VmBackend` dans `crates/adapters/delonix-vm/src/lib.rs`, implémenté par
  `CloudHypervisorBackend` et `LibvirtBackend` dans ce même fichier et par `ProxmoxBackend` dans
  `crates/providers/delonix-proxmox` ([ADR-0008](../../adr/0008-proxmox-vm-backend.md)).
- Ordre de recherche du firmware de Cloud Hypervisor : `DEFAULT_CH_FIRMWARES` (EDK2 `CLOUDHV.fd` avant
  `hypervisor-fw`) ; la ligne de commande du VMM est construite dans `boot_ch`.
- XML de domaine libvirt : `libvirt_domain_xml`.
- Génération du seed NoCloud : `generate_seed_iso` dans `bins/delonix-runtime-bin/src/cmd/vm.rs`.
- Les VM sur le SDN rootless reçoivent un bail DHCP dérivé de leur MAC : `dhcp_lease_ip` dans
  `crates/adapters/delonix-sdn/src/infra.rs`.
- Configuration pratique et pièges connus de l’hôte : [9. Configuration des microVM](09-microvm-setup.md).

**En savoir plus :** [kernel — KVM](https://docs.kernel.org/virt/kvm/index.html),
[spécification virtio (OASIS)](https://docs.oasis-open.org/virtio/virtio/),
[Cloud Hypervisor](https://www.cloudhypervisor.org/) et sa
[documentation](https://github.com/cloud-hypervisor/cloud-hypervisor/tree/main/docs),
[libvirt](https://libvirt.org/docs.html),
[cloud-init NoCloud](https://docs.cloud-init.io/en/latest/reference/datasources/nocloud.html).

---

## 4.8 Réconciliation déclarative

Kubernetes a popularisé un modèle où les utilisateurs soumettent un **état désiré** sous forme d’objets typés
(`apiVersion`, `kind`, `metadata`, `spec`), et où des contrôleurs le comparent de façon répétée à l’**état
réel** et agissent pour converger. `kubectl apply` ajoute un **diff à trois voies** : il stocke sur l’objet la dernière
configuration appliquée, ce qui lui permet de distinguer « vous avez retiré ce champ de votre fichier »
(le rétablir) de « quelqu’un a défini ce champ à la main » (ne pas y toucher).

**Dans Delonix**

- Le moteur a ses propres Kinds dans des groupes d’API (`delonix api-resources` les liste). Les faits concernant
  chaque Kind (domaine, s’il converge, démontage, namespacing) vivent dans une seule table : `KindFacts`
  dans `crates/contexts/delonix-stack/src/kinds.rs`.
- Le planificateur est pur : `plan(desired, actual, stack)` dans
  `crates/contexts/delonix-stack/src/reconcile.rs`. La documentation du module contient la table de vérité à trois voies,
  et la dernière spec appliquée est stockée sur la ressource elle-même sous l’annotation `LAST_APPLIED`
  (`delonix.io/last-applied`) — il n’y a pas de fichier d’état séparé.
- La propriété est une label sur la ressource ; l’historique des révisions se trouve dans `revision.rs`
  ([ADR-0019](../../adr/0019-stack-revision-history.md)).
- Les manifestes sont analysés dans `bins/delonix-runtime-bin/src/cmd/manifest.rs` ; `stack plan`/`apply`
  vivent dans `cmd/stack.rs`.
- Aucune boucle de contrôleur ne tourne en arrière-plan : la réconciliation a lieu lorsqu’une commande
  s’exécute (daemonless). Le réconciliateur pull proposé conserve cette propriété en étant un timer systemd
  qui invoque le même apply, et non un processus résident
  ([ADR-0021](../../adr/0021-gitops-pull-reconciler.md), statut *Proposed*).

**En savoir plus :** [Kubernetes — Objects](https://kubernetes.io/docs/concepts/overview/working-with-objects/),
[Controllers](https://kubernetes.io/docs/concepts/architecture/controller/),
[Declarative management with `kubectl apply`](https://kubernetes.io/docs/tasks/manage-kubernetes-objects/declarative-config/).

---

## 4.9 Observabilité et l’interface MCP

**OpenTelemetry** est un standard de la CNCF pour les traces, les métriques et les logs, exportés via **OTLP** vers un
collecteur. **Prometheus** collecte les métriques depuis un endpoint HTTP `/metrics` dans un format
d’exposition textuel. Le **Model Context Protocol** est un protocole ouvert qui permet à des clients d’IA de découvrir et d’appeler
des outils exposés par un serveur, couramment sur stdio.

**Dans Delonix**

- Journalisation structurée et spans OTLP optionnels : `init` dans
  `crates/adapters/delonix-telemetry/src/telemetry.rs` (les spans ne sont exportés que lorsque
  `DELONIX_OTLP_ENDPOINT` est défini ; l’exportateur s’exécute sur son propre thread, de sorte que la CLI synchrone n’a
  besoin d’aucun runtime async).
- Registre Prometheus (préfixe `delonix`) et encodage textuel : `crates/adapters/delonix-telemetry/src/metrics.rs`
  (`encode`). L’API de gestion locale sert `/metrics` dans `crates/interfaces/delonix-mgmt/src/lib.rs`.
- MCP : `crates/interfaces/delonix-mcp` (construit sur [`rmcp`](https://docs.rs/rmcp), transport stdio),
  avec le binaire dans `bins/delonix-mcp-bin`. Périmètre et limites :
  [ADR-0025](../../adr/0025-mcp-local-ai-control-surface.md).

**En savoir plus :** [documentation OpenTelemetry](https://opentelemetry.io/docs/),
[spécification OTLP](https://opentelemetry.io/docs/specs/otlp/),
[Prometheus — exposition formats](https://prometheus.io/docs/instrumenting/exposition_formats/),
[Model Context Protocol](https://modelcontextprotocol.io/).

---

## 4.10 Daemonless, en un paragraphe

containerd et le Docker Engine conservent un daemon résident qui possède l’état des containers ; Podman a montré
qu’un runtime peut plutôt être une commande qui se termine, avec des processus auxiliaires par container et
systemd pour tout ce qui doit persister. Delonix suit le second modèle : la CLI fait le travail
et se termine, l’état est constitué de fichiers sous la racine d’état protégés par `flock` (voir
[3. Initiation à Rust §3.8](03-rust-primer.md#38-concurrency-and-shared-state)), un processus superviseur
existe par container détaché, le holder réseau n’existe que tant que quelque chose en a besoin, et la persistance
au démarrage repose sur des unités systemd (`bins/delonix-runtime-bin/src/cmd/boot.rs`). Les conséquences —
bonnes et mauvaises — sont discutées dans [5. Architecture](05-architecture.md) et
[7. System design interview](07-system-design-interview.md).
