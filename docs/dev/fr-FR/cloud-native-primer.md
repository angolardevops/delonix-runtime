<!-- translated-from: cloud-native-primer.md sha256:a9064f13ecba90c0cc4992eccd73ec17bf783621b44ed9ce4319b3b07ee11793 -->
# Initiation au cloud native

**Avant de lire :** [Fondations Linux](linux-foundations.md) (namespaces, cgroups v2, descripteurs de fichier) et [IaaS et cloud native](iaas-and-cloud-native.md) (de quoi le moteur est responsable).

Le moteur est une couche fine et soigneuse au-dessus de fonctionnalités du noyau Linux et d’une
poignée de spécifications ouvertes. Cette page vous donne juste assez de chaque concept pour lire
le code, et dit **où il vit** dans ce dépôt. Après elle, vous pourrez prendre n’importe quel
mécanisme — un namespace, une limite de cgroup, une couche d’image, une chaîne de pare-feu, le
démarrage d’une VM, l’apply d’un manifeste — et nommer le fichier et le symbole qui l’implémente.
Pour aller plus loin, suivez les liens officiels — ils valent mieux que n’importe quel résumé ici.

Les concepts sont enseignés une seule fois dans ce manuel. Les primitives du noyau (namespaces,
user namespaces, cgroups v2) sont enseignées en pratique dans
[Fondations Linux](linux-foundations.md), donc les sections 4.1 et 4.2 ne font que les
récapituler et les relier au code. Ce que chaque standard ouvert *exige*, et jusqu’où le moteur y
est conforme, se trouve dans [Standards cloud native](cloud-native-standards.md), plus loin dans le
parcours. Les chemins nomment des crates (`crates/<couche>/<crate>`) ; les couches sont expliquées
dans [Architecture](architecture.md), et pour l’instant un chemin dit simplement où se trouve le
code.

Chaque section comporte trois parties : le concept, **Dans Delonix** (fichiers et symboles que
vous pouvez chercher avec `grep`), et **Pour aller plus loin**. Les chemins sont relatifs à la
racine du dépôt.

