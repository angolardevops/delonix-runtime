<!-- translated-from: microvm-setup.md sha256:10d65caba96f8246274f99b813161fdd1de0d0b5d43e81ebe5352b2b5f524247 -->
# Construire des microVMs

**Avant de lire :** [Préparer votre environnement](environment.md), [Cloner, compiler et tester](build-and-test.md), la [section virtualisation du manuel de cloud native](cloud-native-primer.md#47-virtualization-kvm-virtio-cloud-hypervisor-libvirt-cloud-init), et la Partie 2 de [Delonixfile et VMfile](delonixfile-and-vmfile.md#part-2-vmfile).

Cette page mène un contributeur d'un hôte Linux nu jusqu'à la construction, au démarrage et au test
de VMs avec Delonix, et montre où se trouve le code lorsqu'il faut modifier quelque chose. Après
elle, vous pouvez préparer un hôte pour Cloud Hypervisor et libvirt, prévoir quel backend une VM
obtient, et tester une modification de VM via la CLI. Elle
suppose que vous avez déjà lu [Préparer votre environnement](environment.md) et que vous
savez construire l'arborescence ([Cloner, compiler et tester](build-and-test.md)).

> **Ce qui a été vérifié pour cette page.** Chaque commande et chaque option ci-dessous ont été
> vérifiées contre `delonix <group> --help` d'un binaire construit à partir de cette arborescence,
> et les commandes en lecture seule / de configuration (`vm ls`, `vm reach`, `vm default-backend`,
> `image vm ls`, `manifest validate`, `stack apply --dry-run`, les erreurs de sélection du backend)
> ont été **exécutées** avec `DELONIX_ROOT` et `DELONIX_NET_RUNTIME_DIR` pointant vers un répertoire
> jetable. **Le démarrage d'une VM, la construction d'une image et le pull depuis le registre n'ont
> pas été exécutés lors de cette relecture** — le comportement de ces opérations provient du code,
> des ADR et des tests scriptés référencés ci-dessous.

Utilisez toujours le binaire que vous avez construit (`./target/debug/delonix`), et non celui de
votre `PATH`, et isolez toujours **les deux** racines d'état pendant vos expérimentations — voir
[Isolation](#isolation-first).

---

## 1. Prérequis de l'hôte

### KVM

Les deux backends locaux ont besoin de la virtualisation matérielle :

```bash
ls -l /dev/kvm            # must exist; missing = VT-x/AMD-V off in firmware, or no nested virt
id -nG | tr ' ' '\n' | grep -x kvm   # your user must be in the kvm group
```

`scripts/install.sh` (par défaut, c'est-à-dire sans `--no-vm`) vous ajoute aux groupes `kvm` et
`libvirt` et avertit lorsque `/dev/kvm` est absent. Les changements de groupe nécessitent une
nouvelle session.

### Cloud Hypervisor et son firmware

Le backend est disponible lorsque `cloud-hypervisor` est dans le `PATH`
(`CloudHypervisorBackend::available` dans `crates/adapters/delonix-vm/src/lib.rs`). Lorsque la
distribution ne le fournit pas en paquet, l'installateur télécharge le binaire **statique** amont
dans `/usr/local/bin/cloud-hypervisor`, épinglé à une version **et** à un SHA-256 dans
`scripts/install.sh`.

Pour démarrer une cloud image sans `--kernel`, CH a besoin d'un firmware UEFI.
`default_ch_firmware` utilise `$DELONIX_HYPERVISOR_FW` s'il est défini, sinon le premier fichier
existant de `DEFAULT_CH_FIRMWARES` :

```text
/usr/local/share/delonix/CLOUDHV.fd       ← EDK2 build from cloud-hypervisor/edk2 (preferred)
/usr/share/delonix/CLOUDHV.fd
/usr/local/share/delonix/hypervisor-fw    ← rust-hypervisor-firmware (fallback)
/usr/share/delonix/hypervisor-fw
```

**L'ordre compte.** Mesuré et consigné dans le commentaire de documentation de la constante : sous
`rust-hypervisor-fw`, aucune des images construites par ce projet ne démarre dans CH ; avec EDK2
`CLOUDHV.fd`, elles démarrent. `hypervisor-fw` reste un repli pour les hôtes qui n'ont que lui.
L'installateur récupère les deux (chacun épinglé par tag et SHA-256). Le test unitaire
`o_edk2_vem_antes_do_hypervisor_fw_na_procura_de_firmware` protège cet ordre.

### libvirt / QEMU

Le backend libvirt est disponible lorsque `virsh` et `qemu-system-x86_64` sont tous deux dans le
`PATH` (`LibvirtBackend::available`). L'installateur installe QEMU et un paquet de daemon libvirt,
et active `libvirtd` (par socket activation lorsque c'est pris en charge).

La connexion libvirt utilisée compte plus qu'il n'y paraît (`libvirt_uri_for`) :

| Situation | Connexion | Conséquence |
|---|---|---|
| `--net-mode nat` ou `bridge` | `qemu:///system` | IP joignable ; nécessite le groupe `libvirt` (ou root) |
| pas de `--net-mode`, connexion système utilisable | `qemu:///system`, **`nat` choisi automatiquement** | IP par DHCP depuis le réseau libvirt (`virbr0`) |
| pas de `--net-mode`, connexion système **non** utilisable, rootless | `qemu:///session`, mode utilisateur | **aucune IP visible ni joignable** ; `vm create` avertit |

Sur `qemu:///system`, QEMU s'exécute sous l'utilisateur du service libvirt, qui ne peut pas lire un
disque situé sous un home en 0700. Pour un appelant rootless, le XML du domaine reçoit un
`seclabel` DAC statique qui épingle QEMU à votre uid/gid avec `relabel='no'`, de sorte que votre
propre overlay démarre sans que son propriétaire soit changé.

### Outils que le code VM invoque

| Outil | Paquet (Debian / Fedora) | Utilisé par |
|---|---|---|
| `qemu-img` | `qemu-utils` / `qemu-img` | overlays par VM, `vm convert`, snapshots sur CH, construction d'images |
| `cloud-localds` | `cloud-image-utils` / `cloud-utils` | ISO de seed NoCloud — générée à **chaque** `vm create` d'une image cloud-init sauf si `--seed` est fourni (`crates/adapters/delonix-vm/src/cloudinit.rs`) |
| `virsh` | `libvirt-clients` / `libvirt-client` | backend libvirt |
| `virt-customize`, `virt-sparsify`, `virt-copy-out` | `libguestfs-tools` / `guestfs-tools` | `vm build` / `image vm build` uniquement |

`vmimage::tool_package` associe un binaire manquant à son paquet, de sorte qu'un outil manquant est
signalé par son nom plutôt que par un simple `No such file or directory`.

### Uniquement pour construire des images : `--with-image-build`

`image vm build` exécute `virt-customize`, qui construit une petite appliance avec supermin. Trois
problèmes d'hôte la cassent d'une manière qui ne ressemble pas à des problèmes d'hôte ;
`scripts/install.sh --with-image-build` les traite, et `tool_failure_hint` (`cmd/vmimage.rs`) les
nomme lorsqu'un build échoue :

1. **Aucun client DHCP sur l'hôte.** supermin *copie* les paquets de l'hôte dans l'appliance ; sans
   `isc-dhcp-client`, l'appliance n'a pas de réseau et le build meurt sur
   `Temporary failure resolving …`. L'installateur l'installe.
2. **`/boot/vmlinuz-*` est en 0600** (Debian/Ubuntu). supermin copie le noyau de l'hôte et échoue
   avec `Permission denied`. L'installateur exécute `chmod 0644 /boot/vmlinuz-*` — cela **abaisse
   une frontière de sécurité de l'hôte** (tout utilisateur local peut lire l'image du noyau) ; c'est
   donc opt-in, et l'installateur affiche comment revenir en arrière
   (`sudo chmod 0600 /boot/vmlinuz-*`). L'indication d'échec montre aussi comment faire survivre ce
   réglage aux mises à jour du noyau.
3. **passt**, uniquement avec `image vm build --network`. libguestfs donne un réseau à l'appliance
   via passt. Son profil AppArmor (Debian/Ubuntu) interdit le répertoire d'exécution qu'utilise
   libguestfs, et le passt fourni dans Ubuntu 24.04 démarre mais ne distribue jamais de bail —
   `dhclient` attend ~300 s et le build continue **sans** réseau, pour échouer plus tard sur un miroir
   de paquets. Remèdes :

   ```bash
   mkdir -p /tmp/delonix-run && chmod 700 /tmp/delonix-run
   XDG_RUNTIME_DIR=/tmp/delonix-run ./target/debug/delonix image vm build --network …
   ```

   et, si cela échoue encore, un passt récent **en premier dans le `PATH`** (l'installateur en
   construit un dans `/usr/local/bin`). Ne « désactivez » pas passt avec un stub qui échoue :
   libguestfs utilise alors le stub et meurt dessus.

Le mode `--offline` de la recette dorée évite entièrement le piège 3 : il récupère et vérifie les
paquets sur l'hôte et exécute l'invité avec `--no-network`.

### L'isolation d'abord

Cette machine peut exécuter d'autres charges de travail. Avant toute commande `vm` allant au-delà de
`--help` :

```bash
export DELONIX_ROOT=$HOME/dlx-dev/root
export DELONIX_NET_RUNTIME_DIR=/tmp/dlx-dev-run     # keep it SHORT (AF_UNIX sun_path is 108 bytes)
export TMPDIR=$HOME/dlx-dev/tmp                     # VMfile builds put whole disks here
mkdir -p "$DELONIX_ROOT" "$DELONIX_NET_RUNTIME_DIR" "$TMPDIR"
```

Les deux racines, toujours : isoler uniquement `DELONIX_ROOT` laisse les sockets réseau partagées
avec l'état réel, et cela a déjà redémarré de vraies charges de travail (voir
[Cloner, compiler et tester](build-and-test.md)). Une VM Cloud Hypervisor refuse aussi d'emblée lorsque
`<root>/vms/<name>.sock` ne tiendrait pas dans `sun_path` (`ch_socket_paths_fit`) ; évitez donc les
chemins `DELONIX_ROOT` très profonds.

---

## 2. Les backends et comment l'un d'eux est choisi

### Le port et le registre

`VmBackend` (`crates/adapters/delonix-vm/src/lib.rs`) est le port que chaque hyperviseur
implémente : `id`, `available`, `boot`, `is_running`, `ip`, `stop`, plus des méthodes avec
implémentation par défaut (`destroy`, `pause`, `unpause`, `resume`,
`snapshot`/`restore`/`snapshots`/`delete_snapshot`, `preserve_snapshots`, `ip_is_predicted`,
`manages_own_storage`, `auto_selectable`, `disk_health`). Une implémentation par défaut qui ne peut
pas être honorée **échoue de manière fermée** avec un message, jamais par un no-op silencieux.

Les backends vivent dans un registre (`BACKENDS`), et non dans un `match` :

| Backend | Crate | Alias | Sélection automatique |
|---|---|---|---|
| `cloud-hypervisor` | `delonix-vm` (intégré) | `ch`, `cloudhypervisor` | oui |
| `libvirt` | `delonix-vm` (intégré) | `kvm`, `qemu` | oui |
| `proxmox` | `delonix-proxmox` (enregistré par la CLI) | — | **non** — sélectionné par nom uniquement |

`register_backend` refuse un id/alias qui appartient à un autre backend, et refuse
`auto_selectable: true` pour tout ce qui n'est pas intégré : l'auto-détection interroge
`available()` de chaque candidat, et un backend distant ne peut y répondre que par le réseau.
L'enregistrement ne fait aucune I/O ; la factory s'exécute la première fois que le backend est
sélectionné. `backend_for` sur une VM existante résout le backend enregistré, et un nom inconnu est
une **erreur** (auparavant, il retombait sur CH).

### Ordre de priorité de la sélection pour une nouvelle VM

D'après `delonix_vm::create_with` et `resolve_vm_defaults` (`cmd/vm.rs`), la première
correspondance l'emporte :

1. `--backend` (ou `backend:` dans le manifeste).
2. Le `HYPERVISOR` de l'image (enregistré par un build de VMfile), lorsque `--disk` désigne une
   image locale.
3. `DELONIX_VM_BACKEND` (pour toute la session).
4. `delonix vm default-backend --set <backend>` (pour toute la machine, stocké dans
   `<DELONIX_ROOT>/vm-default-backend`).
5. Heuristique de capacités : `volumes` présents ⇒ `libvirt` (seul libvirt fait du virtio-9p) ; une
   cloud image sans `--kernel` ⇒ `libvirt` **si libvirt est disponible** ; sinon auto-détection — le
   premier backend enregistré sélectionnable automatiquement qui est installé (CH, puis libvirt).

Ainsi, sur un hôte doté des deux hyperviseurs, un simple `vm create` d'une cloud image aboutit sur
**libvirt** ; passez `--backend cloud-hypervisor` (ou définissez un défaut) pour obtenir une microVM
sur le SDN.

```text
$ delonix vm default-backend
none (auto-detection: cloud-hypervisor if installed, else libvirt)
$ delonix vm default-backend --set ch
default backend set to cloud-hypervisor
$ delonix vm default-backend --set bogus
error invalid argument: unknown VM backend: 'bogus' (use 'cloud-hypervisor', 'libvirt')
$ delonix vm default-backend --clear
default backend cleared (falls back to auto-detection)
```

### Proxmox VE (distant)

`bins/delonix-runtime-bin/src/cmd/vmbackends.rs::register_configured` enregistre le backend Proxmox
au démarrage lorsqu'il est configuré par l'environnement (une mauvaise configuration est un
avertissement, jamais une erreur fatale pour des commandes sans rapport) :

| Variable | Signification |
|---|---|
| `DELONIX_PROXMOX_URL` | URL de base de l'API, p. ex. `https://pve.example:8006`. Non définie = backend non enregistré. |
| `DELONIX_PROXMOX_NODE` | Obligatoire. Le nœud unique auquel s'adresse ce backend (tel que `GET /nodes` le nomme). |
| `DELONIX_PROXMOX_SECRET` | Identifiant préféré : nom d'un `kind: Secret` avec `tokenId`+`tokenSecret` (ou `username`+`password`). |
| `DELONIX_PROXMOX_TOKEN_ID` + `DELONIX_PROXMOX_TOKEN` | Jeton d'API depuis l'environnement. |
| `DELONIX_PROXMOX_USER` + `DELONIX_PROXMOX_PASSWORD` | Connexion par mot de passe (ticket, réauthentifié sur 401). |
| `DELONIX_PROXMOX_INSECURE_TLS` | `1`/`true`/`yes` pour ignorer la vérification du certificat. Opt-in, jamais un repli. |
| `DELONIX_PROXMOX_BRIDGE` | Bridge par défaut sur le nœud (un `bridge` par VM l'emporte). |
| `DELONIX_PROXMOX_VLAN` | Tag VLAN par défaut, 1–4094 ; hors plage, c'est une erreur, et non une NIC silencieusement sans tag. |

Sans configuration, `--backend proxmox` répond que le backend « is not available in this build » et
indique ce qu'il faut définir (*exécuté*). Le backend possède son propre stockage
(`manages_own_storage`), donc aucun overlay local ni seed NoCloud n'est créé ; `--hostname`/`--ssh-key`
vont au cloud-init du nœud, et `--user-data` est refusé. Conception et limites :
[ADR-0008](../../adr/0008-proxmox-vm-backend.md). Un backend OpenStack est **seulement proposé**
([ADR-0039](../../adr/0039-openstack-vm-backend.md)) ; il n'existe aucun code pour lui.

---

## 3. Images

Les images VM vivent dans `VmImageStore` (`cmd/vmimage.rs`) sous `<DELONIX_ROOT>/vm-images/` : un
qcow2 plus un enregistrement de métadonnées JSON (`VmImage`) par image. `delonix image vm ls` les
liste avec `TYPE` (cloud-init / appliance) et `DEFAULTS` (vCPU/mémoire enregistrés).

### Images officielles

`OFFICIAL_REPOS` dans `cmd/vmimage.rs` :

| Clé | Dépôt | Contenu |
|---|---|---|
| `k8s` | `ghcr.io/angolardevops/delonix-vm-k8s` | nœud Kubernetes (kubeadm/kubelet/kubectl + `delonix-cri`) |
| `base` | `ghcr.io/angolardevops/delonix-vm-base` | OS de base avec le moteur `delonix`, sans Kubernetes (p. ex. `ubuntu-24.04`) |
| `appliances` | `ghcr.io/angolardevops/delonix-vm-appliances` | appliances de fournisseurs, sans cloud-init |

```bash
delonix vm ls-remote                 # tags of the Kubernetes golden repo
delonix vm ls-remote --no-k8s        # tags of the base repo
delonix vm pull                      # the official Kubernetes golden
delonix vm pull --no-k8s             # the official base image
delonix vm pull <oci-ref> --name <local-name>
```

Les images sont des artefacts OCI à blob unique ; le pull vérifie les digests du manifeste et du
blob, et restaure les métadonnées à partir des annotations du manifeste. Les mêmes verbes existent
sous la forme `delonix image vm pull/ls-remote/push`. **Remarque :** `vm create` sans `--disk` et
sans image locale télécharge l'image dorée officielle ; il a donc besoin du réseau.

### Construire la recette dorée

`delonix vm build -t <tag>` (la même commande que `delonix image vm build` ; les deux partagent un seul jeu
d'arguments) sans `vm.yaml` ni `VMfile` dans le contexte exécute la recette intégrée. Les exemples
ci-dessous utilisent la graphie `image vm` :

```bash
# Kubernetes node, packages fetched and verified on the HOST, guest offline
delonix image vm build --offline --k8s-version 1.34 -t delonix-vm-k8s:1.34
# no Kubernetes: just the engine, rootless-ready
delonix image vm build --no-k8s --distro debian --debian-release bookworm -t delonix-vm-base:debian-bookworm
```

Options pertinentes (consultez `image vm build --help` pour les valeurs par défaut) :
`--distro ubuntu|debian|rocky|fedora`, `--ubuntu-release`, `--debian-release`, `--rocky-release`,
`--fedora-release` (release **et** build, p. ex. `42-1.1`), `--k8s-version`, `--offline`,
`--no-k8s`, `--extra-package`, `--extra-run`, `--cri-bin`, `--delonix-bin`, `--root-password` (sans
elle, aucun compte n'a de mot de passe), `--node-exporter[=<addr>]`, `--no-compress`. Votre propre
recette est un `VMfile` ou un `vm.yaml` — voir [Delonixfile et VMfile](delonixfile-and-vmfile.md). Le dossier `images/` du
dépôt fournit un `vm.yaml` par distribution (`ubuntu`, `debian`, `rocky`, `fedora`, construites hors ligne) et par appliance ;
`scripts/verify-images.sh` les construit dans un `DELONIX_ROOT` isolé et relit le qcow2
(`--self-test` prouve que ses vérifications peuvent échouer). L'installation de paquets, les profils de la recette dorée, le démarrage
des images construites et les builders d'appliances **n'ont pas encore été validés** (notes de release v4.2.0). Les
builds d'images sont uniquement amd64 ([ADR-0018](../../adr/0018-vm-images-stay-amd64.md)).

### Convertir et importer

```bash
delonix vm convert <image-or-path> --to raw|qcow2|vmdk|vdi|vhdx|vhd [-o out] [--compress]
delonix image vm import disk.qcow2 -t opnsense:26.1 --appliance --default-vcpus 2 --default-memory 2G
```

`vm convert` aplatit (aucune chaîne de backing) ; `--compress` n'est accepté que pour `qcow2` et
`vmdk`. `import --appliance` enregistre `cloud_init: false` : `vm create` n'attache alors **aucun**
seed et refuse `--hostname`/`--ssh-key`/`--user-data` en les nommant, car l'invité ne les lirait
jamais. Les scripts de construction d'appliances se trouvent dans `scripts/appliances/`.

---

## 4. Créer et exécuter

### `vm create`

```bash
delonix vm create dev --disk delonix-vm-base:ubuntu-24.04 \
  --backend cloud-hypervisor --vcpus 2 --memory 2G \
  --ssh-key @$HOME/.ssh/id_ed25519.pub --hostname dev --wait
```

Ce qui se passe (`cmd/vm.rs` → `delonix_vm::create_with`) :

1. La **politique du nœud** est imposée avant la résolution de toute image (`policy::enforce`).
2. **Résolution du disque** (`resolve_image_ref`) : `--url-img` l'emporte (téléchargé, mis en cache,
   vérifié contre `<url>.sha256` lorsqu'il est proposé) ; sinon `--disk` est recherché comme nom
   d'image locale, puis comme chemin vers le qcow2 d'une image stockée ; sinon il est utilisé comme
   simple chemin ; sans `--disk`, c'est l'unique image dorée locale, ou l'image officielle est
   récupérée par pull.
3. Les **valeurs par défaut** issues des métadonnées de l'image complètent
   `--vcpus`/`--memory`/`--backend` uniquement là où ils ne sont pas définis.
4. **Seed** : une ISO NoCloud (configuration réseau par MAC, hostname, clés) est générée, sauf si
   `--seed` est fourni, si l'image est une appliance ou si le backend est distant. `--user-data`
   remplace le user-data généré.
5. **Backend** choisi (section 2) ; une vérification d'admission refuse lorsque l'hôte manque de
   RAM ; un `--namespace` autre que `default` est refusé sur libvirt (la VM vit sur `virbr0`, en
   dehors du SDN Delonix).
6. **Overlay** : `<root>/vms/<name>.qcow2`, un qcow2 mince au-dessus de la base
   (`prepare_local_overlay`) ; `--disk-size <GiB>` l'agrandit et ne peut pas être inférieur à la
   base.
7. **Démarrage** : le `boot` du backend. `create` est idempotent : une VM existante et en cours
   d'exécution est renvoyée telle quelle.

Les images publiées ne définissent de mot de passe sur aucun compte (voir `--root-password`
ci-dessus) ; passez donc `--ssh-key` si vous voulez vous connecter en SSH.

**`--wait` et l'IP prédite.** Sur libvirt, l'IP provient d'un vrai bail DHCP ; en avoir une prouve
donc que l'invité a démarré. Sur Cloud Hypervisor, l'IP est **calculée à partir de la MAC** avant
que l'invité ne s'exécute (`ip_is_predicted`) ; `--wait` sonde donc aussi l'adresse par ARP depuis
l'intérieur du holder réseau (`delonix_sdn::infra::sdn_reachable`) jusqu'à `--boot-timeout` (120 s
par défaut). Trois issues : démarrée ; « which could not be verified from here » (la sonde n'a pas
pu être lancée) ; « is running but never answered … computed from the MAC, not observed ». Utilisez
`vm console` pour voir pourquoi.

### Verbes du quotidien (day-2)

| Commande | Remarques |
|---|---|
| `delonix vm ls [--namespace <ns>] [--ports] [-o json]` | liste les VMs |
| `delonix describe vm <name>` / `delonix delete vm <name>` | il n'existe ni `vm describe` ni `vm rm` ; les verbes génériques les remplacent |
| `delonix vm console <name> [-e ^X]` | console série ; détachement avec `Ctrl-]` par défaut (`$DELONIX_CONSOLE_ESCAPE`). libvirt : `virsh console` comme processus enfant ; CH : la socket de console `<root>/vms/<name>.console` |
| `delonix vm ssh <name\|ip> [-l user] [-i key] [-- cmd]` | IP issue de l'enregistrement ; utilisateur par défaut `delonix` sur les images cloud-init, `root` sur les appliances |
| `delonix vm vnc <name>` | uniquement pour les VMs libvirt créées avec `--vnc` (CH n'a pas d'affichage) |
| `delonix vm stop <name>` | conserve le disque, l'enregistrement et les snapshots. libvirt : le domaine est supprimé de la définition (undefine) (les métadonnées des snapshots sont préservées d'abord) |
| `delonix vm start <name>` / `restart <name>` | reconstruisent le démarrage à partir de l'enregistrement et réutilisent l'overlay ; `start` sur une VM en cours d'exécution est un no-op, `restart` redémarre toujours |
| `delonix vm pause` / `unpause <name>` | suspendent les vCPUs, la mémoire est conservée en RAM ; CH et libvirt |
| `delonix vm migrate <name> --host <h> --network <n>` | stop-copy-start par SSH vers un autre hôte ; interruption réelle ([ADR-0031](../../adr/0031-live-vm-migration-no-go.md) explique pourquoi la migration à chaud est hors périmètre) |
| `delonix vm prune` | récupère l'état dont aucun enregistrement de VM ne rend compte |

### Snapshots

`delonix vm snapshot create|ls|rm|restore <vm> [<snapshot>]` :

| Backend | VM en cours d'exécution | VM arrêtée |
|---|---|---|
| libvirt | `create` est un checkpoint système (mémoire + disque) | disque uniquement ; les quatre verbes définissent le domaine juste le temps de la commande |
| cloud-hypervisor | `ls` uniquement (`qemu-img info -U`) ; `create`/`restore`/`rm` **refusés** — le VMM en cours d'exécution verrouille le disque et CH n'a pas d'API de snapshot disque à chaud | les quatre, via `qemu-img snapshot` |
| proxmox | `create` (avec l'état de la VM), `ls`, `restore` | idem ; `rm` non implémenté (échoue de manière fermée) |

Les snapshots libvirt survivent à `vm stop`/`vm start` : `undefine --snapshots-metadata` ne supprime
que la comptabilité de libvirt ; `preserve_snapshots` exporte donc le XML de chaque snapshot dans
`<root>/vms/<vm>/snapshots/` avant l'arrêt, et `boot` les redéfinit (en réécrivant l'uuid du
domaine, qui change à chaque définition).

### Déclaratif

`kind: VirtualMachine` (`compute.delonix.io/v1alpha1`) reflète `vm create` ; un exemple complet et
annoté se trouve dans `examples/vm.yaml`. `kind: Workload` avec `type: microvm` se réduit à une
`VirtualMachine` dont le backend est **forcé** à `cloud-hypervisor` ; demander un autre backend est
une erreur ([ADR-0006](../../adr/0006-workload-type-microvm.md)). Les deux ont été *exécutés* dans une
racine jetable :

```yaml
apiVersion: compute.delonix.io/v1alpha1
kind: VirtualMachine
metadata: { name: dev }
spec:
  disk: delonix-vm-base:ubuntu-24.04
  resources: { vcpus: 2, memory: 2G }
  cloudInit:
    hostname: dev
    sshKeys: ["@~/.ssh/id_ed25519.pub"]
---
apiVersion: compute.delonix.io/v1alpha1
kind: Workload
metadata: { name: fast }
spec:
  type: microvm
  microvm: { disk: delonix-vm-base:ubuntu-24.04, vcpus: 1, memory: 1G }
```

```text
$ delonix manifest validate -f vm.yaml
stack validate: OK — 2 document(s), all references resolved
$ delonix stack apply -f vm.yaml --dry-run | grep backend
  backend: null
  backend: cloud-hypervisor
```

Appliquez avec `delonix vm apply -f vm.yaml` ou `delonix stack apply -f vm.yaml` (non exécuté ici).

---

## 5. Réseau pour les VMs

| | Cloud Hypervisor | libvirt |
|---|---|---|
| Où vit la NIC | un tap sur un réseau Delonix (`--network`, par défaut `ingress`) **à l'intérieur du holder réseau** — le même SDN que les containers | `virbr0` (nat) ou un bridge de l'hôte, dans le namespace réseau de l'hôte |
| IP | bail du DHCP du holder, déterministe à partir de la MAC | DHCP de libvirt ; `--ip` en réserve une (nat uniquement) |
| Isolation par namespace | oui (`--namespace`) | refusée |
| VM ↔ container par IP | directe | container → VM fonctionne via l'hôte ; VM → container nécessite un port publié ou `vm bridge` |

**`vm reach`** (lecture seule, sans privilège) liste les passerelles libvirt et, pour chaque port
publié par un container en cours d'exécution, indique si une VM peut l'atteindre. Un port publié sur
le `127.0.0.1` par défaut est invisible pour les VMs ; la commande affiche la republication exacte,
p. ex. `DELONIX_PUBLISH_ADDR=<gateway> delonix net ingress publish <c> <port>` — joignable depuis les
VMs de ce réseau, pas depuis le LAN externe.

**`vm bridge <network> [--vm-subnet <cidr>] [--apply]`** est **expérimental et nécessite root** : une
veth de l'hôte vers le holder plus des routes, qui donne aux VMs libvirt une joignabilité IP directe
vers un réseau de containers. Sans `--apply`, la commande n'affiche que le plan.
`vm unbridge <network>` le démonte (également en dry-run sans `--apply`). C'est la seule exception
délibérée au rootless dans le code VM (`cmd/vmbridge.rs`).

---

## 6. Dépannage

| Symptôme | Cause | Correctif |
|---|---|---|
| `/dev/kvm does not exist` / la VM ne démarre pas | virtualisation désactivée, ou pas de virtualisation imbriquée | activez VT-x/AMD-V ; dans une VM, activez la virtualisation imbriquée |
| `no VM backend available` | ni `cloud-hypervisor` ni `virsh`+`qemu-system-x86_64` dans le `PATH` | installez-en un ; `scripts/install.sh` le fait |
| VM CH « en cours d'exécution » mais ne répond jamais ; l'overlay reste minuscule | le firmware ne peut pas démarrer l'image (p. ex. seul `hypervisor-fw` est installé) | installez EDK2 `CLOUDHV.fd` dans `/usr/local/share/delonix/`, ou définissez `DELONIX_HYPERVISOR_FW`, ou utilisez `--backend libvirt` |
| `vm ls` n'affiche aucune IP pour une VM libvirt | repli sur `qemu:///session` en mode utilisateur | rejoignez le groupe `libvirt` et reconnectez-vous, ou `--net-mode nat` |
| `warning: cannot reach qemu:///system for NAT networking` | pas dans le groupe `libvirt` | `sudo usermod -aG libvirt $USER`, nouvelle session |
| `vm console` sur libvirt : « Active console session exists » | une session précédente est morte de manière non propre | le code actuel passe `--force` à `virsh console` ; mettez à jour votre binaire |
| VM CH refusée avant le démarrage en mentionnant le chemin de la socket | `<root>/vms/<name>.sock` dépasse 108 octets | `DELONIX_ROOT` ou nom de VM plus court |
| `namespace '…' is not enforceable on the 'libvirt' backend` | les VMs libvirt sont en dehors du SDN | `--backend cloud-hypervisor`, ou retirez `--namespace` |
| `vm snapshot create` refusé sur une VM CH en cours d'exécution | CH n'a pas de snapshot disque à chaud | `vm stop` d'abord, ou utilisez libvirt |
| `--hostname`/`--ssh-key` refusés | l'image est une appliance (`cloud_init: false`) | configurez l'appliance via sa propre console/interface |
| `cloud-localds not found` | `cloud-image-utils` manquant | installez-le (nécessaire pour chaque `vm create` cloud-init) |
| `image vm build` : `cp: cannot open '/boot/vmlinuz-…'` | le noyau de l'hôte est en 0600 | `install.sh --with-image-build`, ou `sudo chmod 0644 /boot/vmlinuz-*` (abaisse une frontière) |
| `image vm build` : `Temporary failure resolving …` | l'appliance n'a pas de réseau : client DHCP de l'hôte manquant, ou passt | installez `isc-dhcp-client` ; pour `--network`, utilisez `XDG_RUNTIME_DIR=/tmp/delonix-run` et un passt récent en premier dans le `PATH` ; ou construisez avec `--offline` |
| une étape se met en pause ~300 s, puis les installations de paquets échouent | passt n'a jamais distribué de bail ; `dhclient` a expiré | comme ci-dessus |
| `--offline belong to the built-in golden recipe …` | option de la recette dorée utilisée avec un VMfile | retirez-la ; le VMfile décrit le build |
| `` `--network` is for VMfile builds `` | `--network` sans VMfile | utilisez `--offline` pour la recette dorée |
| le build d'un VMfile remplit `/tmp` | chaque étape est un disque complet aplati sous `$TMPDIR` | définissez `TMPDIR` sur un système de fichiers volumineux ; supprimez les répertoires `delonix-vmfile-*` restants après des échecs |
| `VM backend 'proxmox' is not available in this build` | le backend n'est pas configuré | définissez les variables `DELONIX_PROXMOX_*` (section 2) |

---

## 7. Pour les contributeurs

### Où se trouve le code

| Domaine | Chemin |
|---|---|
| Port, registre, backends CH et libvirt, `create_with`, snapshots, recherche du firmware | `crates/adapters/delonix-vm/src/lib.rs` |
| Génération du seed NoCloud | `crates/adapters/delonix-vm/src/cloudinit.rs` |
| Backend Proxmox | `crates/providers/delonix-proxmox/` |
| CLI `vm`, `kind: VirtualMachine`, `vm reach` | `bins/delonix-runtime-bin/src/cmd/vm.rs` |
| Store d'images, recette dorée, pull/push/import/convert, indications d'échec | `bins/delonix-runtime-bin/src/cmd/vmimage.rs` |
| VMfile | `bins/delonix-runtime-bin/src/cmd/vmfile.rs` |
| Enregistrement des backends depuis l'environnement | `bins/delonix-runtime-bin/src/cmd/vmbackends.rs` |
| `vm bridge` | `bins/delonix-runtime-bin/src/cmd/vmbridge.rs` |
| Réduction de `kind: Workload` | `bins/delonix-runtime-bin/src/cmd/workload.rs` |
| Construction d'appliances | `scripts/appliances/` |

### Ajouter un backend

Lisez d'abord [ADR-0008](../../adr/0008-proxmox-vm-backend.md) ; c'est le modèle. En résumé :

- Implémentez `VmBackend` dans **son propre crate** s'il parle à une API distante (le crate du moteur
  reste exempt de clients HTTP), placé dans le répertoire de sa couche et listé dans
  `scripts/arch_fitness.py` (voir [Architecture](architecture.md)).
- Enregistrez-le avec `register_backend` depuis le processus qui connaît sa configuration, avec
  `auto_selectable: false`, sauf s'il s'agit d'un backend local, sans configuration, intégré à
  `delonix-vm`.
- Surchargez délibérément `manages_own_storage`, `destroy`, `resume` et `ip_is_predicted` : pour un
  backend distant, `stop` et `destroy` ne sont **pas** la même opération, et `boot` sur une VM
  arrêtée ne doit pas en créer une seconde.
- Laissez les verbes non pris en charge sur leurs implémentations par défaut qui échouent de manière
  fermée ; refusez par leur nom les champs de `VmConfig` non pris en charge avant de créer quoi que ce
  soit.
- Ne publiez pas un backend qu'on n'a jamais vu démarrer une VM. Les nouveaux backends et les
  nouvelles frontières d'hyperviseur passent par un ADR ([docs/adr/](../../adr/)).

### Tests

- **Tests unitaires purs** — les constructeurs d'argv et de XML sont des fonctions pures, ils se
  testent donc sans hyperviseur : p. ex. `libvirt_snapshot_argv_uses_flags_not_positional`,
  `snapshot_xml_with_uuid`, `libvirt_domain_xml`, le test de l'ordre des firmwares ci-dessus, les
  tests du registre et de `auto_detect`, et les tests de parseur/scaffold dans `cmd/vmfile.rs`.
  Exécutez `cargo test -p delonix-vm` et `cargo test -p delonix-runtime-bin vmfile` (voir
  [Cloner, compiler et tester](build-and-test.md) pour `protoc` et le répertoire cible).
- **`scripts/e2e.sh`** — les sections `vm` s'exécutent sans hyperviseur (listage, refus) et, lorsque
  c'est disponible, exercent les snapshots à travers stop/start sur libvirt (nécessite `virsh`,
  `qemu-img` et un `qemu:///system` utilisable) et sur Cloud Hypervisor. Le script isole les deux
  racines d'état par défaut ; la section CH refuse de s'exécuter si seule `DELONIX_ROOT` est isolée.
  Les sections dont l'hyperviseur est absent sont signalées comme ignorées, et non comme réussies.
- **Test en conditions réelles Proxmox** — `crates/providers/delonix-proxmox/tests/live.rs` crée et
  détruit une VM contre un vrai nœud et est ignoré sauf si `DELONIX_PROXMOX_TEST_URL` (ainsi que
  `_NODE`, `_USER`, `_PASS`) est défini : `cargo test -p delonix-proxmox --test live -- --nocapture`.
  Utilisez un nœud jetable.
- **Cycle de vie des providers** — lorsque vous modifiez `delonix-vm` ou un provider, prouvez le cycle
  de vie complet (create, stop, start, snapshot, destroy) **uniquement à travers la CLI `delonix`**,
  en observant l'hyperviseur en lecture seule (`virsh -r`, `GET` d'API), sans jamais le réparer à la
  main entre les étapes.
- **Regardez, ne devinez pas.** Lorsqu'un invité ne démarre pas, la console série (`vm console`) ou
  une capture d'écran libvirt répond en quelques secondes à ce que des hypothèses mettent des heures
  à trouver — et validez avec la commande qu'un utilisateur taperait, et non avec les options
  pratiques pour le débogage (`--vnc` a un jour masqué un échec de démarrage qui ne se produisait
  que sans périphérique vidéo).

---

**Suivant :** [Dépannage](troubleshooting.md) — un index organisé par symptôme pour les gates et les pièges d'hôte que vous pouvez rencontrer en construisant et en testant.
