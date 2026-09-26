<!-- translated-from: environment-variables.md sha256:89a40a69d327ce2599cf5b5695e2cc96151828496b52fecabe7e4b62bdc71387 -->
# Variables d’environnement (`DELONIX_*`)

**Avant de lire :** [Isoler l'état du moteur](build-and-test.md#isolating-the-engines-state) dans Cloner, construire et tester.

Cette page recense chaque nom `DELONIX_*` qui apparaît dans le code du moteur, avec l’endroit où il est lu,
ce qu’il change, et s’il vous arrivera un jour de le définir. C’est une référence : lisez la section qui
correspond à ce que vous faites, pas la page entière. Après elle, vous pouvez isoler une exécution, activer les diagnostics dont vous avez besoin, et reconnaître les variables qui abaissent une frontière de sécurité avant d'en définir une.

## Comment lire cette page

Chaque ligne des tableaux de variables a cinq colonnes :

- **Lu par** — le crate ou le binaire et le `path:symbol` où la valeur est lue (ou écrite).
  Les chemins sont relatifs à la racine du dépôt.
- **Rôle** — ce que la variable change.
- **Valeurs / défaut** — comment le code l’analyse. Lorsque le code se rabat silencieusement sur un défaut pour une valeur qu’il
  ne sait pas analyser, la ligne le dit.
- **Remarques** — quand l’utiliser, et tout avertissement.

La plupart des variables sont lues par le processus qui en a besoin, et **ne sont pas** transmises automatiquement. Quelques-unes sont
définies par le moteur lui-même sur les processus qu’il démarre ; elles figurent dans
[Définies par le moteur lui-même](#set-by-the-engine-itself-internal) et vous ne devez pas les définir.

**Ce tableau est contrôlé par la CI dans les deux sens.** `python3 scripts/dev_docs.py --check`
(fonction `env_var_problems` dans `scripts/dev_docs.py`) extrait chaque nom `DELONIX_*` des littéraux de chaîne
Rust sous `crates/` et `bins/` (y compris `build.rs` et `env!()`) et de
`scripts/install.sh`, et échoue lorsqu’un nom manque ici ou lorsque cette page liste un nom que le code
ne contient plus. Une ligne ne compte que si elle commence par `` | `DELONIX_NAME` ``. L’extraction
est textuelle ; elle ramasse donc aussi quelques noms qui **ne sont pas** des variables d’environnement (une
constante Rust, des fixtures de test, une clé écrite dans une image de VM) ; ceux-ci sont listés dans leurs propres
sections afin que le contrôle reste exact.

Les variables utilisées uniquement par des scripts hors de ce périmètre — par exemple `DELONIX_CHAOS_DIR` et la
famille `DELONIX_CHAOS_TRUENAS_*` dans `scripts/chaos.sh` — sont documentées dans l’en-tête de chaque
script et dans [Cloner, compiler et tester](build-and-test.md), pas ici.

### Précédence

Lorsque le code a une règle de précédence, c’est en général **flag > environnement > défaut** ; le tableau montre chaque cas tel que le code le résout, y compris ceux qui ont un niveau de plus ou aucun flag :

| Paramètre | Ordre (le premier l’emporte) | Où |
|---|---|---|
| Socket CRI, plafond de capabilities et son mode | `--addr` / `--cap-ceiling` / `--cap-ceiling-mode` > `DELONIX_CRI_ADDR` / `DELONIX_CRI_CAP_CEILING` / `DELONIX_CRI_CAP_CEILING_MODE` > `unix:///run/delonix-cri.sock` / aucun plafond / `reject` | `crates/interfaces/delonix-cri/src/bin/delonix-cri.rs:main` |
| Socket de l’API de gestion | `--addr` > `DELONIX_API_ADDR` > `unix:///run/delonix-mgmt.sock` | `bins/delonix-mgmt-bin/src/main.rs:run` |
| Socket de l’API Docker | `--addr` > `DELONIX_DOCKER_ADDR` > `unix:///run/delonix-docker.sock` | `bins/delonix-runtime-bin/src/cmd/dockerapi.rs:run` |
| Langue de sortie | `--l18n` > `DELONIX_L18N` > anglais | `bins/delonix-runtime-bin/src/cmd/po.rs:peek_lang` |
| Touche d’échappement de la console de VM | `--escape` > `DELONIX_CONSOLE_ESCAPE` > `^]` | `bins/delonix-runtime-bin/src/cmd/vm.rs:resolve_escape` |
| Backend de VM | `--backend` (ou le `HYPERVISOR` de l’image) > `DELONIX_VM_BACKEND` > le `defaultProvider` du fichier de providers (ADR-0054) > `vm default-backend --set` > détection automatique (seulement sans fichier de providers) | `crates/adapters/delonix-vm/src/lib.rs:standing_backend_choice` |
| Adresse de liaison des ports publiés | l’adresse dans `-p <ip>:<host>:<container>` > `DELONIX_PUBLISH_ADDR` > `127.0.0.1` | `crates/adapters/delonix-sdn/src/lib.rs:publish_bind_addr` |
| Flux CVE pour `image scan --update` | `--feed` > `DELONIX_ADVISORY_FEED` > erreur | `bins/delonix-runtime-bin/src/cmd/scan.rs:cmd_scan_update` |
| Filtre des logs | `DELONIX_LOG` > `RUST_LOG` > `info` | `crates/adapters/delonix-telemetry/src/telemetry.rs:init` |

`delonix serve cri` et `delonix serve api` transmettent leurs flags **à la fois** comme flags et comme la
variable correspondante au binaire serveur qu’ils `exec` (`bins/delonix-runtime-bin/src/cmd/serve.rs:run`),
de sorte qu’un serveur d’une release plus ancienne qui ne lit que les variables reçoit quand même la valeur.

### Isoler une exécution de développement

Avant d’exécuter quoi que ce soit au-delà de `--help` sur une machine qui exécute aussi des workloads Delonix, faites pointer les deux
emplacements d’état vers un répertoire jetable :

```bash
export DELONIX_ROOT=$HOME/scratch/dlx/root        # records, images, networks, IPAM, volumes, pidfiles
export DELONIX_NET_RUNTIME_DIR=/tmp/dlx-run        # the network holder's control and slirp sockets
mkdir -p "$DELONIX_ROOT" "$DELONIX_NET_RUNTIME_DIR"
./target/debug/delonix system info                # "state root" must show your scratch path
```

**Pourquoi les deux.** L’infrastructure réseau garde ses pidfiles sous `DELONIX_ROOT` mais ses sockets
unix dans un répertoire d’exécution séparé (`crates/adapters/delonix-sdn/src/infra.rs:runtime_dir`),
parce que le chemin d’un socket est limité à environ 108 octets et que `DELONIX_ROOT` peut être arbitrairement profond.
N’en définir qu’une des deux peut laisser deux racines d’état partager un même jeu de sockets. L’histoire complète,
et la façon de démonter l’infra isolée, se trouvent dans
[Isoler l’état du moteur](build-and-test.md#isolating-the-engines-state).

## Configuration courante

| Variable | Lu par | Rôle | Valeurs / défaut | Remarques |
|---|---|---|---|---|
| `DELONIX_PERF_CONF` | `scripts/install.sh` (le `/usr/local/sbin/delonix-performance` généré) | Chemin de la config (drapeaux `CPU=`/`THP=`) que lit l’assistant. | Un chemin de fichier. Non défini : `/etc/delonix/performance.conf`. | Uniquement pour tester l’assistant contre un faux sysfs ; `install.sh` écrit le vrai. |
| `DELONIX_PERF_CPU` | `scripts/install.sh` (le `/usr/local/sbin/delonix-performance` généré) | Racine de l’arbre sysfs du CPU que l’assistant lit et écrit (governor, EPP). | Un chemin de répertoire. Non défini : `/sys/devices/system/cpu`. | Pointez-la vers un faux arbre pour tester `apply`/`revert` sans toucher à l’hôte. |
| `DELONIX_PERF_STATE` | `scripts/install.sh` (le `/usr/local/sbin/delonix-performance` généré) | Répertoire où l’assistant stocke les valeurs du démarrage qu’il restaure au `revert`. | Un chemin de répertoire. Non défini : `/var/lib/delonix-performance`. | Surcharge réservée aux tests, comme les autres. |
| `DELONIX_PERF_THP` | `scripts/install.sh` (le `/usr/local/sbin/delonix-performance` généré) | Le fichier `enabled` des hugepages transparentes que l’assistant lit et écrit. | Un chemin de fichier. Non défini : `/sys/kernel/mm/transparent_hugepage/enabled`. | Surcharge réservée aux tests, comme les autres. |
| `DELONIX_ROOT` | chaque store et chaque binaire : `bins/delonix-runtime-bin/src/cmd/util.rs:state_root`, `crates/adapters/delonix-oci/src/image.rs:ImageStore::default_root`, `crates/adapters/delonix-state/src/store.rs:Store::default_root`, `crates/adapters/delonix-sdn/src/infra.rs:base_root`, `crates/interfaces/delonix-cri/src/bin/delonix-cri.rs:main`, `bins/delonix-mgmt-bin/src/main.rs:run`, `crates/interfaces/delonix-mcp/src/lib.rs` | La racine d’état du moteur : enregistrements de containers, images, réseaux, IPAM, volumes, VM, secrets. | Un chemin de répertoire. Non définie : `$XDG_DATA_HOME/delonix` (ou `~/.local/share/delonix`) hors root, `/var/lib/delonix` en root. **`delonix-cri` et `delonix-mgmt` prennent `/var/lib/delonix` par défaut dans tous les cas**, c’est pourquoi `delonix serve …` passe explicitement la racine de la CLI (`cmd/serve.rs:exec_server`). | La seule variable à définir pour tester. Le moteur la **définit** aussi sur chaque enfant qu’il démarre (passes de re-exec, le holder réseau, les serveurs, les appels de cycle de vie du CRI) afin que les chemins concordent d’un user namespace à l’autre. |
| `DELONIX_NET_RUNTIME_DIR` | `crates/adapters/delonix-sdn/src/infra.rs:runtime_dir` (`RUNTIME_DIR_ENV`) | Répertoire des sockets unix de l’infrastructure réseau (`control.sock`, `slirp.sock`). | Un chemin de répertoire ; gardez-le court (les chemins de socket au-delà d’environ 108 octets échouent avec `SUN_LEN`). Non définie : `/tmp/delonix-net-<uid>` plus un suffixe dérivé d’un `DELONIX_ROOT` non par défaut. | Définissez-la en même temps que `DELONIX_ROOT` pour isoler. Le moteur la passe aussi au holder et aux passes de re-exec `--net <custom>` (`infra::runtime_dir_env`), parce que dans le user namespace du holder l’uid vaut 0 et que le défaut serait différent. |
| `DELONIX_L18N` | `bins/delonix-runtime-bin/src/cmd/po.rs:peek_lang` | Langue de la sortie et du `--help` de la CLI. | `en` (défaut) ou `pt`. | `--l18n` l’emporte. Les *classes* d’erreur (codes de sortie) ne dépendent pas de la langue, les messages si — ne faites pas de grep sur les messages dans les scripts. |
| `DELONIX_LOG` | `crates/adapters/delonix-telemetry/src/telemetry.rs:init` | Filtre des logs de `delonix`, `delonix-cri`, `delonix-mgmt` et `delonix-mcp`. | Une expression de filtre `tracing` (`debug`, `warn`, `delonix_sdn=debug`). Se rabat sur `RUST_LOG`, puis `info`. | Les logs vont sur stderr ; stdout est réservé à la sortie des commandes. |
| `DELONIX_LOG_FORMAT` | `crates/adapters/delonix-telemetry/src/telemetry.rs:init` | Format des lignes de log. | `json` pour des lignes JSON ; toute autre valeur (ou non définie) donne du texte brut. | Utile quand un serveur s’exécute sous systemd et que son journal est expédié ailleurs. |
| `DELONIX_VERBOSE` | `bins/delonix-runtime-bin/src/cmd/output.rs` (`Progress`) | Diffuse la sortie de chaque étape au lieu de la replier en une seule ligne de progression. | Définie et différente de `0` → verbeux. | Même effet que `--verbose` là où une commande l’a. |
| `DELONIX_CONSOLE_ESCAPE` | `bins/delonix-runtime-bin/src/cmd/vm.rs:resolve_escape` | La touche qui détache `delonix vm console`. | Une touche de contrôle sous la forme `^X` ou `X`. Défaut `^]`. Une valeur invalide est une erreur, pas un repli. | Pour les dispositions de clavier où `^]` ne peut pas être saisi (par ex. le portugais). `-e/--escape` l’emporte. |
| `DELONIX_HOSTS_FILE` | `bins/delonix-runtime-bin/src/cmd/hosts_file.rs:hosts_path` | Le fichier hosts dans lequel `hosts: [host]` sur une `HTTPRoute` et `delonix hosts sync` écrivent leur bloc géré. | Un chemin de fichier ; défaut `/etc/hosts`. | Pointez-la vers un fichier temporaire dans une exécution isolée pour ne jamais toucher au vrai `/etc/hosts` (l’écrire requiert root). Comment fonctionne le bloc : [Comment les noms atteignent `/etc/hosts`](service-names-and-hosts.md). |
| `DELONIX_CRI_ADDR` | `crates/interfaces/delonix-cri/src/bin/delonix-cri.rs:main` ; transmise par `bins/delonix-runtime-bin/src/cmd/serve.rs:run` | Socket sur lequel écoute le serveur CRI. | `unix://<path>`. Défaut `unix:///run/delonix-cri.sock`. | `--addr` l’emporte. Le `--container-runtime-endpoint` du kubelet doit correspondre. |
| `DELONIX_CRI_CAP_CEILING` | `crates/interfaces/delonix-cri/src/bin/delonix-cri.rs:main` (`cap_ceiling::CEILING_ENV`, analysée par `crates/interfaces/delonix-cri/src/cap_ceiling.rs:CapCeiling::parse`) ; transmise par `cmd/serve.rs:run` | Borne supérieure, au niveau du nœud, des capabilities de tout container créé via le CRI, `privileged: true` compris. | Vide/non définie ou `all` → aucun plafond (comportement inchangé). `none` → aucune capability. `default` → le jeu par défaut du moteur. `default,NET_ADMIN,…` → le jeu par défaut plus celles nommées. Une liste de noms (préfixe `CAP_` facultatif, insensible à la casse ; séparés par des virgules, des espaces ou `;`) → exactement celles-là. `all` n’importe où dans la liste l’emporte. Un nom inconnu, ou une valeur faite uniquement de séparateurs, **empêche le serveur de démarrer**. | `--cap-ceiling` l’emporte. Ne borne que les capabilities : un pod privilégié obtient quand même un seccomp non confiné et un `/sys` accessible en écriture. Le plafond en vigueur est visible dans `crictl info` (`capabilityCeiling`). |
| `DELONIX_CRI_CAP_CEILING_MODE` | `crates/interfaces/delonix-cri/src/cap_ceiling.rs:CeilingMode::parse` (`MODE_ENV`) ; transmise par `cmd/serve.rs:run` | Ce qui se passe lorsqu’un pod demande explicitement plus que le plafond. | `reject` (défaut ; aussi `enforce` ou vide) → `CreateContainer` échoue en nommant les capabilities refusées. `clamp` (ou `trim`) → réduit au plafond avec un avertissement. Un mot inconnu **empêche le serveur de démarrer**. | `--cap-ceiling-mode` l’emporte. Dans les deux modes, le jeu par défaut implicite du moteur est réduit au plafond sans erreur. |
| `DELONIX_CRI_FROM_SOURCE` | `bins/delonix-runtime-bin/src/cmd/vmimage.rs:locate_cri_bin` (`CRI_FROM_SOURCE_ENV`, analysée par `cri_from_source_requested`) | Opt-in pour compiler `delonix-cri` depuis l’arbre source autour du cwd quand `cluster apply` / `cluster kubeadm` ont besoin d’un binaire à installer sur les nœuds. | Seuls `1` ou `true` (après suppression des espaces) l’activent ; non défini, vide, `0` ou toute autre valeur = désactivé. | Désactivé par défaut pour que le runtime installé sur un cluster ne dépende jamais du répertoire d’où la commande a été lancée. Sans elle l’ordre est `--cri-bin`, le `delonix-cri` à côté de `delonix`, puis l’asset de release de la version en cours, vérifié contre son `SHA256SUMS`. L’origine, le chemin et le sha256 sont toujours affichés. |
| `DELONIX_API_ADDR` | `bins/delonix-mgmt-bin/src/main.rs:run` ; transmise par `cmd/serve.rs:run` | Socket de l’API de gestion locale (`delonix serve api`). | `unix://<path>`. Défaut `unix:///run/delonix-mgmt.sock`. | `--addr` l’emporte. L’API est uniquement locale (l’uid appelant). |
| `DELONIX_DOCKER_ADDR` | `bins/delonix-runtime-bin/src/cmd/dockerapi.rs:run` | Socket de la tranche d’API Docker Engine (`delonix serve docker-api`). | `unix://<path>` (le préfixe `unix://` est facultatif). Défaut `unix:///run/delonix-docker.sock`. | `--addr` l’emporte. |
| `DELONIX_VM_BACKEND` | `crates/adapters/delonix-vm/src/lib.rs:standing_backend_choice` | Backend de VM valable pour toute la session lorsqu’une commande n’en nomme pas. | Un nom de backend (`libvirt`, `cloud-hypervisor`, ou un backend distant enregistré comme `proxmox`). Une valeur vide est ignorée. | En dessous de `--backend` et du `HYPERVISOR` de l’image, au-dessus du défaut de la machine défini par `delonix vm default-backend --set`. Comme un choix explicite, elle outrepasse l’heuristique de capacité et peut échouer tardivement au démarrage si le backend ne peut pas exécuter la VM. |
| `DELONIX_PROVIDERS_CONFIG` | `cmd/providers_config.rs:locate_with` | Chemin du fichier de providers du nœud (ADR-0054), avant `$XDG_CONFIG_HOME/delonix/providers.yaml` et `/etc/delonix/providers.yaml`. | Un chemin. | Doit désigner un fichier existant — ne se rabat jamais sur un autre. Le premier fichier trouvé est la configuration ; les fichiers ne sont jamais fusionnés. Un fichier illisible fait échouer une requête de VM sans `--backend` au lieu de deviner un provider. |
| `DELONIX_NO_CGROUP_WARN` | `crates/adapters/delonix-linux/src/lib.rs` (l’avertissement rootless-sans-délégation et `warn_if_unprotected_memory`), `bins/delonix-runtime-bin/src/cmd/kindmode.rs` | Fait taire les avertissements sur l’absence de délégation de cgroup et sur un container sans aucun plafond mémoire nulle part. | Définie (n’importe quelle valeur) → silencieux. | Le moteur la **définit** lui-même (`cmd/util.rs:silence_cgroup_warning`, `cmd/kindmode.rs`) pour que les enfants de re-exec ne répètent pas un avertissement déjà affiché par le parent. La définir à la main masque une condition réelle : les limites ne sont pas appliquées. |
| `DELONIX_POLICY_LINT` | `bins/delonix-runtime-bin/src/cmd/policy.rs:show_lints` | Fait taire les avertissements de politique d’exécution émis une fois par commande (`warning: runtime policy [...]`). | `0` → silencieux ; toute autre valeur ou non définie → affichés. | Pour quelqu’un qui a lu l’avertissement et a décidé autrement. |
| `DELONIX_NO_AUTO_RECOVER` | `bins/delonix-runtime-bin/src/cmd/netns.rs:reconcile_after_respawn` | Après la reconstruction du holder réseau, signaler les containers échoués et la commande pour les redémarrer au lieu de les redémarrer automatiquement. | Définie (n’importe quelle valeur) → signalement seulement. | Pour les hôtes où vous voulez choisir quand une base de données redémarre. |
| `DELONIX_NO_AUTO_DELEGATE` | `bins/delonix-runtime-bin/src/cmd/kindmode.rs` (vérification préalable de cgroup de `cluster create`) | Désactive le re-exec automatique de `cluster create` sous `systemd-run --user --scope -p Delegate=yes` lorsque le contrôleur `cpu` n’est pas délégué. | Définie (n’importe quelle valeur) → afficher l’erreur au lieu de ré-exécuter. | Pour les scripts et la CI qui préfèrent l’erreur simple. |

## Échappatoires réseau et sécurité

**Chaque variable de cette section abaisse une frontière.** Chacune journalise un `SECURITY WARNING` (ou un
avertissement) lorsqu’elle prend effet. Elles existent pour le débogage et pour des désactivations explicites et informées ; aucune
n’a sa place dans une configuration de production. Lisez la section de `AGENTS.md` indiquée avant d’en utiliser une.

| Variable | Lu par | Rôle | Valeurs / défaut | Remarques |
|---|---|---|---|---|
| `DELONIX_FORWARD_POLICY` | `crates/adapters/delonix-sdn/src/infra.rs` (constructeur du jeu de règles d’ingress) | Fait repasser la chaîne `forward` du netns du holder de refus par défaut (`policy drop`) à autorisation par défaut. | `accept` → autorisation par défaut ; toute autre valeur → refus par défaut. | **Abaisse l’isolation entre réseaux.** Journalisée comme avertissement de sécurité. Voir `AGENTS.md`, *«Bloco 0 do plano 33 (v0.37.1)»*. |
| `DELONIX_ALLOW_LINK_LOCAL` | `crates/adapters/delonix-sdn/src/infra.rs` (chaîne `fwguard`) | Supprime le rejet inconditionnel de `169.254.0.0/16` (métadonnées cloud) et de `127.0.0.0/8` (loopback de l’hôte) pour le trafic des containers. | `1` → autorisé ; toute autre valeur → rejeté. | **Expose les identifiants de métadonnées d’instance sur un hôte cloud.** Avertissement de sécurité. `AGENTS.md`, *«Bloco 0 do plano 33 (v0.37.1)»* (RF-NET-02). |
| `DELONIX_ALLOW_HOLDER_INGRESS` | `crates/adapters/delonix-sdn/src/infra.rs` (chaîne `dlxinput`) | Permet aux containers d’atteindre les services que le holder expose lui-même (le proxy L7, le DNS interne) au-delà de la liste d’autorisation. | `1` → autorisé ; toute autre valeur → nouvelles connexions rejetées. | **Via le proxy, un container atteint n’importe quel backend enregistré, dans n’importe quel namespace, en passant outre la politique d’ingress propre à ce backend.** Avertissement de sécurité. Justification dans `docs/discovery/46_GAPS_ENCONTRADOS.md` §4.2. |
| `DELONIX_ENABLE_IPV6` | `crates/adapters/delonix-sdn/src/infra.rs:ipv6_sdn_enabled` | Redonne aux containers des adresses IPv6 sur le SDN. | `1` → activé ; toute autre valeur → IPv6 désactivé dans le container et transfert refusé. | **Aucune règle de pare-feu, isolation de namespace ou `Dependency` ne s’applique à IPv6** — toutes les politiques sont IPv4 uniquement. Avertissement de sécurité. `AGENTS.md`, *«Bloco 0 do plano 33 (v0.37.1)»*. |
| `DELONIX_ALLOW_UNENFORCED_LIMITS` | `bins/delonix-runtime-bin/src/cmd/container.rs:preflight_resource_limits` | Exécute un container avec `-m`/`--cpus`/`--cpu-weight` même lorsque cette session n’a pas de délégation de cgroup, au lieu de refuser (code de sortie 69). | Définie (n’importe quelle valeur) → exécution, sans limite, avec un avertissement. | Le noyau ne voit jamais les limites. Voir [délégation de cgroup](environment.md#cgroup-delegation-some-limits-are-refused-others-are-not-enforced). |
| `DELONIX_INSECURE_BESTEFFORT` | `crates/adapters/delonix-linux/src/lib.rs:insecure_besteffort` | Saute la vérification fail-closed du confinement (seccomp, capabilities, `no_new_privs`) qui s’exécute avant `execve` dans l’init d’un container et dans `exec`. | Définie (n’importe quelle valeur) → la vérification est sautée. | **Un container peut démarrer avec un confinement qui ne s’est silencieusement pas appliqué.** Lue par le moteur avant que l’environnement du container ne soit appliqué, de sorte qu’un container ne peut pas la définir pour lui-même. |
| `DELONIX_PUBLISH_ADDR` | `crates/adapters/delonix-sdn/src/lib.rs:publish_bind_addr` ; suggérée par `bins/delonix-runtime-bin/src/cmd/vm.rs` (`vm reach`) | Adresse de l’hôte à laquelle se lient les ports publiés lorsque `-p` n’en nomme pas. | Une adresse IPv4 ; une valeur qui n’est pas IPv4 est ignorée. Défaut `127.0.0.1`. | `0.0.0.0` expose les ports publiés sur toutes les interfaces. `vm reach` suggère la passerelle libvirt pour que les VM puissent atteindre un container sans l’exposer au LAN. `AGENTS.md`, *«Revisão do flow `-p` ↔ `ingress`/`egress` (2026-07-27)»*. |
| `DELONIX_SUBNET_BASE` | `crates/adapters/delonix-sdn/src/lib.rs:default_base` | Force le deuxième octet du réseau par défaut (`10.<base>.0.0/16`). | Un entier 0–255 (sans autre vérification de plage). Non définie : la valeur persistée dans `<root>/net/default-base`, sinon un octet libre détecté sur l’hôte. | Uniquement pour une collision que la détection n’a pas vue. La changer après que des containers existent change les adresses du réseau par défaut. |
| `DELONIX_CNI` | `crates/adapters/delonix-sdn/src/cni.rs:enabled_conf`, consommée par `crates/interfaces/delonix-cri/src/runtime_svc/lifecycle.rs` (`RunPodSandbox`) et `runtime_svc.rs` (`UpdateRuntimeConfig`) | **CRI rootless uniquement :** met en réseau les sandboxes de pod via la chaîne de plugins CNI (`/etc/cni/net.d`, plugins issus de `CNI_PATH`), exécutée à l’intérieur du holder réseau, au lieu du SDN natif. | `1` → activé, et seulement si une configuration existe ; toute autre valeur → SDN natif. | Sans effet lorsque le CRI s’exécute en root : le root utilise toujours la configuration CNI du nœud. |
| `DELONIX_TRACE_UNPUBLISH` | `crates/adapters/delonix-sdn/src/infra.rs:trace_unpublish` | Enregistre chaque dépublication de port avec la fonction, le port, le pid, le pid parent, l’exécutable et une backtrace. | `1` ou `stderr` → stderr ; toute autre valeur → ajouté à ce fichier. | Un outil de diagnostic, sans coût lorsqu’elle n’est pas définie. Conservée pour l’enquête décrite dans `AGENTS.md`, *«RESOLVIDO — as portas publicadas morriam sozinhas»*. |

## Build et images

| Variable | Lu par | Rôle | Valeurs / défaut | Remarques |
|---|---|---|---|---|
| `DELONIX_INSECURE_REGISTRIES` | `crates/adapters/delonix-oci/src/registry.rs` | Registres contactés en HTTP simple au lieu de HTTPS. | Entrées `host` ou `host:port` séparées par des virgules (insensibles à la casse). Non définie → HTTPS partout sauf pour les registres en loopback, qui utilisent déjà HTTP. | **Le trafic et les identifiants pour ces hôtes circulent sans TLS.** Un opt-in explicite par hôte, jamais une plage. Lorsqu’une connexion HTTPS à un registre échoue, l’erreur suggère cette variable avec l’hôte déjà renseigné. |
| `DELONIX_SCAN_ON_PULL` | `bins/delonix-runtime-bin/src/cmd/scan.rs:admission_scan_on_pull` | Politique d’admission CVE appliquée après chaque pull d’image. | Non définie/vide → désactivée. `warn` → analyser et signaler. `low`/`medium`/`high`/`critical` → supprimer l’image et refuser lorsqu’une vulnérabilité d’au moins cette sévérité est trouvée. Une valeur inconnue refuse le pull. | Un gate (contrôle CI) fail-closed : une faute de frappe ne le désactive pas. Une image sans SBOM est admise avec un avertissement. |
| `DELONIX_ADVISORIES` | `bins/delonix-runtime-bin/src/cmd/scan.rs:load_advisories` | Chemin d’un fichier de base d’avis de sécurité utilisé par `image scan`. | Un chemin de fichier. | Utilisée uniquement lorsqu’aucune base synchronisée n’existe à `<root>/advisories.json` ; la base synchronisée l’emporte. Sans l’une ni l’autre, le substitut embarqué est utilisé et l’analyse le signale. |
| `DELONIX_ADVISORY_FEED` | `bins/delonix-runtime-bin/src/cmd/scan.rs:cmd_scan_update` | Source pour `delonix image scan --update`. | Une URL ou un fichier (format OSV ou natif). | `--feed` l’emporte. Sans l’un ni l’autre, `--update` échoue avec une erreur qui nomme les deux. |

## Cloud Hypervisor et réglage des ressources des VM

Les plafonds libvirt ci-dessous sont écrits dans le XML de domaine généré
(`crates/adapters/delonix-vm/src/lib.rs:libvirt_domain_xml`) ; ils ne modifient pas un domaine qui
existe déjà tant qu’il n’est pas redémarré.

| Variable | Lu par | Rôle | Valeurs / défaut | Remarques |
|---|---|---|---|---|
| `DELONIX_HYPERVISOR_FW` | `crates/adapters/delonix-vm/src/lib.rs:default_ch_firmware` | Firmware que Cloud Hypervisor démarre lorsque `--firmware` n’est pas donné. | Un chemin de fichier, utilisé seulement s’il existe. Sinon le premier existant parmi `DEFAULT_CH_FIRMWARES` (EDK2 `CLOUDHV.fd` avant `hypervisor-fw`). | Voir [Construire des microVMs](microvm-setup.md) pour savoir pourquoi le build EDK2 passe en premier. |
| `DELONIX_VM_RESERVE_MIB` | `crates/adapters/delonix-vm/src/lib.rs:vm_admission_check` | Mémoire gardée libre pour l’hôte lors de l’admission d’une VM : une VM est refusée si sa mémoire plus cette réserve dépasse `MemAvailable`. | MiB ; défaut `2048` ; une valeur non analysable se rabat sur 2048. | La baisser expose l’hôte à des OOM-kills. |
| `DELONIX_VM_MEM_HARD_LIMIT` | `crates/adapters/delonix-vm/src/lib.rs:mem_hard_limit_kib` | Si les domaines libvirt reçoivent un `<memtune><hard_limit>` sur l’ensemble du processus QEMU. | `off` → pas de limite stricte ; toute autre valeur → activée. | |
| `DELONIX_VM_MEM_OVERHEAD_PCT` | `crates/adapters/delonix-vm/src/lib.rs:mem_hard_limit_kib` | Marge au-dessus de la mémoire de l’invité autorisée par la limite stricte. | Pourcentage, plage acceptée 5–200 ; défaut `25` ; au moins 1 GiB de marge. Hors plage → 25. | |
| `DELONIX_VM_CPU_QUOTA_CORES` | `crates/adapters/delonix-vm/src/lib.rs:cpu_quota_micros` | Plafond CPU (`<cputune><quota>`) d’un domaine libvirt, en cœurs. | Non définie → vCPU + 1 (le cœur supplémentaire est pour l’émulateur et les threads d’E/S de QEMU). Un nombre positif → autant de cœurs. `off` → aucun plafond. | |
| `DELONIX_VM_IO_MAX_BPS` | `crates/adapters/delonix-vm/src/lib.rs:vm_iotune_xml` | Plafond de débit du disque racine (`<iotune><total_bytes_sec>`). | Octets/s, entier positif. Non définie ou 0 → aucun plafond (opt-in). | |
| `DELONIX_VM_IO_MAX_IOPS` | `crates/adapters/delonix-vm/src/lib.rs:vm_iotune_xml` | Plafond d’IOPS du disque racine (`<iotune><total_iops_sec>`). | Entier positif. Non définie ou 0 → aucun plafond (opt-in). | |

### Budget de ressources des containers

Celles-ci règlent les défauts et le plafond agrégé que le moteur applique aux containers
(`crates/adapters/delonix-linux/src/lib.rs`).

| Variable | Lu par | Rôle | Valeurs / défaut | Remarques |
|---|---|---|---|---|
| `DELONIX_RESERVE_PCT` | `crates/adapters/delonix-linux/src/lib.rs:host_reserve_pct` | Part de l’hôte (mémoire et CPU) que la slice cgroup du moteur peut utiliser au total. | Pourcentage, plage acceptée 10–95 ; défaut `85`. Hors plage → 85. | Sert aussi de base aux défauts par workload ci-dessous. |
| `DELONIX_DEFAULT_PCT` | `crates/adapters/delonix-linux/src/lib.rs:default_workload_pct` | Part du budget du moteur qu’un workload peut prendre lorsqu’il ne déclare aucune limite. | Pourcentage, plage acceptée 1–100 ; défaut `25`. Hors plage → 25. | La mémoire par défaut est d’au moins 64 MiB ; le CPU par défaut est borné entre 0,25 et 1,0 cœur. |
| `DELONIX_SWAP_MAX` | `crates/adapters/delonix-linux/src/lib.rs:swap_max_value` | `memory.swap.max` du cgroup d’un container. | Une valeur de cgroup ; défaut `0` (pas de swap). `max` rétablit un swap illimité. | Le swap transforme une limite mémoire en limite souple. |
| `DELONIX_IO_MAX_BPS` | `crates/adapters/delonix-linux/src/lib.rs:host_io_max_bps` | Plafond agrégé de lecture/écriture disque (`io.max`) de la slice du moteur. | Octets/s ; défaut `500000000` (500 MB/s). `0` le désactive. Une valeur non analysable → défaut. | Un plafond de sécurité contre un container qui saturerait le disque, pas une QoS fine. Ne s’applique que là où le contrôleur `io` est disponible. |

## Providers

### Proxmox VE

Lues une seule fois au démarrage de la CLI par `bins/delonix-runtime-bin/src/cmd/vmbackends.rs:register_proxmox`.
Rien n’est contacté tant qu’une commande de VM ne sélectionne pas le backend `proxmox`. Les valeurs vides comptent comme non définies.
Une mauvaise configuration affiche un avertissement et laisse le backend non enregistré ; elle n’arrête pas les autres
commandes. Contexte : `docs/adr/0008-proxmox-vm-backend.md`.

| Variable | Lu par | Rôle | Valeurs / défaut | Remarques |
|---|---|---|---|---|
| `DELONIX_PROXMOX_URL` | `cmd/vmbackends.rs:register_proxmox_with` ; nommée dans l’erreur de `crates/adapters/delonix-vm/src/lib.rs` (`KNOWN_UNREGISTERED`) | Point d’accès API du nœud. C’est sa définition qui active le backend. | `https://<host>:8006`. | Exige `DELONIX_PROXMOX_NODE` et un identifiant. |
| `DELONIX_PROXMOX_NODE` | `cmd/vmbackends.rs:register_proxmox_with` | Le nom du nœud à utiliser (tel que `GET /nodes` le rapporte). | par ex. `pve`. Aucun défaut : le backend ne choisit jamais un nœud à votre place. | |
| `DELONIX_PROXMOX_SECRET` | `cmd/vmbackends.rs:proxmox_auth` | Nom d’un `kind: Secret` contenant l’identifiant. | Secret avec `tokenId`+`tokenSecret` (préféré) ou `username`+`password`. | Vérifiée **en premier**. Préférée aux variables ci-dessous, qui finissent dans l’historique du shell et dans `ps`. |
| `DELONIX_PROXMOX_TOKEN_ID` | `cmd/vmbackends.rs:proxmox_auth` | Identifiant du jeton d’API. | `user@realm!tokenname`. | Utilisée avec `DELONIX_PROXMOX_TOKEN` ; vérifiée après le secret. |
| `DELONIX_PROXMOX_TOKEN` | `cmd/vmbackends.rs:proxmox_auth` | Secret du jeton d’API. | | |
| `DELONIX_PROXMOX_TOKEN_FILE` | `cmd/vmbackends.rs:credential_value` | Chemin d’un fichier contenant le secret du jeton d’API ; préférable à `DELONIX_PROXMOX_TOKEN`, dont hérite tout processus enfant. | Un chemin. | Refusé si quelqu’un d’autre que le propriétaire peut le lire (`chmod 600`). |
| `DELONIX_PROXMOX_USER` | `cmd/vmbackends.rs:proxmox_auth` | Compte pour l’authentification par mot de passe. | `root@pam`, … | Utilisée avec `DELONIX_PROXMOX_PASSWORD` ; vérifiée en dernier. |
| `DELONIX_PROXMOX_PASSWORD` | `cmd/vmbackends.rs:proxmox_auth` | Mot de passe de ce compte. | | |
| `DELONIX_PROXMOX_PASSWORD_FILE` | `cmd/vmbackends.rs:credential_value` | Chemin d’un fichier contenant ce mot de passe ; préféré à `DELONIX_PROXMOX_PASSWORD`. | Un chemin. | Refusé s’il n’est pas lisible par son seul propriétaire (`chmod 600`). Le `passwordFile` du fichier de providers s’y traduit. |
| `DELONIX_PROXMOX_INSECURE_TLS` | `cmd/vmbackends.rs:register_proxmox_with` | Saute la vérification du certificat TLS du nœud. | `1`, `true` ou `yes` → sauter ; par défaut, vérifier. | **Une autre machine répondant au nom du nœud reçoit l’identifiant.** Opt-in uniquement, jamais appliqué comme repli après une erreur TLS. |
| `DELONIX_PROXMOX_BRIDGE` | `cmd/vmbackends.rs:register_proxmox_with` | Bridge par défaut des cartes réseau des VM sur ce nœud. | Un nom de bridge ; le défaut du backend est `vmbr0`. | Un `bridge:` propre à la VM l’emporte. |
| `DELONIX_PROXMOX_VLAN` | `cmd/vmbackends.rs:parse_vlan` | Étiquette VLAN par défaut des cartes réseau des VM sur ce nœud. | 1–4094. Hors plage est une **erreur**, jamais ignorée. | |
| `DELONIX_PROXMOX_CA_FILE` | `cmd/vmbackends.rs:register_proxmox_with` | Un certificat CA (PEM) à faire confiance pour le nœud, en plus des racines système. | Chemin vers un fichier PEM ; illisible est une **erreur**. | La façon de vérifier un nœud dont le certificat a été signé par une CA interne, au lieu de `DELONIX_PROXMOX_INSECURE_TLS`. |
| `DELONIX_PROXMOX_TRACE_ROUTES` | `cmd/vmbackends.rs:register_proxmox_with` (lue une fois, transmise au client comme `ClientOptions::trace_routes` ; la constante `TRACE_ROUTES_ENV` dans `crates/providers/delonix-proxmox/src/lib.rs` la nomme) et `crates/providers/delonix-proxmox/tests/live.rs:backend` (la suite live, dont l’exécution est commitée sous `docs/proxmox/trace-9.2.2.routes`) | Ajoute `METHOD /path` de chaque requête à ce fichier — le numérateur de la matrice de couverture (ADR-0049). | Chemin vers un fichier ; vide = désactivé. | Donnez-le à `scripts/proxmox_api_inventory.py --trace` pour marquer des routes `supported+tested`. |

### OPNsense

Le `GatewayProvider` pour une vraie appliance OPNsense (`kind: NetworkGateway`, ADR-0051),
enregistré de la même façon que le backend Proxmox ci-dessus et lu par `cmd::gatewayproviders`.

| Variable | Lue par | Rôle | Valeurs / défaut | Notes |
|---|---|---|---|---|
| `DELONIX_OPNSENSE_URL` | `cmd/gatewayproviders.rs:register_opnsense_with` | Point d’accès API de l’appliance. La définir est ce qui active le provider. | `https://<host>`. | Exige un identifiant. |
| `DELONIX_OPNSENSE_CREDENTIAL` | `cmd/gatewayproviders.rs:opnsense_auth` | Nom d’un `kind: Secret` qui porte l’identifiant. | Un Secret avec les champs `key`+`secret` — une paire clé/secret d’API générée, jamais le nom d’utilisateur et le mot de passe d’un compte de l’interface graphique (ADR-0051 phase 0 : ceux-là sont refusés par l’API). | Vérifiée **en premier**. |
| `DELONIX_OPNSENSE_KEY` | `cmd/gatewayproviders.rs:opnsense_auth` | Clé d’API. | | Utilisée avec `DELONIX_OPNSENSE_SECRET` ; vérifiée après le `kind: Secret`. |
| `DELONIX_OPNSENSE_SECRET` | `cmd/gatewayproviders.rs:opnsense_auth` (via `credential_value`) | Secret d’API. | Préférez `DELONIX_OPNSENSE_SECRET_FILE` (un chemin en `chmod 600`). | |
| `DELONIX_OPNSENSE_INSECURE_TLS` | `cmd/gatewayproviders.rs:register_opnsense_with` | Saute la vérification du certificat TLS de l’appliance. | `1`, `true` ou `yes` → sauter ; par défaut on vérifie. | Un OPNsense d’origine sert un certificat auto-signé (mesuré en réel, ADR-0051 phase 0). **Une autre machine qui répond au nom de l’appliance reçoit l’identifiant.** Uniquement sur demande explicite. |
| `DELONIX_OPNSENSE_CA_FILE` | `cmd/gatewayproviders.rs:register_opnsense_with` | Un certificat CA (PEM) à faire confiance pour l’appliance, en plus des racines système. | Chemin vers un fichier PEM ; illisible est une **erreur**. | Au lieu de `DELONIX_OPNSENSE_INSECURE_TLS`. |

### TrueNAS

Le provisionneur TrueNAS (`kind: Volume` avec `spec.provision.truenas`) prend sa cible du
manifeste et d’un `kind: Secret`, pas de variables d’environnement. Les seuls noms `DELONIX_TRUENAS_*`
sont des paramètres de test — voir [Tests uniquement](#test-only).

## Observabilité

| Variable | Lu par | Rôle | Valeurs / défaut | Remarques |
|---|---|---|---|---|
| `DELONIX_OTLP_ENDPOINT` | `crates/adapters/delonix-telemetry/src/telemetry.rs:build_otlp_layer` | Exporte les spans de tracing via OTLP/HTTP (protobuf). | Une URL de base comme `http://localhost:4318` ; `/v1/traces` est ajouté s’il manque. Non définie ou vide → aucun exporteur. | Un échec de construction de l’exporteur émet un avertissement et continue avec les logs seuls. Le nom de service est `OTEL_SERVICE_NAME` ou le nom de l’exécutable. |
| `DELONIX_METRICS_ADDR` | `crates/interfaces/delonix-cri/src/lib.rs` (démarrage du serveur CRI) | Active un listener HTTP Prometheus `/metrics` dans `delonix-cri`. | `host:port`, par ex. `127.0.0.1:9100`. Non définie → aucun listener. | Un listener TCP : liez-le au loopback à moins que les métriques ne doivent être accessibles depuis le réseau. |
| `DELONIX_WALK_THREADS` | `crates/adapters/delonix-volume/src/lib.rs:walk_threads` (le parcours de disque derrière `system df`, l’usage des volumes et le quota rootless) | Définit combien de workers utilise le parcours de répertoire parallèle. | Un entier positif ; `1` → le parcours séquentiel. Non définie → le nombre de CPU, plafonné. | Les parcours parallèle et séquentiel donnent le MÊME total (testé avec des hardlinks vus par des workers différents) ; la variable existe pour le prouver sur un hôte donné, et comme échappatoire si un système de fichiers se comporte mal sous `readdir` concurrent. |

## Définies par le moteur lui-même / internes

**Ne les définissez pas.** Le moteur les écrit sur les processus qu’il démarre ; les définir à la main fait
qu’une commande normale se comporte comme une passe interne.

| Variable | Lu par | Rôle | Valeurs / défaut | Remarques |
|---|---|---|---|---|
| `DELONIX_BIN` | `crates/contexts/delonix-node/src/dispatch.rs:cli_bin` (utilisée par `delonix-cri`, `delonix-mgmt`, `delonix-mcp`) | L’exécutable `delonix` qu’un serveur rappelle pour les opérations de cycle de vie. | Un chemin. Non définie : le `delonix` situé à côté de l’exécutable du serveur, puis `delonix` dans le `PATH`. | Définie par `delonix serve …` / `delonix mcp` (`cmd/serve.rs:exec_server`) sur la CLI en cours d’exécution. La définir à la main n’est légitime que lorsque vous démarrez directement un binaire serveur et voulez qu’il appelle une CLI précise. `scripts/cli-tree.sh` et `scripts/docs_cli_gate.py` lisent aussi une variable de ce nom pour choisir le binaire qu’ils inspectent (voir [Cloner, compiler et tester](build-and-test.md)). |
| `DELONIX_DISPATCH_VERSION` | `crates/contexts/delonix-node/src/dispatch.rs:check_version` (dans `delonix-cri`, `delonix-mgmt`, `delonix-mcp`) | La version que le serveur doit avoir ; un serveur d’une autre release refuse de démarrer. | Définie par `cmd/serve.rs:exec_server` sur la version de la CLI. | Un serveur démarré directement (par exemple par une unité systemd) n’a aucune attente et n’est pas vérifié. |
| `DELONIX_REEXEC_ID` | `bins/delonix-runtime-bin/src/cmd/container.rs` (`cmd_run`, `reexec_env`) | Marque la seconde passe de `container run/start --net <custom>` ou `--pod`, ré-exécutée dans les namespaces du holder, et porte l’id du container. | Définie par `cmd/container.rs:reexec_env`. | Sa présence saute les vérifications déjà faites par la première passe (propriété des ports). |
| `DELONIX_REEXEC_IP` | `bins/delonix-runtime-bin/src/cmd/container.rs` | L’adresse SDN que la première passe a attachée, pour que la seconde passe l’enregistre. | Définie par `cmd/container.rs:reexec_env`. | |
| `DELONIX_PIN_SYNC` | `crates/adapters/delonix-sdn/src/pin_userns.rs` (`SYNC_ENV`) | Descripteurs de fichier des pipes de handshake entre l’appelant et le pin réseau pendant l’écriture des maps du user namespace. | `<read-fd>,<write-fd>`. | Définie uniquement lorsque le pin doit créer de nouveaux namespaces ; un pin qui adopte ne la reçoit jamais. |
| `DELONIX_DELEGATE_ATTEMPTED` | `bins/delonix-runtime-bin/src/cmd/kindmode.rs:reexec_under_delegated_scope` | Garde l’unique re-exec automatique de `cluster create` sous un scope délégué, afin qu’un hôte à qui il manque encore `cpu` affiche l’erreur réelle au lieu de boucler. | Définie à `1` sur le processus ré-exécuté. | Pour désactiver le re-exec, utilisez `DELONIX_NO_AUTO_DELEGATE`. |
| `DELONIX_INTERNAL` | définie sur les enfants par `crates/adapters/delonix-sdn/src/infra.rs` (`start_control` et les autres lancements du holder), `crates/adapters/delonix-vm/src/lib.rs:launch_vmm`, `crates/interfaces/delonix-cri/src/runtime_svc/lifecycle.rs`, `spdy.rs`, `streaming.rs` | Marque une invocation de machine à machine. | Définie à `1`. | **Aucun lecteur dans le code actuel** : le commentaire dans `runtime_svc/lifecycle.rs:delonix` dit qu’elle « bypasses the grouped-commands barrier », mais plus rien ne lit la variable. Elle est toujours écrite, et elle apparaît dans l’environ des processus réseau (voir les tests de `infra.rs:env_names_this_root`). |

## Au moment du build

Valeurs de compilation : elles sont fixées lorsque le binaire est compilé et ne peuvent pas être changées en définissant une
variable au moment où le programme s’exécute.

| Variable | Lu par | Rôle | Valeurs / défaut | Remarques |
|---|---|---|---|---|
| `DELONIX_GIT_HASH` | écrite par `bins/delonix-runtime-bin/build.rs` ; lue avec `env!` dans `bins/delonix-runtime-bin/src/main.rs` | Hash court du commit affiché par `delonix --version`. | `git rev-parse --short=9 HEAD`, ou `unknown` sans git. | |
| `DELONIX_GIT_SINCE` | écrite par `bins/delonix-runtime-bin/build.rs` ; lue dans `src/main.rs` | Distance depuis le tag le plus récent, affichée sous la forme `(+N commits since vX.Y.Z)`. | `<count>\|<tag>`, vide sur un tag ou sans git. | C’est ainsi que vous distinguez deux builds portant la même version. |
| `DELONIX_BUILD_DATE` | écrite par `bins/delonix-runtime-bin/build.rs` ; lue dans `src/main.rs` et `src/cmd/man.rs` | Date du build dans `--version` et dans les pages de manuel générées. | `date -u +%Y-%m-%d`, ou `unknown`. | Les pages de manuel l’utilisent à la place de l’horloge pour que deux exécutions du générateur sur un même build produisent la même sortie. |
| `DELONIX_GIT_COMMIT` | lue avec `option_env!` dans `bins/delonix-runtime-bin/src/cmd/dockerapi.rs` (réponse `/version`, `GitCommit`) | Commit rapporté par la tranche d’API Docker. | Prise dans l’environnement de build si définie ; sinon `unknown`. | **Rien dans le dépôt ne la définit** (ni `build.rs`, ni les workflows), de sorte que les binaires publiés rapportent `unknown`. |
| `DELONIX_BPF_OBJECT` | écrite par `crates/adapters/delonix-sdn/build.rs` ; lue avec `env!` dans `crates/adapters/delonix-sdn/src/bpf.rs` | Chemin de l’objet eBPF compilé de comptabilité des flux, embarqué dans le binaire. | Définie uniquement lorsque `clang` et les en-têtes libbpf sont présents au moment du build (avec `cfg(bpf_object)`). | Facultative : sans elle, le runtime se dégrade vers les compteurs nftables. |
| `DELONIX_ASSET` | `scripts/install.sh` (section binaire) | Variable shell contenant le nom de fichier de l’asset de release choisi pour ce CPU (`delonix` ou sa variante `-v3`). | Affectée par le script à partir de l’étape de téléchargement. | Pas lue depuis votre environnement ; la définir avant d’exécuter l’installateur n’a aucun effet. |

## Tests uniquement

Celles-ci ne sont lues que par des tests. Sans elles, les tests réels **se sautent** et affichent `SKIP: … is not set`
(un test réel qui passerait silencieusement sans sa cible ne prouverait rien).

| Variable | Lu par | Rôle | Valeurs / défaut | Remarques |
|---|---|---|---|---|
| `DELONIX_PROXMOX_TEST_URL` | `crates/providers/delonix-proxmox/tests/live.rs:target` | Nœud Proxmox contre lequel exécuter les tests réels du backend. | `https://<host>:8006`. | Active les tests. Ils créent et détruisent une VM ; ne les exécutez que contre un nœud qui vous appartient. |
| `DELONIX_PROXMOX_TEST_NODE` | `crates/providers/delonix-proxmox/tests/live.rs:target` | Nom du nœud. | Défaut `pve`. | |
| `DELONIX_PROXMOX_TEST_USER` | `crates/providers/delonix-proxmox/tests/live.rs:target` | Compte pour l’authentification par mot de passe. | par ex. `root@pam`. Obligatoire. | La vérification TLS est désactivée dans ces tests. |
| `DELONIX_PROXMOX_TEST_PASS` | `crates/providers/delonix-proxmox/tests/live.rs:target` | Son mot de passe. | Obligatoire. | |
| `DELONIX_PROXMOX_TEST_STORAGE` | `crates/providers/delonix-proxmox/tests/live.rs` | Stockage du disque de la VM de test. | Défaut `local-lvm`. | |
| `DELONIX_PROXMOX_TEST_BACKUP_STORAGE` | `crates/providers/delonix-proxmox/tests/live.rs` | Stockage où atterrit l’archive de sauvegarde — le nœud peut refuser du contenu de sauvegarde sur le stockage du disque (un pool thin-LVM ne le peut pas). | Défaut : le même que `DELONIX_PROXMOX_TEST_STORAGE`. | |
| `DELONIX_PROXMOX_TEST_MOVE_STORAGE` | `crates/providers/delonix-proxmox/tests/live.rs` | Un SECOND stockage vers lequel `move_disk` déplace le disque de boot de la VM de test — le nœud refuse un déplacement vers le même stockage avec le même format, donc celui-ci doit être un pool réellement différent. | Défaut `local` (doit avoir `content=images` activé dessus). | |
| `DELONIX_PROXMOX_TEST_MOVE_NODE` | `crates/providers/delonix-proxmox/tests/live.rs` | Le nœud du même cluster vers lequel les cas en direct de `vm move` déplacent une VM (ADR-0053). Nécessite un cluster de laboratoire de deux nœuds — jamais la production. | Non défini : les deux cas de déplacement sont sautés. | |
| `DELONIX_PROXMOX_TEST_SHARED_STORAGE` | `crates/providers/delonix-proxmox/tests/live.rs` | Un stockage partagé par tous les nœuds du cluster (NFS, Ceph RBD), où les cas de déplacement placent le disque de la VM — un disque sur un stockage local est refusé, pas copié. | Non défini : les deux cas de déplacement sont sautés. | |
| `DELONIX_PROXMOX_TEST_CALLBACK_ADDR` | `crates/providers/delonix-proxmox/tests/live.rs:sdn_controllers_fabric_dhcp_and_ip_reservations_round_trip_through_the_node` | L’adresse à laquelle le NŒUD peut joindre cet hôte : le test démarre un serveur HTTP bouchon et donne au nœud `http://<addr>:<port>/…` comme URL d’un contrôleur IPAM et d’un contrôleur DNS, parce que le nœud vérifie les deux en les appelant. | Une IP que le nœud route (`192.168.122.1` pour un nœud de labo en NAT libvirt). | Sans elle, la moitié « contrôleur » de ce test est sautée ; le reste s’exécute quand même. |
| `DELONIX_PROXMOX_TEST_AGENT_VMID` | `crates/providers/delonix-proxmox/tests/live.rs:o_ip_vem_do_agente_de_um_convidado_a_serio` | Une VM existante, avec l’agent invité QEMU en cours d’exécution, dont le test lit l’IP. | Un id de VM. | Sauté lorsqu’elle n’est pas définie, même avec l’URL définie. |
| `DELONIX_TRUENAS_TEST_URL` | `crates/providers/delonix-truenas/tests/live.rs:target` | Appliance TrueNAS contre laquelle exécuter les tests réels du provisionneur. | `https://<host>`. | Active les tests. Ils créent et détruisent `<pool>/dlxlive-<pid>`. |
| `DELONIX_TRUENAS_TEST_POOL` | `crates/providers/delonix-truenas/tests/live.rs:target` | Pool du dataset de test. | Défaut `tank`. | |
| `DELONIX_TRUENAS_TEST_KEY` | `crates/providers/delonix-truenas/tests/live.rs:target` | Clé d’API. | | Utilisée à la place de l’utilisateur et du mot de passe lorsqu’elle est définie. |
| `DELONIX_TRUENAS_TEST_USER` | `crates/providers/delonix-truenas/tests/live.rs:target` | Compte pour l’authentification par mot de passe. | Obligatoire sans clé. | La vérification TLS est désactivée dans ces tests. |
| `DELONIX_TRUENAS_TEST_PASS` | `crates/providers/delonix-truenas/tests/live.rs:target` | Son mot de passe. | Obligatoire sans clé. | |
| `DELONIX_OPNSENSE_TEST_URL` | `crates/providers/delonix-opnsense/tests/live.rs:target` | Appliance OPNsense contre laquelle exécuter les tests réels du client. | `https://<host>`. | Active les tests. Ils créent puis retirent un alias et une règle, puis confirment que l’appliance est laissée propre. |
| `DELONIX_OPNSENSE_TEST_KEY` | `crates/providers/delonix-opnsense/tests/live.rs:target` | Clé d’API. | Obligatoire. | |
| `DELONIX_OPNSENSE_TEST_SECRET` | `crates/providers/delonix-opnsense/tests/live.rs:target` | Secret d’API. | Obligatoire. | La vérification TLS est désactivée dans ces tests. |
| `DELONIX_UPDATE_FIXTURES` | `crates/adapters/delonix-linux/tests/advisor_fixtures.rs:goldens_match_the_rules_as_they_are_today` | Réécrit les fixtures de référence de l’advisor dans `crates/adapters/delonix-linux/tests/fixtures/advisor/` au lieu de s’y comparer. | Définie (n’importe quelle valeur) → réécriture. | Régénérez-les dans le **même commit** que le changement de règle. |

Exécuter les tests réels (remplacez les espaces réservés ; ne commitez jamais de vrais identifiants) :

```bash
DELONIX_PROXMOX_TEST_URL=https://<node>:8006 \
DELONIX_PROXMOX_TEST_NODE=pve \
DELONIX_PROXMOX_TEST_USER=root@pam \
DELONIX_PROXMOX_TEST_PASS='<password>' \
  cargo test -p delonix-proxmox --test live -- --nocapture

DELONIX_TRUENAS_TEST_URL=https://<appliance> \
DELONIX_TRUENAS_TEST_USER=<user> \
DELONIX_TRUENAS_TEST_PASS='<password>' \
DELONIX_TRUENAS_TEST_POOL=tank \
  cargo test -p delonix-truenas --test live -- --nocapture

DELONIX_UPDATE_FIXTURES=1 cargo test -p delonix-linux --test advisor_fixtures
```

## Noms qui ressemblent à des variables mais n’en sont pas

Quelques noms `DELONIX_*` apparaissent dans le code sans être lus depuis l’environnement d’aucun processus.
Ils sont exclus du contrôle ci-dessus par `NOT_ENV` dans `scripts/dev_docs.py`, chacun avec sa raison,
et vous n’avez jamais besoin de les définir :

- `DELONIX_CRI_SOCKET` — une constante Rust dans `bins/delonix-runtime-bin/src/cmd/cluster.rs`, passée à
  `kubeadm … --cri-socket=`. Le socket sur lequel écoute le serveur CRI est `DELONIX_CRI_ADDR`.
- `DELONIX_ROOTX`, `DELONIX_ROOT_BACKUP` — fixtures de test dans `crates/adapters/delonix-sdn/src/infra.rs`,
  prouvant que la lecture de l’environ d’un processus compare des noms entiers, pas des préfixes.
- `DELONIX_IMAGE`, `DELONIX_DISTRO`, `DELONIX_RELEASE`, `DELONIX_BUILT_BY`, `DELONIX_BASE_IMAGE`,
  `DELONIX_BASE_SHA256`, `DELONIX_K8S_VERSION`, `DELONIX_OFFLINE`, `DELONIX_NODE_EXPORTER`,
  `DELONIX_EXTRA_PACKAGES` — clés du fichier de provenance `/etc/delonix-image-release` que
  `image vm build` écrit **à l’intérieur** d’une image de VM construite (`bins/delonix-runtime-bin/src/cmd/vmimage.rs`).
  Lisez-les dans l’invité avec `cat /etc/delonix-image-release` ; aucun processus ne les lit.

---

**Suivant :** [Glossaire](glossary.md) — les termes que vous rencontrez dans ce dépôt, avec leur signification chez Delonix et où chacun est expliqué.
