<!-- translated-from: delonixfile-and-vmfile.md sha256:16ac2cc5b199b5914e1dadc9be3d70cd285c0991f4618a9e1db95713c841dc02 -->
# Delonixfile et VMfile

**Avant de lire :** [Cloner, construire et tester](build-and-test.md) (un binaire et une racine d'état isolée) et [Images OCI, stockage adressé par contenu et overlayfs](cloud-native-primer.md#44-oci-images-content-addressed-storage-and-overlayfs) dans le manuel de cloud native.

Delonix a deux fichiers de build, et ils se ressemblent volontairement : quiconque a écrit un
Dockerfile peut lire les deux. Ce qu'ils construisent est différent.

- Un **Delonixfile** construit une **image de container OCI** (des couches d'un système de fichiers).
  C'est une grammaire Dockerfile enrichie de quelques instructions Delonix, construite par
  `delonix build`.
- Un **VMfile** construit un **disque qcow2 amorçable** pour une VM. Il emprunte la *forme* du
  Dockerfile, mais le mécanisme est `qemu-img` + `virt-customize` sur un disque entier, construit par
  `delonix image vm build`.

Cette page décrit ce que les parseurs de ce dépôt acceptent réellement — et non ce que Docker
accepte. Chaque règle ci-dessous renvoie au code qui l'impose. Après elle, vous pouvez écrire les deux fichiers, prévoir ce que chaque parseur accepte ou refuse, et trouver où modifier une grammaire.

> Les exemples marqués *parse-checked* ont été exécutés contre un binaire construit à partir de
> cette arborescence, avec `DELONIX_ROOT`, `DELONIX_NET_RUNTIME_DIR` et `TMPDIR` pointant vers un
> répertoire jetable, jusqu'au moment où le build commencerait à récupérer des images ou à
> télécharger des disques de base. Les builds complets n'ont **pas été exécutés lors de cette
> relecture** (ils nécessitent un accès au registre/au réseau et, pour les VMs, libguestfs ainsi que
> plusieurs Go de RAM et de disque).

---

## Partie 1 — Delonixfile

### Où se trouve le code

| Aspect | Fichier | Symbole |
|---|---|---|
| Grammaire (parseur) | `crates/adapters/delonix-oci/src/build.rs` | `parse_dockerfile_with_args`, `parse_run_flags`, `parse_secret_mount`, `resolve_target_stage` |
| Orchestration du build | `bins/delonix-runtime-bin/src/cmd/build.rs` | `run`, `build_from_spec`, `build_one_stage`, `default_build_file` |
| Commit de l'image | `crates/adapters/delonix-oci/src/build.rs` | `ImageStore::commit_flat_rootfs` (rootless), `commit_upper` + `build_image` (root) |
| Modèles de projet | `bins/delonix-runtime-bin/templates/<name>/Delonixfile` | rendus par `delonix init` / `stack init` |

### Recherche du fichier

`delonix build [CONTEXT]` sans `-f` appelle `default_build_file` (`cmd/build.rs`) : il utilise
`<context>/Delonixfile` s'il existe, sinon `<context>/Dockerfile`. **La grammaire est la même pour
les deux noms** — les instructions Delonix ci-dessous sont aussi acceptées dans un fichier nommé
`Dockerfile`. `Delonixfile` est simplement le nom découvert en premier. `-t/--tag` est obligatoire.

```bash
delonix build -t myapp:dev .                 # Delonixfile, else Dockerfile
delonix build -t myapp:dev -f build/Other .  # explicit file
```

### Instructions acceptées par le parseur

`parse_dockerfile_with_args` **échoue de manière fermée** : une instruction qu'il ne connaît pas est
une erreur avec le numéro de ligne, et toute instruction autre que `ARG` avant le premier `FROM` est
une erreur. Les lignes terminées par `\` sont jointes ; les lignes commençant par `#` sont des
commentaires. Les noms d'instructions ne sont pas sensibles à la casse.

| Instruction | Ce qui se passe | Remarques |
|---|---|---|
| `ARG NAME[=default]` | Déclare une variable de build ; `${NAME}`/`$NAME` est substitué dans toutes les lignes suivantes | Autorisé avant `FROM` (pour le paramétrer). `--build-arg NAME=VALUE` ne surcharge qu'un `ARG` déclaré. Simplification : les arguments vivent dans **une seule** portée pour tout le fichier, et non par étape. Pas de formes `${NAME:-default}` (`substitute_vars`). |
| `FROM <image> [AS <name>]` | Ouvre une étape | Une étape ultérieure peut aussi écrire `FROM <earlier-stage>` (voir multi-étapes). |
| `RUN <shell>` | S'exécute dans un container de travail via `exec` | Seul `--mount=type=secret,...` est accepté comme option (voir plus bas). |
| `COPY [--from=<stage>] <src> <dst>` | Écrit dans le rootfs de l'étape sur disque | Confiné au contexte/rootfs (`safe_join`, `confine_to`) : `..`, les évasions absolues et les liens symboliques qui sortent de la base sont refusés. |
| `ADD` | **Identique à `COPY`** | Pas de téléchargement d'URL, pas d'extraction automatique d'archive. |
| `ENV K=V [K2="v 2" …]` ou `ENV K V` | Affecte les `RUN` suivants ; l'`ENV` de l'étape finale va dans la configuration de l'image | Les valeurs sont développées à partir des `ENV` précédents (`expand_env_value`). |
| `WORKDIR <dir>` | Répertoire de travail des `RUN` suivants ; la valeur finale va dans la configuration de l'image | |
| `USER <name\|uid[:gid]>` | Enregistré dans la configuration de l'image | Vide → hérite de celui de l'image de base. |
| `CMD`, `ENTRYPOINT` | Enregistrés dans la configuration de l'image | Formes exec (JSON) et shell. |
| `HEALTHCHECK [opts] CMD <cmd>` / `HEALTHCHECK NONE` | La partie après `CMD` est stockée comme commande de santé de l'image | Les options avant `CMD` sont ignorées. Utilisé par `delonix container healthcheck <id>` et par `depends_on: condition: service_healthy` de compose. |
| `LABEL`, `EXPOSE`, `MAINTAINER`, `VOLUME`, `STOPSIGNAL`, `SHELL`, `ONBUILD` | **Acceptées et ignorées** | Métadonnées uniquement ; aucun effet sur le build. |

**Extensions Delonix** (analysées dans des champs de `Dockerfile` du même fichier) :

| Instruction | Analysée dans | Effet dans cette arborescence |
|---|---|---|
| `SCAN fail-on=<sev>` | `scan_fail_on` (`high` par défaut en l'absence de `fail-on=`) | **Analysée mais non imposée par `delonix build`** — aucun appelant ne lit le champ. Contrôlez explicitement les images avec `delonix image scan --fail-on <sev> <image>`. |
| `CPUS <n>` | `cpus` | Écrit dans la configuration de l'image (clé non standard `Cpus`) ; hérité de la base en cas d'absence. Aucun consommateur à l'exécution n'a été trouvé lors de cette relecture. |
| `MEMORY <n>` | `memory` | Identique à `CPUS` (clé `Memory`). |
| `SECURITY <opt>...` | `security` | Identique à `CPUS` (clé `Security`). |

Considérez `CPUS`/`MEMORY`/`SECURITY` comme une *intention enregistrée* ; définissez les vraies
limites à l'exécution (`container run -m/--cpus`, ou `resources` dans un manifeste). Si vous les
câblez, mettez à jour ce tableau.

Comportements du parseur bons à connaître (lus dans le code, pas une promesse de conception) :

- `COPY a b c /dst` ne conserve que la **première** source et le **dernier** argument ; les sources
  intermédiaires sont abandonnées silencieusement.
- `COPY --chown=…`/`--chmod=…` ne sont pas reconnus ; le jeton de l'option serait pris comme chemin
  source.
- `RUN --network=…`, `RUN --security=…` et toute autre option `RUN --<flag>` sont refusés.

### Multi-étapes, `COPY --from` et `--target`

Chaque étape (y compris la finale) reçoit son **propre container de travail et son propre rootfs**
(`build_one_stage`). Le container de travail (`sleep infinity` au-dessus du rootfs de l'étape) est
créé par `ensure_container` à travers le même `WorkloadRuntime` que celui qu'utilisent
`container run` et `start` (`container::with_host_workload`), de sorte qu'un build ne peut pas
diverger de la spécification de lancement d'un container. Les étapes intermédiaires restent sur
disque jusqu'à la fin du build complet, donc :

- `COPY --from=<name-or-index> <src> <dst>` lit directement dans le rootfs de cette étape
  (`resolve_copy_source`, `copy_into_rootfs`).
- `FROM <earlier-stage>` clone le rootfs de cette étape avec `cp -a --reflink=auto`
  (`clone_rootfs`) — `cp -a` afin que les liens symboliques comme `/bin -> usr/bin` restent des liens
  symboliques.
- `--target <name-or-index>` (`resolve_target_stage`) s'arrête à cette étape et l'empaquette ; les
  étapes ultérieures ne sont pas construites. Un nom inconnu est refusé et l'erreur liste les étapes
  existantes. Avec `--target` sur une étape intermédiaire,
  `CMD`/`ENTRYPOINT`/`USER`/`ENV`/`WORKDIR`/`HEALTHCHECK` proviennent de **cette** étape, et non de
  l'étape finale du fichier.

Deux restrictions du mode root (overlay), toutes deux refusées d'emblée avec un message clair :
l'étape finale ne peut pas être `FROM <earlier-stage>` (le commit OCI a besoin d'une vraie image de
base pour la filiation), et `--target` sur une étape intermédiaire est refusé. Le mode rootless n'a
aucune de ces restrictions.

### Secrets de build

```bash
delonix build -t app:dev --secret id=npmrc,src=$HOME/.npmrc .
```

```dockerfile
RUN --mount=type=secret,id=npmrc,target=/root/.npmrc npm ci
```

- `--secret id=<name>,src=<path>` est répétable ; une entrée mal formée ou un fichier `src` manquant
  est une erreur bloquante (`parse_build_secrets`).
- Dans le fichier : `--mount=type=secret,id=<name>[,target=<path>][,required=true|false]`. `target`
  vaut par défaut `/run/secrets/<id>` ; `required` vaut par défaut `false` (un secret optionnel
  manquant est ignoré, comme dans Docker).
- `type=ssh`, `type=cache` et `type=bind` sont **refusés** (`parse_secret_mount`).
- Le secret est monté en bind à chaud (`mount_run_secrets`) dans le mount namespace du container de
  travail uniquement pour ce `RUN`, puis démonté — la vue côté hôte du rootfs que lisent le commit et
  le cache de couches ne le contient jamais. Le contenu du secret n'entre pas non plus dans le hash de
  la clé de cache.

### `--platform` et binfmt

`--platform linux/<arch>` (`parse_platform` : seul `linux/` est accepté) résout l'image de base pour
cette architecture et appose l'architecture sur le résultat. **Exécuter** un `RUN` d'une architecture
étrangère nécessite l'enregistrement `binfmt_misc` + `qemu-user-static` propre à l'hôte, que Delonix
ne gère pas. `build_from_spec` vérifie `/proc/sys/fs/binfmt_misc/qemu-<arch>` avant de construire et
refuse en nommant l'interpréteur s'il est absent ou désactivé.

### Rootless ou root, et le cache de couches

| | Rootless (chemin normal) | Root |
|---|---|---|
| Rootfs d'une étape | répertoire à plat (`prepare_rootfs_flat`) | overlay |
| Commit | `commit_flat_rootfs` — une couche écrasée au-dessus des couches de la base | `commit_upper` (tar de l'upperdir) + `build_image` |
| Cache de couches | **oui** | **jamais** |

Le cache (`<DELONIX_ROOT>/build-cache/<hash>/rootfs`, `--no-cache` pour le contourner) est une chaîne
de hash glissante, un maillon par instruction. `RUN`/`COPY` prennent un instantané du rootfs complet
après exécution ; `ENV`/`WORKDIR` s'intègrent à la chaîne sans instantané. Un maillon `COPY` hache
les **octets** copiés, de sorte qu'un fichier modifié invalide tout ce qui suit. En cas de
correspondance, l'instantané en cache est cloné dans le rootfs d'un container neuf — jamais
synchronisé sur un container vivant (cela corrompait les montages `/proc`/`/sys`/`/dev` et a été
abandonné). Compromis énoncés dans la documentation du module : instantanés du rootfs complet, et non
des différences par couche (atténué par `--reflink=auto`), et **aucun GC du cache** — `build-cache/`
ne fait que croître.

Comme chaque `RUN` s'exécute dans un vrai container, les prérequis d'hôte de
[Préparer votre environnement](environment.md) (user namespaces, subuid/subgid, AppArmor sur
Ubuntu) s'appliquent aussi aux builds.

### Exemple détaillé : un modèle

`delonix init -t <template> [DIR]` (ou `delonix stack init --template <template>`) rend
`bins/delonix-runtime-bin/templates/<template>/`, en remplaçant `__NAME__`, `__PORT__` et
`__TEMPLATE_VERSION__` (la valeur par défaut `version=` de `template.meta`, ou
`-v/--template-version`). Dans un répertoire vide, il écrit le projet complet ; dans un répertoire
non vide, il n'ajoute que la colle Delonix (Delonixfile, manifeste, fichiers de CI). Généré dans un
répertoire jetable à partir de `httpd` (*exécuté*) :

```bash
$ delonix init -t httpd web
detected an empty directory → stack init --template httpd
  created: web/.dockerignore
  created: web/Delonixfile
  ...
```

```dockerfile
# Delonixfile — production-ready Apache httpd.
# Build with:  delonix build -t web:dev .
FROM httpd:2.4-alpine
# Listen on 8080 and harden a little (no version banner).
RUN sed -i 's/^Listen 80$/Listen 8080/' /usr/local/apache2/conf/httpd.conf && \
    printf '\nServerTokens Prod\nServerSignature Off\nTraceEnable Off\n' >> /usr/local/apache2/conf/httpd.conf
COPY public /usr/local/apache2/htdocs
EXPOSE 8080
HEALTHCHECK CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
CMD ["httpd-foreground"]
```

Le `delonix-manifest.yaml` généré passe `delonix manifest validate` (*exécuté*). `--up` construirait,
ferait un `stack apply` et attendrait l'état de santé — non exécuté ici.

Un fichier multi-étapes utilisant l'essentiel de la grammaire, *parse-checked* (l'erreur
`--target nosuch` prouve que le fichier a été analysé et liste ses étapes, avant tout pull) :

```dockerfile
ARG GO_VERSION=1.23
FROM golang:${GO_VERSION}-alpine AS builder
WORKDIR /src
COPY go.mod ./
RUN --mount=type=secret,id=netrc,target=/root/.netrc go mod download
COPY . .
RUN CGO_ENABLED=0 go build -o /out/app ./cmd/app

FROM alpine:3.20
COPY --from=builder /out/app /usr/local/bin/app
USER 65534
HEALTHCHECK CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
CMD ["/usr/local/bin/app"]
```

```text
$ delonix build -t demo:dev --target nosuch .
error invalid argument: no stage named 'nosuch' in this Dockerfile — known stages: builder
$ delonix build -t d -f Dockerfile.cache .        # RUN --mount=type=cache,...
error invalid argument: RUN --mount=type=cache: só type=secret é suportado (ssh/cache/bind ainda não)
$ delonix build -t d --platform windows/amd64 .
error invalid argument: --platform 'windows/amd64': only 'linux/<arch>' is supported (this engine does not run another OS)
```

(Certaines erreurs du parseur sont encore en portugais ; elles comptent comme dette LANG-01 — voir
[Flux de contribution](contributing-workflow.md).)

### Le `Delonixfile` du dépôt lui-même n'est pas construit par `delonix`

Le `Delonixfile` à la racine du dépôt empaquette la CLI `delonix` dans une image de container. Il est
construit avec **Docker ou Podman** (`docker build -f Delonixfile …`, ou `make image`), et non avec
`delonix build` : il utilise `# syntax=docker/dockerfile:1` et `RUN --mount=type=cache`, que
`delonix build` refuse. Ne l'utilisez pas comme exemple de la grammaire Delonix ; utilisez les
modèles.

---

## Partie 2 — VMfile

### Où se trouve le code

| Aspect | Fichier | Symbole |
|---|---|---|
| Grammaire, scaffold, constructeur | `bins/delonix-runtime-bin/src/cmd/vmfile.rs` | `parse`, `classify_base`, `resolve_base`, `stage_ops`, `build`, `finalize`, `scaffold` |
| Point d'entrée CLI, recette dorée | `bins/delonix-runtime-bin/src/cmd/vmimage.rs` | `VmImageCmd::Build`, `VmImageStore`, `VmImage`, `customize_args`, `tool_failure_hint` |
| Build déclaratif | `bins/delonix-runtime-bin/src/cmd/vm.rs` | `VmBuildSpec` (`spec.build` de `kind: VirtualMachine`) |

### Scaffold

```bash
delonix vm init --vmfile [DIR] [--name <n>]   # or: delonix image vm init <name> [-d DIR]
```

Les deux écrivent `VMfile` et `cloud-init/user-data.yaml` (*exécuté* dans un répertoire jetable). Le
scaffold est censé être une recette fonctionnelle, et le test `parseia_o_scaffold_que_escrevemos` le
garde analysable. Deux choses sont à corriger à la main dans l'indication « Next: » qu'il affiche :
l'option est `vm create --disk`, et non `--disk-image` ; et voyez la remarque sur `CLOUDINIT`
ci-dessous à propos du nom de fichier.

### Construire

```bash
delonix vm build [-f vm.yaml|VMfile] [-t <tag>] [--target <image>] [--network] [--no-compress] [CONTEXT]
```

`delonix image vm build` est la même commande (les deux partagent un seul `BuildArgs`). La recette exécutée se
décide comme `docker build` choisit entre des fichiers : un `-f` explicite l'emporte (un `.yaml`/`.yml` est lu
comme un `vm.yaml`, tout le reste comme un `VMfile`) ; sans `-f`, un `vm.yaml` dans le contexte l'emporte sur un
`VMfile`, qui l'emporte sur la recette dorée intégrée (voir [Construire des microVMs](microvm-setup.md)). Les
options de la recette dorée (`--k8s-version`, `--extra-package`, `--extra-run`, `--offline`, `--no-k8s`,
`--cri-bin`, `--delonix-bin`) sont **refusées** avec un `vm.yaml` ou un `VMfile`, et `--network` est
refusé sans l'un des deux.

### `vm.yaml` : le front end de style compose

Un `VMfile` est à une image de VM ce qu'un `Dockerfile` est à une image de container. Un `vm.yaml` est à celle-ci ce
qu'un `compose.yaml` est à un container : il nomme les images d'un dossier et porte les paramètres qui rendent le
qcow2 complet — `size`, `packages.install`, `users`, `services`, `files`, `env`, `cloud_init`,
`run`, ce qu'il faut **retirer** (`remove.packages/paths/users/services`, appliqué après tout le reste pour
pouvoir élaguer ce qu'un paquet ou un `run:` a fait entrer) et `cleanup` (cache des paquets, logs, historique,
`/tmp`, `machine-id`).

C'est uniquement un front end (`cmd/vmspec.rs`). Chaque image se compile vers un builder qui existe déjà —
un `VMfile` synthétisé (`profile: custom`, le défaut), la recette dorée (`profile: rootless|k8s`)
ou un fichier existant (`build.file`) — il n'y a donc pas de second moteur de build. Règles à connaître :

- **Strict** : une clé inconnue est une erreur, tout comme un champ que la voie choisie ne peut pas honorer (un
  `hostname:` avec `profile: k8s` est refusé par son nom, pas ignoré).
- **`${TAG}` / `${VAR:-default}`** sont développés sur les valeurs parsées (un `${TAG}` dans un commentaire n'est
  pas évalué). `-t` est la tag du résultat **et** `${TAG}`.
- **Les chemins relatifs sont relatifs au dossier propre du `vm.yaml`**, quoi que dise le contexte.
- **`packages` requiert `network: true`** : un build qui atteint internet donne une image différente un jour
  différent, donc c'est opt-in et le refus le dit.
- `remove.paths` doit être absolu, sans `..`, et jamais un répertoire système de premier niveau.

**Appliances** (`appliance:`). Certaines images ne peuvent pas se décrire comme des modifications d'une image
cloud : l'installateur du fournisseur doit s'exécuter (Proxmox depuis son ISO, OpenStack qui tire ~20 GiB de
containers). Pour celles-ci, la recette nomme un **builder**, pas un chemin : `appliance: {builder: proxmox, args: [pve,
"9.2-1"]}` exécute `scripts/appliances/build-proxmox.sh` (trouvé dans le dossier du `vm.yaml` ou dans n'importe quel
dossier au-dessus) avec un `OUT_DIR` isolé à côté du store d'images, prend l'unique `*.qcow2` qu'il
laisse (un `.raw.qcow2` est ignoré), et l'enregistre avec la sémantique de `image vm import` —
`--appliance` sauf si `cloud_init: true`. Comme le nom est validé (`[a-z0-9-]`) et résolu
à l'intérieur de `scripts/appliances/`, un `vm.yaml` ne peut pas faire exécuter à l'hôte un fichier de son choix ; `args`
et `env` sont eux aussi validés (pas de `-` initial, pas de `PATH`/`LD_*`/`BASH_ENV`…). Les champs qu'un builder
décide lui-même (`packages`, `users`, `hostname`, `network`, `profile`…) sont refusés par leur nom.

Les dossiers par distribution sous `images/` (`images/ubuntu/` en premier) portent chacun un `vm.yaml`, le
fichier cloud-init, les artefacts et un README.

### Instructions

`parse` échoue de manière fermée : une instruction inconnue est une erreur qui nomme l'ensemble
supporté. Seul `FROM` peut venir en premier. Les continuations `\` sont jointes ; un `#` ne commence
un commentaire qu'en début de ligne (ainsi `RUN sed 's/#x/y/'` n'est pas altéré).

| Instruction | Portée | Effet (`stage_ops` → `virt-customize`) |
|---|---|---|
| `FROM <ref> [AS <name>]` | ouvre une étape | Voir *Ce que `FROM` accepte*. `as` n'est pas sensible à la casse. |
| `RUN <shell>` | étape de travail | `--run-command` dans l'invité ; **hors ligne** sauf avec `--network`. |
| `COPY <src> <dst>` | étape de travail | Copie depuis le contexte de build ; `src` confiné au contexte (`build::safe_join`). Exactement deux arguments. |
| `COPY --from=<stage> <src> <dst>` | étape de travail | `virt-copy-out` depuis le disque de cette étape vers un répertoire intermédiaire de l'hôte, puis copie. L'étape doit être **nommée et déclarée plus haut** — vérifié à l'analyse. |
| `ENV KEY=value` | étape de travail | Ajoute `KEY=value` à `/etc/environment` (une VM n'a pas de configuration d'image). Une paire par ligne ; la clé et la valeur sont passées à un shell **sans guillemets**, évitez donc les espaces et les métacaractères du shell. |
| `USER <name>` | étape de travail | `useradd -m -s /bin/bash <name>` si le compte n'existe pas. |
| `PASSWORD <user>:<password>` | étape de travail | Définit le mot de passe de ce compte. Il est intégré à chaque copie de l'image. |
| `ROOTPASSWORD <password>` | étape de travail | Définit le mot de passe de root. Même mise en garde. |
| `SSHKEY <user> <path-or-key>` | étape de travail | Ajoute à `/home/<user>/.ssh/authorized_keys` (`~/` est développé). La valeur doit être un fichier lisible ou une clé commençant par `ssh-`/`ecdsa-`. Le compte doit exister (créez-le d'abord avec `USER`). |
| `CLOUDINIT <path>` | étape de travail | Copie ce fichier (confiné au contexte) dans `/etc/cloud/cloud.cfg.d/`, **en conservant son nom de fichier**. cloud-init ne charge depuis ce répertoire que les fichiers se terminant par `.cfg` ; nommez-le donc p. ex. `99-myimage.cfg` — le nom `user-data.yaml` du scaffold n'est pas pris en compte tel quel (non exécuté lors de cette relecture ; d'après le chemin du code et le comportement documenté de cloud-init). `vm create` ajoute quand même par-dessus son propre seed NoCloud par instance. |
| `SIZE <n>G` | propriété de l'étape | `qemu-img resize` **avant** l'exécution de toute étape de travail — agrandir après qu'un `RUN` a rempli le disque serait trop tard, c'est pourquoi ce n'est pas une étape de travail. |
| `HOSTNAME <name>` | propriété de l'étape | Écrit `/etc/hostname` (première opération de l'étape). |
| `VCPUS <n>` | image | Enregistré comme `default_vcpus`. |
| `MEMORY <n>` | image | Enregistré comme `default_memory`. |
| `HYPERVISOR <backend>` | image | Validé et canonisé par `delonix_vm::valid_backend_name` (`ch` → `cloud-hypervisor`) ; enregistré comme `default_backend`. |
| `LABEL k=v` | image | **Analysé mais non enregistré** dans les métadonnées de l'image dans cette arborescence. |

