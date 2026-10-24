# 04 — Imagens: inspect, build, push/pull por digest, integridade

SUT: `delonix 5.1.1` @ `bca34f6c3`, rootless. Registry local: `registry:2` em `127.0.0.1:5000`.

| # | Caso | Comando | Verificação independente | Observado | Estado |
|---|---|---|---|---|---|
| IMAGE-1 | inspect/digest | `image describe` / `image history` | — | ID `sha256:4ac41adb…`, 1 layer `sha256:bd5db2d0…`, config (Cmd/User/Env) | **PASSOU** |
| IMAGE-2 | build | `build -t relc-img:v1 <dir>` (Dockerfile `FROM ubuntu:22.04` + RUN + CMD) | correr o resultado | build OK (RUN+package), `run` imprime `relc-layer-1792753040` | **PASSOU** |
| IMAGE-3a | push p/ registry local | `image tag` + `push 127.0.0.1:5000/relc-img:v1` | HTTP ao registry | push OK; **digest do delonix == `Docker-Content-Digest` do registry** (`sha256:4056b726…`) | **PASSOU** |
| IMAGE-3b | pull por digest + integridade | `pull …@sha256:4056b726…` após `rmi` local | correr e comparar conteúdo | recupera id `4ac41adbafec`; `run` imprime **`relc-layer-1792753040` idêntico** | **PASSOU** |

## Observações

- **Integridade por digest confirmada ponta-a-ponta:** o digest do manifesto que o
  `push` reportou bate exactamente com o que o registry devolve em
  `Docker-Content-Digest`, e um `pull` por esse digest (depois de apagar a cópia local)
  recupera conteúdo byte-idêntico (a camada `RUN` baked). Isto satisfaz §7
  "push seguido de pull deve recuperar o artefacto esperado por digest".
- Registries `http` em `127.0.0.1` são aceites sem flag extra (localhost tratado como inseguro por default) — razoável.
- **Parity note (não defeito):** `delonix rmi` aceita **um** `<IMAGE>` (docker `rmi` é variádico). Para lote: `image rm`/`image prune`. Diferença de superfície, documentada.
- Superfície de imagens rica e coerente: `describe/history/sbom/scan/verify/sign/export/save/load/tag/prune/apply`. Não testados nesta ronda: `scan`/`sbom`/`sign`/`verify` (cobertos em §16 security — adiados, ver riscos).

## Limpeza

`relc-registry` container removido; imagens `relc-img` (todas as formas, incl. a puxada por digest) removidas; **porta 5000 libertada**; `kaeso-*` preservados; `registry:2` fica em cache (reutilizável, não é órfão).
