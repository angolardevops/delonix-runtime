# Exemplos / Examples

**PT:** Os manifestos estão agrupados por **grupo de API** (o `apiVersion` de cada
kind) — um subdirectório por grupo, e **um ficheiro por kind**. Cada ficheiro reúne
os cenários desse kind em documentos YAML separados por `---`, com um cabeçalho
numerado bilingue (`── N. … ──`). Aplica-se um ficheiro com
`delonix stack apply -f examples/<grupo>/<kind>.yaml`.

**EN:** Manifests are grouped by **API group** (each kind's `apiVersion`) — one
sub-directory per group, and **one file per kind**. Each file gathers that kind's
scenarios as `---`-separated YAML documents under a numbered bilingual header
(`── N. … ──`). Apply a file with `delonix stack apply -f examples/<group>/<kind>.yaml`.

## Convenção / Convention

- **PT:** Comentários bilingues: o bloco-cabeçalho usa linhas `PT:`/`EN:`; os
  comentários de campo usam `# PT: … | EN: …` na mesma linha. Cada documento começa
  por um separador `── N. título PT ──` / `título EN`.
  **EN:** Bilingual comments: the header block uses `PT:`/`EN:` lines; field comments
  use `# PT: … | EN: …` inline. Each document opens with a `── N. PT title ──` /
  `EN title` separator.
- **PT:** Cada cenário é COMPLETO e VÁLIDO (passa `delonix stack validate`), sem `...`,
  sem campos inventados, sem segredos reais. Mostra defaults importantes de forma explícita.
  **EN:** Each scenario is COMPLETE and VALID (passes `delonix stack validate`), with no
  `...`, no invented fields, no real secrets. Important defaults are made explicit.
- **PT:** O exemplar de referência do estilo é [`storage/objectstorage.yaml`](storage/objectstorage.yaml).
  **EN:** The reference exemplar for the style is [`storage/objectstorage.yaml`](storage/objectstorage.yaml).

## Grupos / Groups

| Grupo / Group | `apiVersion` | Kinds (ficheiros / files) |
|---|---|---|
| `compute/` | `compute.delonix.io/v1alpha1` | container, pod, virtualmachine, systemcontainer, workload |
| `networking/` | `networking.delonix.io/v1alpha1` | network (inclui provider Proxmox/SDN), service, networkpolicy, networkroute, networkaccessrule, networkgateway, ippool, dependency |
| `gateway/` | `gateway.delonix.io/v1alpha1` | httproute, ingress, gateway |
| `storage/` | `storage.delonix.io/v1alpha1` | volume (local/raw/nfs/cifs/webdav/share/provision/pool), storagepool, objectstorage |
| `artifact/` | `artifact.delonix.io/v1alpha1` | image, app |
| `core/` | `core.delonix.io/v1alpha1` | secret, stack |
| `security/` | `security.delonix.io/v1alpha1` | runtimepolicy |
| `gitops/` | `gitops.delonix.io/v1alpha1` | gitopssource |
| `cluster/` | `infrastructure.delonix.io/v1alpha1` | kubernetescluster (modos kind/ssh/vm + perfil CNI + etcd externo) |
| `_labs/` | — | laboratórios e apps de apoio (lab-rede, compose-webapp, delonix-temp) — não são manifestos deste motor / lab & helper apps, not manifests of this engine |
