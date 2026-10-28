<!-- translated-from: plano-conformidade-oci-cri.md sha256:fd53478c1e7c11c6637436232549328ed2872216e71bd1550ffd75b59d30c549 -->
# Plan de conformité OCI + CRI — aligner Delonix avec Docker/Podman

> **Objectif.** Faire passer à Delonix les **mêmes contrats d'interopérabilité** que le
> containerd/CRI-O (CRI) et que n'importe quel runtime OCI (image/distribution/runtime) passent, pour
> être un remplaçant direct (drop-in) du kubelet et des registries — et, dans un track séparé et avec
> réserve, couvrir la Docker Engine API que des outils tiers appellent via `DOCKER_HOST`.
>
> **Principe de mesure.** Chaque phase se clôt contre une **suite upstream avec un chiffre**, pas
> « semble fait ». Jamais de divergence silencieuse (refus explicite + raison). Un test qui échoue
> sans la correction et passe avec elle, comme le reste du projet. Là où un test ne s'applique pas au
> modèle (daemonless, rootless), on le nomme **N/A** avec la raison — on ne gonfle pas le
> pourcentage.
>
> **Portée et la réserve du `paridade-docker-podman.md §5`.** OCI (image/distribution/runtime) et
> CRI sont des contrats d'interop où la conformité totale en vaut la peine — ils font de Delonix un
> véritable substitut. La Docker Engine API (Track D) est une commodité d'écosystème ; le projet
> lui-même déconseille de poursuivre la parité totale avec la Docker CLI, c'est pourquoi le Track D
> ne couvre que ce que des outils automatiques appellent, pas `docker run` interactif.

## Baseline mesurée (binaire `ee615f3b`, 2026-10-24)

| Contrat | État | Source |
|---|---|---|
| CRI — critest v1.36.0 | **79/103 est de v0.63 (2026-08) — À RE-MESURER sur la v5.1.1** (le propriétaire fait tourner critest maintenant sur C2/C3) | `scripts/critest.sh` |
| OCI Image | **interop prouvée sur la v5.1.1** : delonix→docker **et** delonix→podman + docker→delonix, par digest (IMG-1 corrigé 2026-10-24) | `reports/conformance/A1-oci-image-distribution.md` |
| OCI Distribution | push/pull cross-engine OK sur la v5.1.1 ; la suite `distribution-spec` est server-side → **N/A** (Delonix est client) | A1 |
| OCI Runtime — bundle | **v5.1.1** : ociVersion 1.3.0 ✅, seccomp ✅, `runc run` rootless rc=0 ✅ (RT-1/2/3) — `oci-runtime-tool validate` OK | rapports B1/B2 |
| OCI Runtime — CLI runc | **n'existe pas** (le seul vrai gap de runtime) — RT-4 / ADR-0080 | B1 |
| Docker Engine API | **15/26** routes (mesuré en conditions réelles sur la v5.1.1) | `delonix serve docker-api --matrix` |

> **Avertissement de provenance (important).** La première version de ce plan a hérité de la matrice
> de gaps du rapport **v3.1.0** (`fa8c6297`). Nous sommes sur la **v5.1.1** : re-mesurer contre le
> code actuel a montré que plusieurs « gaps » sont **périmés** — la mount-propagation fonctionne déjà
> (B2), le `.tmp` de layer est déjà nettoyé en-processus (G-035), et le `tag@digest` est un format
> valide, pas un bug (G-033). **Ce plan est conduit par des mesures sur la v5.1.1, pas par la matrice
> v3.1.0.** Un « gap » n'entre que s'il SE REPRODUIT sur la v5.1.1.

**Les 24 échecs du CRI, par domaine :** AppArmor 9 · Security Context 4 · Mount Propagation 3 ·
Image Manager 2 · Streaming 2 · Multiple Containers 1 · Networking 1 · RO-mount 1 · OOM 1.

## Décisions du propriétaire (2026-10-24)

1. **Priorité : CRI d'abord** (kubelet drop-in — le différenciateur container+VM+k8s).
2. **`NoNewPrivs=false` :** honorer la demande du CRI (passe la conformité) **tout en gardant NNP
   toujours activé par défaut dans le chemin CLI/normal**. La garantie par défaut ne change pas ;
   seul le CRI peut la relâcher.
