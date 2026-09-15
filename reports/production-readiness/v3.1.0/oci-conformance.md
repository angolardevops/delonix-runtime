# Conformidade OCI — medido sobre `fa8c6297`

Estado isolado (`DELONIX_ROOT=/tmp/dxo`), registo real `registry-1.docker.io`, 2026-09-15.
**Nenhuma suite upstream foi corrida** (as suites OCI de runtime/image não estão instaladas no
host) — os resultados abaixo são testes dirigidos, não conformidade certificada.

## OCI Image / distribuição

| Requisito | Teste | Resultado | Estado |
|---|---|---|---|
| Pull por tag | `image pull alpine:3.20` | rc=0, `bf8527eb54c3` | PASS |
| Pull por digest correcto | `alpine@sha256:d9e853e8…` (digest do index lido ao registo) | rc=0, mesmo conteúdo | PASS |
| Digest inexistente | `alpine@sha256:000…0` | rc=4, nada instalado | PASS |
| Tag + digest em conflito | `alpine:3.19@<digest de 3.20>` | rc=0, **grava `alpine:3.19` com conteúdo 3.20, sem aviso** | FAIL (G-033) |
| Selecção de plataforma | `platform selected: linux/amd64` em todos os pulls | multi-arch index resolvido | PASS |
| Repo inexistente | `library/does-not-exist-audit-xyz:1` | rc=1 (401 do registo) | PARTIAL — classe 4 esperada |
| Referência malformada | `alpine::bad`, `Alpine/UPPER:1`, `alpine@sha256:abc` | enviados à rede; rc=4/4/1 | PARTIAL (G-034) |
| Verificação manifesto vs digest pinado | leitura de código `verify_manifest_digest` (auditoria #3) | presente | NOT RE-TESTED (sem registo adulterado) |
| `image load` formato docker actual | archive `blobs/sha256/*` + `manifest.json` | carregado | PASS |
| `image load` formato antigo | `layer.tar` + `manifest.json` | rc=74 `I/O error: No such file or directory` | PARTIAL (G-039) |
| Path traversal na layer (`../`, absoluto, symlink→/tmp) | `container run` da imagem maliciosa | **recusado** «trying to unpack outside of destination path»; nada escrito fora do root | PASS |
| Limpeza após extracção recusada | 4 tentativas | 4 `layers/.*.tmp` órfãos; `image prune` não os remove | PARTIAL (G-035) |
| Imagens assinadas (`image verify`/`--verify`) | — | — | NOT TESTED |
| Import/export OCI bundle | — | — | NOT TESTED |
| Archive/decompression bomb | — | — | NOT TESTED |

## OCI Runtime (leitura de código, ver `architecture-inventory.md §4`)

| Requisito | Estado |
|---|---|
| lifecycle create/start/kill/delete | PASS via E2E (729 checks) |
| namespaces / capabilities / seccomp / no_new_privs | PASS via E2E + caos |
| masked/readonly paths | PARTIAL — falha de mount só avisa (G-009) |
| mount propagation | PARTIAL — critest falha por ambiente, não exercitado no motor |
| rro | UNSUPPORTED (G-010) |
| hooks | UNSUPPORTED (G-019) |
| rlimits | PARTIAL — nome desconhecido/erro ignorados (G-009) |
| exit status | PASS (E2E: wait, 137, códigos reais) |
