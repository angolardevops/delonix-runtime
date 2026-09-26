<!-- translated-from: build-and-test.md sha256:a509235c85941dead7c564eb0fe69325be76ad29f67752755461f3637af80128 -->
# Cloner, compiler et tester

**Avant de lire :** [Préparer votre environnement](environment.md) : la toolchain épinglée, `protoc`, et un hôte qui passe ses vérifications.

Cette page suppose l’hôte décrit dans [Préparer votre environnement](environment.md) : la toolchain
Rust épinglée et `protoc`. Après elle, vous saurez compiler et installer votre arborescence, exécuter
chaque gate de CI localement, et exécuter la batterie E2E et le harnais de chaos sans toucher à un
état réel du moteur.

Tout ce qui suit s’exécute depuis la racine de votre checkout — idéalement un **git worktree** : un
répertoire de travail séparé avec sa propre branche, un par tâche, créé à partir de `origin/main`
(la commande se trouve dans [Commencer ici, étape 2](start-here.md#2-open-a-worktree-from-originmain) ;
les règles se trouvent dans [Flux de contribution](contributing-workflow.md#one-worktree-per-task)).

## Cloner

```bash
git clone https://github.com/angolardevops/delonix-runtime.git
cd delonix-runtime
git fetch --tags origin      # several gates compare against the release tags
```

## Compiler

```bash
cargo build --workspace                    # every crate and every binary
cargo build -p delonix-runtime-bin         # just the `delonix` CLI
cargo build --release -p delonix-runtime-bin   # what the docs generator and the CLI gates use
```

Le workspace fournit ces binaires, à partir de ces paquets :

| Binaire | Paquet | Sortie |
|---|---|---|
| `delonix` (la CLI) | `delonix-runtime-bin` | `target/debug/delonix` ou `target/release/delonix` |
| `delonix-cri` | `delonix-cri` | `target/<profile>/delonix-cri` |
| `delonix-mcp` | `delonix-mcp-bin` | `target/<profile>/delonix-mcp` |
| `delonix-mgmt` | `delonix-mgmt-bin` | `target/<profile>/delonix-mgmt` |

Le workflow de release compile exactement ces quatre paquets. Si vous définissez `CARGO_TARGET_DIR`, les binaires
y atterrissent au lieu de `target/`.

Deux remarques pratiques :

- **Testez toujours le binaire que vous avez compilé** (`./target/debug/delonix`), jamais un `delonix` présent dans votre
  `PATH` — c’est une release installée, souvent en retard de plusieurs versions.
- Si vous travaillez dans plusieurs worktrees, les faire tous pointer vers un même `CARGO_TARGET_DIR` partagé économise du disque,
  mais deux builds qui s’y exécutent en même temps s’attendront mutuellement et peuvent invalider
  les artefacts l’un de l’autre. Un répertoire target par worktree est plus lent la première fois et prévisible
  ensuite.

## Installer votre build localement

La plupart des modifications n’ont jamais besoin d’un build installé : exécutez `./target/debug/delonix` depuis votre worktree. N’installez
que lorsque vous avez besoin d’un chemin stable — une unité systemd, un script dans un autre shell, un kubelet qui parle à
`delonix-cri`. Avant et après l’installation, vérifiez **quel build** vous exécutez :

```bash
./target/debug/delonix --version    # commit: <hash> (+N commits since vX.Y.Z) · built: <date>
command -v delonix                  # which `delonix` your shell would run instead
```

La ligne `commit:` provient de `bins/delonix-runtime-bin/build.rs` (`DELONIX_GIT_HASH`,
`DELONIX_GIT_SINCE`). Entre deux releases, chaque build porte le même numéro de version ; le commit
est donc le seul moyen de distinguer votre build de celui publié.

### Comment `delonix` trouve ses binaires serveur

`delonix serve cri`, `delonix serve api` et `delonix mcp` ne contiennent pas les serveurs : ils font un `exec` de
`delonix-cri`, `delonix-mgmt` et `delonix-mcp` (`exec_server` dans
`bins/delonix-runtime-bin/src/cmd/serve.rs`). La recherche est la suivante :

1. le fichier de ce nom **à côté du `delonix` en cours d’exécution** ;
2. sinon le nom dans le `PATH`.

`delonix` passe au serveur sa propre version dans `DELONIX_DISPATCH_VERSION`, et un serveur d’une
autre release refuse de démarrer. Il se passe aussi lui-même dans `DELONIX_BIN`, afin que le serveur rappelle
la même CLI. Un serveur démarré directement (par exemple par une unité) trouve la CLI via
`DELONIX_BIN`, puis un `delonix` situé à côté de lui-même, puis le `PATH` (`cli_bin` dans
`crates/contexts/delonix-node/src/dispatch.rs`). **Gardez ensemble les quatre binaires d’un même build** ;
un mélange de votre build et d’une release est refusé, ou exécute du code que vous n’aviez pas l’intention de tester.

`delonix cluster kubeadm` et `delonix image vm build` cherchent `delonix-cri` dans leur propre ordre
(`resolve_cri_bin` dans `bins/delonix-runtime-bin/src/cmd/vmimage.rs`) : `--cri-bin`, puis à côté de
`delonix`, puis un `cargo build --release -p delonix-cri` si le répertoire courant se trouve dans un
checkout des sources, et seulement ensuite un téléchargement de l’asset publié.

Compilez les quatre avant de les installer :

```bash
cargo build --release -p delonix-runtime-bin -p delonix-cri -p delonix-mgmt-bin -p delonix-mcp-bin
```

### Option A — l’exécuter depuis le worktree (le plus sûr)

Rien n’est copié ; rien en dehors de votre checkout ne peut donc le prendre par accident :

```bash
alias delonix-dev="$PWD/target/release/delonix"
delonix-dev --version
```

Les serveurs sont trouvés parce qu’ils se trouvent à côté de lui dans `target/release/`. Sur Ubuntu 23.10+, ce chemin
a besoin de son propre profil AppArmor (voir
[AppArmor](environment.md#ubuntu-2310-apparmor-blocks-user-namespaces-for-your-dev-binary)).

### Option B — installer pour votre utilisateur dans `~/.local/bin`

```bash
install -d ~/.local/bin
install -m 0755 target/release/delonix target/release/delonix-cri \
  target/release/delonix-mgmt target/release/delonix-mcp ~/.local/bin/
hash -r                              # forget the path your shell cached
command -v delonix && delonix --version
```

Si une release est aussi installée dans `/usr/local/bin`, c’est le répertoire qui vient en premier dans le `PATH` qui l’emporte.

**AppArmor.** `scripts/install.sh` écrit un seul profil, `/etc/apparmor.d/delonix`, lié à
`<install dir>/delonix`, et uniquement sur les hôtes avec
`kernel.apparmor_restrict_unprivileged_userns=1`. Un binaire que vous avez copié vers un nouveau chemin n’est pas couvert.
Ne relancez pas l’installateur pour « déplacer » ce profil sur une machine qui utilise aussi une installation
publiée : le fichier de profil est réécrit, et le binaire publié le perd. Ajoutez plutôt un second profil
avec un nom différent — de la même forme que celle écrite par l’installateur, il ne remplace donc rien :

```bash
printf 'abi <abi/4.0>,\ninclude <tunables/global>\nprofile delonix-dev %s flags=(unconfined) {\n  userns,\n}\n' \
  "$HOME/.local/bin/delonix" | sudo tee /etc/apparmor.d/delonix-dev >/dev/null
sudo apparmor_parser -r /etc/apparmor.d/delonix-dev
```

*Non vérifié ici :* cette commande reproduit `install.sh` (le bloc AppArmor) avec un nom de profil et un fichier
différents ; elle n’a pas été chargée sur un hôte où la restriction était active lors de la rédaction de cette page.

L’installateur ajoute aussi la complétion du shell, les pages de manuel et les fichiers de syntaxe pour éditeurs, mais seulement dans sa phase
binaire. Pour votre propre build, générez-les à partir du binaire si vous les voulez :

```bash
mkdir -p ~/.local/share/bash-completion/completions
delonix completion shell bash > ~/.local/share/bash-completion/completions/delonix
delonix man --dir ~/.local/share/man
```

### Option C — installation système dans `/usr/local/bin`

```bash
sudo install -m 0755 target/release/delonix target/release/delonix-cri \
  target/release/delonix-mgmt target/release/delonix-mcp /usr/local/bin/
```

**Uniquement sur une machine où aucun workload Delonix n’est en service.** Le binaire installé n’est pas qu’une
simple commande :

- les unités de démarrage écrites par `delonix system boot enable` lancent `ExecStart=<exe> container start <name>`,
  où `<exe>` est le chemin du binaire qui a exécuté `enable` (`bins/delonix-runtime-bin/src/cmd/boot.rs`,
  préfixe d’unité `delonix-boot-`) ; remplacer ce fichier change ce qui démarre après le prochain redémarrage ;
- `dist/delonix-cri.service` exécute `/usr/local/bin/delonix-cri` ; sur un nœud Kubernetes, le kubelet
  reçoit donc votre build au prochain redémarrage de cette unité ;
- les processus de longue durée démarrés auparavant (le pin et le processus de contrôle du réseau, les superviseurs de containers)
  continuent d’exécuter le code avec lequel ils ont démarré ; pendant un temps, deux builds tournent donc côte à côte.

Vérifiez d’abord :

```bash
delonix container ls -a; delonix vm ls
ls ~/.config/systemd/user/delonix-boot-* /etc/systemd/system/delonix-* 2>/dev/null
pgrep -a delonix
```

### Préparer l’hôte

`scripts/install.sh` fait deux travaux distincts. Seul le premier concerne le binaire :

| Partie | Ce qu’elle fait | Flag qui la saute ou l’active |
|---|---|---|
| Binaire | télécharge une release, vérifie la signature minisign et le sha256, installe `delonix` (plus `delonix-mcp`, `delonix-mgmt`, et `delonix-cri` avec `--with-cri`), puis la complétion, les pages de manuel, la syntaxe pour éditeurs et l’extension d’éditeur | sautée avec `--no-binary` ; `--user` choisit `~/.local/bin` |
| Paquets de l’hôte | `slirp4netns`, `uidmap`, `nftables`, `iproute2`, `conntrack` | toujours |
| Identité rootless | plages `/etc/subuid` et `/etc/subgid` pour votre utilisateur | toujours |
| AppArmor | profil pour `<dir>/delonix` lorsque la restriction de userns est active | toujours (lorsque la restriction est active) |
| Ancien Debian | `kernel.unprivileged_userns_clone=1` lorsqu’il vaut `0` | toujours (lorsque nécessaire) |
| Dépendances des VM | libvirt, qemu, outillage cloud-init ; Cloud Hypervisor et son firmware téléchargés depuis l’amont | sautée avec `--no-vm` |
| Provider de VM par défaut | `providers.yaml` avec `defaultProvider: libvirt` (ADR-0054) : `/etc/delonix/`, ou `~/.config/delonix/` avec `--user` ; écrit seulement s’il n’existe pas, jamais réécrit | `--vm-provider cloud-hypervisor` change le défaut ; sauté avec `--no-vm` |
| Réglage du noyau | `/etc/modules-load.d/delonix.conf`, `/etc/sysctl.d/99-delonix.conf` | sautée avec `--no-tune` |
| Délégation de cgroup | drop-in de `user@.service`, uniquement si ce n’est pas déjà délégué | sautée avec `--no-delegate` |
| Accélérateurs | CDI NVIDIA et groupe `render`, uniquement lorsqu’un GPU est présent | sautée avec `--no-gpu` |
| Options à activer | ports inférieurs à 1024 (`--low-ports`), construction d’images de VM (`--with-image-build`), réglage pour la montée en charge (`--production`) | désactivées par défaut |

Pour préparer un hôte pour votre propre build **sans télécharger aucune release Delonix**, exécutez l’installateur
depuis votre checkout avec `--no-binary` :

```bash
bash scripts/install.sh --no-binary            # add --no-vm if you do not need VM dependencies
bash scripts/install.sh --help                 # the full flag list, from the script header
```

Avec `--no-binary`, le profil AppArmor est écrit pour le répertoire du `delonix` que
`command -v delonix` trouve (ou `/usr/local/bin` s’il n’y en a aucun) — la même précaution que ci-dessus s’applique sur une
machine dotée d’une installation publiée. Le script utilise `sudo` pour les étapes concernant l’hôte.

Demandez ensuite au binaire si l’hôte est prêt (en lecture seule) :

```bash
delonix system doctor     # every prerequisite, and how to fix each; --strict exits non-zero on a failure
delonix system info       # state root, rootless, cgroup delegation, network infra
```

Voir [Diagnostiquer l’hôte](environment.md#diagnosing-the-host) pour la signification de chaque vérification.

### Utiliser une racine d’état isolée

Un build installé utilise par défaut votre racine d’état **réelle** : les mêmes containers, réseaux et
volumes que la release. Exportez d’abord `DELONIX_ROOT` et `DELONIX_NET_RUNTIME_DIR` (voir
[Isoler l’état du moteur](#isolating-the-engines-state)), et consultez
[Variables d’environnement](environment-variables.md) pour toutes les autres variables que lit votre build.

### Désinstaller et revenir en arrière

Il n’existe pas de flag de désinstallation dans `install.sh`. Supprimez ce que vous avez copié :

```bash
rm -f ~/.local/bin/delonix ~/.local/bin/delonix-cri ~/.local/bin/delonix-mgmt ~/.local/bin/delonix-mcp
hash -r
sudo apparmor_parser -R /etc/apparmor.d/delonix-dev && sudo rm /etc/apparmor.d/delonix-dev   # if you added it
```

Pour revenir à un binaire publié, relancez l’installateur ; il remplace les binaires de son répertoire
d’installation par la release que vous nommez :

```bash
curl -fsSL https://github.com/angolardevops/delonix-runtime/releases/latest/download/install.sh | bash -s -- --user
curl -fsSL https://github.com/angolardevops/delonix-runtime/releases/latest/download/install.sh | bash -s -- --version vX.Y.Z
```

Les fichiers de complétion et les pages de manuel générés à la main ne sont supprimés par aucune de ces deux étapes. Terminez par
`delonix --version` pour confirmer le commit sur lequel vous êtes revenu.

## Exécuter les tests

```bash
cargo test --workspace                       # the whole suite
cargo test -p delonix-sdn                    # one crate
cargo test -p delonix-stack -- reconcile       # tests whose path contains "reconcile"
cargo test -p delonix-stack -- --exact kinds::tests::nenhum_kind_aparece_duas_vezes
```

Les tests qui ont besoin de privilèges ou d’un vrai hôte se sautent eux-mêmes au lieu d’échouer, de sorte que la suite a
un sens sur un portable comme en CI. Quelques tests réels sont marqués `#[ignore]` et nomment la commande pour
les exécuter dans leur commentaire de documentation (par exemple dans `crates/adapters/delonix-vm/src/lib.rs`) ; n’exécutez ceux-ci
que sur une machine qui vous appartient :

```bash
cargo test -p <crate> -- --ignored <test-name>
```

Un `cargo test` vert prouve la logique pure. Il ne prouve **pas** qu’une modification des namespaces,
des cgroups, du holder réseau ou du démarrage des VM fonctionne — cela exige une exécution réelle (voir
[Batterie de bout en bout](#end-to-end-battery-scriptse2esh) et [Harnais de chaos](#chaos-harness-scriptschaossh)).

## Les gates exécutés par la CI

<!-- dev-docs:begin ci-gates -->
| Job CI | Ce qu'il vérifie |
|---|---|
| `fmt` | rustfmt |
| `lang` | lang ratchet |
| `arch` | arch fitness |
| `contract` | contract gate |
| `version` | version gate |
| `cli-surface` | cli surface |
| `clippy` | clippy -D warnings |
| `test` | test |
| `test-arm64` | test (arm64) |
| `deny` | cargo-deny |
| `fuzz` | fuzz (60s smoke, per target) |
| `script-tests` | script tests (Python gates) |
| `perf-probe` | perf probe (environment and bench) |
| `perf` | perf gate (regression against the baseline) |
| `release-verify` | release verify |
| `docs` | generated docs and valid examples |
<!-- dev-docs:end ci-gates -->

Chaque job de `.github/workflows/ci.yml` peut être reproduit localement. Exécutez ceux qui correspondent à ce que vous
avez touché avant de pousser ; exécutez-les tous avant de demander une revue.

| Job | Commande locale | Échoue lorsque |
|---|---|---|
| `fmt` | `cargo fmt --all --check` | le code n’est pas formaté par rustfmt (configuration par défaut) |
| `lang` | `python3 scripts/lang_ratchet.py` | les identifiants, commentaires ou messages en portugais **augmentent** — ou diminuent sans abaisser `scripts/lang_baseline.json` dans le même commit (`--list` les affiche, `--update` abaisse la ligne de base) |
| `arch` | `python3 scripts/arch_fitness.py` | une dépendance va à l’encontre de la direction des couches, un crate se trouve dans le mauvais répertoire, un crate membre épingle une version de dépendance, un nom de consommateur apparaît dans le code, ou un ratchet (cliquet) de dette bouge (`--list`, `--update`) |
| `arch` | `python3 scripts/dev_docs.py --check` | un fait généré dans `docs/dev/` est obsolète — exécutez `python3 scripts/dev_docs.py` et commitez |
| `contract` | `python3 scripts/contract_gate.py` | le contrat de nœud dans `proto/delonix/node/v1` n’est pas propre au sens de `buf format`, échoue à `buf lint`, rompt la compatibilité avec le dernier tag, n’a pas de mapping HTTP, ou `docs/api/openapi.yaml` n’est pas celui généré (`--update` le réécrit). Nécessite `protoc`, `buf` v1.73.0 et `protoc-gen-openapi` v0.7.1 dans le `PATH`, ainsi que les tags |
| `version` | `python3 scripts/version_gate.py` | la version du workspace n’est pas le tag le plus récent que contient le commit (voir [Flux de contribution](contributing-workflow.md#version-alignment)) ou la branch ne contient pas le tag le plus récent. Nécessite les tags |
| `cli-surface` | `cargo build --release -p delonix-runtime-bin && scripts/cli-tree.sh --gate` | une feuille de la CLI a été ajoutée, supprimée ou reclassée sans mise à jour de `scripts/cli_baseline.tsv` dans le même commit (`scripts/cli-tree.sh --update`) |
| `cli-surface` | `python3 scripts/docs_cli_gate.py` | une commande `delonix …` citée dans la documentation actuelle n’existe pas dans l’arbre du binaire |
| `clippy` | `cargo clippy --workspace --all-targets --locked -- -D warnings` | n’importe quel avertissement |
| `test` | `cargo build --workspace --locked && cargo test --workspace --locked --no-fail-fast` | n’importe quel test échoue |
| `deny` | `cargo deny check advisories licenses sources` | un avis RUSTSEC, une licence ou une source non autorisée (`deny.toml`) |
| `docs` | `cargo build --release -p delonix-runtime-bin && python3 docs/gen.py && git diff --exit-code -- docs/` | le site commité n’est pas ce que le générateur produit à partir de ce binaire |
| `docs` | `./target/release/delonix stack apply -f examples/<file>.yaml --dry-run` et `./target/release/delonix stack validate -f examples/<file>.yaml` | un exemple publié utilise une forme dépréciée ou a des références non résolues |

`cli-tree.sh` et `docs_cli_gate.py` lisent l’arbre depuis le vrai `--help` du binaire ; définissez
`DELONIX_BIN=/path/to/delonix` pour choisir quel binaire. `docs/gen.py` utilise par défaut
`target/release/delonix` et nécessite le module Python `markdown`. Le job `docs` génère aussi les
pages de manuel (`delonix man --dir <dir> --index`) et les vérifie avec `groff -mandoc -ww -z`.

Workflows séparés, non requis à chaque modification : `chaos.yml` exécute le harnais de chaos sur un
runner propre (et signale `skipped` lorsque le runner bloque les user namespaces), `release.yml` publie un
tag, et `vm-image.yml` / `vm-appliances.yml` construisent des images de VM.

## Isoler l’état du moteur

Tout ce qui va au-delà de `--help` touche l’état du moteur. Par défaut, c’est **votre état réel** : vos
containers, réseaux, volumes et le holder réseau. Avant d’exécuter le moteur pour tester —
à la main, via `e2e.sh`, ou via n’importe quel script — faites pointer **les deux** racines d’état vers un
répertoire jetable :

```bash
export DELONIX_ROOT=$HOME/scratch/dlx/root          # containers, images, networks, IPAM, volumes
export DELONIX_NET_RUNTIME_DIR=/tmp/dlx-run          # the holder's control and slirp sockets
mkdir -p "$DELONIX_ROOT" "$DELONIX_NET_RUNTIME_DIR"
```

**Les deux, toujours. Une demi-isolation est pire que pas d’isolation du tout.** Les sockets réseau et les pidfiles sont
résolus séparément : les pidfiles vivent sous la *racine d’état*, tandis que les sockets de contrôle et slirp du holder
vivent dans un *répertoire d’exécution* (par défaut `/tmp/delonix-net-<uid>`). Lorsque deux racines d’état
se sont retrouvées sur le même répertoire d’exécution, chacune a lu son propre pidfile (absent), en a conclu qu’il n’y avait pas
d’infrastructure réseau, et a démarré ou démoli l’infrastructure par-dessus les sockets de l’autre. Sur un hôte de
développement exécutant des workloads actifs, cela s’est terminé avec la racine réelle reconstruisant son infrastructure réseau et
redémarrant de vrais containers.

Le moteur dérive désormais un suffixe à partir d’un `DELONIX_ROOT` non par défaut pour le répertoire d’exécution
(`runtime_dir`/`root_suffix` dans `crates/adapters/delonix-sdn/src/infra.rs`), ce qui ferme cette
collision dans le cas courant. Continuez malgré tout à exporter les deux : cela rend l’isolation explicite, garde
le chemin du socket court et sous votre contrôle, et c’est ce que font `scripts/e2e.sh` et
`scripts/chaos.sh` (e2e renseigne la variable que vous n’avez pas exportée).

Gardez `DELONIX_NET_RUNTIME_DIR` court : un chemin de socket unix plus long qu’environ 108 octets échoue avec
`path must be shorter than SUN_LEN`. `e2e.sh` refuse un répertoire d’exécution de plus de 80 octets.

Lorsque vous avez terminé, démontez l’infrastructure réseau isolée avec les deux mêmes variables exportées :

```bash
./target/debug/delonix net netns down
```

## Recettes d’images de VM (`scripts/verify-images.sh`)

Les recettes de `images/` sont vérifiées de deux façons. Un test unitaire dans le crate de la CLI
(`vmspec::every_shipped_recipe_is_valid_and_complete`) échoue si une recette cesse de se parser ou pointe vers un
fichier ou un builder qui n’existe pas. `scripts/verify-images.sh` va plus loin : il construit hors ligne les quatre
distributions à image cloud dans un `DELONIX_ROOT` isolé et relit le qcow2 obtenu face à ce que la recette déclarait ;
`--self-test` prouve que les vérifications peuvent échouer sur une image que personne n’a construite. Il requiert
`libguestfs-tools` (voir [Construire des microVMs](microvm-setup.md)) et ne fait pas partie des gates de la CI. Les
phases `--packages`, `--profile`, `--boot` et `--appliance` existent mais n’avaient pas été exécutées à la sortie de
la v4.2.0.

## Batterie de bout en bout (`scripts/e2e.sh`)

`e2e.sh` exécute la CLI contre le vrai noyau : le `--help` de chaque feuille, plus des exécutions réelles d’une
grande partie de la surface, et affiche un rapport PASS/FAIL/SKIP/XFAIL (détail JSONL dans
`$OUT/results.jsonl`, par défaut `OUT=/tmp/delonix-e2e`).

```bash
./scripts/e2e.sh                          # uses ./target/debug/delonix
./scripts/e2e.sh ./target/release/delonix
```

- Il **s’isole lui-même par défaut** : il fixe `DELONIX_ROOT` et `DELONIX_NET_RUNTIME_DIR` vers ses propres
  répertoires (sauf si vous exportez d’abord les deux) et démonte l’infrastructure qu’il a démarrée.
  `E2E_SHARED_STATE=1` s’exécute contre l’état réel de la machine — uniquement pour diagnostiquer un hôte.
- Le code de sortie est non nul lorsqu’une vérification échoue, ou lorsqu’une vérification marquée comme défaut connu (`XFAIL`)
  réussit de manière inattendue. Les SKIP ne font pas échouer l’exécution mais sont listés dans leur propre bloc : une vérification
  sautée n’a rien prouvé.
- Il a besoin d’un accès réseau pour récupérer des images ; les sections dont les préconditions manquent sont sautées avec la
  raison.
- Une exécution verte signifie que le `--help` de chaque feuille a été vérifié et que *certaines* feuilles ont été exécutées.
  Lisez l’en-tête du script pour savoir ce qui est exécuté et ce qui ne l’est pas.

## Harnais de chaos (`scripts/chaos.sh`)

Le harnais de chaos casse volontairement un moteur en cours d’exécution — tuer le holder, remplir le disque,
attaches concurrents, applies partiels — et indique s’il s’est dégradé de la manière qu’il promet.

```bash
scripts/chaos.sh                        # every scenario, ./target/debug/delonix
scripts/chaos.sh holder_kill oom        # selected scenarios
scripts/chaos.sh --keep scale           # leave the sandbox up for a post-mortem
scripts/chaos.sh --clean                # tear the kept sandbox down
```

- Il redirige toujours les deux racines vers son sandbox (`DELONIX_CHAOS_DIR`, par défaut `/tmp/dlx-chaos`)
  et ne touche jamais aux containers, réseaux ou enregistrements du moteur réel. Les répertoires d’images
  (`images`, `layers`, `blobs`) sont des **liens symboliques vers votre store réel** pour éviter les téléchargements : en pratique, le harnais
  ne fait que les lire, mais un scénario qui écrirait une image écrirait dans le store réel.
- Il **refuse de s’exécuter sur une machine chargée** (charge au-dessus d’un seuil, partagé avec `scripts/bench.sh`
  via `scripts/bancada.sh`) : sous charge, les scénarios échouent pour des raisons qui relèvent du banc,
  pas du produit. `--max-load N` modifie le seuil ; `--force` s’exécute quand même et marque le
  verdict comme non publiable.
- Le code de sortie n’est 0 que lorsqu’aucun scénario n’échoue. Les SKIP sont listés séparément.
- Certains scénarios ont besoin de ressources externes et sont sautés sans elles (par exemple `truenas_destroy`
  a besoin de `DELONIX_CHAOS_TRUENAS_URL`/`_USER`/`_PASS`).

Les répertoires jetables sous `/tmp` conviennent pour ces sandboxes à usage unique. Vos **worktrees**, non
— voir [Flux de contribution](contributing-workflow.md#one-worktree-per-task).

---

**Suivant :** [Structure du projet](project-structure.md) — la carte du dépôt : ce qu’est chaque répertoire, qui le modifie, et ce qui est généré.