3. **OOM post-mortem :** **investir dans le superviseur** — le superviseur par-container (qui existe
   déjà pour `--restart`/`wait`) lit le cgroup AVANT le teardown et enregistre `OOMKilled`/137. Ce
   n'est pas N/A.

---

## Track C — CRI (PRIORITAIRE) · gate : re-mesurer sur la v5.1.1, puis → ~100/103 en rootful

**Étape zéro : re-mesurer le critest sur la v5.1.1** (`scripts/critest.sh`). Le 79/103 est de v0.63
(2026-08) et la répartition des échecs ci-dessous est de cette exécution — elle peut déjà ne plus
correspondre. Le propriétaire fait tourner ceci maintenant (worktrees
`cri-image-repodigests-and-user` = C2, `cri-portforward-conformance` = C3). Les phases ci-dessous
restent comme carte des DOMAINES, mais les chiffres (`+4`, etc.) ne valent qu'après la re-mesure.

Chaque phase = un domaine d'échec du critest, avec la spec qui échoue nommée.

| Phase | Domaine (nb specs) | Quoi faire | Gate |
|---|---|---|---|
| **C1** | Security Context (4) | namespace `PodPID` ; `RunAsUserName` ; `SeccompProfilePath` par chemin (relie au seccomp-custom qui erre déjà au lieu d'ignorer) ; **`NoNewPrivs=false`** (décision 2 — relâchable seulement par le CRI) | +4 |
| **C2** | Image Manager (2) | pull par digest via CRI ; `Uid`/`Username` dans `ImageStatus` | +2 |
| **C3** | Streaming (2) | serveur de streaming CRI : `PortForward` + base de `Exec`/`Attach` avec hijack (**partagé avec D3**) | +2 |
| **C4** | Networking (1) + Mount Propagation (3) | port mapping seulement avec port de container ; `rshared`/`rslave` (**vient du Track B2**) | +4 |
| **C5** | RO-mount (1) + Multiple Containers (1) | mounts read-only non-récursifs ; cas multi-container | +2 |
| **C6** | OOM (1) | le superviseur capte l'OOM avant le teardown du cgroup (décision 3) | +1 |
| **C7** | AppArmor (9) | traduire le profil par-container du CRI ; exécuter la suite **rootful** (charger des profils exige un privilège — N/A en rootless, nommé) ; prouver ceux qui ne sont pas d'environnement | +0 à +9 selon rootful/rootless |

> **N/A honnêtes à nommer dans le rapport final :** les specs AppArmor qui exigent de charger un
> profil (privilège que le rootless n'a pas) restent N/A en rootless et se prouvent en rootful.
> Aucune autre.

**Pré-requis du Track B :** C4 dépend de B2 (mount propagation). Faire B2 au démarrage du Track C.

---

## Track B — OCI Runtime Spec (active le CRI)

| Phase | Quoi faire | Gate |
|---|---|---|
| **B1** | Exécuter la validation `opencontainers/runtime-tools` contre (a) le bundle d'`image export` + runc/crun et (b) l'init propre de Delonix (config.json → namespaces/caps/seccomp/mounts/hooks/rlimits). Script reproductible. | baseline publié |
| **B2** | Mount propagation `rshared`/`rslave` (**ferme 3 du CRI — C4**) | runtime-tools + critest |
| **B3** | Readonly récursif + masked/readonly paths (décision G-009) | runtime-tools |

## Track A — OCI Image + Distribution (fondation d'interop d'images)

**Re-baseline sur la v5.1.1 (2026-10-24) : ce track est en grande partie FAIT — le reste est
optionnel.** L'interop OCI d'image a été **prouvée en conditions réelles** sur les trois moteurs
(IMG-1 corrigé : `push` publie un manifeste OCI, tirable par docker ET podman). Les « gaps »
G-033/035/039 du rapport v3.1.0 **sont sortis** après re-mesure :

- **G-033** (tag+digest) — **N'est PAS un bug** : `repo:tag@digest` est un format OCI/Docker valide
  et courant (kind l'utilise) ; le refuser casserait kind. Si un jour on veut détecter un *conflit*
  réel (la tag résout vers un autre digest), c'est une décision de représentation locale — hors
  portée jusqu'à ce que ce soit demandé.
- **G-035** (`.tmp` de layer) — **déjà nettoyé en-processus** (`overlay.rs:492-508`). Résidu
  minimal : seulement un `.tmp` orphelin d'un processus MORT à mi-chemin, que le `prune` ne balaie
  pas — petite amélioration, regroupable dans une future passe du `prune`, pas un gap de conformité.
- **G-039** (archive docker-save LEGACY `<hash>/layer.tar`) — réel mais **n'affecte que les archives
  pré-OCI** ; le `docker save` moderne produit le layout `blobs/sha256/` que le `load` accepte déjà.
  Basse priorité ; la correction (carte path→digest) a une subtilité dans les layers.

| Optionnel | Ce qui reste | Gate |
|---|---|---|
| A-sbom/scan/sign | `image sbom`/`scan`/`verify`/`sign` — mesurer la couverture réelle sur la v5.1.1 (non mesuré dans cette campagne) | baseline publié |
| A-manifest-list | multi-arch dans le `push` (aujourd'hui 1 arch par invocation) | round-trip multi-arch |

> `oci-image-tool` (image-spec) **ne compile pas** (breakage `Sirupsen/logrus`) et la
> `distribution-spec` est server-side — donc la preuve d'image sur la v5.1.1 est l'**interop entre
> moteurs**, pas une suite.

## Track D — Docker Engine API (écosystème ; réserve §5)

| Phase | Route(s) | Pourquoi |
|---|---|---|
| **D1** | `GET /containers/{id}/logs` (framing stdout/stderr) | débloque `wait_for_logs` de testcontainers — la plus grande valeur isolée |
| **D2** | `/events`, `/build`, `/networks`, `/volumes`, `GET …/stats` | surface que compose/kind/outils touchent |
| **D3** | `POST …/exec` + `…/attach` (hijack HTTP) | **partage le serveur de streaming du C3** |

> Hors portée délibérément : `docker run` interactif contre le serveur (comportement interne du CLI
> Go), et la parité de `--format`/CLI que le `§5` déconseille.

---

## Séquence et dépendances

**État sur la v5.1.1 :** Track A (image) **prouvé** (IMG-1) ; Track B RT-1/2/3 + B2 **faits** — il ne
reste que **RT-4** (CLI runc, ADR-0080, GO-gated). Le vrai travail restant est RT-4, le Track C (à
re-mesurer) et le Track D.

```
[v5.1.1 feito]  A (interop) ✅   B2 mount-prop ✅   RT-1/2/3 bundle ✅
[real restante]
   RT-4 (ADR-0080, GO) ─► runtime-tools validation suite contra o Delonix
   C0 re-medir critest ─► C1 ─► C2 ─► C3 ─┬─► C4 ─► C5 ─► C6 ─► C7
                                          └─(C3 streaming)─► D3 ─► D1 ─► D2
```

1. **Track C** : re-mesurer le critest sur la v5.1.1 (C0), puis fermer par domaine — le retour
   stratégique (en cours chez le propriétaire : C2/C3).
2. **RT-4** (quand le propriétaire donne le GO) : débloque la mesure du runtime par la suite upstream.
3. **Track D** en dernier, avec la réserve ; D3 tombe gratuitement avec le C3 (même serveur de
   streaming).
4. **Track A** : seulement l'optionnel (sbom/scan/sign, manifest-list) — l'interop est déjà prouvée.

## Comment mesurer (reproductible, par phase)

- CRI : `scripts/critest.sh` → `Ran N of 122 ... X Passed | Y Failed`. Publier le diff de X à chaque phase.
- OCI runtime : validation `runtime-tools` (installer en A1/B1) ; publier pass/fail par section du spec.
- OCI image/distribution : suites upstream `image-spec`/`distribution-spec` ; publier baseline et deltas.
- Docker API : `delonix serve docker-api --matrix` (routes servies/refusées) + `tests/compat/docker_api_smoke.py` et un run de testcontainers.
- Chaque phase apporte un test qui échoue sans la correction (la méthode du projet).

## Risques / tensions connues

- **NNP vs CRI** (décision 2) : le chemin qui relâche NNP ne peut être atteignable que par le CRI, jamais par la CLI.
- **AppArmor rootless** (C7) : une partie des 9 specs est une limite de privilège, pas une lacune — mesurer en rootful pour séparer.
- **OOM daemonless** (C6) : le superviseur doit lire le cgroup dans la fenêtre entre la mort et le teardown ; vraie course.
- **Disque de l'hôte à 96 % :** les suites OCI/CRI tirent des images et écrivent des artefacts — libérer de l'espace avant A1/B1.
