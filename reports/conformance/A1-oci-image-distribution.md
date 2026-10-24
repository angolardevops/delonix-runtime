# A1 — OCI Image + Distribution (Track A) · interop entre motores

**Data:** 2026-10-24 · **Binário:** `delonix 5.1.1 @ ee615f3b` (com GAP-RT-1 fix) ·
docker 29.9.2 · podman 4.9.3 · registry `registry:2` local em `127.0.0.1:5000`.

> A ferramenta `oci-image-tool` (`opencontainers/image-tools`) **não compila** (breakage clássico
> `Sirupsen/logrus` vs `sirupsen/logrus`; projeto abandonado). Em vez de um validador morto, o teste
> real de "igual ao Docker/Podman como imagem OCI" é a **interop entre motores**: empurrar com um,
> puxar/correr com os outros. Mais forte do que um schema-check e sem colisão com o CRI.

## Matriz de interop (push → pull/run)

| Origem → destino | Resultado | Evidência |
|---|---|---|
| **delonix → docker** | **PASSOU** | digest idêntico `sha256:3cc8f85a…`; `docker run` → `RELC-OCI-7f3a` |
| **delonix → podman** | **FALHOU** | `unsupported docker v2s2 media type: "application/vnd.oci.image.layer.v1.tar"` |
| **docker → delonix** | **PASSOU** | `delonix pull` + `run` da imagem empurrada pelo docker → `delonix-ran-docker-image-OK` |

## GAP-OCI-IMG-1 — manifesto híbrido não-conforme no push

O manifesto que o `delonix push` produz mistura esquemas:

```
manifest mediaType: application/vnd.docker.distribution.manifest.v2+json   (Docker schema-2)
config  mediaType: application/vnd.docker.container.image.v1+json          (Docker)
layer   mediaType: application/vnd.oci.image.layer.v1.tar                  (OCI, NÃO comprimido)
```

Um manifesto **Docker schema-2** exige **layers Docker** (`application/vnd.docker.image.rootfs.diff.tar.gzip`).
O Delonix põe um **layer OCI dentro de um manifesto Docker** — o `docker` (tolerante) aceita, o
`podman`/`containers-image` (estrito, correcto) recusa. Liga ao gap conhecido "layers de build não
comprimidos" (`.tar`, não `.tar+gzip`).

**Impacto:** imagens empurradas pelo Delonix **não são puxáveis por podman/skopeo/containers-image** —
quebra interop com metade do ecossistema. (O `pull` do Delonix está bem: consome manifestos Docker
standard sem problema — ver linha docker→delonix.)

**Correcção (duas vias limpas):**
- **(a) Emitir um manifesto OCI** `application/vnd.oci.image.manifest.v1+json` + config
  `application/vnd.oci.image.config.v1+json` + layers OCI — coerente com o layer `.tar` já usado; é o
  alvo moderno e correcto. **Recomendada.**
- **(b) Emitir um manifesto Docker v2s2 a sério** com layers `…diff.tar.gzip` (implica comprimir os
  layers — fecha também o gap da compressão).

### Correcção APLICADA (via a), ainda NÃO verificada

`crates/adapters/delonix-oci/src/registry.rs` `push_to_registry` (~2334): o push passou de
`docker_manifest(...)` + `DOCKER_MANIFEST_MEDIA_TYPE` para
`to_oci_manifest(&docker_manifest(...))` + `MediaType::ImageManifest.as_ref()` — reutilizando a
função OCI **que já existia** (usada pelo registry interno e pelo Proxmox). Os blobs de config/layer
são os mesmos (image id inalterado; só muda o digest do manifesto). Mesmo padrão dos outros dois
pushes OCI do ficheiro (linhas 2490, 2578), logo alta confiança de que compila.

**Estado:** aplicado, **por compilar/testar** — adiado porque o disco está a **9.8G (99%)** com a
sessão CRI do dono a consumir; um build a frio (removi `target/debug`) encheria o `/`.

**Verificar quando houver disco:**
```bash
cargo build --release -p delonix-runtime-bin          # incremental
delonix run -d --name r -p 5000:5000 registry:2
delonix build -t x:1 . && delonix tag x:1 127.0.0.1:5000/x:1 && delonix push 127.0.0.1:5000/x:1
podman pull --tls-verify=false 127.0.0.1:5000/x:1     # DEVE passar agora (era o que falhava)
docker pull 127.0.0.1:5000/x:1                        # continua a passar
# + inspeccionar o manifesto: mediaType=application/vnd.oci.image.manifest.v1+json
```

## Distribution-spec (nota de âmbito)

A suite `distribution-spec` conformance testa um **registry** (servidor). O Delonix é **cliente**
(pull/push) — o seu `internal_registry.rs` é scaffolding sem CLI. Logo a conformância server-side é
**N/A** para o Delonix-como-cliente; o que importa (e foi medido) é a interop de cliente acima.

## Veredicto A1

- **Pull do Delonix: conforme** (consome Docker e OCI).
- **Push do Delonix: NÃO conforme** (GAP-OCI-IMG-1) — é o único bloqueio real de interop de imagem
  com o podman, e tem correcção clara (emitir manifesto OCI). Entra como a próxima tarefa do Track A.
- RT-1 (ociVersion 1.3.0) já fecha a validação do bundle de runtime (`oci-runtime-tool` → success).
