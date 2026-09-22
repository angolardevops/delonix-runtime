<!-- translated-from: environment.md sha256:0505a35c5a1e921e51ecd51f7feb5352158dd1985176b27ff3e1a76b6adc1719 -->
# Préparer votre environnement

**Avant de lire :** [Commencer ici](start-here.md#day-0-in-30-minutes) (Jour 0) et [Fondations Linux](linux-foundations.md) — les pièges de l’hôte ci-dessous sont expliqués en termes de user namespaces et de délégation de cgroup.

Delonix Runtime est **exclusivement Linux** : chaque primitive qu’il utilise — namespaces, cgroups v2, nftables,
`pivot_root`, la nouvelle API de montage — vit dans le noyau Linux. Vous pouvez *compiler* la plus grande partie du
workspace et exécuter ses tests de logique pure sur n’importe quelle machine Linux disposant de la toolchain ci-dessous ; pour *exécuter*
des containers et exercer les chemins réels, il vous faut un hôte qui satisfait les exigences de noyau et de
paquets de cette page. Après elle, votre hôte passe `delonix system info`, et quand quelque chose
échoue vous savez distinguer un prérequis de l’hôte d’un bug du moteur.

Une grande partie de ce qui ressemble à un bug du moteur sur une machine neuve est un prérequis de l’hôte. Lisez la
section [Pièges connus de l’hôte](#known-host-traps) avant d’ouvrir une issue.

## Toolchain

<!-- dev-docs:begin toolchain -->
- **Chaîne d'outils Rust :** `1.96.0` (épinglée dans `rust-toolchain.toml` ; `rustup` l'installe au premier appel à `cargo`)
- **Composants :** `rustfmt`, `clippy`
<!-- dev-docs:end toolchain -->

Installez [`rustup`](https://rustup.rs/) et laissez-le prendre le canal épinglé ; ne le remplacez pas
par `stable`. La CI utilise exactement le même fichier (`rustup show` dans chaque job).

### `protoc` (requis pour compiler)

`crates/interfaces/delonix-cri/build.rs` compile le protobuf du CRI de Kubernetes avec
`tonic-build`/`prost`, qui a besoin du compilateur Protocol Buffers dans le `PATH`. Le binaire `delonix`
dépend de `delonix-cri`, donc **un simple `cargo build --workspace` échoue sans lui** :

```bash
# Debian / Ubuntu
sudo apt install protobuf-compiler
# Fedora / RHEL family
sudo dnf install protobuf-compiler
```

Ou téléchargez une release depuis <https://github.com/protocolbuffers/protobuf/releases>. La CI installe
`protobuf-compiler` depuis apt dans chaque job qui compile.

### Outils optionnels, uniquement pour certains gates

| Outil | Nécessaire pour | Où il est épinglé |
|---|---|---|
| Python 3.11+ | chaque gate `scripts/*.py` (ils utilisent `tomllib`) | — |
| `buf` v1.73.0 et `protoc-gen-openapi` v0.7.1 (toolchain Go pour les installer) | `scripts/contract_gate.py` | job `contract` dans `.github/workflows/ci.yml` |
| `cargo-deny` | la vérification de la chaîne d’approvisionnement | job `deny`, configuration dans `deny.toml` |
| Module Python `markdown` | `docs/gen.py` (produit le rendu de `ARCHITECTURE.md`) | job `docs` |
| `groff` | vérifier les pages de manuel générées | job `docs` |

Voir [Cloner, compiler et tester](build-and-test.md) pour la façon d’exécuter chacun d’eux.

## Exigences du noyau

Voici les fonctionnalités du noyau sur lesquelles le moteur s’appuie. L’installateur (`scripts/install.sh`) et
`delonix system doctor` vérifient la plupart d’entre elles pour vous.

| Exigence | Pourquoi | Comment vérifier |
|---|---|---|
| **cgroup v2** (hiérarchie unifiée) | les limites de ressources et la comptabilité sont écrites dans `/sys/fs/cgroup` | `stat -fc %T /sys/fs/cgroup` affiche `cgroup2fs` |
| **User namespaces non privilégiés** | le modèle rootless : le moteur ne devient « root » qu’à l’intérieur de son propre user namespace | `unshare -r -n true` réussit |
| **`/dev/net/tun`** | `slirp4netns` (réseau rootless) et les taps des VM | `test -e /dev/net/tun` |
| **overlayfs** avec la nouvelle API de montage et `lowerdir+` (Linux **6.5** ou plus récent) | les systèmes de fichiers racine des containers sont des montages overlay construits avec `fsopen`/`fsconfig`/`fsmount`, un appel `lowerdir+` par couche — voir [ADR-0037](../../adr/0037-overlay-mount-new-api.md) | `uname -r` |
| **`br_netfilter`** chargé, `net.bridge.bridge-nf-call-iptables=1` | l’isolation des namespaces est imposée dans les chaînes nftables `forward` ; sans ce module, le trafic entre deux containers sur le même bridge ne les atteint jamais, et l’isolation est silencieusement inerte | `delonix system doctor` |
| **KVM** (`/dev/kvm`) | uniquement pour les microVM — voir [Construire des microVMs](microvm-setup.md) | `test -w /dev/kvm` |

L’exigence 6.5 n’a **aucune vérification préalable** : un noyau plus ancien échoue au montage du premier rootfs
de container, pas au démarrage (l’ADR-0037 consigne cela comme un choix délibéré).

Sur les anciens noyaux Debian, les user namespaces non privilégiés sont désactivés par
`kernel.unprivileged_userns_clone=0` ; l’installateur le met à `1`.

## Paquets de l’hôte

La source de vérité est [`scripts/install.sh`](../../../scripts/install.sh), qui est aussi
l’installateur officiel (il est publié comme asset de release). Il détecte le gestionnaire de paquets via
`/etc/os-release` et prend en charge **apt** (Debian, Ubuntu et dérivées), **dnf** (Fedora, RHEL,
CentOS Stream, Rocky, AlmaLinux), **zypper** (openSUSE, SLES) et **pacman** (Arch et
dérivées). L’installateur installe des binaires précompilés pour **x86_64** et **aarch64** (`arm64` est
normalisé) : le nom de l’asset est composé à partir de `uname -m` sous la forme `<name>-<arch>-linux`, et toute autre
architecture s’arrête avec « no prebuilt binary for <arch> yet » ; compilez alors depuis les sources. Trois choses
diffèrent sur aarch64 : la variante `-v3` est un niveau de microarchitecture x86-64 et n’y existe pas ; le Cloud Hypervisor
statique épinglé, l’EDK2 `CLOUDHV.fd` et `hypervisor-fw` sont des builds x86-64, donc ils ne sont pas téléchargés (un
paquet `cloud-hypervisor` de la distribution est tout de même installé si le gestionnaire de paquets le propose) et le
backend de VM est libvirt ; et la sonde QEMU demande `qemu-system-aarch64`. Les assets de release aarch64 existent depuis
la v4.2.0 (la release v4.1.0 n’en a aucun).
**Non validé :** une installation complète sur un vrai hôte aarch64, et les noms des paquets QEMU par distribution sur
arm64 — ce qui a été vérifié, c’est la composition du nom d’asset face aux assets v4.2.0 publiés.

Pour exécuter des containers, le moteur a besoin de :

| Commande | Paquet (noms apt / dnf) | Pourquoi |
|---|---|---|
| `slirp4netns` | `slirp4netns` | réseau rootless et ports publiés — sans lui, `run -p` échoue |
| `newuidmap` / `newgidmap` | `uidmap` / `shadow-utils` | utilitaires setuid qui mappent plus d’un uid dans le user namespace ; sans eux, les images avec un utilisateur non root échouent dans `chown()` |
| `nft` | `nftables` | pare-feu SDN, isolation et DNAT des ports |
| `ip` | `iproute2` / `iproute` | câblage veth, bridge et netns |
| `conntrack` (optionnel) | `conntrack` / `conntrack-tools` | nettoyage des connexions lorsqu’un port est dépublié |

Pour les VM, `qemu-img`, `cloud-localds` (`cloud-image-utils`), `virsh` (libvirt) et/ou
Cloud Hypervisor avec son firmware — voir [Construire des microVMs](microvm-setup.md). Pour construire des images de VM,
`libguestfs-tools` (`install.sh --with-image-build`).

Il vous faut aussi une **plage d’uid/gid subordonnés** pour votre utilisateur dans `/etc/subuid` et `/etc/subgid` ;
sans elle, le user namespace ne peut mapper qu’un seul uid.

### Le chemin le plus rapide vers un hôte fonctionnel

Vous n’avez pas à reproduire l’installateur à la main. Pour installer uniquement les dépendances et la
configuration de l’hôte, en conservant le binaire que vous compilez vous-même :

```bash
bash scripts/install.sh --no-binary
```

Lisez d’abord la liste des options en tête du script : certaines options modifient des réglages de sécurité à l’échelle de l’hôte
(`--low-ports` permet à n’importe quel programme local de se lier aux ports à partir de 80, `--with-image-build` rend
`/boot/vmlinuz-*` lisible par tous), et `--no-tune` saute les modules du noyau et les sysctls, y compris
`br_netfilter`. `--performance` / `--no-performance` contrôlent le mode performance du CPU, les hugepages
transparentes avec `irqbalance`, et un timer utilisateur qui exécute `system prune --auto --threshold 75` ; sans
aucune de ces options, l’installateur pose la question pour chacun et, sans terminal, répond non. La partie CPU est un
service systemd qui mémorise les valeurs du démarrage et les remet lors du `stop`. Rien de cela n’est nécessaire pour
développer.

### Mémoire et disque

Il n’y a pas de minimum fixe. Ce qui coûte des ressources, c’est ce que vous exécutez : images et couches de containers,
disques de VM, et le répertoire Rust `target/` lui-même (les builds de débogage de tout le workspace occupent
plusieurs gigaoctets). Surveillez l’espace disque libre — les kubelets d’un cluster local commencent à évincer
des pods en cas de pression sur le disque, ce qui ressemble alors à un problème du moteur.

## Diagnostiquer l’hôte

Compilez le binaire (voir [Cloner, compiler et tester](build-and-test.md)) et interrogez-le. Les commandes ci-dessous sont
en lecture seule, sauf si vous passez `--delegate` :

```bash
./target/debug/delonix system doctor          # is every prerequisite met? says how to fix each
./target/debug/delonix system info            # rootless?, cgroup delegation, network infra, counts
./target/debug/delonix system setup           # diagnose cgroup delegation
./target/debug/delonix system resources       # which controllers are delegated, which flags are ignored
```

`delonix system doctor --strict` se termine avec un code non nul lorsqu’une vérification échoue, ce qui est utile dans un
script de provisionnement. Si vous exécutez ces commandes sur une machine où une installation de Delonix est déjà utilisée,
isolez d’abord l’état (voir [Isoler l’état du moteur](build-and-test.md#isolating-the-engines-state)).

## Pièges connus de l’hôte

### Ubuntu 23.10+ : AppArmor bloque les user namespaces pour votre binaire de développement

Les versions récentes d’Ubuntu fixent `kernel.apparmor_restrict_unprivileged_userns=1`. Un binaire sans profil AppArmor
ne peut alors pas créer de user namespace, et le moteur meurt à `unshare()` avec `EPERM` —
ce qui ressemble à un bug du moteur.

`install.sh` installe un profil (`/etc/apparmor.d/delonix`, `flags=(unconfined)` avec `userns`),
mais ce profil est **lié à un seul chemin** : `<install dir>/delonix` (`/usr/local/bin/delonix` par
défaut, `~/.local/bin/delonix` avec `--user`). Un binaire que vous venez de compiler dans
`target/debug/delonix` — ou copié dans `/tmp` — n’est **pas** couvert.

Options, de la moins à la plus invasive :

1. Ajoutez un second profil pour votre chemin de développement (par exemple le
   `target/debug/delonix` de votre worktree), sur le même modèle que celui qu’écrit l’installateur, et chargez-le
   avec `sudo apparmor_parser -r <file>`. Cela ne touche à rien de ce qui est déjà en cours d’exécution.
2. Installez votre build sur le chemin couvert par le profil (`sudo install -m 0755 target/debug/delonix /usr/local/bin/`)
   — **uniquement sur une machine où aucun workload Delonix n’est utilisé**. Le binaire installé est celui que les unités de démarrage
   (`ExecStart=<exe> container start …`) et les serveurs ré-exécutent ; sur un hôte avec des
   workloads actifs, un build de débogage deviendrait donc silencieusement le moteur de production.
3. Fixez `kernel.apparmor_restrict_unprivileged_userns=0` — cela abaisse une frontière à l’échelle de l’hôte ; seulement
   sur une machine qui vous appartient.

### Délégation de cgroup : certaines limites sont refusées, d’autres ne sont pas appliquées

Les limites de ressources n’atteignent le noyau que si le shell depuis lequel vous lancez le moteur se trouve dans un cgroup
**délégué**. C’est une règle de cgroup v2, pas une limitation de Delonix — Podman rootless a la même
exigence. Sans délégation, le moteur fait deux choses différentes, selon l’option :

- `-m`/`--memory`, `-c`/`--cpus` et `--cpu-weight` : `container run` **refuse** avant de créer
  quoi que ce soit, avec une erreur qui nomme le correctif, et sort avec **69** (`Error::Unavailable`, la classe
  `EX_UNAVAILABLE` — `preflight_resource_limits` dans
  `bins/delonix-runtime-bin/src/cmd/container.rs`). `DELONIX_ALLOW_UNENFORCED_LIMITS=1` exécute
  malgré tout le container, sans limite, avec un avertissement.
- `--cpuset`, `--io-weight` et la famille `--device-read-bps`/`--device-write-bps`/`--device-read-iops`/
  `--device-write-iops` ne sont **pas** vérifiés par cette sonde : ils sont acceptés et appliqués
  au mieux, donc sans les contrôleurs `cpuset`/`io` ils n’ont aucun effet et rien n’échoue.

Il n’existe pas d’option `--pids-limit` ; le plafond de pids est une propriété du groupe de cgroups du moteur, pas
de `container run`.

Le cas courant est une **session SSH** : sa scope se trouve en dehors de votre sous-arborescence
déléguée, et la session ne peut pas s’y déplacer elle-même — le pourquoi, avec les commandes pour le
voir, se trouve dans [Fondations Linux — Délégation aux
utilisateurs](linux-foundations.md#delegation-to-users). Le correctif par commande ne nécessite
pas root :

```bash
systemd-run --user --scope -p Delegate=yes -- ./target/debug/delonix container run -d -m 128M alpine sleep 60
```

Pour les workloads de longue durée, utilisez une unité systemd **utilisateur** avec `Delegate=yes`. Certains hôtes ne délèguent que
`cpu memory pids` aux sessions utilisateur ; `cpuset` et `io` peuvent ne jamais être disponibles en rootless, et
`delonix system resources` nomme les options qui seront ignorées. `delonix system setup --delegate`
écrit un drop-in à l’échelle du système (nécessite root, prend effet à la prochaine connexion) lorsque le contrôleur
`cpu` lui-même est absent.

### Un `delonix` obsolète dans votre `PATH`

Si Delonix est installé sur la machine, le `delonix` de votre `PATH` est la release installée, pas votre
arbre. Exécutez toujours `./target/debug/delonix` (ou `target/release/delonix`) lorsque vous testez une modification.
`--version` affiche le commit et la distance depuis le dernier tag
(`commit: <hash> (+N commits since vX.Y.Z)`), car entre deux releases, deux builds partagent le même
numéro de version.

### Autres pièges que vous pourriez rencontrer

- **Les ports inférieurs à 1024** échouent en rootless avec `slirp_add_hostfwd failed` : le port est lié par
  `slirp4netns`, un processus non privilégié. Utilisez un port élevé, ou activez-les explicitement avec `install.sh --low-ports`.
- **Les runners de CI hébergés** (hébergés par GitHub) bloquent les user namespaces non privilégiés. Le workflow de chaos
  le détecte et signale `skipped`, pas `success` ; pour exercer les chemins réels, il vous faut un vrai
  hôte, un runner auto-hébergé ou une VM.
- **Les pièges du firmware des VM et de la construction d’images** (le choix du firmware de Cloud Hypervisor, un `passt` ancien
  dans les builds libguestfs, les permissions de `/boot/vmlinuz-*`) sont traités dans [Construire des microVMs](microvm-setup.md).

---

**Suivant :** [Cloner, compiler et tester](build-and-test.md) — compiler, installer et tester votre arborescence, et chaque gate de CI comme commande locale.
