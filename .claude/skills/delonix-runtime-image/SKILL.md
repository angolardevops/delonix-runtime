---
name: delonix-runtime-image
description: Domínio das imagens do delonix — OCI pull/push, CAS e layers, o registo e a sua autenticação, `build` (Dockerfile/Delonixfile, multi-stage, secrets, cross-arch), scan de CVE/SBOM, e o catálogo de repositórios oficiais. Usa quando mexeres em `crates/delonix-image`, `crates/delonix-scan`, nos grupos `delonix image`/`build` da CLI, ou quando algo falhar num pull/push/build.
---

# Imagens — a cadeia de confiança, e o que já a furou

## Onde as coisas estão

`crates/delonix-image/src/{registry,cas,overlay,save,auth}.rs` (motor),
`crates/delonix-runtime-bin/src/cmd/{image,build,vmimage}.rs` (CLI),
`crates/delonix-scan` (SBOM + CVE).

## A cadeia de confiança — cada elo já foi um achado

**Verificar o BLOB não chega: verifica-se o MANIFESTO.** O digest-pinning
(`pull …@sha256:X`) conferia cada blob contra o que o manifesto declarava e
NUNCA o manifesto contra o digest pedido — um registo comprometido devolvia um
manifesto totalmente diferente, internamente consistente, e instalava o conteúdo
do atacante sem um erro. `verify_manifest_digest` corre nos dois caminhos de
pull e no sub-manifesto multi-arch. Foi ALTO na auditoria #3.

**Um artefacto OCI também se verifica.** O `pull_oci_artifact` não conferia o
digest do blob (CRÍTICO #3 de 2026-07): uma imagem VM dourada adulterada passava
sem detecção. Anotações do manifesto só são lidas DEPOIS da verificação.

**Um download sem checksum não é uma cadeia de confiança**, mesmo por HTTPS. As
cloud images verificam-se contra `SHA256SUMS`/`SHA512SUMS`/`.CHECKSUM` do
próprio publicador — e cada distro publica noutro formato (GNU `<hash>  <f>`,
BSD `SHA256 (<f>) = <hash>`, e o Debian só publica SHA512).

**`Cas::has` antes de cada GET de blob.** Existia e nunca era chamado: cada pull
redescarregava tudo mesmo com o conteúdo exacto em disco, e isso fazia um
`kubeadm init` real estourar o próprio deadline interno.

## O que já enganou

**Um tag nu não se adivinha — procura-se.** `vm pull rocky-9` assumia o
repositório de appliances e dava `no such image …/delonix-vm-appliances:rocky-9`
para uma imagem publicada, pública e noutro repositório. Os espaços de nomes não
estão particionados por regra nenhuma que este código possa derivar, e uma
tabela de prefixos seria o mesmo defeito noutra forma. Três GETs de kilobytes
antes de um download de centenas de MB; zero repositórios dá o comando para ver
o que existe, mais do que um é ambiguidade NOMEADA.

**«Não está no store de containers» não é «não é local».** O `image scan` de uma
imagem VM anunciava «not local» e ia à Docker Hub buscar `library/<nome>`.

**O timeout de um transfer não é o de um pedido.** 600s cortava o push de 1 GiB
a meio; `connect_timeout` curto + `timeout` de horas nas rotas que movem blobs.

**Havia quatro formas de trazer uma imagem VM e nenhuma de a tirar.** O
`image vm rm` recusa enquanto uma VM assenta nela — o overlay tem-na como
backing file, e apagá-la não liberta a VM, torna-a ilegível. **A verificação lê o
DISCO** (`qemu-img`), não o registo: uma VM feita fora deste motor segura a
imagem na mesma. Disco primeiro, contabilidade em ÚLTIMO.

**`qemu-img info` NÃO falha num ficheiro que não é qcow2** — lê-o como `raw` e
diz que não tem base. Falha quando não consegue ABRIR o ficheiro. Um comentário
que afirmasse o contrário foi escrito e corrigido.

## Build

Um container de trabalho por ESTÁGIO, `RUN` via `exec`, `COPY` no rootfs,
`commit_flat_rootfs` (rootless) ou `commit_upper` (root). Cache por instrução em
rootless.

**Secrets de build nunca chegam a uma layer, estruturalmente**: bind-mount ao
vivo só durante a janela do `RUN`, no mnt-ns do container de trabalho, logo
invisível do lado do host que faz o commit. Validado: valor lido durante o `RUN`,
ausente (nem sequer um ficheiro vazio) na imagem final.

**`COPY` é confinado** (`safe_join`/`confine_to`) — um `..` no `src`/`dst` era
path traversal (CRÍTICO #4 de 2026-07). O mesmo para whiteouts na extracção OCI.

**Cross-arch é preflight, não tentativa**: verifica-se
`/proc/sys/fs/binfmt_misc/qemu-<arch>` antes de arrancar, e o binfmt é
pré-requisito do HOST — não gerido por este motor, tal como no buildx real.

## Repositórios oficiais

`OFFICIAL_REPOS` é o catálogo (k8s, base, appliances) e decide o destino de um
`push` sem argumento a partir dos METADADOS da imagem — `official_repo_for` e
`official_tag_for` têm de concordar com o que o `pull` resolve, e há teste a
exigi-lo. Os metadados atravessam o registo em annotations do manifesto (antes
perdiam-se: um appliance publicado voltava a receber cloud-init do outro lado).

## Antes de dar por feito

Um push «bem sucedido» prova-se no REGISTO, não no rc do comando nem num grep à
saída — `image vm ls-remote` sem credenciais é a verificação, e mostra o que um
utilizador vê. Corre `delonix-runtime-sec` para qualquer mudança em pull/push,
extracção ou credenciais: esta superfície já deu um ALTO e dois CRÍTICOS.