Il n'y a ni `CMD` ni `ENTRYPOINT` : une VM démarre un init. `CLOUDINIT` est l'équivalent le plus
proche.

### Ce que `FROM` accepte

`classify_base` est pure et décide du type de référence :

1. **Une étape antérieure nommée** — l'emporte sur tout le reste (`resolve_base`).
2. **`ubuntu:<rel>`, `debian:<rel>`, `rocky:<rel>`, `fedora:<rel>`** — d'abord l'image de base propre
   au projet pour cette distribution/release (copie locale, puis registre officiel —
   `vmimage::official_distro_base`) ; s'il n'y en a pas, la cloud image de la distribution elle-même,
   téléchargée et vérifiée contre le fichier de sommes de contrôle de l'éditeur
   (`vmimage::download_base`).
3. **URL `http://` / `https://`** — téléchargée ; vérifiée contre `<url>.sha256` lorsque l'éditeur en
   propose un, sinon considérée fiable sur la seule base de TLS, et le build le signale.
4. **Tout le reste** — une image VM déjà présente dans le store local (`delonix image vm ls`). Tout
   autre `name:tag` est traité comme un tag local, et non comme une distribution inconnue.

### Ce qu'est une étape, et où arrive le résultat

Une étape est un **disque entier**, pas une couche (`build`) :

