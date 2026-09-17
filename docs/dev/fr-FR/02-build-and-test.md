<!-- translated-from: 02-build-and-test.md sha256:f6f565efdea9ee4a51a174b403817cc3a4bf45f688e9a1906b64109a1e8b5797 -->
# 2. Cloner, compiler et tester

Cette page suppose l’hôte décrit dans [01 — Préparer votre environnement](01-environment.md) : la toolchain
Rust épinglée et `protoc`. Tout ce qui suit s’exécute depuis la racine de votre checkout — idéalement un
**git worktree**, voir [10 — Flux de contribution](10-contributing-workflow.md).

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
| `deny` | cargo-deny |
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
| `version` | `python3 scripts/version_gate.py` | la version du workspace n’est pas le tag le plus récent que contient le commit (voir [10](10-contributing-workflow.md#version-alignment)) ou la branch ne contient pas le tag le plus récent. Nécessite les tags |
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
— voir [10](10-contributing-workflow.md#one-worktree-per-task).
