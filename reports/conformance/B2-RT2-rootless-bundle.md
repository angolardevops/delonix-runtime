# GAP-RT-2 — bundle rootless exportável · CORRIGIDO + verificado

**Data:** 2026-10-24 · **Binário:** `delonix 5.1.1` (árvore `ee615f3b` + fixes), rootless.

## O gap (de B1)

`delonix image export` produzia um bundle **sem mapeamentos uid/gid** (root-only); `runc`/`crun`
recusavam-no rootless com `rootless container requires user namespaces`. Portável só como root.

## Correcção

Nova flag **`image export --rootless`**: adiciona um **user namespace** e os mapeamentos
uid/gid **deste utilizador**, espelhando o mapa que o próprio init do motor usa
(`write_userns_maps`): container 0 → o nosso euid (1 id), depois container 1.. → os subuids
delegados (`USERNS_UID_BASE`). Default continua a ser o bundle root (sem mapas). Ficheiros:
`cmd/image.rs` (flag, `build_runtime_spec(rootless)`, aviso ajustado), reusa
`delonix_linux::USERNS_UID_BASE/USERNS_RANGE`.

## Verificado ao vivo (`image export --rootless alpine`)

```
has user ns: True
uidMappings: [{hostID:1000, containerID:0, size:1}, {hostID:100000, containerID:1, size:65535}]
gidMappings: [{hostID:1000, containerID:0, size:1}, {hostID:100000, containerID:1, size:65535}]
oci-runtime-tool validate: Bundle validation succeeded
runc --root <tmp> run  (rootless):  rc=0        ← era "requires user namespaces"
```

Default (sem `--rootless`): sem user ns, sem mapas — inalterado.

## Testes e gates

- Teste `bundle_exportado_e_conformante_oci_runtime` estendido: default sem userns/mapas;
  `--rootless` com userns + mapa container-0→euid-size-1 + container-1→`USERNS_UID_BASE`. **Passa.**
- Gates verdes: `lang_ratchet` (2 strings novas → `pt.po` + baseline 114→116), `docs_cli_gate`,
  `cli_exec_ratchet` (90.7%), `dev_docs --check`. **Sem gate pendente** para a flag nova.

## Limitação documentada (follow-up)

`architectures` do seccomp e o mapa assumem o host de exportação (x86_64, subuid 100000:65536 de
`/etc/subuid`). Um bundle para outra arquitectura/ outro `/etc/subuid` precisa de ler esses valores
em vez da convenção `USERNS_*`. Pequeno; não bloqueia o caso comum.

## Estado do Track B (runtime OCI)

| Gap | Estado |
|---|---|
| RT-1 ociVersion 1.3.0 | ✅ corrigido + validate OK |
| RT-2 bundle rootless | ✅ corrigido + `runc run` rootless OK |
| RT-3 seccomp no bundle | ✅ corrigido (política própria, 263 syscalls) |
| B2 mount propagation | ✅ já existia (verificado) |
| RT-4 CLI estilo runc (create/start/state/kill/delete) | ⬜ aberto (o que falta p/ correr a *validation suite* do runtime-tools contra o Delonix) |

Tudo na branch `fix/campaign-defects-doctor-rm-compose`, **não empurrado**.
