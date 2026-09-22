<!-- translated-from: delonixfile-and-vmfile.md sha256:40e8ce16ae06683b26d6f4cd83f6266c6b58c55570381c2f91aee3bb1d09acc5 -->
# Delonixfile e VMfile

**Antes de leres:** [Clonar, construir e testar](build-and-test.md) (um binário e um state root
isolado) e [Imagens OCI, armazenamento endereçado por conteúdo e overlayfs](cloud-native-primer.md#44-oci-images-content-addressed-storage-and-overlayfs)
no Manual de cloud native.

O Delonix tem dois ficheiros de build, e são parecidos de propósito: quem já escreveu um Dockerfile
consegue ler os dois. O que constroem é diferente.

- Um **Delonixfile** constrói uma **imagem de container OCI** (camadas de um sistema de ficheiros).
  É uma gramática de Dockerfile com algumas instruções Delonix por cima, construída por
  `delonix build`.
- Um **VMfile** constrói um **disco qcow2 arrancável** para uma VM. Pede emprestada a *forma* do
  Dockerfile, mas o mecanismo é `qemu-img` + `virt-customize` sobre um disco inteiro, construído por
  `delonix image vm build`.

Esta página descreve o que os parsers deste repositório aceitam de facto — não o que o Docker
aceita. Cada regra abaixo aponta para o código que a impõe. Depois dela consegues escrever os dois
ficheiros, prever o que cada parser aceita ou recusa, e encontrar onde mudar uma gramática.

> Os exemplos marcados como *verificados pelo parser* foram corridos contra um binário construído a
> partir desta árvore, com `DELONIX_ROOT`, `DELONIX_NET_RUNTIME_DIR` e `TMPDIR` apontados para um
> directório de rascunho, até ao ponto em que o build começaria a fazer pull de imagens ou a
> descarregar discos base. Builds completos **não foram executados nesta revisão** (precisam de
> acesso ao registo/à rede e, para VMs, de libguestfs e de vários GB de RAM e de disco).

---

## Parte 1 — Delonixfile

### Onde vive o código

| Assunto | Ficheiro | Símbolo |
|---|---|---|
| Gramática (parser) | `crates/adapters/delonix-oci/src/build.rs` | `parse_dockerfile_with_args`, `parse_run_flags`, `parse_secret_mount`, `resolve_target_stage` |
| Orquestração do build | `bins/delonix-runtime-bin/src/cmd/build.rs` | `run`, `build_from_spec`, `build_one_stage`, `default_build_file` |
| Commit da imagem | `crates/adapters/delonix-oci/src/build.rs` | `ImageStore::commit_flat_rootfs` (rootless), `commit_upper` + `build_image` (root) |
| Templates de projecto | `bins/delonix-runtime-bin/templates/<name>/Delonixfile` | renderizados por `delonix init` / `stack init` |

### Procura do ficheiro

`delonix build [CONTEXT]` sem `-f` chama `default_build_file` (`cmd/build.rs`): usa
`<context>/Delonixfile` se existir, caso contrário `<context>/Dockerfile`. **A gramática é a mesma
para os dois nomes** — as instruções Delonix abaixo também são aceites num ficheiro chamado
`Dockerfile`. `Delonixfile` é só o nome que é descoberto primeiro. `-t/--tag` é obrigatório.

```bash
delonix build -t myapp:dev .                 # Delonixfile, else Dockerfile
delonix build -t myapp:dev -f build/Other .  # explicit file
```

### Instruções que o parser aceita

O `parse_dockerfile_with_args` **falha fechado**: uma instrução que não conhece é um erro com o
número da linha, e qualquer instrução excepto `ARG` antes do primeiro `FROM` é um erro. As linhas
que terminam em `\` são juntadas; as linhas que começam por `#` são comentários. Os nomes das
instruções não distinguem maiúsculas de minúsculas.

| Instrução | O que acontece | Notas |
|---|---|---|
| `ARG NAME[=default]` | Declara uma variável de build; `${NAME}`/`$NAME` é substituído em todas as linhas seguintes | Permitido antes de `FROM` (para o parametrizar). `--build-arg NAME=VALUE` só sobrepõe um `ARG` declarado. Simplificação: os args vivem num **único** âmbito para o ficheiro inteiro, não por estágio. Sem formas `${NAME:-default}` (`substitute_vars`). |
| `FROM <image> [AS <name>]` | Abre um estágio | Um estágio posterior também pode dizer `FROM <earlier-stage>` (ver multi-stage). |
| `RUN <shell>` | Corre num container de trabalho via `exec` | Só `--mount=type=secret,...`/`--mount=type=cache,...` são aceites como flag (abaixo). |
| `COPY [--from=<stage>] <src> <dst>` | Escreve no rootfs do estágio em disco | Confinado ao contexto/rootfs (`safe_join`, `confine_to`): `..`, fugas absolutas e symlinks que saem da base são recusados. |
| `ADD` | **Igual a `COPY`** | Sem download de URL, sem extracção automática de arquivos. |
| `ENV K=V [K2="v 2" …]` ou `ENV K V` | Afecta os `RUN`s seguintes; o `ENV` do estágio final vai para a config da imagem | Os valores são expandidos contra `ENV`s anteriores (`expand_env_value`). |
| `WORKDIR <dir>` | Directório de trabalho dos `RUN`s seguintes; o valor final vai para a config da imagem | |
| `USER <name\|uid[:gid]>` | Registado na config da imagem | Vazio → herda o da imagem base. |
| `CMD`, `ENTRYPOINT` | Registados na config da imagem | Formas exec (JSON) e shell. |
| `HEALTHCHECK [opts] CMD <cmd>` / `HEALTHCHECK NONE` | A parte depois de `CMD` é guardada como o comando de saúde da imagem | As opções antes de `CMD` são ignoradas. Usado por `delonix container healthcheck <id>` e pelo `depends_on: condition: service_healthy` do compose. |
| `LABEL`, `EXPOSE`, `MAINTAINER`, `VOLUME`, `STOPSIGNAL`, `SHELL`, `ONBUILD` | **Aceites e ignoradas** | Só metadados; nenhum efeito no build. |

**Extensões Delonix** (analisadas para campos de `Dockerfile` no mesmo ficheiro):

| Instrução | Analisada para | Efeito nesta árvore |
|---|---|---|
| `SCAN fail-on=<sev>` | `scan_fail_on` (por omissão `high` se não houver `fail-on=`) | **Analisada mas não imposta pelo `delonix build`** — nenhum chamador lê o campo. Faz de gate às imagens explicitamente com `delonix image scan --fail-on <sev> <image>`. |
| `CPUS <n>` | `cpus` | Escrita na config da imagem (chave não normalizada `Cpus`); herdada da base se ausente. Nenhum consumidor em runtime foi encontrado nesta revisão. |
| `MEMORY <n>` | `memory` | Igual a `CPUS` (chave `Memory`). |
| `SECURITY <opt>...` | `security` | Igual a `CPUS` (chave `Security`). |

Trata `CPUS`/`MEMORY`/`SECURITY` como *intenção registada*; define os limites reais em runtime
(`container run -m/--cpus`, ou `resources` num manifesto). Se os ligares, actualiza esta tabela.

Comportamento do parser que convém conhecer (lido no código, não uma promessa de desenho):

- `COPY a b c /dst` mantém só a **primeira** origem e o **último** argumento; as origens do meio são
  descartadas em silêncio.
- `COPY --chown=…`/`--chmod=…` não são reconhecidas; o token da flag seria tomado como caminho de
  origem.
- `RUN --network=…`, `RUN --security=…` e qualquer outro `RUN --<flag>` são recusados.

### Multi-stage, `COPY --from` e `--target`

Cada estágio (o final incluído) tem o seu **próprio container de trabalho e rootfs**
(`build_one_stage`). O container de trabalho (`sleep infinity` sobre o rootfs do estágio) é criado
por `ensure_container` através do mesmo `WorkloadRuntime` que o `container run` e o `start` usam
(`container::with_host_workload`), por isso um build não pode divergir da especificação de arranque
de um container. Os estágios intermédios ficam em disco até o build inteiro terminar, por isso:

- `COPY --from=<name-or-index> <src> <dst>` lê directamente do rootfs desse estágio
  (`resolve_copy_source`, `copy_into_rootfs`).
- `FROM <earlier-stage>` clona o rootfs desse estágio com `cp -a --reflink=auto`
  (`clone_rootfs`) — `cp -a` para que symlinks como `/bin -> usr/bin` continuem a ser symlinks.
- `--target <name-or-index>` (`resolve_target_stage`) pára nesse estágio e empacota-o; os estágios
  seguintes não são construídos. Um nome desconhecido é recusado e o erro lista os estágios que
  existem. Com `--target` num estágio intermédio, `CMD`/`ENTRYPOINT`/`USER`/`ENV`/`WORKDIR`/`HEALTHCHECK`
  vêm **desse** estágio, não do estágio final do ficheiro.

Duas restrições do modo root (overlay), ambas recusadas logo à partida com uma mensagem clara: o
estágio final não pode ser `FROM <earlier-stage>` (o commit OCI precisa de uma imagem base real para
a linhagem), e `--target` num estágio intermédio é recusado. O rootless não tem nenhuma das duas
restrições.

### Segredos de build

```bash
delonix build -t app:dev --secret id=npmrc,src=$HOME/.npmrc .
```

```dockerfile
RUN --mount=type=secret,id=npmrc,target=/root/.npmrc npm ci
```

- `--secret id=<name>,src=<path>` é repetível; uma entrada malformada ou um ficheiro `src` em falta é
  um erro duro (`parse_build_secrets`).
- No ficheiro: `--mount=type=secret,id=<name>[,target=<path>][,required=true|false]`. `target` é por
  omissão `/run/secrets/<id>`; `required` é por omissão `false` (um segredo opcional em falta é
  saltado, como no Docker).
- O segredo é montado ao vivo por bind (`mount_run_secrets`) dentro do mount namespace do container
  de trabalho só durante esse único `RUN`, e depois desmontado — a vista do rootfs do lado do host
  que o commit e a cache de camadas lêem nunca o contém. O material do segredo também não entra no
  hash da chave da cache.

### Cache mounts (M03 do programa das 13 melhorias)

```dockerfile
RUN --mount=type=cache,target=/root/.cache/pip pip install -r requirements.txt
```

- `--mount=type=cache,target=<path>[,id=<name>]`. `target` é obrigatório; `id` é por omissão o
  próprio `target` (a regra do próprio Docker), por isso dois `RUN`s que montam o mesmo `target`
  sem nomear um `id` partilham o mesmo directório persistente.
- Ao contrário de um segredo, este directório é **de leitura-escrita** e **sobrevive entre builds**
  (e entre Dockerfiles diferentes): vive em
  `<DELONIX_ROOT>/build-cache/mounts/<sha256(id)>` — em hash, nunca a string crua de `id`/`target`,
  já que um `id` omitido é por omissão um caminho arbitrário controlado pelo Dockerfile
  (`cache_mount_dir`). Ainda sem GC/TTL, a mesma lacuna já assumida abertamente para a cache de
  camadas abaixo.
- Um **hit** da cache de camadas nessa instrução salta o `RUN` por inteiro, logo nunca toca no
  directório do cache mount também — só um *miss* toca (`mount_run_caches`).
- `sharing=`/`ro` (campos extra do próprio Docker) são **recusados**, nunca aceites e ignorados em
  silêncio: ainda não há bloqueio entre processos para dois `delonix build` concorrentes a
  partilharem o mesmo `id`, e aqui todo o cache mount é de leitura-escrita.
- `type=ssh` e `type=bind` continuam **recusados** (`parse_mount_flag`) — ainda por implementar.

### `--platform` e binfmt

`--platform linux/<arch>` (`parse_platform`: só `linux/` é aceite) resolve a imagem base para essa
arquitectura e carimba a arch no resultado. **Correr** um `RUN` de arquitectura estrangeira precisa
do registo `binfmt_misc` + `qemu-user-static` do próprio host, que o Delonix não gere. O
`build_from_spec` verifica `/proc/sys/fs/binfmt_misc/qemu-<arch>` antes de construir e recusa com o
nome do interpretador se estiver em falta ou desactivado.

### Rootless vs root, e a cache de camadas

| | Rootless (caminho normal) | Root |
|---|---|---|
| Rootfs de um estágio | directório flat (`prepare_rootfs_flat`) | overlay |
| Commit | `commit_flat_rootfs` — uma camada achatada por cima das camadas da base | `commit_upper` (tar do upperdir) + `build_image` |
| Cache de camadas | **sim** | **nunca** |

A cache (`<DELONIX_ROOT>/build-cache/<hash>/rootfs`, `--no-cache` para a contornar) é uma cadeia de
hashes encadeados, um elo por instrução. `RUN`/`COPY` tiram um snapshot do rootfs completo depois de
correrem; `ENV`/`WORKDIR` entram na cadeia sem snapshot. Um elo de `COPY` faz hash dos **bytes** que
estão a ser copiados, por isso um ficheiro alterado invalida tudo o que vem depois dele. Num acerto,
o snapshot em cache é clonado para o rootfs de um container novo — nunca sincronizado para um que
esteja vivo (isso corrompia os mounts de `/proc`/`/sys`/`/dev` e foi abandonado). Compromissos
declarados no doc do módulo: snapshots do rootfs completo, não diffs por camada (mitigado por
`--reflink=auto`), e **sem GC da cache** — `build-cache/` só cresce.

Como cada `RUN` executa num container real, os pré-requisitos do host de
[Preparar o ambiente](environment.md) (user namespaces, subuid/subgid, AppArmor no Ubuntu)
aplicam-se também aos builds.

### Exemplo trabalhado: um template

`delonix init -t <template> [DIR]` (ou `delonix stack init --template <template>`) renderiza
`bins/delonix-runtime-bin/templates/<template>/`, substituindo `__NAME__`, `__PORT__` e
`__TEMPLATE_VERSION__` (o default `version=` em `template.meta`, ou `-v/--template-version`). Num
directório vazio escreve o projecto completo; num que não esteja vazio só acrescenta a cola Delonix
(Delonixfile, manifesto, ficheiros de CI). Gerado num directório de rascunho a partir de `httpd`
(*corrido*):

```bash
$ delonix init -t httpd web
detected an empty directory → stack init --template httpd
  created: web/.dockerignore
  created: web/Delonixfile
  ...
```

```dockerfile
# Delonixfile — production-ready Apache httpd.
# Build with:  delonix build -t web:dev .
FROM httpd:2.4-alpine
# Listen on 8080 and harden a little (no version banner).
RUN sed -i 's/^Listen 80$/Listen 8080/' /usr/local/apache2/conf/httpd.conf && \
    printf '\nServerTokens Prod\nServerSignature Off\nTraceEnable Off\n' >> /usr/local/apache2/conf/httpd.conf
COPY public /usr/local/apache2/htdocs
EXPOSE 8080
HEALTHCHECK CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
CMD ["httpd-foreground"]
```

O `delonix-manifest.yaml` gerado passa o `delonix manifest validate` (*corrido*). O `--up`
construiria, faria `stack apply` e esperaria pela saúde — não executado aqui.

Um ficheiro multi-stage que usa a maior parte da gramática, *verificado pelo parser* (o erro de
`--target nosuch` prova que o ficheiro foi analisado e lista os seus estágios, antes de qualquer
pull):

```dockerfile
ARG GO_VERSION=1.23
FROM golang:${GO_VERSION}-alpine AS builder
WORKDIR /src
COPY go.mod ./
RUN --mount=type=secret,id=netrc,target=/root/.netrc go mod download
COPY . .
RUN CGO_ENABLED=0 go build -o /out/app ./cmd/app

FROM alpine:3.20
COPY --from=builder /out/app /usr/local/bin/app
USER 65534
HEALTHCHECK CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
CMD ["/usr/local/bin/app"]
```

```text
$ delonix build -t demo:dev --target nosuch .
error invalid argument: no stage named 'nosuch' in this Dockerfile — known stages: builder
$ delonix build -t d -f Dockerfile.ssh .          # RUN --mount=type=ssh,...
error invalid argument: RUN --mount=type=ssh: only type=secret and type=cache are supported (ssh/bind not yet)
$ delonix build -t d --platform windows/amd64 .
error invalid argument: --platform 'windows/amd64': only 'linux/<arch>' is supported (this engine does not run another OS)
```

### O próprio `Delonixfile` do repositório não é construído pelo `delonix`

O `Delonixfile` na raiz do repositório empacota a CLI `delonix` numa imagem de container. É
construído com **Docker ou Podman** (`docker build -f Delonixfile …`, ou `make image`), não com
`delonix build`. Usa só cache mounts `type=cache` (agora aceites) e a directiva `# syntax=` (um
comentário para este parser, já que qualquer linha começada por `#` o é) — mas construí-lo com
`delonix build` nunca foi tentado: compila o workspace Rust inteiro dentro do container de
trabalho, uma ordem de grandeza diferente dos templates abaixo e fora do âmbito com que esta
funcionalidade foi validada. Não o uses como exemplo da gramática Delonix; usa os templates.

---

## Parte 2 — VMfile

### Onde vive o código

| Assunto | Ficheiro | Símbolo |
|---|---|---|
| Gramática, scaffold, builder | `bins/delonix-runtime-bin/src/cmd/vmfile.rs` | `parse`, `classify_base`, `resolve_base`, `stage_ops`, `build`, `finalize`, `scaffold` |
| Entrada da CLI, receita dourada | `bins/delonix-runtime-bin/src/cmd/vmimage.rs` | `VmImageCmd::Build`, `VmImageStore`, `VmImage`, `customize_args`, `tool_failure_hint` |
| Build declarativo | `bins/delonix-runtime-bin/src/cmd/vm.rs` | `VmBuildSpec` (`spec.build` de `kind: VirtualMachine`) |

### Scaffold

```bash
delonix vm init --vmfile [DIR] [--name <n>]   # or: delonix image vm init <name> [-d DIR]
```

Os dois escrevem `VMfile` e `cloud-init/user-data.yaml` (*corrido* num directório de rascunho). O
scaffold pretende ser uma receita que funciona, e o teste `parseia_o_scaffold_que_escrevemos`
mantém-no analisável. Duas coisas a corrigir à mão na dica «Next:» que ele imprime: a flag é
`vm create --disk`, não `--disk-image`; e vê a nota sobre `CLOUDINIT` abaixo quanto ao nome do
ficheiro.

### Construir

```bash
delonix vm build [-f vm.yaml|VMfile] [-t <tag>] [--target <image>] [--network] [--no-compress] [CONTEXT]
```

O `delonix image vm build` é o mesmo comando (os dois partilham um único `BuildArgs`). Qual receita
corre decide-se como o `docker build` decide entre ficheiros: um `-f` explícito ganha (um
`.yaml`/`.yml` é lido como `vm.yaml`, qualquer outra coisa como `VMfile`); sem `-f`, um `vm.yaml` no
contexto ganha a um `VMfile`, que ganha à receita dourada embutida (ver [Construir microVMs](microvm-setup.md)). As
flags da receita dourada (`--k8s-version`, `--extra-package`, `--extra-run`, `--offline`, `--no-k8s`,
`--cri-bin`, `--delonix-bin`) são **recusadas** com um `vm.yaml` ou um `VMfile`, e `--network` é
recusada sem um deles.

### `vm.yaml`: o front end ao estilo compose

Um `VMfile` está para uma imagem de VM como um `Dockerfile` está para uma imagem de container. Um
`vm.yaml` é o que um `compose.yaml` é para ela: nomeia as imagens de uma pasta e leva os parâmetros
que tornam o qcow2 completo — `size`, `packages.install`, `users`, `services`, `files`, `env`,
`cloud_init`, `run`, o que **remover** (`remove.packages/paths/users/services`, aplicado depois de
tudo o resto para poder podar o que um pacote ou um `run:` trouxe) e `cleanup` (cache de pacotes,
logs, histórico, `/tmp`, `machine-id`).

É só um front end (`cmd/vmspec.rs`). Cada imagem compila para um builder que já existe — um `VMfile`
sintetizado (`profile: custom`, o valor por omissão), a receita dourada (`profile: rootless|k8s`) ou um
ficheiro existente (`build.file`) — por isso não há um segundo motor de build. Regras que vale a
pena conhecer:

- **Estrito**: uma chave desconhecida é um erro, e também o é um campo que a via escolhida não
  consegue honrar (um `hostname:` com `profile: k8s` é recusado pelo nome, não ignorado).
- **`${TAG}` / `${VAR:-default}`** são expandidos sobre os valores já analisados (um `${TAG}` num
  comentário não é avaliado). O `-t` é a tag do resultado **e** o `${TAG}`.
- **Os caminhos relativos são relativos à pasta do próprio `vm.yaml`**, diga o contexto o que disser.
- **`packages` precisa de `network: true`**: um build que chega à internet dá uma imagem diferente
  num dia diferente, por isso é opt-in e a recusa di-lo.
- `remove.paths` tem de ser absoluto, sem `..`, e nunca um directório de sistema de topo.

**Appliances** (`appliance:`). Algumas imagens não se descrevem como edições a uma cloud image: o
instalador do fabricante tem de correr (o Proxmox a partir do seu ISO, o OpenStack a puxar ~20 GiB de
containers). Para essas a receita nomeia um **builder**, não um caminho: `appliance: {builder:
proxmox, args: [pve, "9.2-1"]}` corre `scripts/appliances/build-proxmox.sh` (encontrado na pasta do
`vm.yaml` ou em qualquer pasta acima dela) com um `OUT_DIR` isolado junto ao store de imagens, toma o
único `*.qcow2` que ele deixa (um `.raw.qcow2` é ignorado), e regista-o com a semântica do `image vm
import` — `--appliance` a menos que `cloud_init: true`. Como o nome é validado (`[a-z0-9-]`) e
resolvido dentro de `scripts/appliances/`, um `vm.yaml` não consegue fazer o host correr um ficheiro
à sua escolha; `args` e `env` também são validados (sem `-` inicial, sem `PATH`/`LD_*`/`BASH_ENV`…).
Os campos que um builder decide por si (`packages`, `users`, `hostname`, `network`, `profile`…) são
recusados pelo nome.

As pastas por distro em `images/` (primeiro `images/ubuntu/`) levam cada uma um `vm.yaml`, o ficheiro
cloud-init, os artefactos e um README.

### Instruções

O `parse` falha fechado: uma instrução desconhecida é um erro que nomeia o conjunto suportado. Só
`FROM` pode vir primeiro. As continuações com `\` são juntadas; um `#` só começa um comentário no
início de uma linha (por isso `RUN sed 's/#x/y/'` não é estragado).

| Instrução | Âmbito | Efeito (`stage_ops` → `virt-customize`) |
|---|---|---|
| `FROM <ref> [AS <name>]` | abre um estágio | Ver *O que `FROM` aceita*. `as` não distingue maiúsculas de minúsculas. |
| `RUN <shell>` | passo | `--run-command` dentro do convidado; **offline** a menos que seja dado `--network`. |
| `COPY <src> <dst>` | passo | Copy-in a partir do contexto de build; `src` confinado ao contexto (`build::safe_join`). Exactamente dois argumentos. |
| `COPY --from=<stage> <src> <dst>` | passo | `virt-copy-out` do disco desse estágio para um directório de staging do host, e depois copy-in. O estágio tem de ter **nome e ser declarado antes** — verificado na análise. |
| `ENV KEY=value` | passo | Acrescenta `KEY=value` a `/etc/environment` (uma VM não tem config de imagem). Um par por linha; a chave e o valor são passados a uma shell **sem aspas**, por isso evita espaços e metacaracteres da shell. |
| `USER <name>` | passo | `useradd -m -s /bin/bash <name>` se a conta não existir. |
| `PASSWORD <user>:<password>` | passo | Define a password dessa conta. Fica gravada em todas as cópias da imagem. |
| `ROOTPASSWORD <password>` | passo | Define a password do root. A mesma ressalva. |
| `SSHKEY <user> <path-or-key>` | passo | Acrescenta a `/home/<user>/.ssh/authorized_keys` (`~/` é expandido). O valor tem de ser um ficheiro legível ou uma chave a começar por `ssh-`/`ecdsa-`. A conta tem de existir (cria-a primeiro com `USER`). |
| `CLOUDINIT <path>` | passo | Copia esse ficheiro (confinado ao contexto) para `/etc/cloud/cloud.cfg.d/`, **mantendo o nome do ficheiro**. O cloud-init só carrega desse directório ficheiros terminados em `.cfg`, por isso dá-lhe um nome como `99-myimage.cfg` — o nome `user-data.yaml` do scaffold não é apanhado tal como está escrito (não executado nesta revisão; a partir do caminho do código e do comportamento documentado do cloud-init). O `vm create` continua a acrescentar por cima o seu próprio seed NoCloud por instância. |
| `SIZE <n>G` | propriedade do estágio | `qemu-img resize` **antes** de qualquer passo correr — crescer depois de um `RUN` ter enchido o disco seria tarde demais, e é por isso que não é um passo. |
| `HOSTNAME <name>` | propriedade do estágio | Escreve `/etc/hostname` (primeira operação do estágio). |
| `VCPUS <n>` | imagem | Registado como `default_vcpus`. |
| `MEMORY <n>` | imagem | Registado como `default_memory`. |
| `HYPERVISOR <backend>` | imagem | Validado e canonicalizado por `delonix_vm::valid_backend_name` (`ch` → `cloud-hypervisor`); registado como `default_backend`. |
| `LABEL k=v` | imagem | **Analisado mas não registado** nos metadados da imagem nesta árvore. |

Não há `CMD`/`ENTRYPOINT`: uma VM arranca um init. O `CLOUDINIT` é o equivalente mais próximo.

### O que `FROM` aceita

O `classify_base` é puro e decide o tipo de referência:

1. **Um estágio anterior com nome** — ganha a tudo (`resolve_base`).
2. **`ubuntu:<rel>`, `debian:<rel>`, `rocky:<rel>`, `fedora:<rel>`** — primeiro a imagem base do
   próprio projecto para essa distro/release (cópia local, depois o registo oficial —
   `vmimage::official_distro_base`); se não houver nenhuma, a cloud image da própria distro,
   descarregada e verificada contra o ficheiro de checksum do publicador (`vmimage::download_base`).
3. **URL `http://` / `https://`** — descarregado; verificado contra `<url>.sha256` quando o
   publicador oferece um, caso contrário confiado só pelo TLS, e o build di-lo.
4. **Qualquer outra coisa** — uma imagem de VM que já esteja no store local (`delonix image vm ls`).
   Qualquer outro `name:tag` é tratado como uma tag local, não como uma distro desconhecida.

### O que é um estágio, e onde vai parar o resultado

Um estágio é um **disco inteiro**, não uma camada (`build`):

1. A base é **achatada** para `<work>/<stage>.qcow2` com `qemu-img convert` (sem backing file, para
   que o artefacto nunca dependa de a base ainda estar presente).
2. O `SIZE` é aplicado, e depois todos os passos correm numa só chamada a `virt-customize`
   (`--no-network` a menos que seja dado `--network`). O `vmimage::customize_args` acrescenta sempre
   um relabel de SELinux como último comando (e desliga o relabel adiado do próprio libguestfs), para
   que os convidados SELinux não arranquem com ficheiros sem etiqueta.
3. Os estágios com nome são mantidos para `COPY --from`; os sem nome não são endereçáveis. Só o
   **último** estágio se torna uma imagem.
4. A menos que seja dado `--no-compress`: `virt-sparsify --in-place` (melhor esforço), e depois
   `qemu-img convert -c -o compression_type=zstd`. zstd porque a imagem se torna o backing file só de
   leitura de cada VM criada a partir dela, por isso a velocidade de descompressão é uma propriedade
   de runtime.
5. O qcow2 é movido para `VmImageStore::qcow2_path(tag)` em `<DELONIX_ROOT>/vm-images/`, e um
   registo de metadados `VmImage` é guardado ao lado dele.

Metadados registados: `digest` e `size`; `default_vcpus`/`default_memory`/`default_backend` a partir
do ficheiro; `cloud_init: true`; `built_by: "delonix <version>"`; `distro` e `kernel_version`
**herdados** só quando o `FROM` final é uma imagem local (um URL não dá nada para herdar, e o código
recusa-se a adivinhar).

O `vm create` aplica os defaults registados só onde quem chama não decidiu: as flags
`--vcpus`/`--memory` ganham a `VCPUS`/`MEMORY`; para o backend, `--backend` > o `HYPERVISOR` da
imagem > `DELONIX_VM_BACKEND` > `vm default-backend` > auto-detecção (`resolve_vm_defaults` em
`cmd/vm.rs`, e `delonix_vm::create_with`).

**Offline por omissão, e porquê.** Um `RUN` que chega à internet produz uma imagem diferente
conforme a altura em que correu. O `--network` é opt-in porque a coisa mais comum que um VMfile quer
é instalar um pacote. Com `--network`, o appliance do libguestfs obtém a rede através do `passt`, que
tem armadilhas de host documentadas em [Construir microVMs](microvm-setup.md#6-troubleshooting).

**Espaço de rascunho.** O directório de trabalho é criado em `std::env::temp_dir()`
(`delonix-vmfile-<pid>`), ou seja `$TMPDIR` ou `/tmp`, e contém um disco achatado completo por
estágio. Aponta o `TMPDIR` para um sistema de ficheiros com espaço — o `/tmp` é muitas vezes um tmpfs
pequeno e é esvaziado no reinício. Um build que falha deixa esse directório para trás (observado
durante esta revisão); remove-o à mão.

### Build declarativo

O `kind: VirtualMachine` pode construir o seu próprio disco com `spec.build` (`VmBuildSpec`):
`context` (relativo ao directório do **manifesto**), `file` (por omissão `<context>/VMfile`), `tag`
(por omissão `<metadata.name>:latest`), `compress`, `network`. O `apply` chama o mesmo
`vmfile::build`. `disk` e `build` são mutuamente exclusivos.

### Exemplo trabalhado (verificado pelo parser)

```dockerfile
FROM my-base:1.0 AS builder
RUN make -C /src

FROM my-base:1.0
SIZE 20G
HOSTNAME web
COPY --from=builder /src/app /usr/local/bin/app
ENV APP_ENV=production
USER app
VCPUS 2
MEMORY 2G
HYPERVISOR ch
LABEL org.opencontainers.image.title=web
```

Sem nenhuma imagem chamada `my-base:1.0` no store de rascunho, o ficheiro é analisado e o build pára
na resolução da base — antes de qualquer trabalho em disco:

```text
$ delonix image vm build -t web:1.0 .
[1/2] builder: FROM my-base:1.0
error invalid argument: FROM my-base:1.0: no such local VM image, and it is not a URL nor a known cloud image (ubuntu:/debian:/rocky:) — see `delonix vm ls`
```

E as recusas do parser:

```text
VMfile:2: unknown instruction 'CMD' — supported: FROM RUN COPY ENV USER PASSWORD ROOTPASSWORD CLOUDINIT SSHKEY SIZE HOSTNAME VCPUS MEMORY HYPERVISOR LABEL
VMfile:2: no earlier stage named 'nope'
VMfile:2: invalid argument: unknown VM backend: 'vmware' (use 'cloud-hypervisor', 'libvirt')
--offline belong to the built-in golden recipe and mean nothing with a VMfile — the VMfile describes all of that itself
```

Um build completo (`virt-customize`, downloads, compressão) **não foi executado nesta revisão**.

---

## Comparação

| | Dockerfile (Docker/BuildKit) | Delonixfile (`delonix build`) | VMfile (`delonix image vm build`) |
|---|---|---|---|
| Resultado | imagem OCI | imagem OCI (pode fazer push, e pull pelo Docker) | qcow2 arrancável + metadados `VmImage` |
| Unidade de um estágio | camadas de sistema de ficheiros | um container de trabalho + rootfs | um disco inteiro achatado |
| `FROM` | imagem | imagem ou estágio anterior | distro:release, URL, imagem de VM local, ou estágio anterior |
| `RUN` executa em | container de build | container de trabalho via `exec` (userns rootless) | convidado, via `virt-customize` (offline por omissão) |
| `COPY --from` | sim | sim (nome ou índice) | sim (só estágios anteriores com nome; `virt-copy-out`) |
| Flags de `COPY` `--chown/--chmod` | sim | não | não |
| `ADD` de URL / extracção de arquivos | sim | não (`ADD` = `COPY`) | sem `ADD` |
| `RUN --mount` | secret, ssh, cache, bind, tmpfs | só `type=secret`/`type=cache` | nenhum |
| `--target` | sim | sim | não |
| `--platform` | sim | `linux/<arch>`, binfmt do host necessário | não (as imagens ficam amd64 — [ADR-0018](../../adr/0018-vm-images-stay-amd64.md)) |
| Cache de camadas | sim | só rootless, sem GC | nenhuma |
| `CMD`/`ENTRYPOINT`/`USER`/`ENV` | config da imagem | config da imagem | sem `CMD`; `USER` cria uma conta; `ENV` → `/etc/environment` |
| Indicações de recursos | — | `CPUS`/`MEMORY`/`SECURITY` registados, não aplicados | `VCPUS`/`MEMORY`/`HYPERVISOR` aplicados pelo `vm create` como defaults |
| Gate de vulnerabilidades | — | `SCAN` analisado, não imposto (usa `image scan --fail-on`) | — |
| Contas / chaves / passwords | — | — | `USER`, `SSHKEY`, `PASSWORD`, `ROOTPASSWORD` |
| Tamanho do disco | — | — | `SIZE` (antes de qualquer passo) |
| Instrução desconhecida | erro | erro | erro |

## Se mudares uma gramática

- Mantém os dois parsers a **falhar fechado**: uma instrução desconhecida é um erro, nunca uma linha
  saltada.
- Uma instrução nova entra com um teste unitário no `mod tests` do parser, uma linha nesta página e —
  se o scaffold a usar — com o teste do scaffold ainda a passar.
- Se uma instrução for analisada mas ainda não estiver ligada a um efeito (como estão hoje `SCAN`,
  `CPUS`/`MEMORY`/`SECURITY` e o `LABEL` do VMfile), di-lo aqui; um campo que o utilizador escreve e o
  motor ignora tem de ser documentado como tal ou removido.
- O parser do Delonixfile vive num crate de biblioteca (`delonix-oci`), por isso não pode imprimir; o
  parser do VMfile está no binário da CLI. Ver [Crates](crates.md).

---

**Seguinte:** [Construir microVMs](microvm-setup.md) — os pré-requisitos do host, os backends, as
imagens e os verbos de dia-2 para arrancar e testar microVMs.