Pour vous orienter dans l’écosystème plus large, le [CNCF Landscape](https://landscape.cncf.io/)
et le [CNCF Glossary](https://glossary.cncf.io/) sont de bonnes cartes. Les comparaisons avec
runc, crun, containerd ou Podman n’apparaissent que là où elles aident à expliquer un choix de
conception.

---

## 4.1 Namespaces Linux et fonctionnement rootless

**Récapitulatif.** Un container est un processus démarré dans un nouvel ensemble de namespaces
(mount, PID, network, IPC, UTS, cgroup, user). Le **user namespace** est ce qui le rend rootless :
à l’intérieur, le processus est uid 0 sur les ressources que ce namespace possède ; à l’extérieur,
c’est un utilisateur ordinaire. Un utilisateur non privilégié ne peut mapper que son propre uid ;
une *plage* nécessite `newuidmap`/`newgidmap` et `/etc/subuid`. Les deux sont enseignés, avec des
commandes à taper, dans [Fondations Linux — Namespaces](linux-foundations.md#namespaces) et
[User namespaces et mappage d’uid](linux-foundations.md#user-namespaces-and-uid-mapping).
Certaines distributions restreignent en plus les user namespaces non privilégiés via AppArmor —
voir [Environnement](environment.md#ubuntu-2310-apparmor-blocks-user-namespaces-for-your-dev-binary)
pour les conséquences pratiques.

**Dans Delonix**

- `fn spawn` dans `crates/adapters/delonix-linux/src/lib.rs` construit les `CloneFlags`
  (`CLONE_NEWNS`, `CLONE_NEWPID`, `CLONE_NEWNET`, `CLONE_NEWUSER`, …) et appelle
  `nix::sched::clone`. Le partage d’IPC/UTS entre membres d’un pod est géré par `setns` dans
  `container_init`.
- `write_userns_maps` dans le même fichier écrit les mappages depuis le parent : un mappage à un
  seul uid (`0 <euid> 1`) en rootless, ou une plage de subuid via `newuidmap`/`newgidmap` quand
  `have_subid_helpers()` indique qu’ils sont disponibles. `USERNS_UID_BASE`/`USERNS_RANGE`
  définissent la plage utilisée en mode root.
- `setup_rootfs` monte la racine du container et appelle `pivot_root` ; `container_init` est le
  code qui s’exécute dans les nouveaux namespaces avant `execvp`.
- Les opérations rootless sur des fichiers appartenant à des subuids mappés ré-exécutent le
  binaire dans un user namespace mappé : `reexec_mapped`, `reexec_mapped_hold`,
  `remove_tree_mapped`.

**Pour aller plus loin :** [`namespaces(7)`](https://man7.org/linux/man-pages/man7/namespaces.7.html),
[`user_namespaces(7)`](https://man7.org/linux/man-pages/man7/user_namespaces.7.html),
[`newuidmap(1)`](https://man7.org/linux/man-pages/man1/newuidmap.1.html),
[`subuid(5)`](https://man7.org/linux/man-pages/man5/subuid.5.html),
[`pivot_root(2)`](https://man7.org/linux/man-pages/man2/pivot_root.2.html).

---

## 4.2 cgroups v2 et délégation

**Récapitulatif.** cgroup v2 est un arbre unique à `/sys/fs/cgroup` dont les fichiers
(`memory.max`, `cpu.max`, `pids.max`, `memory.events`) limitent et comptabilisent un groupe de
processus. Un utilisateur non privilégié ne peut écrire que dans une sous-arborescence que systemd
lui a **déléguée**, et un shell dans une scope de session SSH se trouve généralement *en dehors* —
si bien que des limites peuvent, en silence, ne pas s’y appliquer. L’arbre, la règle « pas de
processus internes » et la délégation sont enseignés en pratique dans
[Fondations Linux — cgroups v2](linux-foundations.md#cgroups-v2) ; le contrat de délégation en tant
que standard, et la conformité du moteur, se trouvent dans
[Standards cloud native — 13.15](cloud-native-standards.md#1315-linux-cgroup-v2-and-systemd-delegation).
Le moteur ne prend en charge que v2.

**Dans Delonix**

- Le mode root place les containers sous `delonix_compute::DELONIX_SLICE`
  (`/sys/fs/cgroup/delonix.slice`).
- Le mode rootless trouve le cgroup de service de l’utilisateur et crée des feuilles sous
  `<user@uid.service>/dlx-containers` — voir `user_service_base` et `try_delegated_base` dans
  `crates/adapters/delonix-linux/src/lib.rs`. `cgroup_limits_apply` répond à « les limites
  s’appliqueront-elles sur cet hôte ? » sans démarrer de container.
- La décision de conception sur le niveau intermédiaire est l’
  [ADR-0015](../../adr/0015-intermediate-cgroup-level.md) ; la façon dont le CRI suit la hiérarchie
  de cgroups du kubelet est l’[ADR-0038](../../adr/0038-cri-follows-kubelet-resource-model.md), avec
  le parent du kubelet validé par `KubeCgroupParent::parse` dans
  `crates/contexts/delonix-compute/src/record.rs`.

**Pour aller plus loin :** [Noyau Linux — Control Group v2](https://docs.kernel.org/admin-guide/cgroup-v2.html),
[systemd — Control Group APIs and Delegation](https://systemd.io/CGROUP_DELEGATION/),
[`cgroups(7)`](https://man7.org/linux/man-pages/man7/cgroups.7.html).

---

## 4.3 Capabilities, seccomp, AppArmor, chemins masqués

Le pouvoir de root se divise en **capabilities** (`CAP_NET_ADMIN`, `CAP_SYS_ADMIN`, …). Un
container conserve un petit ensemble par défaut et abandonne le reste. **seccomp** installe un
filtre BPF qui autorise ou refuse des appels système, réduisant la surface d’attaque du noyau.
**AppArmor** (et SELinux sur d’autres distributions) sont des Linux Security Modules qui confinent
un processus par profil. Enfin, les runtimes **masquent** des chemins sensibles de `/proc` et
`/sys` (montent quelque chose de vide par-dessus) et rendent d’autres lecture seule, parce que ces
fichiers révèlent des informations sur l’hôte ou permettent de le contrôler.

Un point subtil que le code documente : `clone3` passe ses flags via un pointeur qu’un filtre
seccomp ne peut pas inspecter, si bien qu’un filtre qui bloque `clone(CLONE_NEWUSER)` doit aussi
faire échouer `clone3` avec `ENOSYS` pour forcer la libc à revenir au `clone` filtrable.

**Dans Delonix**

- Capabilities : `KEPT_CAPS` et `resolve_cap_keep` dans
  `crates/adapters/delonix-linux/src/capabilities.rs` ; `drop_capabilities` dans `lib.rs`.
- seccomp : `apply_seccomp` dans `crates/adapters/delonix-linux/src/lib.rs` (construit avec le
  crate [`seccompiler`](https://docs.rs/seccompiler), y compris le pré-filtre `clone3` →
  `ENOSYS`) ; les profils JSON personnalisés sont analysés et compilés dans
  `seccomp_profile.rs` (`parse`, `compile`).
- AppArmor : `apply_apparmor` dans `lib.rs`.
- Chemins masqués et lecture seule : `DEFAULT_MASKED_PATHS`, `DEFAULT_READONLY_PATHS`,
  `apply_masked_paths`, `apply_readonly_paths`, `mask_proc_paths` dans `lib.rs`.
- Les décisions de sécurité au niveau du nœud (politique, admission, événements, score) forment un
  crate pur séparé : `evaluate` dans
  `crates/contexts/delonix-security-runtime/src/admission.rs`
  ([ADR-0026](../../adr/0026-security-runtime-decision-crate.md)).

**Pour aller plus loin :** [`capabilities(7)`](https://man7.org/linux/man-pages/man7/capabilities.7.html),
[noyau — Seccomp BPF](https://docs.kernel.org/userspace-api/seccomp_filter.html),
[Documentation AppArmor](https://gitlab.com/apparmor/apparmor/-/wikis/Documentation),
[OCI runtime spec — Linux config](https://github.com/opencontainers/runtime-spec/blob/main/config-linux.md)
(les champs `maskedPaths`/`readonlyPaths`/`seccomp` que d’autres runtimes consomment).

---

## 4.4 Images OCI, stockage adressé par contenu et overlayfs

L’**Open Container Initiative** publie trois spécifications :

- la **image spec** — une image est un *manifeste* (JSON) qui pointe vers une *config* et une
  liste ordonnée de *layers* (des tarballs), et éventuellement un *index* pointant vers un
  manifeste par plateforme ;
- la **distribution spec** — l’API HTTP que servent les registres (`/v2/<name>/manifests/<ref>`,
  `/v2/<name>/blobs/<digest>`, authentification par jeton) ;
- la **runtime spec** — comment un runtime tel que runc ou crun reçoit l’instruction d’exécuter un
  bundle de système de fichiers.

Tout est **adressé par contenu** : un blob est nommé par le digest SHA-256 de ses octets, si bien
qu’un client vérifie ce qu’il a téléchargé en le hachant. Un pull par digest (`name@sha256:…`)
n’est une garantie que si le *manifeste* lui-même est vérifié par rapport à ce digest, en plus de
chaque blob par rapport au manifeste.

À l’exécution, les couches sont empilées avec **overlayfs** : des `lowerdir` en lecture seule, un
`upperdir` inscriptible où les changements sont copiés, et un `workdir`. Beaucoup de containers
peuvent partager les mêmes couches inférieures.

**Dans Delonix**

- Client de registre (distribution spec) : `crates/adapters/delonix-oci/src/registry.rs` —
  fonctions `pull_from_registry*`, les types de média `ACCEPT_MANIFEST`, et
  `verify_manifest_digest`. Les types viennent du crate [`oci-spec`](https://docs.rs/oci-spec).
- Store de blobs adressé par contenu : `Cas` dans `crates/adapters/delonix-oci/src/cas.rs`.
- Écriture d’une archive au format OCI image layout : `write_oci_archive` dans `save.rs`.
- Préparation de l’overlay : `ImageStore::prepare_overlay` dans `overlay.rs` écrit un marqueur
  `overlay-lowers` (`LOWERS_FILE`) ; le montage lui-même se produit à l’intérieur des namespaces
  user et mount du container dans `mount_overlay_if_marked`
  (`crates/adapters/delonix-linux/src/lib.rs`), via la nouvelle API de montage — voir
  [ADR-0016](../../adr/0016-filesystem-under-the-state-root.md) et
  [ADR-0037](../../adr/0037-overlay-mount-new-api.md).
- Le moteur exécute lui-même les containers plutôt que de confier un bundle de runtime OCI à
  runc/crun.

**Pour aller plus loin :** [OCI image spec](https://github.com/opencontainers/image-spec),
[OCI distribution spec](https://github.com/opencontainers/distribution-spec),
[OCI runtime spec](https://github.com/opencontainers/runtime-spec),
[noyau — Overlay Filesystem](https://docs.kernel.org/filesystems/overlayfs.html).

---

## 4.5 Réseau des containers

Briques de base du réseau Linux :

- un **network namespace** a ses propres interfaces, routes et pare-feu ;
- une **paire veth** est un câble virtuel avec une extrémité dans chaque namespace ;
- une **bridge** est un commutateur virtuel reliant plusieurs extrémités veth ;
- **nftables** est le filtre de paquets et moteur NAT du noyau ; **DNAT** réécrit une destination
  (comment un port publié atteint un container), et **conntrack** suit les flux pour que le
  trafic de réponse d’une connexion autorisée passe (`ct state established,related`) ;
- **slirp4netns** donne une connectivité sortante à un network namespace non privilégié en émulant
  une pile TCP/IP en espace utilisateur, et redirige des ports de l’hôte vers celui-ci ;
- **VXLAN** transporte des trames L2 sur UDP entre hôtes, et **WireGuard** chiffre un tunnel.

**CNI** (Container Network Interface) est une spécification où un runtime exécute des binaires de
plugin (`bridge`, `host-local`, `portmap`, …) avec des commandes `ADD`/`DEL` et une configuration
JSON depuis `/etc/cni/net.d`. Les runtimes Kubernetes l’utilisent pour le réseau des pods.

**Dans Delonix**

- Le réseau rootless ne peut pas créer d’interfaces sur l’hôte, donc le moteur maintient un
  network namespace **holder** de longue durée : un processus *pin* minimal possède les
  namespaces, et un processus *control* redémarrable sert un socket Unix. Voir `start_pin`,
  `start_control` et `ensure_up` dans `crates/adapters/delonix-sdn/src/infra.rs`.
- Attacher un workload : `attach_container` (IPAM + commande de contrôle) et `do_attach` (veth
  vers la bridge, à l’intérieur du holder). Les noms de bridge viennent de `bridge_name`, dans le
  crate sans dépendances `crates/foundation/delonix-net-rules/src/lib.rs`.
- Sortie et redirection de ports : `slirp_attach` et `slirp_add_hostfwd` dans
  `crates/adapters/delonix-sdn/src/lib.rs` (qui démarrent `slirp4netns`) ; la publication à
  l’intérieur du holder dans `publish_port`/`do_publish` (`infra.rs`).
- Pare-feu : `table ip dlxing` avec les base chains `fwguard`, `fwdeny`, `fwcont` et la verdict map
  `fwmap` (`FWMAP`), générée dans `infra.rs` (`do_firewall`, `apply_firewall_all`, `ns_set_join`
  pour les sets d’isolement de namespace).
- DNS interne (nom standard `<name>.<namespace>.svc.delonix.internal`, l’ancien
  `<name>.<namespace>.delonix.internal` répond toujours ; `service_fqdn`, `parse_internal_name`) :
  `dns_server_main`, `handle_dns`,
  `dns_resolve_for`, `dns_resolve_multi_for` dans `infra.rs`.
- Réseaux overlay : `set_vxlan` (`infra.rs`) et les assistants WireGuard dans
  `crates/adapters/delonix-sdn/src/wg.rs`, orchestrés par `realize_overlay` dans
  `bins/delonix-runtime-bin/src/cmd/network.rs`.
- CNI : `crates/adapters/delonix-sdn/src/cni.rs` — `add`, `del`, `readiness`,
  `attach_named_netns`. L’usage en rootless est opt-in (`enabled_conf` vérifie `DELONIX_CNI=1`) ;
  le chemin CRI en root utilise la chaîne CNI du nœud (`root_cni_readiness` dans
  `crates/interfaces/delonix-cri/src/runtime_svc.rs`).
- Décisions de topologie : [ADR-0013](../../adr/0013-network-topology.md),
  [ADR-0014](../../adr/0014-runtime-dir-per-root.md).

**Pour aller plus loin :** [`network_namespaces(7)`](https://man7.org/linux/man-pages/man7/network_namespaces.7.html),
[`veth(4)`](https://man7.org/linux/man-pages/man4/veth.4.html),
[wiki nftables](https://wiki.nftables.org/),
[slirp4netns](https://github.com/rootless-containers/slirp4netns),
[noyau — VXLAN](https://docs.kernel.org/networking/vxlan.html),
[WireGuard](https://www.wireguard.com/),
[CNI](https://www.cni.dev/) et sa [spécification](https://www.cni.dev/docs/spec/).

---

## 4.6 Kubernetes : CRI, kubelet, kubeadm et kind

Le **kubelet** est l’agent de nœud de Kubernetes. Il n’exécute pas lui-même les containers ; il
parle à un runtime de containers via la **Container Runtime Interface**, une API gRPC
(`RuntimeService`, `ImageService`) servie sur un socket Unix local. Le kubelet crée une *pod
sandbox* (`RunPodSandbox`) puis les containers à l’intérieur. Il a aussi un réglage de **cgroup
driver** (`systemd` ou `cgroupfs`) qui doit correspondre à la façon dont le runtime gère les
cgroups, sinon les cgroups du pod et ceux du container divergent.

**kubeadm** amorce un cluster sur des machines existantes (`kubeadm init`, `kubeadm join`).
**kind** exécute des nœuds Kubernetes sous forme de containers construits à partir de l’image
`kindest/node`.

**Dans Delonix**

- `crates/interfaces/delonix-cri` est un serveur CRI `runtime.v1`. Le protobuf est
  `proto/api.proto` à l’intérieur de ce crate, compilé par `build.rs` avec `tonic-build`. Le
  binaire est `src/bin/delonix-cri.rs`.
- Le cgroup driver rapporté au kubelet : `engine_cgroup_driver` dans `runtime_svc.rs` ; son
  commentaire de doc explique pourquoi la réponse est celle-là, et ce qui devrait changer pour que
  ce soit l’autre.
- Un aller-retour sur du gRPC réel est testé dans
  `crates/interfaces/delonix-cri/tests/grpc_status.rs`.
- Commandes d’amorçage de cluster : kubeadm via SSH dans
  `bins/delonix-runtime-bin/src/cmd/cluster.rs` (avec `kubeadm_config.rs`, `etcd.rs`, `lb.rs`), et
  des clusters locaux façon kind dans `kindmode.rs`.

**Pour aller plus loin :** [Kubernetes — Container Runtime Interface](https://kubernetes.io/docs/concepts/architecture/cri/),
[dépôt cri-api](https://github.com/kubernetes/cri-api),
[Configurer un cgroup driver](https://kubernetes.io/docs/tasks/administer-cluster/kubeadm/configure-cgroup-driver/),
[kubeadm](https://kubernetes.io/docs/reference/setup-tools/kubeadm/),
[kind](https://kind.sigs.k8s.io/).

---

## 4.7 Virtualisation : KVM, virtio, Cloud Hypervisor, libvirt, cloud-init

**KVM** est l’hyperviseur du noyau, exposé comme `/dev/kvm` ; un **VMM** en espace utilisateur
(QEMU, Cloud Hypervisor) l’utilise pour exécuter des invités. **virtio** est la famille de
périphériques paravirtuels (disque, réseau, partage de système de fichiers 9p) que les invités
utilisent pour des E/S efficaces. **Cloud Hypervisor** est un VMM en Rust centré sur les workloads
cloud, capable de s’exécuter sans privilège avec accès à `/dev/kvm` ; il démarre les invités via
un firmware (une build UEFI EDK2 ou `rust-hypervisor-firmware`) ou directement depuis une image de
noyau. **libvirt** gère des domaines QEMU/KVM décrits en XML, via `virsh` et `libvirtd`.

Les cloud images sont génériques ; la configuration par instance (hostname, clés SSH,
utilisateurs, réseau) vient de **cloud-init**, qui lit une datasource. La datasource **NoCloud**
est un petit ISO étiqueté `cidata` contenant `user-data`, `meta-data` et éventuellement
`network-config`.

**Dans Delonix**

- Le port est `VmBackend` dans `crates/adapters/delonix-vm/src/lib.rs`, implémenté par
  `CloudHypervisorBackend` et `LibvirtBackend` là, et par `ProxmoxBackend` dans
  `crates/providers/delonix-proxmox` ([ADR-0008](../../adr/0008-proxmox-vm-backend.md)).
- Ordre de recherche du firmware Cloud Hypervisor : `DEFAULT_CH_FIRMWARES` (EDK2 `CLOUDHV.fd`
  avant `hypervisor-fw`) ; la ligne de commande du VMM est construite dans `boot_ch`.
- XML de domaine libvirt : `libvirt_domain_xml`.
- Génération de la seed NoCloud : `generate_seed_iso` dans `bins/delonix-runtime-bin/src/cmd/vm.rs`.
- Les VM sur le SDN rootless obtiennent un bail DHCP dérivé de leur MAC : `dhcp_lease_ip` dans
  `crates/adapters/delonix-sdn/src/infra.rs`.
- Configuration pratique et pièges d’hôte connus : [Construire des microVM](microvm-setup.md).

**Pour aller plus loin :** [noyau — KVM](https://docs.kernel.org/virt/kvm/index.html),
[spécification virtio (OASIS)](https://docs.oasis-open.org/virtio/virtio/),
[Cloud Hypervisor](https://www.cloudhypervisor.org/) et sa
[documentation](https://github.com/cloud-hypervisor/cloud-hypervisor/tree/main/docs),
[libvirt](https://libvirt.org/docs.html),
[cloud-init NoCloud](https://docs.cloud-init.io/en/latest/reference/datasources/nocloud.html).

---

## 4.8 Réconciliation déclarative

Kubernetes a popularisé un modèle où les utilisateurs soumettent un **état désiré** sous forme
d’objets typés (`apiVersion`, `kind`, `metadata`, `spec`), et des contrôleurs le comparent
répétitivement à l’**état réel** et agissent pour converger. `kubectl apply` ajoute un **diff à
trois voies** : il stocke la dernière configuration appliquée sur l’objet, ce qui permet de
distinguer « vous avez retiré ce champ de votre fichier » (le rétablir) de « quelqu’un a défini ce
champ à la main » (le laisser tranquille).

Le principe, et ce qu’il vous demande quand vous ajoutez un Kind, se trouvent dans
[IaaS et cloud native — Déclaratif et convergent](iaas-and-cloud-native.md#declarative-and-convergent).

**Dans Delonix**

- Le moteur a ses propres Kinds dans des groupes d’API (`delonix api-resources` les liste). Les
  faits sur chaque Kind (domaine, s’il converge, teardown, namespacing) vivent dans une seule
  table : `KindFacts` dans `crates/contexts/delonix-stack/src/kinds.rs`.
- Le planificateur est pur : `plan(desired, actual, stack)` dans
  `crates/contexts/delonix-stack/src/reconcile.rs`. Le commentaire de module contient la table de
  vérité à trois voies, et le dernier spec appliqué est stocké sur la ressource elle-même sous
  l’annotation `LAST_APPLIED` (`delonix.io/last-applied`) — il n’y a pas de fichier d’état séparé.
- La propriété est une étiquette sur la ressource ; l’historique de révisions se trouve dans
  `revision.rs` ([ADR-0019](../../adr/0019-stack-revision-history.md)).
- Les manifestes sont analysés dans `bins/delonix-runtime-bin/src/cmd/manifest.rs` ; `stack
  plan`/`apply` vivent dans `cmd/stack.rs`.
- Aucune boucle de contrôleur ne s’exécute en arrière-plan : la réconciliation se produit quand une
  commande s’exécute (daemonless). Le réconciliateur de pull proposé conserve cette propriété en
  étant un timer systemd qui invoque le même apply, et non un processus résident
  ([ADR-0021](../../adr/0021-gitops-pull-reconciler.md), état *Proposed*).

**Pour aller plus loin :** [Kubernetes — Objects](https://kubernetes.io/docs/concepts/overview/working-with-objects/),
[Controllers](https://kubernetes.io/docs/concepts/architecture/controller/),
[Gestion déclarative avec `kubectl apply`](https://kubernetes.io/docs/tasks/manage-kubernetes-objects/declarative-config/).

---

## 4.9 Observabilité et l’interface MCP

**OpenTelemetry** est un standard CNCF pour les traces, métriques et logs, exportés via **OTLP**
vers un collecteur. **Prometheus** récupère des métriques depuis un endpoint HTTP `/metrics` dans
un format d’exposition texte. Le **Model Context Protocol** est un protocole ouvert qui permet à
des clients d’IA de découvrir et d’appeler des outils exposés par un serveur, généralement via
stdio.

**Dans Delonix**

- Logging structuré et spans OTLP optionnels : `init` dans
  `crates/adapters/delonix-telemetry/src/telemetry.rs` (les spans ne sont exportés que quand
  `DELONIX_OTLP_ENDPOINT` est définie ; l’exportateur s’exécute sur son propre thread pour que la
  CLI synchrone n’ait besoin d’aucun runtime asynchrone).
- Registre Prometheus (préfixe `delonix`) et encodage texte :
  `crates/adapters/delonix-telemetry/src/metrics.rs` (`encode`). L’API de gestion locale sert
  `/metrics` dans `crates/interfaces/delonix-mgmt/src/lib.rs`.
- MCP : `crates/interfaces/delonix-mcp` (construit sur [`rmcp`](https://docs.rs/rmcp), transport
  stdio), avec le binaire dans `bins/delonix-mcp-bin`. Portée et limites :
  [ADR-0025](../../adr/0025-mcp-local-ai-control-surface.md).

**Pour aller plus loin :** [Documentation OpenTelemetry](https://opentelemetry.io/docs/),
[Spécification OTLP](https://opentelemetry.io/docs/specs/otlp/),
[Prometheus — formats d’exposition](https://prometheus.io/docs/instrumenting/exposition_formats/),
[Model Context Protocol](https://modelcontextprotocol.io/).

---

## 4.10 Daemonless, en un paragraphe

containerd et le Docker Engine gardent un daemon résident qui possède l’état des containers ;
Podman a montré qu’un runtime peut à la place être une commande qui se termine, avec des
processus auxiliaires par container et systemd pour tout ce qui doit persister. Delonix suit le
second modèle : la CLI fait le travail et se termine, l’état ce sont des fichiers sous la racine
d’état protégés par `flock` (voir
[Initiation à Rust §3.8](rust-primer.md#38-concurrency-and-shared-state)), un processus
superviseur existe par container détaché, le holder réseau n’existe que tant que quelque chose en
a besoin, et la persistance au démarrage passe par des units systemd
(`bins/delonix-runtime-bin/src/cmd/boot.rs`). Les conséquences — bonnes et mauvaises — sont
discutées dans [Architecture](architecture.md) et
[System Design Interview](system-design-interview.md). Le principe lui-même, et la règle qu’un
nouveau daemon exige un ADR, se trouvent dans
[IaaS et cloud native — Daemonless](iaas-and-cloud-native.md#daemonless).

---
