# Evidência reproduzível — #18 / #19

Reprodução ao vivo com `crictl` v1.36.0 contra um `delonix-cri` isolado
(`DELONIX_ROOT` temporário, socket próprio, `CAP_CEILING=all`), rootless.

## Comando

```bash
ROOT=$(mktemp -d); SOCK=/tmp/s.sock; export DELONIX_ROOT=$ROOT
DELONIX_ROOT=$ROOT DELONIX_CRI_ADDR="unix://$SOCK" DELONIX_CRI_CAP_CEILING=all \
  setsid target/debug/delonix-cri >/tmp/srv.log 2>&1 &
C="crictl --runtime-endpoint unix://$SOCK --image-endpoint unix://$SOCK"
$C pull gcr.io/k8s-staging-cri-tools/test-image-digest@sha256:9700f9a2f5bf2c45f2f605a0bd3bce7cf37420ec9d3ed50ac2758413308766bf
$C pull gcr.io/k8s-staging-cri-tools/test-image-user-username-group:latest
$C pull gcr.io/k8s-staging-cri-tools/test-image-user-uid:latest
$C images -o json   # inspeccionar repoTags/repoDigests/uid/username
```

## ANTES (baseline, main @ ab4751ac)

```
test-image-digest@sha256:9700…
  repoTags    = ['gcr.io/k8s-staging-cri-tools/test-image-digest@sha256:9700…']   ← ERRADO (devia ser [])
  repoDigests = []                                                                 ← ERRADO (devia ter o digest)
test-image-user-username-group:latest
  uid = None   username = 'www-data:1004'                                          ← ERRADO (grupo colado)
test-image-user-uid:latest
  uid = {value:'1002'}   username = ''                                             ← já correcto
```

→ `critest`: `public image with digest …` FALHA em `image.go:76`
(`[...digest ref...] to be empty`); `image status … Uid|Username` FALHA em
`image.go:144` (username `www-data:1004` ≠ `www-data`).

## DEPOIS (fix/cri-image-repodigests-and-user)

```
test-image-digest@sha256:9700…
  repoTags    = []                                                                 ✓
  repoDigests = ['gcr.io/k8s-staging-cri-tools/test-image-digest@sha256:9700…']    ✓
test-image-user-username-group:latest
  uid = None   username = 'www-data'                                               ✓
test-image-user-uid:latest
  uid = {value:'1002'}   username = ''                                             ✓
```

→ `critest -focus "Image Manager"`: **10 Passed | 0 Failed | 112 Skipped**.

## Testes de regressão (unitários)

`cargo test -p delonix-cri --lib` → **93 passed**, incl.:
- `tests::image_user_maps_user_to_uid_or_username`
- `tests::image_user_drops_group_from_name`
- `tests::partition_refs_splits_digests_from_tags`
