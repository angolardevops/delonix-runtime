# B1 — Baseline OCI Runtime Spec (Track B do plano de conformidade)

**Data:** 2026-10-24 · **Binário:** `delonix 5.1.1 @ ee615f3b` · **Validador:** `oci-runtime-tool`
`v0.9.1-0.20261017-bd3b0bd` (de `@master`; o `@latest`/v0.9.0 não compila sob Go 1.25) ·
**runc:** 1.5.2 · rootless, host kernel 7.0, cgroup v2.

## Método

1. `delonix image export alpine <bundle>` → `config.json` + `rootfs/`.
2. `oci-runtime-tool validate --path <bundle>` (validação estrutural+semântica contra o runtime-spec).
3. `runc --root <tmp> run` do bundle, tal-e-qual, rootless (prova de interop).

Reproduzível; nenhuma suite upstream estava instalada no host (como o `oci-conformance.md` já notava).

## Resultado

| Verificação | Resultado |
|---|---|
| Estrutura do `config.json` vs runtime-spec | **CONFORME** — `Bundle validation succeeded` (com a string de versão em 1.3.0) |
| `runc run` do bundle exportado, rootless, as-is | **FALHA** — `rootless container requires user namespaces` |

`config.json` emitido: `ociVersion 1.0.2`, `process.user {0,0}`, namespaces
`pid/network/ipc/uts/mount/cgroup` (**sem user**), `capabilities.bounding` = 6 (conjunto reduzido),
**sem `uidMappings`/`gidMappings`**, **sem secção `seccomp`**.

## Gaps encontrados (entram no Track B)

| ID | Gap | Impacto | Esforço |
|---|---|---|---|
| **GAP-RT-1** | `ociVersion` emitido é **1.0.2** (spec corrente 1.2/1.3); ferramentas que fixam a versão corrente recusam o bundle | ferramentas OCI modernas não validam o bundle sem bump manual | baixo — carimbar versão corrente + campos novos exigidos |
| **GAP-RT-2** | export **não emite `uidMappings`/user-ns** — bundle é root-only; `runc run` rootless falha | o bundle não é portátil para runtime rootless (runc/crun) sem edição | médio — `image export --rootless` que injecta o mapping subuid |
| **GAP-RT-3** | export **não leva `seccomp`** — o perfil do Delonix não viaja para o bundle; um `runc` do bundle corre sem seccomp | perda de endurecimento ao sair do motor | médio — serializar o perfil para o config |
| **GAP-RT-4** | o Delonix **não expõe um CLI estilo runc** (`create/start/state/kill/delete <bundle>`) — é produtor de bundles + init próprio, não um runtime drop-in | não pode ser medido pela *validation suite* do `runtime-tools` nem substituir runc/crun diretamente | alto — um `delonix oci-runtime` shim sobre o init |

## Conclusão de B1

- **Positivo:** a forma do bundle que o Delonix produz é **estruturalmente conforme** ao runtime-spec
  (o validador só tropeçou na string de versão).
- A **conformidade de comportamento do runtime próprio do Delonix** continua coberta pelo E2E/caos,
  **não** pelo `runtime-tools` — porque este exige um CLI runc-style que o Delonix não tem (GAP-RT-4).
  Fechar GAP-RT-4 (um shim `oci-runtime`) é o que permitiria correr a *validation suite* upstream
  contra o próprio Delonix e dar um número comparável ao do runc/crun.
- Próximo em Track B: **B2 (mount propagation `rshared`/`rslave`)**, que também fecha 3 falhas do CRI.

## Repro

```bash
export PATH=$PATH:/usr/local/go/bin
go install github.com/opencontainers/runtime-tools/cmd/oci-runtime-tool@master
delonix image export alpine /tmp/b && (cd /tmp/b && ~/go/bin/oci-runtime-tool validate --path .)
cd /tmp/b && runc --root /tmp/rr run delonix-oci   # rootless → "requires user namespaces"
```
