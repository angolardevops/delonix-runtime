# DEF-002 — `stack plan`/`apply` reporta "unchanged" ao alterar `command` de um Pod existente (convergência silenciosamente incompleta)

- **Requisito/teste:** MANIFEST-01 (§9 — "alterar campo mutável", "alteração que exige substituição", "detectar e reconciliar")
- **Severidade:** P2 (correcção / segurança operacional — divergência silenciosa entre desejado e observado)
- **Ambiente:** delonix v5.1.1 binário `350a6320…` (develop `558e5f20`), host rootless uid 1000

## Esperado
O princípio "cloud native — declarativo e convergente" e os ADRs definem que uma mudança que exige recriação ou (a) **recusa** o apply sem `--replace` (ADR-0059:856 "`stack apply` refuses without `--replace`, as every change to this Kind does") ou (b) é **nomeada** numa condition `FieldsNotCompared` (ADR-0055, VM). Em qualquer caso, o `plan` deve **revelar a divergência**.

## Observado
Alterar `spec.containers[0].command` de `["sleep","600"]` para `["sleep","500"]` num Pod **já aplicado**:
- `stack plan -f v2.yaml` → `= Pod/dlxcamp-pod` / **`Summary: 1 unchanged`** (não detecta a mudança)
- `stack apply -f v2.yaml` → `pod/dlxcamp-pod: already exists (1 container(s)), nothing to do` (não converge)
- processo real continua `sleep 600`; `container describe` → `Command: sleep 600`
- Nenhum erro, nenhum pedido de `--replace`, nenhuma condition `FieldsNotCompared`.

`apply --replace Pod/dlxcamp-pod` converge correctamente (`describe` → `Command: sleep 500`) — mas exige o operador saber de antemão o que mudou, precisamente o que o `plan` lhe deveria dizer.

## Passos mínimos de reprodução
```
cat > m.yaml <<'Y'
apiVersion: compute.delonix.io/v1alpha1
kind: Pod
metadata: {name: p, namespace: ns}
spec: {containers: [{name: app, image: busybox:latest, command: ["sleep","600"]}]}
Y
delonix stack apply -f m.yaml
sed 's/"600"/"500"/' m.yaml > m2.yaml
delonix stack plan -f m2.yaml      # => "1 unchanged"  (ESPERADO: mudança sinalizada ou replace exigido)
delonix stack apply -f m2.yaml     # => "nothing to do" (ESPERADO: converge ou recusa)
delonix container describe p-app | grep Command   # => sleep 600 (não convergiu)
```

## Impacto
Um operador que edita o manifesto e corre `apply` recebe confirmação de convergência ("unchanged"/"nothing to do") quando a máquina continua com o spec antigo. Divergência silenciosa entre desejado e observado — o oposto da garantia declarativa. Afecta qualquer campo do Pod que exija recriação.

## Hipótese de causa
O caminho de convergência do Pod compara identidade/existência (ensure-present) e não o conteúdo do spec `command`, sem o declarar como `FieldsNotCompared` (como a VM faz em ADR-0055) nem recusar sem `--replace` (como os Kinds de ADR-0059). Inconsistência entre Kinds no comparador de `stack plan`.

## Critério de encerramento
`stack plan` de um Pod cujo `command` (ou outro campo que exija recriação) mudou reporta a divergência — como `~`/replace-needed ou nomeando o campo em `FieldsNotCompared` — em vez de `unchanged`; e `stack apply` converge ou recusa explicitamente pedindo `--replace`.

## Nota (não-defeitos descartados nesta área, por rigor)
- `stack validate --strict` rejeita campo desconhecido com **exit 1** (verificado isolado; o "exit 0" aparente era artefacto de `| tail`).
- Parar o container NÃO é drift: o motor é daemonless (sem reconciler contínuo, princípio 2); `drift` compara campos declarados, não estado running/stopped — comportamento coerente, não defeito.