1. La base est **aplatie** dans `<work>/<stage>.qcow2` avec `qemu-img convert` (pas de backing file,
   de sorte que l'artefact ne dépend jamais de la présence continue de la base).
2. `SIZE` est appliqué, puis toutes les étapes de travail s'exécutent en un seul appel à
   `virt-customize` (`--no-network` sauf avec `--network`). `vmimage::customize_args` ajoute toujours
   un réétiquetage SELinux comme dernière commande (et désactive le réétiquetage différé propre à
   libguestfs), afin que les invités SELinux ne démarrent pas avec des fichiers non étiquetés.
3. Les étapes nommées sont conservées pour `COPY --from` ; les étapes anonymes ne sont pas
   adressables. Seule la **dernière** étape devient une image.
4. Sauf avec `--no-compress` : `virt-sparsify --in-place` (au mieux), puis
   `qemu-img convert -c -o compression_type=zstd`. zstd parce que l'image devient le backing file en
   lecture seule de chaque VM créée à partir d'elle ; la vitesse de décompression est donc une
   propriété d'exécution.
5. Le qcow2 est déplacé vers `VmImageStore::qcow2_path(tag)` sous `<DELONIX_ROOT>/vm-images/`, et un
   enregistrement de métadonnées `VmImage` est sauvegardé à côté.

Métadonnées enregistrées : `digest` et `size` ; `default_vcpus`/`default_memory`/`default_backend`
issus du fichier ; `cloud_init: true` ; `built_by: "delonix <version>"` ; `distro` et
`kernel_version` **hérités** uniquement lorsque le `FROM` final est une image locale (une URL ne donne
rien à hériter, et le code refuse de deviner).

`vm create` applique les valeurs par défaut enregistrées uniquement là où l'appelant n'a pas décidé :
les options `--vcpus`/`--memory` l'emportent sur `VCPUS`/`MEMORY` ; pour le backend, `--backend` >
le `HYPERVISOR` de l'image > `DELONIX_VM_BACKEND` > `vm default-backend` > auto-détection
(`resolve_vm_defaults` dans `cmd/vm.rs`, et `delonix_vm::create_with`).

**Hors ligne par défaut, et pourquoi.** Un `RUN` qui accède à Internet produit une image différente
selon le moment où il s'est exécuté. `--network` est opt-in, car la chose la plus courante que veut
un VMfile est d'installer un paquet. Avec `--network`, l'appliance de libguestfs obtient son réseau
de `passt`, qui comporte des pièges côté hôte documentés dans
[Construire des microVMs](microvm-setup.md#6-troubleshooting).

**Espace de travail.** Le répertoire de travail est créé sous `std::env::temp_dir()`
(`delonix-vmfile-<pid>`), c'est-à-dire `$TMPDIR` ou `/tmp`, et contient un disque complet aplati par
étape. Faites pointer `TMPDIR` vers un système de fichiers qui a de la place — `/tmp` est souvent un
petit tmpfs et est vidé au redémarrage. Un build qui échoue laisse ce répertoire derrière lui
(observé lors de cette relecture) ; supprimez-le à la main.

### Build déclaratif

`kind: VirtualMachine` peut construire son propre disque avec `spec.build` (`VmBuildSpec`) :
`context` (relatif au répertoire du **manifeste**), `file` (par défaut `<context>/VMfile`), `tag` (par
défaut `<metadata.name>:latest`), `compress`, `network`. `apply` appelle le même `vmfile::build`.
`disk` et `build` sont mutuellement exclusifs.

### Exemple détaillé (parse-checked)

```dockerfile
FROM my-base:1.0 AS builder
RUN make -C /src

FROM my-base:1.0
SIZE 20G
HOSTNAME web
COPY --from=builder /src/app /usr/local/bin/app
ENV APP_ENV=production
USER app
VCPUS 2
MEMORY 2G
HYPERVISOR ch
LABEL org.opencontainers.image.title=web
```

Sans image nommée `my-base:1.0` dans le store jetable, le fichier est analysé et le build s'arrête à
la résolution de la base — avant tout travail sur disque :

```text
$ delonix image vm build -t web:1.0 .
[1/2] builder: FROM my-base:1.0
error invalid argument: FROM my-base:1.0: no such local VM image, and it is not a URL nor a known cloud image (ubuntu:/debian:/rocky:) — see `delonix vm ls`
```

Et les refus du parseur :

```text
VMfile:2: unknown instruction 'CMD' — supported: FROM RUN COPY ENV USER PASSWORD ROOTPASSWORD CLOUDINIT SSHKEY SIZE HOSTNAME VCPUS MEMORY HYPERVISOR LABEL
VMfile:2: no earlier stage named 'nope'
VMfile:2: invalid argument: unknown VM backend: 'vmware' (use 'cloud-hypervisor', 'libvirt')
--offline belong to the built-in golden recipe and mean nothing with a VMfile — the VMfile describes all of that itself
```

Un build complet (`virt-customize`, téléchargements, compression) n'a **pas été exécuté lors de cette
relecture**.

---

## Comparaison

| | Dockerfile (Docker/BuildKit) | Delonixfile (`delonix build`) | VMfile (`delonix image vm build`) |
|---|---|---|---|
| Sortie | image OCI | image OCI (poussable, récupérable par Docker) | qcow2 amorçable + métadonnées `VmImage` |
| Unité d'une étape | couches de système de fichiers | un container de travail + rootfs | un disque entier aplati |
| `FROM` | image | image ou étape antérieure | distro:release, URL, image VM locale, ou étape antérieure |
| `RUN` s'exécute dans | le container de build | le container de travail via `exec` (userns rootless) | l'invité, via `virt-customize` (hors ligne par défaut) |
| `COPY --from` | oui | oui (nom ou index) | oui (étapes nommées et antérieures uniquement ; `virt-copy-out`) |
| Options `--chown/--chmod` de `COPY` | oui | non | non |
| `ADD` URL / extraction d'archive | oui | non (`ADD` = `COPY`) | pas d'`ADD` |
| `RUN --mount` | secret, ssh, cache, bind, tmpfs | `type=secret` uniquement | aucun |
| `--target` | oui | oui | non |
| `--platform` | oui | `linux/<arch>`, binfmt de l'hôte requis | non (les images restent amd64 — [ADR-0018](../../adr/0018-vm-images-stay-amd64.md)) |
| Cache de couches | oui | rootless uniquement, sans GC | aucun |
| `CMD`/`ENTRYPOINT`/`USER`/`ENV` | configuration de l'image | configuration de l'image | pas de `CMD` ; `USER` crée un compte ; `ENV` → `/etc/environment` |
| Indications de ressources | — | `CPUS`/`MEMORY`/`SECURITY` enregistrées, non appliquées | `VCPUS`/`MEMORY`/`HYPERVISOR` appliquées par `vm create` comme valeurs par défaut |
| Contrôle des vulnérabilités | — | `SCAN` analysé, non imposé (utilisez `image scan --fail-on`) | — |
| Comptes / clés / mots de passe | — | — | `USER`, `SSHKEY`, `PASSWORD`, `ROOTPASSWORD` |
| Taille du disque | — | — | `SIZE` (avant toute étape de travail) |
| Instruction inconnue | erreur | erreur | erreur |

## Si vous modifiez une grammaire

- Gardez les deux parseurs **fermés en cas d'échec** : une instruction inconnue est une erreur, jamais
  une ligne ignorée.
- Une nouvelle instruction arrive avec un test unitaire dans le `mod tests` du parseur, une ligne
  dans cette page et — si le scaffold l'utilise — le test du scaffold qui passe toujours.
- Si une instruction est analysée mais pas encore câblée à un effet (comme `SCAN`,
  `CPUS`/`MEMORY`/`SECURITY` et le `LABEL` du VMfile aujourd'hui), dites-le ici ; un champ que
  l'utilisateur écrit et que le moteur ignore doit être documenté comme tel ou supprimé.
- Le parseur du Delonixfile vit dans un crate de bibliothèque (`delonix-oci`), il ne doit donc pas
  afficher ; le parseur du VMfile se trouve dans le binaire de la CLI. Voir
  [Crates](crates.md).

---

**Suivant :** [Construire des microVMs](microvm-setup.md) — les prérequis d'hôte, les backends, les images et les verbes de jour 2 pour amorcer et tester des microVMs.
